//! Приложение ferry — одно на любом устройстве. В фоне — агент: связь со всеми связанными
//! устройствами. В трее — режим этого устройства. Окно открывается по клику; закрытое окно
//! не останавливает агента, выход — из меню трея.

use ferry_net::{Agent, EndpointId, Mode, Options, PairingInfo, PeerView, Role, Snapshot};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_updater::UpdaterExt;

type Reply<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn device(id: Option<String>) -> Reply<Option<EndpointId>> {
    id.map(|id| id.parse().map_err(|_| format!("не ключ устройства: {id}")))
        .transpose()
}

#[tauri::command]
fn state(agent: State<Agent>) -> Snapshot {
    agent.snapshot()
}

#[tauri::command]
fn open_pairing(agent: State<Agent>) -> PairingInfo {
    agent.open_pairing()
}

#[tauri::command]
fn close_pairing(agent: State<Agent>) {
    agent.close_pairing()
}

#[tauri::command]
async fn pair(agent: State<'_, Agent>, code: String) -> Reply<PeerView> {
    agent.pair(&code).await.map_err(|e| format!("{e:#}"))
}

/// Роль этого устройства (`id` пуст) или связанного.
#[tauri::command]
async fn set_role(agent: State<'_, Agent>, id: Option<String>, role: Role) -> Reply<()> {
    agent
        .set_role(device(id)?, role)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn rename(agent: State<'_, Agent>, id: Option<String>, name: String) -> Reply<()> {
    agent
        .rename(device(id)?, &name)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn set_mode(agent: State<Agent>, mode: String) -> Reply<()> {
    let mode = Mode::parse(&mode).ok_or_else(|| format!("режим {mode:?}"))?;
    agent.set_mode(mode).map_err(err)
}

#[tauri::command]
fn forget(agent: State<Agent>, id: String) -> Reply<()> {
    agent.forget(device(Some(id))?.unwrap()).map_err(err)
}

#[tauri::command]
fn autostart(app: AppHandle) -> Reply<bool> {
    app.autolaunch().is_enabled().map_err(err)
}

/// Запуск при входе. Узлу он нужен всегда, поэтому включён сразу; выключил человек — помним.
#[tauri::command]
fn set_autostart(app: AppHandle, on: bool) -> Reply<()> {
    let marker = ferry_core::ferry_home().map_err(err)?.join("no-autostart");
    if on {
        let _ = std::fs::remove_file(&marker);
        app.autolaunch().enable().map_err(err)
    } else {
        std::fs::write(&marker, "").map_err(err)?;
        app.autolaunch().disable().map_err(err)
    }
}

/// CLI `ferry` — рядом с приложением (в установщике он едет вместе с ним).
fn cli_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cli = exe
        .parent()?
        .join(if cfg!(windows) { "ferry.exe" } else { "ferry" });
    cli.exists().then_some(cli)
}

/// Куда на unix класть ссылку `ferry`: первый каталог из PATH терминала, куда можно писать.
#[cfg(unix)]
fn link_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> =
        vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into()];
    if let Ok(home) = ferry_core::home() {
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

/// Команда `ferry` в терминале: где CLI и стоит ли он уже.
#[derive(serde::Serialize)]
struct Cli {
    path: Option<String>,
    /// Где команда доступна (ссылка или каталог в PATH); пусто — не поставлена.
    installed: Option<String>,
}

#[tauri::command]
fn cli() -> Cli {
    let Some(path) = cli_path() else {
        return Cli {
            path: None,
            installed: None,
        };
    };
    #[cfg(unix)]
    let installed = link_dirs()
        .into_iter()
        .map(|dir| dir.join("ferry"))
        .find(|link| std::fs::read_link(link).is_ok_and(|target| target == path));
    #[cfg(windows)]
    let installed = user_path()
        .ok()
        .filter(|p| {
            p.split(';')
                .any(|d| std::path::Path::new(d) == path.parent().unwrap())
        })
        .map(|_| path.parent().unwrap().to_path_buf());
    Cli {
        path: Some(path.display().to_string()),
        installed: installed.map(|p| p.display().to_string()),
    }
}

/// Поставить команду `ferry`: на unix — ссылка в каталог из PATH, на Windows — каталог
/// приложения в PATH пользователя (без прав администратора). Новые терминалы её увидят.
#[tauri::command]
fn install_cli() -> Reply<String> {
    let path =
        cli_path().ok_or("CLI ferry рядом с приложением нет — это сборка без установщика")?;
    #[cfg(unix)]
    {
        let home_bin = ferry_core::home().map_err(err)?.join(".local/bin");
        for dir in link_dirs() {
            if dir == home_bin {
                std::fs::create_dir_all(&dir).map_err(err)?;
            }
            let link = dir.join("ferry");
            let _ = std::fs::remove_file(&link);
            if std::os::unix::fs::symlink(&path, &link).is_ok() {
                let note = if dir == home_bin {
                    " — добавь ~/.local/bin в PATH, если его там нет"
                } else {
                    ""
                };
                return Ok(format!("Команда ferry: {}{note}", link.display()));
            }
        }
        Err("не вышло положить ссылку ни в один каталог".into())
    }
    #[cfg(windows)]
    {
        let dir = path.parent().unwrap().display().to_string();
        let current = user_path().map_err(err)?;
        if !current.split(';').any(|d| d == dir) {
            let joined = if current.is_empty() {
                dir.clone()
            } else {
                format!("{current};{dir}")
            };
            powershell(&format!(
                "[Environment]::SetEnvironmentVariable('Path', '{}', 'User')",
                joined.replace('\'', "''")
            ))
            .map_err(err)?;
        }
        Ok(format!(
            "Команда ferry: {dir} в PATH — открой новый терминал"
        ))
    }
}

/// PATH пользователя из реестра — без того, что добавила система.
#[cfg(windows)]
fn user_path() -> std::io::Result<String> {
    powershell("[Environment]::GetEnvironmentVariable('Path', 'User')")
        .map(|s| s.trim().to_string())
}

#[cfg(windows)]
fn powershell(script: &str) -> std::io::Result<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Версия новее этой, если есть.
#[tauri::command]
async fn check_update(app: AppHandle) -> Reply<Option<String>> {
    let update = app.updater().map_err(err)?.check().await.map_err(err)?;
    Ok(update.map(|u| u.version))
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Reply<String> {
    update(&app).await
}

/// Обновить все связанные устройства на связи (и пульты), а затем это — оно перезапустится.
#[tauri::command]
async fn update_all(app: AppHandle, agent: State<'_, Agent>) -> Reply<String> {
    let mut said = Vec::new();
    for peer in agent.snapshot().peers.into_iter().filter(|p| p.online) {
        let line = match agent.device(&peer.id) {
            Ok(device) => device.update().await.unwrap_or_else(|fail| fail.message),
            Err(fail) => fail.message,
        };
        said.push(format!("{}: {line}", peer.name));
    }
    said.push(update(&app).await?);
    Ok(said.join("\n"))
}

/// Новая версия — из релизов на GitHub (`plugins.updater` в tauri.conf.json): скачать (подпись
/// сверяется публичным ключом), ответить и через секунду поставить и перезапуститься. На Windows
/// установщик закрывает приложение сам — ответ должен успеть уйти.
async fn update(app: &AppHandle) -> Reply<String> {
    let Some(update) = app.updater().map_err(err)?.check().await.map_err(err)? else {
        return Ok(format!("ferry {} — последняя версия", ferry_core::VERSION));
    };
    let bytes = update.download(|_, _| {}, || {}).await.map_err(err)?;
    let said = format!(
        "ferry {} → {}: ставлю и перезапускаюсь",
        ferry_core::VERSION,
        update.version
    );
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        match update.install(&bytes) {
            Ok(()) => app.restart(),
            Err(e) => tracing::warn!("обновление не встало: {e}"),
        }
    });
    Ok(said)
}

