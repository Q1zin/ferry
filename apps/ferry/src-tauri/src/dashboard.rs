//! Fixed dashboard operations are validated again by the bundled CLI.
use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const LIMIT: u64 = 8 * 1024 * 1024;
struct Spool(PathBuf);
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn read(file: fs::File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("ответ панели превышает 8 МиБ"));
    }
    Ok(bytes)
}
fn run(cli: PathBuf, json: String) -> Result<Value, String> {
    let dir = ferry_core::ferry_home()
        .map_err(super::err)?
        .join("dashboard-runners");
    fs::create_dir_all(&dir).map_err(super::err)?;
    let dir = dir.join(ferry_core::jobs::unique_id());
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(super::err)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(&dir).map_err(super::err)?;
    let spool = Spool(dir);
    let create = |name: &str| -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(spool.0.join(name))
    };
    let stdout = create("stdout").map_err(super::err)?;
    let stderr = create("stderr").map_err(super::err)?;
    let mut command = Command::new(cli);
    command
        .arg("dashboard")
        .arg(json)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(super::err)?;
    let deadline = Instant::now() + Duration::from_secs(150);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(super::err)? {
            break status;
        }
        let oversized = ["stdout", "stderr"]
            .iter()
            .any(|name| fs::metadata(spool.0.join(name)).is_ok_and(|m| m.len() > LIMIT));
        if oversized || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if oversized {
                "ответ панели превышает 8 МиБ"
            } else {
                "панель не ответила за 150 секунд; проверь состояние прогона"
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let out =
        read(fs::File::open(spool.0.join("stdout")).map_err(super::err)?).map_err(super::err)?;
    let err =
        read(fs::File::open(spool.0.join("stderr")).map_err(super::err)?).map_err(super::err)?;
    if !status.success() {
        let message = String::from_utf8_lossy(&err)
            .trim()
            .chars()
            .take(2000)
            .collect::<String>();
        return Err(if message.is_empty() {
            format!("команда панели завершилась с кодом {:?}", status.code())
        } else {
            message
        });
    }
    serde_json::from_slice(&out).map_err(super::err)
}
#[tauri::command]
pub async fn dashboard(request: Value) -> Result<Value, String> {
    let json = serde_json::to_string(&request).map_err(super::err)?;
    if json.len() > 4096 {
        return Err("запрос панели слишком большой".into());
    }
    let cli = super::cli_path().ok_or("CLI ferry отсутствует рядом с приложением")?;
    tauri::async_runtime::spawn_blocking(move || run(cli, json))
        .await
        .map_err(super::err)?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn inherited_output_handle_does_not_block_dashboard() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "ferry-dashboard-test-{}",
            ferry_core::jobs::unique_id()
        ));
        fs::write(&path, "#!/bin/sh\n(sleep 2) &\nprintf '{\"ok\":true}\\n'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let result = run(path.clone(), "{}".into()).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(result["ok"], true);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
