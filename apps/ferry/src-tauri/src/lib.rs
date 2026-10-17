//! Приложение ferry — одно на любом устройстве. В фоне — агент: связь со всеми связанными
//! устройствами. В трее — режим этого устройства. Окно открывается по клику; закрытое окно
//! не останавливает агента, выход — из меню трея.

use ferry_net::{Agent, EndpointId, Mode, Options, PairingInfo, PeerView, Role, Snapshot};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

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

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
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

/// Трей: режим этого устройства — его выбирает человек за ним; открыть окно; выйти.
struct Tray {
    icon: TrayIcon,
    modes: Vec<(Mode, CheckMenuItem<tauri::Wry>)>,
    mode: std::sync::Mutex<Option<Mode>>,
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
        let open = MenuItem::with_id(app, "open", "Открыть ferry", true, None::<&str>)?;
        let pair = MenuItem::with_id(app, "pair", "Связать устройство…", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Выйти из ferry", true, None::<&str>)?;
        let separator = || PredefinedMenuItem::separator(app);
        let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = Vec::new();
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
        let icon = TrayIconBuilder::with_id("ferry")
            .icon(tray_icon(Mode::Active))
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(|app, event| match event.id().as_ref() {
                "open" => show(app),
                "pair" => {
                    show(app);
                    let _ = app.emit("open-pairing", ());
                }
                "quit" => app.exit(0),
                id => {
                    if let Some(mode) = id.strip_prefix("mode:").and_then(Mode::parse) {
                        let _ = app.state::<Agent>().set_mode(mode);
                    }
                }
            })
            .build(app)?;
        Ok(Tray {
            icon,
            modes,
            mode: std::sync::Mutex::default(),
        })
    }

    /// Отметка, иконка и подсказка — по текущему режиму и тому, сколько устройств на связи.
    fn update(&self, snapshot: &Snapshot) {
        let mode = snapshot.me.mode;
        if self.mode.lock().unwrap().replace(mode) != Some(mode) {
            for (m, item) in &self.modes {
                let _ = item.set_checked(*m == mode);
            }
            let _ = self.icon.set_icon(Some(tray_icon(mode)));
        }
        let online = snapshot.peers.iter().filter(|p| p.online).count();
        let tip = format!(
            "ferry — {}: {}\nна связи {online} из {}",
            snapshot.me.name,
            label(mode),
            snapshot.peers.len()
        );
        let _ = self.icon.set_tooltip(Some(tip));
    }
}

fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Active => "Работает",
        Mode::NoDesktop => "Не трогать рабочий стол",
        Mode::Paused => "Отключён от других устройств",
    }
}

pub fn run() {
    // Лог в stderr: `RUST_LOG=ferry_net=debug` — подробности связи.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,ferry_net=info".into()),
        )
        .init();
    tauri::Builder::default()
        // Второй запуск (ярлык, автозапуск) не поднимает второго агента — показывает окно.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .setup(|app| {
            let dir = ferry_core::ferry_home()?;
            let agent = tauri::async_runtime::block_on(Agent::start(Options::new(dir.clone())))?;
            app.manage(agent.clone());
            // CLI на этой машине (`ferry devices`, `ferry pair` …) ходит сюда.
            tauri::async_runtime::spawn(ferry_net::control::serve(agent.clone(), dir.clone()));

            // Живёт в трее, а не в доке.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let tray = Tray::build(app.handle())?;
            tray.update(&agent.snapshot());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut changes = agent.subscribe();
                while changes.changed().await.is_ok() {
                    let snapshot = agent.snapshot();
                    tray.update(&snapshot);
                    let _ = handle.emit("state", snapshot);
                }
            });

            if !dir.join("no-autostart").exists()
                && !app.autolaunch().is_enabled().unwrap_or(true)
                && let Err(e) = app.autolaunch().enable()
            {
                tracing::warn!("автозапуск не включился: {e}");
            }
            if let Some(window) = app.get_webview_window("main") {
                fit_to_screen(&window)?;
                // При входе в систему (автозапуск) окно не нужно — только трей.
                if !std::env::args().any(|a| a == "--hidden") {
                    window.show()?;
                }
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