/// Связанное устройство на связи — по ключу.
fn on_line(agent: &Agent, id: &str) -> Reply<ferry_net::Device> {
    agent.device(id).map_err(err)
}

/// Свойства устройства: пары «подпись — значение» и предупреждения.
#[derive(serde::Serialize)]
struct Detail {
    properties: Vec<(String, String)>,
    warnings: Vec<String>,
}

#[tauri::command]
async fn detail(agent: State<'_, Agent>, id: String) -> Reply<Detail> {
    let pairs = on_line(&agent, &id)?.info().await.map_err(err)?;
    Ok(Detail {
        properties: ferry_core::info::describe(&pairs),
        warnings: ferry_core::info::warnings(&pairs),
    })
}

/// Снимок экрана устройства как data-URL — показать прямо в окне.
#[tauri::command]
async fn screenshot(agent: State<'_, Agent>, id: String) -> Reply<String> {
    use base64::Engine;
    let file =
        ferry_net::files::temp(&ferry_core::ferry_home().map_err(err)?, "png").map_err(err)?;
    on_line(&agent, &id)?.shot(&file).await.map_err(err)?;
    let bytes = std::fs::read(&file).map_err(err);
    let _ = std::fs::remove_file(&file);
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes?)
    ))
}

#[tauri::command]
async fn input(agent: State<'_, Agent>, id: String, args: Vec<String>) -> Reply<String> {
    on_line(&agent, &id)?.input(args).await.map_err(err)
}

