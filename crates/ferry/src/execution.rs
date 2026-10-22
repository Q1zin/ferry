//! Explicit session lifetime, isolated workspaces and durable launch receipts.
use crate::devices::finish;
use ferry_core::{
    control::{self, Request, Response},
    desk::Shell,
    device::{Dir, Fail},
    jobs::execution::{Resource, Source, StartRequest},
    session, sync,
};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};

fn response(request: Request, session: &str) -> Result<Response, Fail> {
    match control::call_session(request, session)? {
        Response::Error(e) => Err(e),
        other => Ok(other),
    }
}
fn report(request: Request, session: &str) -> Result<Value, Fail> {
    match response(request, session)? {
        Response::Report(s) => serde_json::from_str(&s).map_err(|e| Fail::new(e.to_string())),
        _ => Err(Fail::new("ожидался JSON-отчёт")),
    }
}
fn device(name: &str) -> Result<Option<String>, Fail> {
    if name == "local" {
        return Ok(None);
    }
    if name.is_empty() || name.contains(',') || name == "all" || name.starts_with("os:") {
        return Err(Fail::new(
            "для управляемого запуска нужно одно устройство или local; для матрицы используй check",
        ));
    }
    Ok(Some(name.into()))
}
fn session_name(name: &str) -> String {
    if name.contains(':') {
        name.into()
    } else {
        format!("agent:{name}")
    }
}
pub fn session(args: &[&str]) -> io::Result<i32> {
    let Some((action, mut rest)) = args.split_first() else {
        return crate::usage();
    };
    let mut id = None;
    if let [value, tail @ ..] = rest
        && !value.starts_with('-')
    {
        id = Some((*value).strip_prefix("agent:").unwrap_or(value).to_string());
        rest = tail;
    }
    let (mut owner_pid, mut ttl) = (None, 3600u32);
    while !rest.is_empty() {
        match rest {
            ["--pid", pid, tail @ ..] if *action == "begin" && owner_pid.is_none() => {
                owner_pid = Some(pid.parse::<u32>().map_err(io::Error::other)?);
                rest = tail;
            }
            ["--ttl", seconds, tail @ ..] if matches!(*action, "begin" | "renew") => {
                ttl = seconds.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let id = match id {
        Some(id) => id,
        None if *action == "begin" => format!("cli-{}", ferry_core::jobs::unique_id()),
        None => return crate::usage(),
    };
    let request = match *action {
        "begin" => session::Request::Begin { id, owner_pid, ttl },
        "renew" => session::Request::Renew { id, ttl },
        "end" => session::Request::End { id },
        "show" => session::Request::Show { id },
        _ => return crate::usage(),
    };
    finish((|| {
        let value = report(Request::Session { request }, &control::session())?;
        println!("{value}");
        Ok(0)
    })())
}
#[derive(Debug)]
struct StartOptions {
    request_id: String,
    session: String,
    keep: bool,
    timeout: Option<u32>,
    shell: Shell,
    headless: bool,
    home: bool,
    no_sync: bool,
    workspace: Option<String>,
    command: String,
}
fn parse_start(mut args: &[&str]) -> io::Result<StartOptions> {
    let mut options = StartOptions {
        request_id: format!("cli-{}", ferry_core::jobs::unique_id()),
        session: control::session(),
        keep: false,
        timeout: None,
        shell: Shell::Default,
        headless: false,
        home: false,
        no_sync: false,
        workspace: None,
        command: String::new(),
    };
    while !args.is_empty() {
        match args {
            ["--request", id, rest @ ..] => {
                options.request_id = (*id).into();
                args = rest;
            }
            ["--session", id, rest @ ..] => {
                options.session = session_name(id);
                args = rest;
            }
            ["--timeout", seconds, rest @ ..] => {
                options.timeout = Some(seconds.parse().map_err(io::Error::other)?);
                args = rest;
            }
            ["--shell", shell, rest @ ..] => {
                options.shell = shell.parse()?;
                args = rest;
            }
            ["--workspace", path, rest @ ..] if options.workspace.is_none() => {
                options.workspace = Some((*path).into());
                args = rest;
            }
            ["--keep", rest @ ..] => {
                options.keep = true;
                args = rest;
            }
            ["--headless", rest @ ..] => {
                options.headless = true;
                args = rest;
            }
            ["--home", rest @ ..] => {
                options.home = true;
                args = rest;
            }
            ["--no-sync", rest @ ..] => {
                options.no_sync = true;
                args = rest;
            }
            ["--", rest @ ..] => {
                options.command = rest.join(" ");
                break;
            }
            [flag, ..] if flag.starts_with("--") => {
                return Err(io::Error::other(format!(
                    "неизвестный параметр start: {flag}; перед командой поставь --"
                )));
            }
            rest => {
                options.command = rest.join(" ");
                break;
            }
        }
    }
    if options.command.is_empty()
        || options.workspace.is_some() && (options.home || options.no_sync)
    {
        return Err(io::Error::other(
            "нужна команда; --workspace нельзя сочетать с --home/--no-sync",
        ));
    }
    if options.timeout.is_some_and(|s| s == 0 || s > 86400) {
        return Err(io::Error::other("timeout: 1..86400 секунд"));
    }
    Ok(options)
}
fn manifest(path: &Path) -> Result<Value, Fail> {
    if std::fs::metadata(path)?.len() > 16384 {
        return Err(Fail::new("workspace manifest больше 16 КиБ"));
    }
    let mut value: Value =
        serde_json::from_slice(&std::fs::read(path)?).map_err(|e| Fail::new(e.to_string()))?;
    if value["workspace"].is_object() {
        value = value["workspace"].take();
    }
    let source: Source =
        serde_json::from_value(value["source"].clone()).map_err(|e| Fail::new(e.to_string()))?;
    ferry_core::workspace::validate(&source)?;
    if value["ready"] != true {
        return Err(Fail::new("workspace не готов"));
    }
    Ok(value)
}
fn prefix(value: &Value) -> Result<String, Fail> {
    let prefix = value["prefix"].as_str().unwrap_or_default();
    if !prefix.is_empty()
        && (prefix.contains('\\')
            || Path::new(prefix)
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_))))
    {
        return Err(Fail::new(
            "workspace prefix должен находиться внутри проекта",
        ));
    }
    Ok(prefix.into())
}
fn prepare(
    device: &Option<String>,
    repo: &sync::Repo,
    id: &str,
    session: &str,
) -> Result<Value, Fail> {
    let (commit, tree) = sync::snapshot(repo)?;
    let mut result = report(
        Request::WorkspaceSync {
            device: device.clone(),
            root: repo.root.clone(),
            project: repo.name.clone(),
            id: id.into(),
            commit,
            tree,
        },
        session,
    )?;
    result["device"] = device.as_deref().unwrap_or("local").into();
    result["local_root"] = repo.root.to_string_lossy().into_owned().into();
    result["prefix"] = repo.prefix.clone().into();
    Ok(result)
}
struct LaunchContext {
    dir: Dir,
    cwd: Option<PathBuf>,
    source: Option<Source>,
    workspace: Option<Value>,
}
fn launch_context(device: &Option<String>, options: &StartOptions) -> Result<LaunchContext, Fail> {
    let workspace = if let Some(path) = &options.workspace {
        Some(manifest(Path::new(path))?)
    } else if !options.home && !options.no_sync {
        sync::repo()?
            .map(|repo| prepare(device, &repo, &options.request_id, &options.session))
            .transpose()?
    } else {
        None
    };
    if let Some(value) = workspace {
        if value["device"].as_str() != Some(device.as_deref().unwrap_or("local")) {
            return Err(Fail::new("workspace принадлежит другому устройству"));
        }
        let source: Source = serde_json::from_value(value["source"].clone())
            .map_err(|e| Fail::new(e.to_string()))?;
        let prefix = prefix(&value)?;
        let cwd = if device.is_none() {
            Some(
                PathBuf::from(
                    value["cwd"]
                        .as_str()
                        .ok_or_else(|| Fail::new("workspace без cwd"))?,
                )
                .join(&prefix),
            )
        } else {
            None
        };
        return Ok(LaunchContext {
            dir: Dir::Project {
                name: source.workspace.clone(),
                prefix,
            },
            cwd,
            source: Some(source),
            workspace: Some(value),
        });
    }
    let project = crate::devices::project(false, options.home)?;
    let cwd = if device.is_none() && !options.home {
        Some(std::env::current_dir()?)
    } else {
        None
    };
    Ok(LaunchContext {
        dir: crate::devices::dir(&project),
        cwd,
        source: None,
        workspace: None,
    })
}
/// Durable execution for agent sessions or explicit flags; local uses the name `local`.
pub fn start(args: &[&str]) -> io::Result<i32> {
    let [who, name, rest @ ..] = args else {
        return crate::usage();
    };
    let options = parse_start(rest)?;
    finish((|| {
        let device = device(who)?;
        let LaunchContext {
            dir,
            cwd: local_cwd,
            source,
            workspace,
        } = launch_context(&device, &options)?;
        let request = StartRequest {
            request_id: options.request_id.clone(),
            name: (*name).into(),
            command: options.command,
            dir,
            shell: options.shell,
            resource: if options.headless {
                Resource::Headless
            } else {
                Resource::Desktop
            },
            source,
            session: options.session.clone(),
            keep: options.keep,
            timeout_sec: options.timeout,
        };
        // Even a lost response leaves the request id available for start-status.
        let result = report(
            Request::AtomicStart {
                device: device.clone(),
                request,
                local_cwd,
            },
            &options.session,
        );
        match result {
            Ok(mut value) => {
                value["device"] = device.as_deref().unwrap_or("local").into();
                value["session"] = options.session.into();
                value["keep"] = options.keep.into();
                value["workspace"] = workspace.unwrap_or(Value::Null);
                let failed = launch_failed(value["state"].as_str());
                println!("{value}");
                Ok(i32::from(failed))
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"device":device.as_deref().unwrap_or("local"),"name":name,"request_id":options.request_id,"error":error.to_string(),"launch_state":"unconfirmed","next":"start-status с тем же устройством и request_id; не запускай команду с новым id вслепую"})
                );
                Ok(error.code)
            }
        }
    })())
}
fn launch_failed(state: Option<&str>) -> bool {
    match state {
        Some("running" | "starting" | "exit:0") => false,
        Some(value) if value.starts_with("exit:") => value[5..].parse::<i32>() != Ok(0),
        _ => true,
    }
}
pub fn start_status(args: &[&str]) -> io::Result<i32> {
    let [who, id] = args else {
        return crate::usage();
    };
    finish((|| {
        let device = device(who)?;
        let mut value = report(
            Request::ExecutionLookup {
                device: device.clone(),
                request_id: (*id).into(),
            },
            &control::session(),
        )?;
        value["device"] = device.as_deref().unwrap_or("local").into();
        println!("{value}");
        Ok(i32::from(value["started"].is_null()))
    })())
}
pub fn tunnel(args: &[&str]) -> io::Result<i32> {
    let [who, port, rest @ ..] = args else {
        return crate::usage();
    };
    let mut rest = rest;
    let port = port.parse::<u16>().map_err(io::Error::other)?;
    if port == 0 {
        return Err(io::Error::other("нужен порт 1..65535"));
    }
    let (mut session, mut keep, mut close) = (control::session(), false, false);
    while !rest.is_empty() {
        match rest {
            ["--session", id, tail @ ..] => {
                session = session_name(id);
                rest = tail;
            }
            ["--keep", tail @ ..] => {
                keep = true;
                rest = tail;
            }
            ["--close", tail @ ..] => {
                close = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        let Some(device) = device(who)? else {
            return Err(Fail::new("туннелю нужно удалённое устройство"));
        };
        let Response::Message(message) = response(
            Request::OwnedTunnel {
                device: device.clone(),
                port,
                close,
                keep,
            },
            &session,
        )?
        else {
            return Err(Fail::new("не получен результат туннеля"));
        };
        println!(
            "{}",
            json!({"device":device,"port":port,"session":session,"keep":keep,"closed":close,"message":message})
        );
        Ok(0)
    })())
}
pub fn workspace(args: &[&str]) -> io::Result<i32> {
    let Some((action, mut rest)) = args.split_first() else {
        return crate::usage();
    };
    match *action {
        "create" => {
            let mut who = "local";
            if let [value, tail @ ..] = rest
                && !value.starts_with('-')
            {
                who = value;
                rest = tail;
            }
            let (mut id, mut root) = (
                format!("cli-{}", ferry_core::jobs::unique_id()),
                PathBuf::from("."),
            );
            while !rest.is_empty() {
                match rest {
                    ["--id", value, tail @ ..] => {
                        id = (*value).into();
                        rest = tail;
                    }
                    ["--root", value, tail @ ..] => {
                        root = (*value).into();
                        rest = tail;
                    }
                    _ => return crate::usage(),
                }
            }
            finish((|| {
                let repo = sync::repo_at(&root)?
                    .ok_or_else(|| Fail::new("workspace create требует git репозиторий"))?;
                let value = prepare(&device(who)?, &repo, &id, &control::session())?;
                println!("{value}");
                Ok(0)
            })())
        }
        "diff" | "apply" => {
            let Some((path, mut rest)) = rest.split_first() else {
                return crate::usage();
            };
            let (mut root, mut token) = (None, None);
            while !rest.is_empty() {
                match rest {
                    ["--root", value, tail @ ..] => {
                        root = Some(PathBuf::from(value));
                        rest = tail;
                    }
                    ["--token", value, tail @ ..] if *action == "apply" && token.is_none() => {
                        token = Some((*value).to_string());
                        rest = tail;
                    }
                    _ => return crate::usage(),
                }
            }
            if *action == "apply" && token.is_none() {
                return Err(io::Error::other(
                    "workspace apply требует --token из свежего workspace diff",
                ));
            }
            finish((|| {
                let value = manifest(Path::new(path))?;
                let source: Source = serde_json::from_value(value["source"].clone())
                    .map_err(|e| Fail::new(e.to_string()))?;
                let who = value["device"]
                    .as_str()
                    .ok_or_else(|| Fail::new("manifest без устройства"))?;
                let root = root
                    .or_else(|| value["local_root"].as_str().map(PathBuf::from))
                    .ok_or_else(|| Fail::new("manifest без local_root; укажи --root"))?;
                let result = report(
                    Request::WorkspacePatch {
                        device: device(who)?,
                        root,
                        source,
                        apply: token,
                    },
                    &control::session(),
                )?;
                println!("{result}");
                Ok(i32::from(result["clean"] != true))
            })())
        }
        _ => crate::usage(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_flags_preserve_shell_command_and_require_explicit_context() {
        let parsed = parse_start(&[
            "--headless",
            "--session",
            "review",
            "--timeout",
            "12",
            "--request",
            "same",
            "--shell",
            "powershell",
            "--",
            "Write-Output",
            "--keep",
        ])
        .unwrap();
        assert!(parsed.headless);
        assert_eq!(parsed.session, "agent:review");
        assert_eq!(parsed.timeout, Some(12));
        assert!(!parsed.keep);
        assert_eq!(parsed.command, "Write-Output --keep");
        assert!(parse_start(&["--timeout", "0", "--", "test"]).is_err());
        assert!(parse_start(&["--workspace", "manifest.json", "--home", "--", "test"]).is_err());
        assert!(parse_start(&["--misspelled", "test"]).is_err());
    }
    #[test]
    fn immediate_launch_failures_are_not_reported_as_success() {
        for state in ["running", "starting", "exit:0"] {
            assert!(!launch_failed(Some(state)));
        }
        for state in ["exit:7", "killed", "lost", "unavailable"] {
            assert!(launch_failed(Some(state)));
        }
    }
    #[test]
    fn manifest_prefix_cannot_escape_workspace() {
        assert_eq!(prefix(&json!({"prefix":"src/app/"})).unwrap(), "src/app/");
        for path in ["../other", "/absolute", "a/../../other", "a\\..\\other"] {
            assert!(prefix(&json!({"prefix":path})).is_err());
        }
        assert!(device("all").is_err());
        assert!(device("os:windows").is_err());
    }
}
