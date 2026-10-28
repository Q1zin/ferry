mod typed;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use base64::Engine;
use serde_json::{Value, json};

/// Больше — не картинкой, а подсказкой: такую Claude Code не примет.
const IMAGE_LIMIT: u64 = 4 << 20;

pub fn serve() -> io::Result<i32> {
    let output = Arc::new(Mutex::new(io::stdout()));
    let active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let mut workers = Vec::new();
    let mut input = io::stdin().lock();
    let result = (|| -> io::Result<()> {
        loop {
            let mut bytes = Vec::new();
            if input.by_ref().take(1 << 20).read_until(b'\n', &mut bytes)? == 0 {
                break;
            }
            if !bytes.ends_with(b"\n") {
                return Err(io::Error::other(
                    "MCP message exceeds 1 MiB or lacks newline",
                ));
            }
            let message: Value = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    let mut out = output.lock().unwrap();
                    writeln!(out, "{}", error(Value::Null, -32700, &e.to_string()))?;
                    out.flush()?;
                    continue;
                }
            };
            if message["method"] == "notifications/cancelled" {
                if let Some(flag) = active
                    .lock()
                    .unwrap()
                    .get(&message["params"]["requestId"].to_string())
                {
                    flag.store(true, Ordering::Release);
                }
                continue;
            }
            if message["method"] == "tools/call" && message.get("id").is_some() {
                let key = message["id"].to_string();
                let flag = Arc::new(AtomicBool::new(false));
                {
                    let mut entries = active.lock().unwrap();
                    if entries.len() >= 4 || entries.contains_key(&key) {
                        let mut out = output.lock().unwrap();
                        writeln!(
                            out,
                            "{}",
                            error(
                                message["id"].clone(),
                                -32000,
                                "busy or duplicate request id; use check start --detach and poll status"
                            )
                        )?;
                        out.flush()?;
                        continue;
                    }
                    entries.insert(key.clone(), flag.clone());
                }
                let output = output.clone();
                let active = active.clone();
                workers.retain(|worker: &std::thread::JoinHandle<()>| !worker.is_finished());
                workers.push(std::thread::spawn(move || {
                    if let Some(reply) = handle(&message, |args| run(args, &flag))
                        && let Ok(mut out) = output.lock()
                    {
                        let _ = writeln!(out, "{reply}");
                        let _ = out.flush();
                    }
                    active.lock().unwrap().remove(&key);
                }));
            } else if let Some(reply) = handle(&message, |args| run(args, &AtomicBool::new(false)))
            {
                let mut out = output.lock().unwrap();
                writeln!(out, "{reply}")?;
                out.flush()?;
            }
        }
        Ok(())
    })();
    // EOF also ends one-shot piped requests: drain their bounded calls, cancel only malformed transport.
    if result.is_err() {
        for flag in active.lock().unwrap().values() {
            flag.store(true, Ordering::Release);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    result?;
    Ok(0)
}

/// Ответ на сообщение клиента; на уведомления (без `id`) — ничего.
fn handle(message: &Value, run: impl Fn(&[String]) -> Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().filter(|v|["2024-11-05","2025-03-26","2025-06-18","2025-11-25"].contains(v)).unwrap_or("2025-11-25"),
            "instructions":"Use ferry_devices to select a free device. Start durable checks with a unique operation id, then poll status. After a lost response reuse the same id. Desktop work must respect leases and the human. Use check export to collect logs, test results and video. Read the bundled ferry skill for the full CLI.",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "ferry", "version": ferry_core::VERSION},
        }),
        "ping" => json!({}),
        "tools/list" => {
            let mut tools = vec![tool()];
            tools.extend(typed::tools());
            json!({"tools":tools})
        }
        "tools/call" => {
            let args = if params["name"] == "ferry" {
                params["arguments"]["args"]
                    .as_array()
                    .filter(|a| a.len() <= 256)
                    .and_then(|a| {
                        a.iter()
                            .map(|v| {
                                v.as_str()
                                    .filter(|s| s.len() <= 65536 && !s.contains('\0'))
                                    .map(String::from)
                            })
                            .collect::<Option<Vec<_>>>()
                    })
                    .ok_or_else(|| {
                        io::Error::other("args must be an array of strings, at most 256 arguments")
                    })
            } else {
                typed::args(
                    params["name"].as_str().unwrap_or_default(),
                    &params["arguments"],
                )
            };
            match args {
                Ok(args) => run(&args),
                Err(e) => return Some(error(id, -32602, &e.to_string())),
            }
        }
        other => return Some(error(id, -32601, &format!("нет метода {other}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool() -> Value {
    json!({
        "name": "ferry",
        "description": "Работа на других компьютерах (Windows, macOS) через ferry — та же команда, \
            что в терминале: args — аргументы после `ferry`. Вывод и код выхода — текстом, снимки \
            экрана (shot) и листы записи (rec) — картинками. Основное: devices --json; \
            run <устройство> -- <команда>; start/result/log/jobs/kill — фоновые задачи; result [<устройство>] <задача> --wait 30 — ограниченный JSON результата, --cursor — новые строки; для долгих команд используй start и result; \
            check <профиль> --devices local,WIN-11-2 --id ID --detach — устойчивый build/launch/test; check status|resume|collect|cancel ID; diagnose --bundle — переносимые доказательства; history/storage — место и preview очистки; session begin/end — уборка задач и туннелей; start --request ID --session ID --timeout N; workspace create/diff/apply — изоляция и patch; act --from observe.json --click-locator JSON --expect-json JSON — проверяемое действие; \
            select --os windows --shell powershell --tool cargo --desktop — подходящие устройства; observe [<устройство>] [--window <окно>] [--structured] [--no-screenshot] [--since JSON] — состояние интерфейса или delta; runs/artifacts/tests — история, файлы и JUnit; \
            shot <устройство> [--window <окно> | --region x,y,w,h]; windows; focus; ui; text; \
            click <устройство> (x y | --name <имя> | --text <текст>); type; key; drag; clip; \
            wait <устройство> (--window | --name | --text | --port | --log <задача> <текст>); \
            crashes; logs; info — включая возраст настоящего ввода; desk take --note — намерение на рамке; ask DEVICE вопрос — запрос человеку, --poll ID — ответ (done требует проверки). Пустой args — полный список команд.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "аргументы по одному, например [\"run\", \"win-10\", \"--\", \"cargo test\"]"
                }
            },
            "required": ["args"]
        }
    })
}

