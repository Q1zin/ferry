use crate::{Agent, Reply, err, on_line};
use base64::Engine as _;
use ferry_core::screen::{Window, admin::Action};
use ferry_net::{Device, proto::DeskAction};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tauri::{State, ipc::Channel};
use tokio::{sync::Semaphore, task::AbortHandle};

struct Stream {
    task: AbortHandle,
    ack: Arc<Semaphore>,
}
static STREAMS: OnceLock<Mutex<HashMap<u64, Stream>>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
fn streams() -> &'static Mutex<HashMap<u64, Stream>> {
    STREAMS.get_or_init(Mutex::default)
}

#[tauri::command]
pub async fn start_stream(
    agent: State<'_, Agent>,
    id: String,
    channel: Channel<serde_json::Value>,
) -> Reply<u64> {
    let device = on_line(&agent, &id)?;
    let mut rx = device
        .screen_stream(ferry_core::screen::live::Request {
            fps: 10,
            max_width: 1600,
            quality: 70,
            target: None,
        })
        .await
        .map_err(err)?;
    let mut entries = streams().lock().map_err(err)?;
    if entries.len() >= 4 {
        return Err("Закрой лишнюю трансляцию (максимум 4)".into());
    }
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let ack = Arc::new(Semaphore::new(0));
    let wait = ack.clone();
    let task = tauri::async_runtime::spawn(async move {
        let result: Reply<()> = async {
            loop {
                // A timeout drops this stream; no cancelled irpc recv is ever resumed.
                let frame = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                    .await
                    .map_err(err)?
                    .map_err(err)?
                    .ok_or("Трансляция завершена")?
                    .map_err(err)?;
                channel
                    .send(serde_json::json!({"frame":frame}))
                    .map_err(err)?;
                let permit =
                    tokio::time::timeout(std::time::Duration::from_secs(10), wait.acquire())
                        .await
                        .map_err(err)?
                        .map_err(err)?;
                permit.forget();
            }
        }
        .await;
        if let Err(e) = result {
            let _ = channel.send(serde_json::json!({"error":e}));
        }
        if let Ok(mut entries) = streams().lock() {
            entries.remove(&token);
        }
    });
    entries.insert(
        token,
        Stream {
            task: task.inner().abort_handle(),
            ack,
        },
    );
    Ok(token)
}
#[tauri::command]
pub fn stream_ack(token: u64) {
    if let Ok(entries) = streams().lock()
        && let Some(stream) = entries.get(&token)
        && stream.ack.available_permits() == 0
    {
        stream.ack.add_permits(1);
    }
}
#[tauri::command]
pub fn stop_stream(token: u64) {
    if let Ok(mut entries) = streams().lock()
        && let Some(stream) = entries.remove(&token)
    {
        stream.task.abort();
    }
}

#[tauri::command]
pub async fn control_screen(
    agent: State<'_, Agent>,
    id: String,
    args: Option<Vec<String>>,
) -> Reply<()> {
    let device = on_line(&agent, &id)?;
    let session = crate::DESKTOP_SESSION;
    if let Some(args) = args {
        device
            .desk(
                session,
                ferry_net::proto::DeskAction::HoldNote {
                    seconds: 30,
                    note: "Ручное управление из пульта ferry".into(),
                },
            )
            .await
            .map_err(err)?;
        if !args.is_empty() {
            device.input(args).await.map_err(err)?;
        }
    } else {
        device.release_control(session).await.map_err(err)?;
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdminScreenAction {
    List,
    Shot,
    Focus,
    Input,
    Release,
}

#[derive(serde::Deserialize)]
pub struct AdminWindowRef {
    id: i64,
    pid: i64,
}

impl AdminWindowRef {
    fn matches(&self, window: &Window) -> bool {
        self.id == window.id() && self.pid == window.pid()
    }

    async fn probe(&self, device: &Device) -> Reply<Window> {
        let wire = device
            .admin_input(crate::DESKTOP_SESSION, Some(self.id), Action::Probe)
            .await
            .map_err(err)?;
        let window = serde_json::from_str::<Window>(&wire).map_err(err)?;
        if !self.matches(&window) {
            return Err("Окно администратора изменилось. Выбери его заново.".into());
        }
        Ok(window)
    }
}

fn admin_action(args: &[String]) -> Reply<Action> {
    let xy = |x: &str, y: &str| {
        Ok::<_, String>((
            x.parse::<i32>().map_err(err)?,
            y.parse::<i32>().map_err(err)?,
        ))
    };
    match args {
        [verb, x, y] if verb == "move" => {
            let (x, y) = xy(x, y)?;
            Ok(Action::Move { x, y })
        }
        [verb, x, y, rest @ ..] if verb == "click" && (rest.is_empty() || rest == ["right"]) => {
            let (x, y) = xy(x, y)?;
            Ok(Action::Click {
                x,
                y,
                right: !rest.is_empty(),
            })
        }
        [verb, amount] if verb == "scroll" => Ok(Action::Scroll {
            clicks: amount.parse().map_err(err)?,
        }),
        [verb, combo] if verb == "key" => Ok(Action::Key {
            combo: combo.clone(),
        }),
        [verb, text] if verb == "type" => Ok(Action::Type { text: text.clone() }),
        _ => Err("Действие недоступно для окна администратора".into()),
    }
}

async fn admin_frame(
    device: &Device,
    target: &AdminWindowRef,
    checked: &Window,
) -> Reply<serde_json::Value> {
    let file =
        ferry_net::files::temp(&ferry_core::ferry_home().map_err(err)?, "png").map_err(err)?;
    let result = async {
        let window = device
            .shot_window_with_info(&file, &format!("@{}", target.id))
            .await
            .map_err(err)?
            .ok_or("Устройство не вернуло геометрию снимка")?;
        let rect = window.rect().ok_or("Окно свёрнуто")?;
        if !target.matches(&window) || checked.rect() != Some(rect) {
            return Err("Окно сдвинулось или изменилось. Обнови снимок.".into());
        }
        if target.probe(device).await?.rect() != Some(rect) {
            return Err("Окно сдвинулось. Обнови снимок.".into());
        }
        let bytes = std::fs::read(&file).map_err(err)?;
        if !matching_png(&bytes, rect.2, rect.3) {
            return Err("Размер снимка не совпал с геометрией окна".into());
        }
        Ok(serde_json::json!({
            "image": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)),
            "window": window,
            "rect": rect,
        }))
    }.await;
    let _ = std::fs::remove_file(file);
    result
}

