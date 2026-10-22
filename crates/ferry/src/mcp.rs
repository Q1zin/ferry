//! `ferry mcp`: ferry для Claude Code как MCP-сервер — JSON-RPC строками через stdin и stdout.
//! Инструмент один — та же команда, что в терминале: аргументы после `ferry`. Вывод и код
//! выхода — текстом, а снимки (`shot`) и листы записи (`rec`) — картинками: их не надо
//! открывать отдельно.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine;
use serde_json::{Value, json};

/// Больше — не картинкой, а подсказкой: такую Claude Code не примет.
const IMAGE_LIMIT: u64 = 4 << 20;

pub fn serve() -> io::Result<i32> {
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message, run),
            Err(e) => Some(error(Value::Null, -32700, &format!("не JSON: {e}"))),
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(0)
}

/// Ответ на сообщение клиента; на уведомления (без `id`) — ничего.
fn handle(message: &Value, run: impl Fn(&[String]) -> Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "ferry", "version": ferry_core::VERSION},
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [tool()]}),
        "tools/call" if params["name"] == "ferry" => {
            let args: Vec<String> = params["arguments"]["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(String::from))
                .collect();
            run(&args)
        }
        "tools/call" => return Some(error(id, -32602, "инструмент один — ferry")),
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
            check <профиль> --devices local,WIN-11-2 — профили из ferry.checks.json и матрица; diagnose — диагностика запуска; history — место, pin и preview очистки; act --from observe.json --click имя --expect имя — проверяемое действие; \
            select --os windows --shell powershell --tool cargo --desktop — подходящие устройства; observe [<устройство>] [--window <окно>] [--ocr] — состояние интерфейса и снимок; runs/artifacts/tests — история, файлы и JUnit; \
            shot <устройство> [--window <окно> | --region x,y,w,h]; windows; focus; ui; text; \
            click <устройство> (x y | --name <имя> | --text <текст>); type; key; drag; clip; \
            wait <устройство> (--window | --name | --text | --port | --log <задача> <текст>); \
            crashes; logs; info; desk — бронь рабочего стола. Пустой args — полный список команд.",
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
fn run(args: &[String]) -> Value {
    if args.is_empty() {
        return answer(vec![text(crate::USAGE)], false);
    }
    let out = std::env::current_exe()
        .and_then(|exe| Command::new(exe).args(args).stdin(Stdio::null()).output());
    let out = match out {
        Ok(out) => out,
        Err(e) => return answer(vec![text(&format!("ferry не запустился: {e}"))], true),
    };
    let code = out.status.code().unwrap_or(1);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let mut said = format!("{stdout}{stderr}");
    if !said.ends_with('\n') && !said.is_empty() {
        said.push('\n');
    }
    said += &format!("(код {code})");
    let mut parts = vec![text(&said)];
    parts.extend(images(&stdout));
    command_answer(args, &stdout, parts, code)
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
        .map(|path| match std::fs::metadata(path) {
            Ok(meta) if meta.len() > IMAGE_LIMIT => text(&format!(
                "{path}: снимок больше 4 МБ — картинкой не прислать; возьми --window или --region"
            )),
            _ => match std::fs::read(path) {
                Ok(png) => json!({
                    "type": "image",
                    "mimeType": "image/png",
                    "data": base64::engine::general_purpose::STANDARD.encode(png),
                }),
                Err(e) => text(&format!("{path}: не прочитался ({e})")),
            },
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
