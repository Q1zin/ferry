use crate::{Agent, Reply, err, on_line};
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
    let session = format!("console:{}", std::process::id());
    if let Some(args) = args {
        device
            .desk(
                &session,
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
        device
            .desk(&session, ferry_net::proto::DeskAction::ReleaseOwned)
            .await
            .map_err(err)?;
    }
    Ok(())
}