fn matching_png(bytes: &[u8], width: i64, height: i64) -> bool {
    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    if !bytes.starts_with(PNG_HEADER) {
        return false;
    }
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return false;
    };
    bytes.get(16..20) == Some(&width.to_be_bytes())
        && bytes.get(20..24) == Some(&height.to_be_bytes())
}

#[tauri::command]
pub async fn admin_screen(
    agent: State<'_, Agent>,
    id: String,
    action: AdminScreenAction,
    window: Option<AdminWindowRef>,
    args: Option<Vec<String>>,
    rect: Option<(i64, i64, i64, i64)>,
) -> Reply<serde_json::Value> {
    let device = on_line(&agent, &id)?;
    let session = crate::DESKTOP_SESSION;
    if matches!(action, AdminScreenAction::Release) {
        device.release_control(session).await.map_err(err)?;
        return Ok(serde_json::Value::Null);
    }
    let listing = matches!(action, AdminScreenAction::List);
    device
        .desk(
            session,
            DeskAction::HoldNote {
                seconds: if listing { 180 } else { 30 },
                note: "Управление окном администратора из пульта Ferry".into(),
            },
        )
        .await
        .map_err(err)?;
    if listing {
        // Бронь остаётся на время, пока человек подтверждает доступ на Windows.
        let windows = device.admin_windows(session).await.map_err(err)?;
        return serde_json::to_value(windows).map_err(err);
    }
    let target = window.ok_or("Сначала выбери окно администратора")?;
    let checked = target.probe(&device).await?;
    let input = match action {
        AdminScreenAction::Shot => return admin_frame(&device, &target, &checked).await,
        AdminScreenAction::Focus => Action::Focus,
        AdminScreenAction::Input => admin_action(args.as_deref().unwrap_or_default())?,
        _ => return Err("Действие недоступно".into()),
    };
    if matches!(
        input,
        Action::Move { .. } | Action::Click { .. } | Action::Scroll { .. }
    ) && (rect.is_none() || checked.rect() != rect)
    {
        return Err("Окно сдвинулось. Обнови снимок перед управлением мышью.".into());
    }
    device
        .admin_input(session, Some(target.id), input)
        .await
        .map_err(err)?;
    Ok(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_actions_stay_on_supported_native_input() {
        let parse = |args: &[&str]| {
            admin_action(&args.iter().map(|part| part.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&["click", "120", "45", "right"]),
            Ok(Action::Click {
                x: 120,
                y: 45,
                right: true
            })
        );
        assert!(parse(&["click", "120", "45", "double"]).is_err());
        assert!(parse(&["clip", "user clipboard"]).is_err());
        assert!(parse(&["key", "F12", "enter"]).is_err());
    }

    #[test]
    fn coordinates_require_matching_png_dimensions() {
        let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        header.extend([0, 0, 4, 0, 0, 0, 3, 0]);
        assert!(matching_png(&header, 1024, 768));
        assert!(!matching_png(&header, 768, 1024));
        assert!(!matching_png(&header[..16], 1024, 768));
    }
}
