//! Local dashboard: authenticated, bounded, fixed operations, no shell endpoint.
mod http;

use ferry_core::control;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    action: String,
    id: Option<String>,
    value: Option<String>,
    cursor: Option<String>,
}

fn identifier(value: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > 48
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(io::Error::other("неверный идентификатор прогона"));
    }
    Ok(())
}

impl Request {
    fn validate(&self, web: bool, actions: bool) -> io::Result<()> {
        let has_id = matches!(
            self.action.as_str(),
            "run" | "resume" | "cancel" | "export" | "compare"
        );
        if has_id != self.id.is_some()
            || (!matches!(
                self.action.as_str(),
                "setup_install" | "setup_smoke" | "compare"
            ) && self.value.is_some())
            || (self.action != "runs" && self.cursor.is_some())
        {
            return Err(io::Error::other("неверные аргументы операции"));
        }
        if let Some(id) = &self.id {
            identifier(id)?;
        }
        if let Some(cursor) = &self.cursor {
            let (created, id) = cursor
                .split_once(':')
                .ok_or_else(|| io::Error::other("неверный курсор"))?;
            created.parse::<u64>().map_err(io::Error::other)?;
            identifier(id)?;
        }
        match self.action.as_str() {
            "about" | "snapshot" | "runs" | "run" | "export" => {}
            "compare" => identifier(
                self.value
                    .as_deref()
                    .ok_or_else(|| io::Error::other("нужен второй прогон"))?,
            )?,
            "resume" | "cancel" if actions => {}
            "setup_status" if !web => {}
            "setup_install"
                if !web && matches!(self.value.as_deref(), Some("claude" | "codex")) => {}
            "setup_smoke"
                if !web
                    && self.value.as_ref().is_some_and(|s| {
                        !s.is_empty()
                            && s.len() <= 128
                            && !s.starts_with('-')
                            && !s.chars().any(char::is_control)
                    }) => {}
            "resume" | "cancel" => {
                return Err(io::Error::other(
                    "сервер доступен только для просмотра; для управления запусти с --allow-actions",
                ));
            }
            _ => return Err(io::Error::other("операция панели недоступна")),
        }
        Ok(())
    }
}

fn cli(args: &[String]) -> io::Result<Value> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(args).stdin(Stdio::null());
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let result = crate::mcp::capture_command_limit(
        command,
        &cancelled,
        Duration::from_secs(120),
        8 * 1024 * 1024,
    )?;
    if result.interrupted.is_some() {
        return Err(io::Error::other(
            "панель не получила результат за 120 секунд; проверь состояние прогона",
        ));
    }
    if result.out_truncated || result.err_truncated {
        return Err(io::Error::other("ответ панели превышает 8 МиБ"));
    }
    match serde_json::from_slice::<Value>(&result.out) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(io::Error::other(if result.err.is_empty() {
            format!("команда панели завершилась с кодом {:?}", result.code)
        } else {
            String::from_utf8_lossy(&result.err)
                .trim()
                .chars()
                .take(2000)
                .collect()
        })),
    }
}

