use tauri::{AppHandle, Emitter};

#[tauri::command]
pub fn questions() -> Result<Vec<serde_json::Value>, String> {
    ferry_core::ferry_home()
        .and_then(|root| ferry_core::human::pending(&root))
        .map_err(super::err)
}

#[tauri::command]
pub fn answer(window: tauri::WebviewWindow, id: String, answer: String) -> Result<(), String> {
    if window.label() != "main" {
        return Err("ответ доступен только в окне ferry".into());
    }
    let root = ferry_core::ferry_home().map_err(super::err)?;
    ferry_core::human::answer(&root, &id, &answer).map_err(super::err)?;
    Ok(())
}

pub fn watch(app: AppHandle, root: std::path::PathBuf) -> tokio::sync::watch::Receiver<usize> {
    let (count, pending) = tokio::sync::watch::channel(0);
    tauri::async_runtime::spawn(async move {
        let mut stamp = None;
        let mut expires = u64::MAX;
        loop {
            let modified = std::fs::metadata(root.join("questions"))
                .and_then(|m| m.modified())
                .ok();
            if modified != stamp || ferry_core::now() >= expires {
                stamp = modified;
                let read_root = root.clone();
                match tokio::task::spawn_blocking(move || ferry_core::human::pending(&read_root))
                    .await
                {
                    Ok(Ok(questions)) => {
                        count.send_if_modified(|previous| {
                            if *previous == questions.len() {
                                return false;
                            }
                            *previous = questions.len();
                            true
                        });
                        expires = questions
                            .iter()
                            .filter_map(|q| q["expires_at"].as_u64())
                            .min()
                            .unwrap_or(u64::MAX);
                        let _ = app.emit("human-questions", questions);
                    }
                    _ => tracing::warn!("не удалось прочитать вопросы человеку"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    pending
}