#[derive(serde::Serialize)]
struct JobRow {
    name: String,
    label: String,
    ago: String,
    running: bool,
    cmd: String,
}

#[tauri::command]
async fn jobs(agent: State<'_, Agent>, id: String) -> Reply<Vec<JobRow>> {
    let jobs = on_line(&agent, &id)?.jobs().await.map_err(err)?;
    Ok(jobs
        .into_iter()
        .map(|job| JobRow {
            label: job.label(),
            ago: job.ago(),
            running: job.running(),
            name: job.name,
            cmd: job.cmd,
        })
        .collect())
}

/// Лог задачи целиком (без слежения).
#[tauri::command]
async fn job_log(agent: State<'_, Agent>, id: String, name: String) -> Reply<String> {
    let mut rx = on_line(&agent, &id)?.log(&name, false).await.map_err(err)?;
    let mut log = Vec::new();
    while let Ok(Some(output)) = rx.recv().await {
        match output {
            ferry_net::proto::Output::Out(data) | ferry_net::proto::Output::Err(data) => {
                log.extend(data)
            }
            ferry_net::proto::Output::Exit(_) => break,
            ferry_net::proto::Output::Fail(fail) => return Err(fail.message),
        }
    }
    Ok(String::from_utf8_lossy(&log).into_owned())
}

/// Задача из окна — в доме устройства: проекта у окна нет.
#[tauri::command]
async fn start_job(
    agent: State<'_, Agent>,
    id: String,
    name: String,
    cmd: String,
) -> Reply<String> {
    on_line(&agent, &id)?
        .start(&name, &cmd, ferry_net::Dir::Home)
        .await
        .map_err(err)
}

#[tauri::command]
async fn kill_job(agent: State<'_, Agent>, id: String, name: String) -> Reply<String> {
    on_line(&agent, &id)?.kill(&name).await.map_err(err)
}

#[tauri::command]
async fn power(agent: State<'_, Agent>, id: String, action: String) -> Reply<String> {
    on_line(&agent, &id)?.power(&action).await.map_err(err)
}

#[tauri::command]
fn version() -> String {
    ferry_core::VERSION.into()
}

/// Доступы macOS для снимков и ввода; на других системах их нет — `None`.
#[derive(serde::Serialize)]
struct Permissions {
    screen: bool,
    input: bool,
}