/// Выполнить `ferry <args>` этим же бинарником.
fn run(args: &[String], cancelled: &AtomicBool) -> Value {
    if args.is_empty() {
        return answer(vec![text(crate::USAGE)], false);
    }
    if args
        .first()
        .is_some_and(|s| matches!(s.as_str(), "mcp" | "server"))
    {
        return answer(
            vec![text(
                "Use a terminal to start long-lived services; recursive MCP is not allowed.",
            )],
            true,
        );
    }
    let captured = (|| -> io::Result<Captured> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args(args);
        capture_command(command, cancelled, std::time::Duration::from_secs(120))
    })();
    let captured = match captured {
        Ok(value) => value,
        Err(error) => return failure("command_failed", &error.to_string(), false),
    };
    let (stdout, stderr) = (
        String::from_utf8_lossy(&captured.out),
        String::from_utf8_lossy(&captured.err),
    );
    let code = captured.code;
    let (out_truncated, err_truncated) = (captured.out_truncated, captured.err_truncated);
    if let Some(reason) = captured.interrupted {
        let mut value = failure(
            reason,
            "Local command stopped. A durable operation may still be running: query its id before retrying.",
            false,
        );
        value["structuredContent"]["stdout"] = json!(stdout);
        value["structuredContent"]["stderr"] = json!(stderr);
        value["structuredContent"]["truncated"] = json!(out_truncated || err_truncated);
        return value;
    }
    let mut said = format!("{stdout}{stderr}");
    if !said.ends_with('\n') && !said.is_empty() {
        said.push('\n');
    }
    said += &format!("(код {code})");
    let mut parts = vec![text(&said)];
    if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "shot" | "rec"))
    {
        parts.extend(images(&stdout));
    }
    if out_truncated || err_truncated {
        parts.push(text("Output truncated at 512 KiB per stream. Use result --cursor or check export for full evidence."));
    }
    let mut result = command_answer(args, &stdout, parts, code);
    if result.get("structuredContent").is_none() {
        result["structuredContent"] = json!({"schema_version":1,"exit_code":code,"stdout":stdout,"stderr":stderr,"truncated":out_truncated||err_truncated});
    }
    result
}