fn execute(request: Request, web: bool, actions: bool) -> io::Result<Value> {
    request.validate(web, actions)?;
    let id = request.id.unwrap_or_default();
    let value = request.value.unwrap_or_default();
    let requested_id = id.clone();
    let args = match request.action.as_str() {
        "about" => {
            return Ok(json!({"version":ferry_core::VERSION,"read_only":!actions,"web":web}));
        }
        "snapshot" => {
            let response =
                control::call_session_timeout(control::Request::State, "", Duration::from_secs(3))
                    .map_err(|e| io::Error::other(e.to_string()))?;
            let control::Response::State(state) = response else {
                return Err(io::Error::other("нет состояния приложения ferry"));
            };
            let mut value = serde_json::to_value(state).map_err(io::Error::other)?;
            // Pairing tickets and human questions never belong in the web dashboard.
            value["pairing"] = Value::Null;
            value["lan"] = json!([]);
            return Ok(value);
        }
        "runs" => {
            let mut args = vec!["check".into(), "list".into(), "--limit".into(), "50".into()];
            if let Some(cursor) = request.cursor {
                args.extend(["--before".into(), cursor]);
            }
            args
        }
        "run" => vec!["check".into(), "status".into(), id.clone()],
        "resume" => vec!["check".into(), "resume".into(), id, "--detach".into()],
        "cancel" => vec!["check".into(), "cancel".into(), id, "--detach".into()],
        "compare" => vec!["check".into(), "compare".into(), id, value],
        "export" => {
            let parent = if web {
                ferry_core::ferry_home()?.join("dashboard-exports")
            } else {
                ferry_core::home()?.join("Downloads/ferry-reports")
            };
            std::fs::create_dir_all(&parent)?;
            let dest = parent.join(format!("{id}-{}", ferry_core::jobs::unique_id()));
            vec![
                "check".into(),
                "export".into(),
                id,
                "--dest".into(),
                dest.to_string_lossy().into_owned(),
            ]
        }
        "setup_status" => vec!["setup".into(), "status".into()],
        "setup_install" => vec!["setup".into(), "install".into(), value],
        "setup_smoke" => {
            // The setup command resolves the paired device and only runs its built-in smoke.
            vec!["setup".into(), "smoke".into(), value]
        }
        _ => unreachable!(),
    };
    let mut value = cli(&args)?;
    if request.action == "run" {
        let mut preview_budget = (8usize * 1024 * 1024)
            .saturating_sub(serde_json::to_vec(&value)?.len() + 65536)
            .min(1024 * 1024);
        // Only index files owned by this check can become a UI preview. Full evidence stays in ZIP.
        if let Ok(root) = ferry_core::ferry_home()?
            .join("checks")
            .join(&requested_id)
            .canonicalize()
            && let Some(rows) = value["results"].as_array_mut()
        {
            for row in rows {
                let preview = (|| -> io::Result<Value> {
                    let file = row["test_cases"]["file"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("нет индекса testcase"))?;
                    let file = std::path::Path::new(file);
                    let relative = file.strip_prefix(&root).map_err(io::Error::other)?;
                    let file = http::safe_file(&root, relative)?;
                    if std::fs::metadata(&file)?.len() > 4 * 1024 * 1024 {
                        return Err(io::Error::other("индекс testcase превышает 4 МиБ"));
                    }
                    let data: Value =
                        serde_json::from_slice(&http::read_bounded(&file, 4 * 1024 * 1024)?)?;
                    let cases = data["cases"]
                        .as_array()
                        .ok_or_else(|| io::Error::other("нет cases"))?;
                    let mut preview = Vec::new();
                    for case in cases.iter().take(200) {
                        let size = serde_json::to_vec(case)?.len() + 1;
                        if size > preview_budget {
                            break;
                        }
                        preview_budget -= size;
                        preview.push(case);
                    }
                    Ok(
                        json!({"more":cases.len().saturating_sub(preview.len()),"cases":preview,"complete":data["complete"]}),
                    )
                })();
                if row["test_cases"].is_object() {
                    match preview {
                        Ok(preview) => row["test_cases"]["preview"] = preview,
                        Err(e) => row["test_cases"]["preview_error"] = json!(e.to_string()),
                    }
                }
            }
        }
    }
    Ok(value)
}

/// Internal bridge shared by the desktop app and the local web dashboard.
pub fn dashboard(args: &[&str]) -> io::Result<i32> {
    let [json] = args else {
        return crate::usage();
    };
    if json.len() > 4096 {
        return Err(io::Error::other("запрос панели слишком большой"));
    }
    let request = serde_json::from_str(json).map_err(io::Error::other)?;
    println!("{}", execute(request, false, true)?);
    Ok(0)
}

pub fn run(args: &[&str]) -> io::Result<i32> {
    let (mut port, mut assets, mut actions) = (8080, None::<PathBuf>, false);
    let mut args = args;
    while !args.is_empty() {
        match args {
            ["--port", value, rest @ ..] => {
                port = value.parse::<u16>().map_err(io::Error::other)?;
                args = rest;
            }
            ["--assets", value, rest @ ..] if assets.is_none() => {
                assets = Some(PathBuf::from(value));
                args = rest;
            }
            ["--allow-actions", rest @ ..] if !actions => {
                actions = true;
                args = rest;
            }
            _ => return crate::usage(),
        }
    }
    http::serve(port, assets, actions)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_never_accepts_shell_or_setup_from_browser() {
        for action in [
            "run_shell",
            "input",
            "setup_status",
            "setup_install",
            "setup_smoke",
            "resume",
            "cancel",
        ] {
            let request: Request = serde_json::from_value(json!({"action":action})).unwrap();
            assert!(request.validate(true, false).is_err());
        }
        let request: Request =
            serde_json::from_value(json!({"action":"run","id":"../../key"})).unwrap();
        assert!(request.validate(true, false).is_err());
        let request: Request =
            serde_json::from_value(json!({"action":"export","id":"check-1"})).unwrap();
        assert!(request.validate(true, false).is_ok());
        assert!(
            serde_json::from_value::<Request>(
                json!({"action":"run","id":"test","command":"anything"})
            )
            .is_err()
        );
    }
}