#[tauri::command]
fn permissions() -> Option<Permissions> {
    #[cfg(target_os = "macos")]
    {
        let (screen, input) = ferry_core::screen::access();
        Some(Permissions { screen, input })
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Системный запрос и раздел настроек с этим доступом: `screen` или `input`.
#[tauri::command]
fn ask_permission(kind: String) -> Reply<()> {
    #[cfg(target_os = "macos")]
    return ferry_core::screen::ask_access(&kind).map_err(err);
    #[cfg(not(target_os = "macos"))]
    Err(format!("доступ {kind} нужен только на macOS"))
}

/// Запись экрана macOS применяет после перезапуска приложения.
#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

/// Окно из конфига (1100×720) на маленьком экране уходит под панель задач: ужимаем до рабочей
/// области монитора и ставим по центру.
fn fit_to_screen(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    let Some(monitor) = window.current_monitor()? else {
        return Ok(());
    };
    let area = monitor.work_area().size;
    let (outer, inner) = (window.outer_size()?, window.inner_size()?);
    // Рамка и заголовок окна — разница внешнего и внутреннего размера.
    let (frame_w, frame_h) = (outer.width - inner.width, outer.height - inner.height);
    let w = inner.width.min(area.width * 95 / 100 - frame_w);
    let h = inner.height.min(area.height * 95 / 100 - frame_h);
    if (w, h) != (inner.width, inner.height) {
        window.set_size(tauri::PhysicalSize::new(w, h))?;
        window.center()?;
    }
    Ok(())
}

/// Окно пульта: показать, а нет — создать. `pairing` — сразу на связывание.
fn show(app: &AppHandle, pairing: bool) {
    let window = match app.get_webview_window("main") {
        Some(window) => {
            if pairing {
                let _ = app.emit("open-pairing", ());
            }
            window
        }
        None => match main_window(app, pairing) {
            Ok(window) => window,
            Err(e) => {
                tracing::warn!("окно ferry не открылось: {e}");
                return;
            }
        },
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Окно пульта создаётся при первом открытии, а не при запуске: WebView на маке при создании
/// просит активировать приложение, и macOS исполняет это на первом показанном окне — им
/// оказывалась рамка управления, и фокус уходил у того, чем управляют.
fn main_window(app: &AppHandle, pairing: bool) -> tauri::Result<tauri::WebviewWindow> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .cloned()
        .unwrap_or_default();
    // Событие `open-pairing` новое окно ещё не услышит — скажем ему при загрузке.
    let script = if pairing {
        "window.FERRY_PAIRING = true;"
    } else {
        ""
    };
    let window = tauri::WebviewWindowBuilder::from_config(app, &config)?
        .initialization_script(script)
        .build()?;
    fit_to_screen(&window)?;
    Ok(window)
}

/// Рамка по краю каждого экрана, пока устройством управляют: человеку видно, что за компьютером
/// сейчас агент. Клики идут сквозь неё, фокус она не берёт, на снимках (`ferry shot`, запись
/// экрана) и в `ferry windows` её нет. `who` — кто управляет; `None` — убрать.
fn frame(app: &AppHandle, who: Option<&str>) -> tauri::Result<()> {
    let text = who.map(|who| format!("ferry: управляет {who} · остановить — в меню ferry в трее"));
    #[cfg(target_os = "macos")]
    return app.run_on_main_thread(move || mac_frame::set(text.as_deref()));
    #[cfg(not(target_os = "macos"))]
    webview_frame(app, text.as_deref())
}

/// Окна рамки — заново на каждый показ: показ уже созданного окна на Windows его активирует, а
/// новое с `focused(false)` показывается без активации.
#[cfg(not(target_os = "macos"))]
fn webview_frame(app: &AppHandle, text: Option<&str>) -> tauri::Result<()> {
    static SHOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    for (label, window) in app.webview_windows() {
        if label.starts_with("frame-") {
            // destroy, а не close: close у окон ferry только прячет их.
            window.destroy()?;
        }
    }
    let Some(text) = text else {
        return Ok(());
    };
    let script = format!(
        "window.FERRY_WHO = {};",
        serde_json::to_string(text).unwrap_or_default()
    );
    // Своё имя на каждый показ: уничтожаемое окно может ещё не уйти из списка.
    let shown = SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for (i, monitor) in app.available_monitors()?.iter().enumerate() {
        let (area, scale) = (monitor.work_area(), monitor.scale_factor());
        let url = tauri::WebviewUrl::App("frame.html".into());
        let window = tauri::WebviewWindowBuilder::new(app, format!("frame-{shown}-{i}"), url)
            .title("")
            .initialization_script(&script)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .focusable(false)
            .content_protected(true)
            .position(
                f64::from(area.position.x) / scale,
                f64::from(area.position.y) / scale,
            )
            .inner_size(
                f64::from(area.size.width) / scale,
                f64::from(area.size.height) / scale,
            )
            .build()?;
        window.set_ignore_cursor_events(true)?;
        // Логические координаты на экранах с разным масштабом неточны — ставим физические.
        window.set_position(area.position)?;
        window.set_size(area.size)?;
    }
    Ok(())
}

/// Рамка на маке — свои панели AppKit, а не окна Tauri: WebView при создании, а любое окно при
/// первом показе после запуска активируют приложение — рамка уводила бы фокус у того, чем
/// управляют. Неактивирующая панель (`NonactivatingPanel`) приложение не активирует.
#[cfg(target_os = "macos")]
mod mac_frame {
    use std::cell::RefCell;

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFloatingWindowLevel, NSFont, NSPanel,
        NSScreen, NSTextField, NSTitlePosition, NSView, NSWindowCollectionBehavior,
        NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSString};

    thread_local! {
        static PANELS: RefCell<Vec<Retained<NSPanel>>> = RefCell::default();
    }

    /// Рамка с надписью `text` на каждом экране; `None` — убрать. Только с главного потока.
    pub fn set(text: Option<&str>) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        PANELS.with_borrow_mut(|panels| {
            for panel in panels.drain(..) {
                panel.orderOut(None);
            }
            let Some(text) = text else {
                return;
            };
            for screen in NSScreen::screens(mtm) {
                panels.push(panel(mtm, screen.visibleFrame(), text));
            }
        });
    }

    fn panel(mtm: MainThreadMarker, area: NSRect, text: &str) -> Retained<NSPanel> {
        // #f25c3a — коралл ferry.
        let coral = NSColor::colorWithSRGBRed_green_blue_alpha(0.949, 0.361, 0.227, 1.0);
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            area,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // Панель держит `PANELS`; закрытие не должно освобождать её второй раз.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(true);
        panel.setLevel(NSFloatingWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setSharingType(NSWindowSharingType::None);

        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), area.size);
        let root = NSView::initWithFrame(NSView::alloc(mtm), bounds);
        let border = NSBox::initWithFrame(NSBox::alloc(mtm), bounds);
        border.setBoxType(NSBoxType::Custom);
        border.setTitlePosition(NSTitlePosition::NoTitle);
        border.setBorderColor(&coral);
        border.setBorderWidth(4.0);
        border.setFillColor(&NSColor::clearColor());
        root.addSubview(&border);
        let label = NSTextField::labelWithString(&NSString::from_str(&format!("  {text}  ")), mtm);
        label.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setDrawsBackground(true);
        label.setBackgroundColor(Some(&coral));
        label.sizeToFit();
        let size = label.frame().size;
        label.setFrameOrigin(NSPoint::new(
            ((area.size.width - size.width) / 2.0).round(),
            area.size.height - size.height,
        ));
        root.addSubview(&label);
        panel.setContentView(Some(&root));
        panel.orderFrontRegardless();
        panel
    }
}