const OUTPUT_LIMIT: u64 = 512 * 1024;
pub(crate) struct Captured {
    pub(crate) out: Vec<u8>,
    pub(crate) err: Vec<u8>,
    pub(crate) out_truncated: bool,
    pub(crate) err_truncated: bool,
    pub(crate) code: i32,
    pub(crate) interrupted: Option<&'static str>,
}
struct CaptureDir(std::path::PathBuf);
impl Drop for CaptureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub(crate) fn capture_command(
    command: Command,
    cancelled: &AtomicBool,
    timeout: std::time::Duration,
) -> io::Result<Captured> {
    capture_command_limit(command, cancelled, timeout, OUTPUT_LIMIT)
}
pub(crate) fn capture_command_limit(
    mut command: Command,
    cancelled: &AtomicBool,
    timeout: std::time::Duration,
    limit: u64,
) -> io::Result<Captured> {
    if limit == 0 || limit > 8 * 1024 * 1024 {
        return Err(io::Error::other("output limit must be 1..8 MiB"));
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(Captured {
            out: Vec::new(),
            err: Vec::new(),
            out_truncated: false,
            err_truncated: false,
            code: 1,
            interrupted: Some("cancelled"),
        });
    }
    let root = std::env::temp_dir().join(format!("ferry-mcp-{}", ferry_core::jobs::unique_id()));
    let directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let mut directory = directory;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(&root)?;
    let temporary = CaptureDir(root);
    let out_path = temporary.0.join("stdout");
    let err_path = temporary.0.join("stderr");
    let create = |path: &Path| -> io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    };
    command
        .stdin(Stdio::null())
        .stdout(create(&out_path)?)
        .stderr(create(&err_path)?);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x08000000 | 0x00000200);
    let mut child = command.spawn()?;
    let _tree = ferry_core::desk::Tree::track(child.id());
    let deadline = std::time::Instant::now() + timeout;
    let mut interrupted = None;
    let code = loop {
        interrupted = if cancelled.load(Ordering::Acquire) {
            Some("cancelled")
        } else if std::time::Instant::now() >= deadline {
            Some("deadline_exceeded")
        } else if std::fs::metadata(&out_path)?.len() > limit
            || std::fs::metadata(&err_path)?.len() > limit
        {
            Some("output_limit_exceeded")
        } else {
            interrupted
        };
        if interrupted.is_some() {
            // Kill the private process group before reaping its leader; no reader can outlive this request.
            let _ = ferry_core::desk::force_kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            break 1;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(error) => {
                let _ = ferry_core::desk::force_kill_tree(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };
    let read = |path: &Path| -> io::Result<(Vec<u8>, bool)> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        let truncated = bytes.len() as u64 > limit;
        bytes.truncate(limit as usize);
        Ok((bytes, truncated))
    };
    let (out, out_truncated) = read(&out_path)?;
    let (err, err_truncated) = read(&err_path)?;
    Ok(Captured {
        out,
        err,
        out_truncated,
        err_truncated,
        code,
        interrupted,
    })
}
fn failure(code: &str, message: &str, retryable: bool) -> Value {
    let mut value = answer(vec![text(message)], true);
    value["structuredContent"] =
        json!({"schema_version":1,"error":{"code":code,"message":message,"retryable":retryable}});
    value
}

fn command_answer(args: &[String], stdout: &str, parts: Vec<Value>, code: i32) -> Value {
    let mut result = answer(parts, code != 0);
    if args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "result"
                | "observe"
                | "runs"
                | "artifacts"
                | "tests"
                | "select"
                | "history"
                | "act"
                | "check"
                | "diagnose"
                | "ask"
                | "session"
                | "workspace"
                | "storage"
                | "start-status"
                | "start"
                | "tunnel"
                | "queue"
                | "workflow"
                | "capture"
                | "setup"
        )
    }) && let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(stdout)
    {
        if args.first().is_some_and(|a| a == "observe")
            && let Some(path) = value["screenshot"].as_str()
        {
            result["content"]
                .as_array_mut()
                .unwrap()
                .extend(images(path));
        }
        if args.first().is_some_and(|a| a == "act")
            && let Some(path) = value["observation"]["screenshot"].as_str()
        {
            result["content"]
                .as_array_mut()
                .unwrap()
                .extend(images(path));
        }
        result["structuredContent"] = value;
    }
    result
}

