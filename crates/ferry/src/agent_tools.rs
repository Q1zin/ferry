//! Команды с одним ограниченным JSON-ответом для агента.
use crate::devices::{ask, finish, state};
use ferry_core::{
    control::{Request, Response},
    device::{Dir, Fail, PeerView},
    jobs::evidence::{Action, Query},
    mode::Mode,
};
use serde_json::{Value, json};
use std::{
    io,
    path::{Path, PathBuf},
};
pub(crate) fn report(request: Request) -> Result<Value, Fail> {
    match ask(request)? {
        Response::Report(text) => serde_json::from_str(&text).map_err(|e| Fail::new(e.to_string())),
        _ => Err(Fail::new("ожидался JSON-отчёт")),
    }
}
pub fn observe(args: &[&str]) -> io::Result<i32> {
    let (device, mut rest) = match args {
        [name, tail @ ..] if !name.starts_with('-') => (Some((*name).to_string()), tail),
        _ => (None, args),
    };
    let (mut window, mut ocr) = (String::new(), false);
    while !rest.is_empty() {
        match rest {
            ["--window", name, tail @ ..] => {
                window = (*name).into();
                rest = tail;
            }
            ["--ocr", tail @ ..] => {
                ocr = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file = ferry_core::ferry_home()?
            .join("shots")
            .join(format!("observe-{}-{nonce}.png", std::process::id()));
        let mut value = report(Request::Observe {
            device: device.clone(),
            request: ferry_core::screen::observe::Request { window, ocr },
            file,
        })?;
        value["device"] = device.unwrap_or_else(|| "local".into()).into();
        println!("{value}");
        Ok(0)
    })())
}
fn target<'a>(args: &'a [&'a str]) -> Option<(Option<String>, String, &'a [&'a str])> {
    match args {
        [device, name, tail @ ..] if !name.starts_with("--") => {
            Some((Some((*device).into()), (*name).into(), tail))
        }
        [name, tail @ ..] if !name.starts_with("--") => Some((None, (*name).into(), tail)),
        _ => None,
    }
}
pub fn evidence(kind: &str, args: &[&str]) -> io::Result<i32> {
    let Some((device, name, mut rest)) = target(args) else {
        return crate::usage();
    };
    let (mut run_id, mut add, mut get, mut dest, mut artifact) = (None, None, None, None, None);
    while !rest.is_empty() {
        match rest {
            ["--run", id, tail @ ..] if run_id.is_none() => {
                run_id = Some((*id).into());
                rest = tail;
            }
            ["--add", path, tail @ ..] if add.is_none() => {
                add = Some((*path).to_string());
                rest = tail;
            }
            ["--get", id, tail @ ..] if get.is_none() => {
                get = Some((*id).to_string());
                rest = tail;
            }
            ["--dest", path, tail @ ..] if dest.is_none() => {
                dest = Some((*path).to_string());
                rest = tail;
            }
            ["--junit", id, tail @ ..] if artifact.is_none() => {
                artifact = Some((*id).to_string());
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let action = match kind {
        "runs"
            if run_id.is_none()
                && add.is_none()
                && get.is_none()
                && dest.is_none()
                && artifact.is_none() =>
        {
            Action::Runs
        }
        "artifacts"
            if artifact.is_none()
                && !(add.is_some() && get.is_some())
                && (dest.is_none() || get.is_some()) =>
        {
            match add {
                Some(path) => Action::Add { path },
                None => Action::List,
            }
        }
        "tests" if artifact.is_some() && add.is_none() && get.is_none() && dest.is_none() => {
            Action::Tests {
                artifact: artifact.unwrap(),
            }
        }
        _ => return crate::usage(),
    };
    finish((|| {
        let mut value = report(Request::Evidence {
            device: device.clone(),
            query: Query {
                name,
                run_id,
                action,
            },
        })?;
        if let Some(id) = get {
            download(&device, &mut value, &id, dest.as_deref())?;
        }
        value["device"] = device.unwrap_or_else(|| "local".into()).into();
        let failed = kind == "tests"
            && (value["failed"].as_u64().unwrap_or(0) > 0
                || value["errors"].as_u64().unwrap_or(0) > 0
                || value["tests"].as_u64().unwrap_or(0) == 0
                || value["warnings"].as_array().is_some_and(|w| !w.is_empty()));
        println!("{value}");
        Ok(i32::from(failed))
    })())
}
pub fn historical_result(
    device: Option<&str>,
    name: &str,
    run: &str,
    cursor: Option<&str>,
    max_bytes: u32,
) -> io::Result<i32> {
    finish((|| {
        let mut value = report(Request::Evidence {
            device: device.map(String::from),
            query: Query {
                name: name.into(),
                run_id: Some(run.into()),
                action: Action::Result {
                    cursor: cursor.map(String::from),
                    max_bytes,
                },
            },
        })?;
        let result: ferry_core::jobs::JobResult =
            serde_json::from_value(value.clone()).map_err(|e| Fail::new(e.to_string()))?;
        value["device"] = device.unwrap_or("local").into();
        println!("{value}");
        Ok(result.command_code())
    })())
}
fn download(
    device: &Option<String>,
    value: &mut Value,
    id: &str,
    dest: Option<&str>,
) -> Result<(), Fail> {
    let item = value["artifacts"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["id"] == id))
        .ok_or_else(|| Fail::new("нет такого артефакта"))?;
    let path = item["path"]
        .as_str()
        .ok_or_else(|| Fail::new("нет пути артефакта"))?;
    let run = value["run_id"]
        .as_str()
        .ok_or_else(|| Fail::new("нет запуска"))?;
    for component in [run, id] {
        if component.is_empty()
            || !component
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(Fail::new("неверный идентификатор артефакта"));
        }
    }
    let dest = std::path::absolute(dest.unwrap_or("./ferry-evidence"))?
        .join(run)
        .join(id);
    if dest.exists() {
        return Err(Fail::new(format!(
            "каталог уже существует: {} — выбери другой --dest",
            dest.display()
        )));
    }
    std::fs::create_dir_all(&dest)?;
    let files = match device {
        Some(device) => match ask(Request::Get {
            device: device.clone(),
            path: path.into(),
            dir: Dir::Home,
            dest: dest.clone(),
        })? {
            Response::Files(files) => files,
            _ => return Err(Fail::new("не получены файлы")),
        },
        None => {
            let filename = Path::new(path)
                .file_name()
                .ok_or_else(|| Fail::new("нет имени файла"))?;
            std::fs::copy(path, dest.join(filename))?;
            vec![filename.to_string_lossy().into_owned()]
        }
    };
    value["downloaded"] = json!(files.iter().map(|f| dest.join(f)).collect::<Vec<PathBuf>>());
    Ok(())
}

fn rejected(peer: &PeerView, os: Option<&str>, desktop: bool) -> Vec<String> {
    let mut reasons = Vec::new();
    if !peer.online {
        reasons.push("не на связи".into());
    }
    match &peer.status {
        None => reasons.push("статус неизвестен".into()),
        Some(s) => {
            if os.is_some_and(|os| s.os != os) {
                reasons.push(format!("нужна другая ОС: здесь {}", s.os));
            }
            if !s.role.accepts_commands() {
                reasons.push("пульт не принимает команды".into());
            }
            if s.mode == Mode::Paused {
                reasons.push("устройство отключено пользователем".into());
            } else if desktop && !s.mode.allows_desktop() {
                reasons.push("пользователь запретил рабочий стол".into());
            }
        }
    };
    reasons
}
pub fn select(args: &[&str]) -> io::Result<i32> {
    let (mut os, mut shell, mut desktop, mut tools) =
        (None, ferry_core::desk::Shell::Default, false, Vec::new());
    let mut rest = args;
    while !rest.is_empty() {
        match rest {
            ["--os", value @ ("windows" | "macos"), tail @ ..] => {
                os = Some(*value);
                rest = tail;
            }
            ["--shell", value, tail @ ..] => {
                shell = value.parse()?;
                rest = tail;
            }
            ["--tool", value, tail @ ..] => {
                tools.push((*value).into());
                rest = tail;
            }
            ["--desktop", tail @ ..] => {
                desktop = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let request = ferry_core::capabilities::Request {
        shell,
        tools,
        desktop,
    };
    ferry_core::capabilities::validate(&request)?;
    finish((|| {
        let snap = state()?;
        let mut matches = Vec::new();
        let mut rejections = Vec::new();
        for batch in snap.peers.chunks(8) {
            let rows = std::thread::scope(|scope| {
                let handles:Vec<_>=batch.iter().map(|peer| {let request=&request;scope.spawn(move||{
                    let mut reasons=rejected(peer,os,desktop);
                    let mut cap=Value::Null;
                    if reasons.is_empty() && desktop {
                        match ask(Request::Desk{device:peer.name.clone(),action:"show".into(),minutes:0}) {
                            Ok(Response::Message(text)) if text=="свободен"=>{},
                            Ok(Response::Message(text))=>reasons.push(text),
                            Ok(_)=>reasons.push("бронь неизвестна".into()),
                            Err(e)=>reasons.push(e.to_string()),
                        }
                    }
                    if reasons.is_empty() {
                        match ask(Request::Capabilities{device:peer.name.clone(),request:request.clone()}) {
                            Ok(Response::Capabilities(c))=>{
                                if !c.shell_available {reasons.push("оболочка недоступна".into());}
                                if !c.missing_tools.is_empty() {reasons.push(format!("нет инструментов: {}",c.missing_tools.join(", ")));}
                                if desktop && c.locked!=Some(false) {reasons.push("экран заблокирован или состояние неизвестно".into());}
                                reasons.extend(c.warnings.clone()); cap=serde_json::to_value(c).unwrap_or(Value::Null);
                            }
                            Err(e)=>reasons.push(e.to_string()),
                            _=>reasons.push("не удалось проверить инструменты".into()),
                        }
                    }
                    json!({"device":peer.name,"eligible":reasons.is_empty(),"reasons":reasons,"capabilities":cap,"status":peer.status})
                })}).collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .map_err(|_| Fail::new("проверка устройства прервалась"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })?;
            for row in rows {
                if row["eligible"] == true {
                    matches.push(row);
                } else {
                    rejections.push(row);
                }
            }
        }
        let code = i32::from(matches.is_empty());
        println!(
            "{}",
            json!({"matches":matches,"rejected":rejections,"checked_at":ferry_core::now(),"requirements":request,"os":os,"reserved":false,"note":"Снимок доступности ferry; наличие человека не определяется. Перед GUI-прогоном займи стол через desk take."})
        );
        Ok(code)
    })())
}
/// Просмотр места; удаление требует token именно этого preview.
pub fn history(args: &[&str]) -> io::Result<i32> {
    let Some((device, name, mut rest)) = target(args) else {
        return crate::usage();
    };
    let (mut days, mut bytes, mut apply, mut pin) = (None, None, None, None);
    while !rest.is_empty() {
        match rest {
            ["--older-than", n, tail @ ..] if days.is_none() => {
                days = Some(n.parse::<u32>().map_err(io::Error::other)?);
                rest = tail;
            }
            ["--max-bytes", n, tail @ ..] if bytes.is_none() => {
                bytes = Some(n.parse::<u64>().map_err(io::Error::other)?);
                rest = tail;
            }
            ["--apply", token, tail @ ..] if apply.is_none() => {
                apply = Some((*token).into());
                rest = tail;
            }
            [flag @ ("--pin" | "--unpin"), run, tail @ ..] if pin.is_none() => {
                pin = Some(((*run).to_string(), *flag == "--pin"));
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    if pin.is_some() && (days.is_some() || bytes.is_some() || apply.is_some()) {
        return crate::usage();
    }
    let (run_id, action) = match pin {
        Some((id, pinned)) => (Some(id), Action::Pin { pinned }),
        None => (
            None,
            Action::Storage {
                older_than_days: days,
                max_bytes: bytes,
                apply,
            },
        ),
    };
    finish((|| {
        let mut value = report(Request::Evidence {
            device: device.clone(),
            query: Query {
                name,
                run_id,
                action,
            },
        })?;
        value["device"] = device.unwrap_or_else(|| "local".into()).into();
        let failed = value["errors"].as_array().is_some_and(|a| !a.is_empty());
        println!("{value}");
        Ok(i32::from(failed))
    })())
}

pub fn act(args: &[&str]) -> io::Result<i32> {
    let (device, mut rest) = match args {
        [d, tail @ ..] if !d.starts_with('-') => (Some((*d).to_string()), tail),
        _ => (None, args),
    };
    let (mut from, mut action, mut expect, mut timeout, mut dry_run) =
        (None, None, None, 10, false);
    while !rest.is_empty() {
        match rest {
            ["--from", p, tail @ ..] if from.is_none() => {
                from = Some(*p);
                rest = tail;
            }
            ["--click", s, tail @ ..] if action.is_none() => {
                action = Some(ferry_core::screen::act::Action::Click((*s).into()));
                rest = tail;
            }
            ["--key", s, tail @ ..] if action.is_none() => {
                action = Some(ferry_core::screen::act::Action::Key((*s).into()));
                rest = tail;
            }
            ["--type", s, tail @ ..] if action.is_none() => {
                action = Some(ferry_core::screen::act::Action::Type((*s).into()));
                rest = tail;
            }
            ["--expect", s, tail @ ..] if expect.is_none() => {
                expect = Some((*s).to_string());
                rest = tail;
            }
            ["--timeout", s, tail @ ..] => {
                timeout = s.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            ["--dry-run", tail @ ..] => {
                dry_run = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let (Some(from), Some(action), Some(expect)) = (from, action, expect) else {
        return crate::usage();
    };
    finish((|| {
        if std::fs::metadata(from)?.len() > 128 * 1024 {
            return Err(Fail::new("слишком большой observe JSON"));
        }
        let value: Value =
            serde_json::from_slice(&std::fs::read(from)?).map_err(|e| Fail::new(e.to_string()))?;
        if value["device"].as_str() != Some(device.as_deref().unwrap_or("local"))
            || value["changed_during_capture"] != false
            || value["ui_truncated"] != false
        {
            return Err(Fail::new(
                "нужен полный стабильный observe этого устройства",
            ));
        }
        let request = ferry_core::screen::act::Request {
            window: serde_json::from_value(value["window"].clone())
                .map_err(|e| Fail::new(e.to_string()))?,
            ui: value["ui"].as_str().unwrap_or_default().into(),
            captured_at: value["captured_at"].as_u64().unwrap_or_default(),
            action,
            expect,
            timeout,
            dry_run,
        };
        let file = shot_path()?;
        let mut result = report(Request::Act {
            device: device.clone(),
            request,
            file,
        })?;
        result["device"] = device.clone().unwrap_or_else(|| "local".into()).into();
        if result["observation"].is_object() {
            result["observation"]["device"] = result["device"].clone();
        }
        let failed = result["error"].is_string() || (!dry_run && result["verified"] != true);
        println!("{result}");
        Ok(i32::from(failed))
    })())
}
pub(crate) fn shot_path() -> Result<PathBuf, Fail> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(ferry_core::ferry_home()?
        .join("shots")
        .join(format!("observe-{}-{nonce}.png", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_unknown_offline_and_wrong_os_before_probing() {
        let peer:PeerView=serde_json::from_value(json!({"id":"id","name":"test","online":false,"direct":null,"rtt_ms":null,"status":null})).unwrap();
        assert_eq!(rejected(&peer, Some("windows"), true).len(), 2);
    }
}