/// Кто управляет устройством для рамки: действует на рабочем столе сейчас или только что.
/// В других режимах ввод и снимки и так отклоняются.
fn controller(agent: &Agent) -> Option<String> {
    (agent.snapshot().me.mode == Mode::Active)
        .then(|| agent.acting())
        .flatten()
}

/// Иконки трея по режиму (`apps/ferry/scripts/icons.mjs`): коралл, фиолетовый, серое кольцо.
fn tray_icon(mode: Mode) -> tauri::image::Image<'static> {
    let bytes: &[u8] = match mode {
        Mode::Active => include_bytes!("../icons/tray/active-32.png"),
        Mode::NoDesktop => {
            include_bytes!("../icons/tray/no-desktop-32.png")
        }
        Mode::Paused => include_bytes!("../icons/tray/paused-32.png"),
    };
    tauri::image::Image::from_bytes(bytes).expect("иконка трея — PNG из репозитория")
}

/// Иконка трея, пока устройством управляют: белый круг с коралловым маршрутом.
fn tray_controlled() -> tauri::image::Image<'static> {
    tauri::image::Image::from_bytes(include_bytes!("../icons/tray/controlled-32.png"))
        .expect("иконка трея — PNG из репозитория")
}

const TRAY: &str = "ferry";

/// Трей: режим этого устройства — его выбирает человек за ним; открыть окно; выйти.
struct Tray {
    /// Сам значок не держим: владеет им Tauri. Своя копия пережила бы `cleanup_before_exit`
    /// (выход, установка обновления), и на Windows в трее оставался бы мёртвый значок.
    app: AppHandle,
    modes: Vec<(Mode, CheckMenuItem<tauri::Wry>)>,
    /// «Остановить управление» — активен, пока устройством управляют.
    stop: MenuItem<tauri::Wry>,
    /// Режим и управляют ли — что сейчас показано.
    look: std::sync::Mutex<Option<(Mode, bool)>>,
}