/// Картинки по путям к PNG в выводе: строка — путь, у нескольких целей — с `[устройство] `.
fn images(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| match line.split_once("] ") {
            Some((device, path)) if device.starts_with('[') => path.trim(),
            _ => line.trim(),
        })
        .filter(|path| path.ends_with(".png") && Path::new(path).is_file())
        .take(2)
        .map(|path| {
            let bytes=(||->io::Result<Vec<u8>>{let mut bytes=Vec::new();std::fs::File::open(path)?.take(IMAGE_LIMIT+1).read_to_end(&mut bytes)?;Ok(bytes)})();
            match bytes {
                Ok(png) if png.len() as u64>IMAGE_LIMIT=>text(&format!("{path}: снимок больше 4 МБ — картинкой не прислать; возьми --window или --region")),
                Ok(png)=>json!({"type":"image","mimeType":"image/png","data":base64::engine::general_purpose::STANDARD.encode(png)}),
                Err(error)=>text(&format!("{path}: не прочитался ({error})")),
            }
        })
        .collect()
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn answer(content: Vec<Value>, failed: bool) -> Value {
    json!({"content": content, "isError": failed})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(message: Value) -> Option<Value> {
        handle(&message, |args| answer(vec![text(&args.join("|"))], false))
    }

    #[test]
    fn cancellation_before_dispatch_does_not_spawn_a_command() {
        let command = Command::new("ferry-nonexistent-test-executable");
        let result = capture_command(
            command,
            &AtomicBool::new(true),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(result.interrupted, Some("cancelled"));
    }
    #[cfg(unix)]
    #[test]
    fn cancellation_does_not_wait_for_inherited_output_handles() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "(trap '' TERM; sleep 30) & wait"]);
        let start = std::time::Instant::now();
        let value = capture_command(
            command,
            &AtomicBool::new(false),
            std::time::Duration::from_millis(150),
        )
        .unwrap();
        assert_eq!(value.interrupted, Some("deadline_exceeded"));
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
    #[cfg(unix)]
    #[test]
    fn output_capture_stops_flooding_commands_without_reader_threads() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "while :; do printf '0123456789012345678901234567890123456789'; done",
        ]);
        let value = capture_command(
            command,
            &AtomicBool::new(false),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(value.interrupted, Some("output_limit_exceeded"));
        assert_eq!(value.out.len(), OUTPUT_LIMIT as usize);
        assert!(value.out_truncated);
    }
    #[test]
    fn failed_job_results_keep_json_and_mark_tool_failure() {
        let args = vec!["result".into(), "win".into(), "build".into()];
        let stdout = r#"{"state":"failed","exit_code":7,"log":"error"}"#;
        let reply = command_answer(&args, stdout, vec![text(stdout)], 7);
        assert_eq!(reply["isError"], true);
        assert_eq!(reply["structuredContent"]["exit_code"], 7);
        assert_eq!(reply["content"][0]["text"], stdout);
        assert!(
            command_answer(&args, "", vec![], 1)
                .get("structuredContent")
                .is_none()
        );
    }

    #[test]
    fn speaks_mcp() {
        let init = ask(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}))
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "ferry");
        // Уведомление — без ответа.
        assert!(ask(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());

        let tools = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert_eq!(tools["result"]["tools"][0]["name"], "ferry");
        assert_eq!(
            tools["result"]["tools"][0]["inputSchema"]["required"][0],
            "args"
        );

        let call = ask(json!({"jsonrpc": "2.0", "id": "x", "method": "tools/call",
            "params": {"name": "ferry", "arguments": {"args": ["run", "win-10", "--", "ls"]}}}))
        .unwrap();
        assert_eq!(call["id"], "x");
        assert_eq!(call["result"]["content"][0]["text"], "run|win-10|--|ls");

        let other = ask(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "bash", "arguments": {}}}))
        .unwrap();
        assert_eq!(other["error"]["code"], -32602);
        let unknown = ask(json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn png_paths_in_output_become_images() {
        let dir = std::env::temp_dir().join(format!("ferry-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (shot, big) = (dir.join("win.png"), dir.join("big.png"));
        std::fs::write(&shot, b"\x89PNG...").unwrap();
        std::fs::write(&big, vec![0u8; (IMAGE_LIMIT + 1) as usize]).unwrap();
        let out = format!(
            "{}\n[mac] {}\nне путь.png\n{}/missing.png\n",
            shot.display(),
            big.display(),
            dir.display()
        );
        let parts = images(&out);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0]["type"], "image");
        assert_eq!(parts[0]["data"], "iVBORy4uLg==");
        assert!(parts[1]["text"].as_str().unwrap().contains("больше 4 МБ"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