impl Tray {
    fn build(app: &AppHandle) -> tauri::Result<Tray> {
        let modes = Mode::ALL
            .into_iter()
            .map(|mode| {
                let id = format!("mode:{}", mode.name());
                CheckMenuItem::with_id(app, id, label(mode), true, false, None::<&str>)
                    .map(|item| (mode, item))
            })
            .collect::<tauri::Result<Vec<_>>>()?;
        let stop = MenuItem::with_id(
            app,
            "stop-control",
            "Остановить управление",
            false,
            None::<&str>,
        )?;
        let open = MenuItem::with_id(app, "open", "Открыть ferry", true, None::<&str>)?;
        let pair = MenuItem::with_id(app, "pair", "Связать устройство…", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Выйти из ferry", true, None::<&str>)?;
        let separator = || PredefinedMenuItem::separator(app);
        let s0 = separator()?;
        let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&stop, &s0];
        for (_, item) in &modes {
            items.push(item);
        }
        let (s1, s2) = (separator()?, separator()?);
        items.extend([
            &s1 as &dyn tauri::menu::IsMenuItem<_>,
            &open,
            &pair,
            &s2,
            &quit,
        ]);
        let menu = Menu::with_items(app, &items)?;
        TrayIconBuilder::with_id(TRAY)
            .icon(tray_icon(Mode::Active))
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(|app, event| match event.id().as_ref() {
                "open" => show(app, false),
                "pair" => show(app, true),
                "quit" => app.exit(0),
                // Человек за устройством забирает рабочий стол: ввод, снимки и окна отклоняются.
                "stop-control" => {
                    let _ = app.state::<Agent>().set_mode(Mode::NoDesktop);
                }
                id => {
                    if let Some(mode) = id.strip_prefix("mode:").and_then(Mode::parse) {
                        let _ = app.state::<Agent>().set_mode(mode);
                    }
                }
            })
            .build(app)?;
        Ok(Tray {
            app: app.clone(),
            modes,
            stop,
            look: std::sync::Mutex::default(),
        })
    }

    /// Отметка, иконка и подсказка — по текущему режиму и тому, сколько устройств на связи.
    /// Ответ — через сколько снять отметку «управляют», если она стоит.
    fn update(&self, snapshot: &Snapshot, control: Option<String>) -> Option<std::time::Duration> {
        let icon = self.app.tray_by_id(TRAY)?;
        let mode = snapshot.me.mode;

        let look = (mode, control.is_some());
        if self.look.lock().unwrap().replace(look) != Some(look) {
            for (m, item) in &self.modes {
                let _ = item.set_checked(*m == mode);
            }
            let image = if control.is_some() {
                tray_controlled()
            } else {
                tray_icon(mode)
            };
            let _ = icon.set_icon(Some(image));
            let _ = self.stop.set_enabled(control.is_some());
        }
        let online = snapshot.peers.iter().filter(|p| p.online).count();
        // Подсказка трея в Windows — до 128 знаков.
        let state = match &control {
            Some(who) => format!("управляет {who}"),
            None => label(mode).to_string(),
        };
        let tip = format!(
            "ferry — {}: {state}\nна связи {online} из {}",
            snapshot.me.name,
            snapshot.peers.len()
        );
        let _ = icon.set_tooltip(Some(tip));
        control.map(|_| std::time::Duration::from_secs(1))
    }
}

fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Active => "Работает",
        Mode::NoDesktop => "Не трогать рабочий стол",
        Mode::Paused => "Отключён от других устройств",
    }
}

/// Лог дописывается в конец; больше 5 МБ — старый уезжает в `ferry.log.1`.
// ponytail: ротация только при запуске; если за одну сессию лог разрастётся — tracing-appender.
fn open_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 5 << 20) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// В релизе паника — сразу abort (`panic = "abort"`), а stderr у приложения никто не видит:
/// сообщение и место пишем ещё и в лог, его покажет `ferry logs`.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("паника: {info}");
        default(info);
    }));
}

pub fn run() {
    // Лог — в ~/.ferry/logs/ferry.log (его хвост отдаёт `ferry logs`, в том числе издалека) и в
    // stderr. `RUST_LOG=ferry_net=debug` — подробности связи.
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "warn,ferry_net=info".into())
    };
    match ferry_core::ferry_home().and_then(|dir| open_log(&ferry_core::app_log(&dir))) {
        Ok(file) => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file).and(std::io::stderr))
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter()).init(),
    }
    log_panics();
    let builder = tauri::Builder::default();
    // Запуск (вход в систему, перезапуск после обновления) не забирает фокус: иначе macOS
    // исполняет эту активацию на первом показанном окне — рамке управления (см. `main_window`).
    #[cfg(target_os = "macos")]
    let builder = builder.activate_ignoring_other_apps(false);
    builder
        // Второй запуск (ярлык, автозапуск) не поднимает второго агента — показывает окно.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show(app, false)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let dir = ferry_core::ferry_home()?;
            // Обновить это устройство по команде с другого (`ferry update <цели>`).
            let handle = app.handle().clone();
            let hook: ferry_net::agent::UpdateHook = std::sync::Arc::new(move || {
                let app = handle.clone();
                Box::pin(async move { update(&app).await })
            });
            let options = Options {
                update: Some(hook),
                ..Options::new(dir.clone())
            };
            let agent = tauri::async_runtime::block_on(Agent::start(options))?;
            app.manage(agent.clone());
            // CLI на этой машине (`ferry devices`, `ferry pair` …) ходит сюда.
            tauri::async_runtime::spawn(ferry_net::control::serve(agent.clone(), dir.clone()));

            // Живёт в трее, а не в доке.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let tray = Tray::build(app.handle())?;
            let mut expires = tray.update(&agent.snapshot(), controller(&agent));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut changes = agent.subscribe();
                loop {
                    // Ждать изменений, а пока устройством управляют — не дольше, чем до конца
                    // отметки: тогда трей вернётся к обычному виду сам.
                    let wait = expires.unwrap_or(std::time::Duration::from_secs(3600));
                    let changed = match tokio::time::timeout(wait, changes.changed()).await {
                        Ok(Ok(())) => true,
                        Ok(Err(_)) => break,
                        Err(_) => false,
                    };
                    let snapshot = agent.snapshot();
                    expires = tray.update(&snapshot, controller(&agent));
                    if changed {
                        let _ = handle.emit("state", snapshot);
                    }
                }
            });

            // Рамка: начало и конец действий событий не шлют — раз в секунду смотрим, кто управляет.
            let (agent, handle) = (app.state::<Agent>().inner().clone(), app.handle().clone());
            tauri::async_runtime::spawn(async move {
                let mut shown = None;
                loop {
                    let who = controller(&agent);
                    if who != shown {
                        if let Err(e) = frame(&handle, who.as_deref()) {
                            tracing::warn!("рамка управления: {e}");
                        }
                        shown = who;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });

            if !dir.join("no-autostart").exists()
                && !app.autolaunch().is_enabled().unwrap_or(true)
                && let Err(e) = app.autolaunch().enable()
            {
                tracing::warn!("автозапуск не включился: {e}");
            }
            // При входе в систему (автозапуск) окно не нужно — только трей.
            if !std::env::args().any(|a| a == "--hidden") {
                show(app.handle(), false);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            state,
            open_pairing,
            close_pairing,
            pair,
            set_role,
            rename,
            set_mode,
            forget,
            autostart,
            set_autostart,
            check_update,
            install_update,
            update_all,
            permissions,
            ask_permission,
            restart_app,
            cli,
            install_cli,
            detail,
            screenshot,
            input,
            jobs,
            job_log,
            start_job,
            kill_job,
            power,
            version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn panics_land_in_the_log() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Buffer(writer.clone()))
            .finish();
        super::log_panics();
        tracing::subscriber::with_default(subscriber, || {
            let _ = std::panic::catch_unwind(|| panic!("проверка лога"));
        });
        let log = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("паника: ") && log.contains("проверка лога"),
            "{log}"
        );
    }
}
