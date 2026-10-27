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
    let (mut window, mut ocr, mut screenshot, mut structured, mut query, mut subtree, mut since) =
        (String::new(), false, true, false, None, false, None);
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
            ["--structured", tail @ ..] => {
                structured = true;
                rest = tail;
            }
            ["--no-screenshot", tail @ ..] => {
                screenshot = false;
                structured = true;
                rest = tail;
            }
            [flag @ ("--element" | "--subtree"), value, tail @ ..] if query.is_none() => {
                query = Some(json_arg::<ferry_core::screen::elements::Locator>(value)?);
                subtree = *flag == "--subtree";
                structured = true;
                rest = tail;
            }
            ["--since", path, tail @ ..] if since.is_none() => {
                since = Some(*path);
                structured = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        let file = shot_path()?;
        let mut value = if structured {
            report(Request::ObserveV2 {
                device: device.clone(),
                request: ferry_core::screen::observe::RequestV2 {
                    window,
                    ocr,
                    screenshot,
                    query,
                    subtree,
                },
                file,
            })?
        } else {
            report(Request::Observe {
                device: device.clone(),
                request: ferry_core::screen::observe::Request { window, ocr },
                file,
            })?
        };
        value["device"] = device.unwrap_or_else(|| "local".into()).into();
        if structured {
            value["schema_version"] = 2.into();
            if let Some(path) = since {
                let previous = read_observation(path)?;
                let delta = observation_delta(&previous, &value)?;
                value["snapshot_file"] = save_observation(&value)?
                    .to_string_lossy()
                    .into_owned()
                    .into();
                value.as_object_mut().unwrap().remove("elements");
                value["delta"] = delta;
            }
        }
        println!("{value}");
        Ok(0)
    })())
}
fn json_arg<T: serde::de::DeserializeOwned>(text: &str) -> io::Result<T> {
    if text.len() > 16384 {
        return Err(io::Error::other("JSON аргумент больше 16 КиБ"));
    }
    serde_json::from_str(text).map_err(io::Error::other)
}
fn read_json(path: &Path) -> Result<Value, Fail> {
    if std::fs::metadata(path)?.len() > 2 * 1024 * 1024 {
        return Err(Fail::new("наблюдение больше 2 МиБ"));
    }
    serde_json::from_slice(&std::fs::read(path)?).map_err(|e| Fail::new(e.to_string()))
}
fn read_observation(path: &str) -> Result<Value, Fail> {
    let value = read_json(Path::new(path))?;
    if value["elements"].is_array() {
        return Ok(value);
    }
    if let Some(snapshot) = value["snapshot_file"].as_str() {
        let root = ferry_core::ferry_home()?
            .join("observations")
            .canonicalize()?;
        let snapshot = Path::new(snapshot).canonicalize()?;
        if !snapshot.starts_with(root) {
            return Err(Fail::new(
                "snapshot_file должен быть сохранён ferry в observations",
            ));
        }
        return read_json(&snapshot);
    }
    Ok(value)
}
fn save_observation(value: &Value) -> Result<PathBuf, Fail> {
    let root = ferry_core::ferry_home()?.join("observations");
    std::fs::create_dir_all(&root)?;
    let path = root.join(format!(
        "{}-{}.json",
        std::process::id(),
        ferry_core::jobs::unique_id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(&path)?
        .write_all(&serde_json::to_vec(value).map_err(|e| Fail::new(e.to_string()))?)?;
    Ok(path)
}
fn observation_delta(previous: &Value, current: &Value) -> Result<Value, Fail> {
    use ferry_core::screen::elements::{Element, ancestors};
    use std::collections::BTreeMap;
    for field in ["device", "query", "subtree"] {
        if previous[field] != current[field] {
            return Err(Fail::new(
                "--since требует то же устройство и область элементов",
            ));
        }
    }
    if previous["window"]["id"] != current["window"]["id"]
        || previous["window"]["pid"] != current["window"]["pid"]
    {
        return Err(Fail::new(
            "--since требует то же окно; сделай новое observe",
        ));
    }
    let parse = |v: &Value| {
        serde_json::from_value::<Vec<Element>>(v["elements"].clone())
            .map_err(|e| Fail::new(e.to_string()))
    };
    let old = parse(previous)?;
    let new = parse(current)?;
    let fields_complete = !old
        .iter()
        .chain(new.iter())
        .any(|e| e.name_truncated || e.id_truncated || e.value_truncated);
    let keyed = |all: &[Element]| {
        let mut map: BTreeMap<String, Vec<Element>> = BTreeMap::new();
        for e in all {
            let identity = |e: &Element| (e.role.clone(), e.id.clone(), e.name.clone());
            let mut parts: Vec<_> = ancestors(e, all)
                .iter()
                .rev()
                .map(|e| identity(e))
                .collect();
            parts.push(identity(e));
            let key = serde_json::to_string(&parts).unwrap();
            map.entry(key).or_default().push(e.clone());
        }
        map
    };
    let mut old = keyed(&old);
    let new = keyed(&new);
    let (mut added, mut removed, mut changed, mut ambiguous) =
        (Vec::new(), Vec::new(), Vec::new(), 0usize);
    for (key, after) in new {
        let before = old.remove(&key).unwrap_or_default();
        match (before.as_slice(), after.as_slice()) {
            ([before], [after]) => {
                let mut a = before.clone();
                let mut b = after.clone();
                a.index = 0;
                a.parent = None;
                b.index = 0;
                b.parent = None;
                if a != b {
                    changed.push(json!({"before":before,"after":after}));
                }
            }
            ([], _) => added.extend(after),
            _ => {
                ambiguous += 1;
                removed.extend(before);
                added.extend(after);
            }
        }
    }
    for rows in old.into_values() {
        removed.extend(rows);
    }
    Ok(
        json!({"added":added,"removed":removed,"changed":changed,"ambiguous_groups":ambiguous,
        "complete":previous["complete"]==true && current["complete"]==true && fields_complete,
        "note":"Индексы относятся к одному наблюдению. Неоднозначные совпадения показаны как removed/added; исчезновение из неполного дерева не доказывает исчезновение из приложения."}),
    )
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
pub(crate) fn download(
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
                    let human = if peer.online {
                        report(Request::Human { device:Some(peer.name.clone()), request:ferry_core::human::Request::Presence })
                            .unwrap_or_else(|e|json!({"available":false,"error":e.to_string()}))
                    } else { json!({"available":false,"error":"устройство не на связи"}) };
                    json!({"device":peer.name,"eligible":reasons.is_empty(),"reasons":reasons,"capabilities":cap,"status":peer.status,"human":human})
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
            json!({"matches":matches,"rejected":rejections,"checked_at":ferry_core::now(),"requirements":request,"os":os,"reserved":false,"note":"Снимок доступности ferry. Human/idle — сигнал активности, не разрешение трогать рабочий стол. Перед GUI-прогоном учитывай указания пользователя и займи стол через desk take."})
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
    let structured = args.iter().any(|s| {
        matches!(
            *s,
            "--click-locator"
                | "--target"
                | "--expect-json"
                | "--expect-window"
                | "--allow-already"
                | "--no-screenshot"
        )
    }) || args
        .windows(2)
        .find(|w| w[0] == "--from")
        .and_then(|w| read_observation(w[1]).ok())
        .is_some_and(|v| v["elements"].is_array());
    if structured {
        return act_structured(args);
    }

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
fn act_structured(args: &[&str]) -> io::Result<i32> {
    use ferry_core::screen::{
        act::{ActionV2, Assertion, Property, RequestV2, Snapshot},
        elements::Locator,
    };
    let (device, mut rest) = match args {
        [d, tail @ ..] if !d.starts_with('-') => (Some((*d).to_string()), tail),
        _ => (None, args),
    };
    let (mut from, mut action, mut target, mut expect, mut new_window) =
        (None, None, None, None, None);
    let (mut timeout, mut dry_run, mut allow_already, mut screenshot) = (10, false, false, true);
    while !rest.is_empty() {
        match rest {
            ["--from", s, tail @ ..] if from.is_none() => {
                from = Some(*s);
                rest = tail;
            }
            ["--click", s, tail @ ..] if action.is_none() => {
                action = Some(ActionV2::Click(Locator {
                    name: Some((*s).into()),
                    ..Default::default()
                }));
                rest = tail;
            }
            ["--click-locator", s, tail @ ..] if action.is_none() => {
                action = Some(ActionV2::Click(json_arg(s)?));
                rest = tail;
            }
            ["--key", s, tail @ ..] if action.is_none() => {
                action = Some(ActionV2::Key((*s).into()));
                rest = tail;
            }
            ["--type", s, tail @ ..] if action.is_none() => {
                action = Some(ActionV2::Type((*s).into()));
                rest = tail;
            }
            ["--target", s, tail @ ..] if target.is_none() => {
                target = Some(json_arg::<Locator>(s)?);
                rest = tail;
            }
            ["--expect", s, tail @ ..] if expect.is_none() => {
                expect = Some(Assertion {
                    locator: Locator {
                        name: Some((*s).into()),
                        ..Default::default()
                    },
                    property: Property::Exists,
                    value: None,
                    state: None,
                });
                rest = tail;
            }
            ["--expect-json", s, tail @ ..] if expect.is_none() => {
                expect = Some(json_arg::<Assertion>(s)?);
                rest = tail;
            }
            ["--expect-window", s, tail @ ..] if new_window.is_none() => {
                new_window = Some((*s).to_string());
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
            ["--allow-already", tail @ ..] => {
                allow_already = true;
                rest = tail;
            }
            ["--no-screenshot", tail @ ..] => {
                screenshot = false;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    let (Some(from), Some(action)) = (from, action) else {
        return crate::usage();
    };
    if expect.is_none() && new_window.is_none()
        || matches!(action, ActionV2::Click(_)) && target.is_some()
    {
        return crate::usage();
    }
    finish((|| {
        let value = read_observation(from)?;
        if value["device"].as_str() != Some(device.as_deref().unwrap_or("local")) {
            return Err(Fail::new("наблюдение другого устройства"));
        }
        let snapshot: Snapshot = serde_json::from_value(value)
            .map_err(|e| Fail::new(format!("нужно observe --structured: {e}")))?;
        let request = RequestV2 {
            snapshot,
            action,
            target,
            expect,
            new_window,
            allow_already,
            timeout,
            dry_run,
            screenshot,
        };
        let mut result = report(Request::ActV2 {
            device: device.clone(),
            request: Box::new(request),
            file: shot_path()?,
        })?;
        result["device"] = device.unwrap_or_else(|| "local".into()).into();
        if result["observation"].is_object() {
            result["observation"]["device"] = result["device"].clone();
            result["observation"]["schema_version"] = 2.into();
        }
        let failed = result["error"].is_string() || !dry_run && result["verified"] != true;
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
    fn compact_delta_tracks_values_without_treating_indices_as_identity() {
        use ferry_core::screen::elements::Element;
        let e = Element {
            index: 1,
            parent: None,
            role: "Edit".into(),
            name: "Name".into(),
            id: "input".into(),
            rect: Some((0, 0, 20, 20)),
            enabled: Some(true),
            focused: Some(true),
            checked: None,
            selected: None,
            value: Some("old".into()),
            protected: false,
            name_truncated: false,
            id_truncated: false,
            value_truncated: false,
        };
        let before = json!({"device":"local","window":{"id":1,"pid":2},"query":null,"subtree":false,"complete":true,"elements":[e]});
        let mut after = before.clone();
        after["elements"][0]["index"] = 99.into();
        let delta = observation_delta(&before, &after).unwrap();
        assert!(delta["changed"].as_array().unwrap().is_empty());
        after["elements"][0]["value"] = "new".into();
        assert_eq!(
            observation_delta(&before, &after).unwrap()["changed"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        after["window"]["pid"] = 3.into();
        assert!(observation_delta(&before, &after).is_err());
    }
    #[test]
    fn unknown_locator_fields_and_ambiguous_diffs_are_not_silently_accepted() {
        assert!(
            json_arg::<ferry_core::screen::elements::Locator>(r#"{"automation_id":"typo"}"#)
                .is_err()
        );
        assert!(
            json_arg::<ferry_core::screen::act::Assertion>(
                r#"{"locator":{"name":"Save"},"property":"enabled","state":false}"#
            )
            .unwrap()
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn refuses_unknown_offline_and_wrong_os_before_probing() {
        let peer:PeerView=serde_json::from_value(json!({"id":"id","name":"test","online":false,"direct":null,"rtt_ms":null,"status":null})).unwrap();
        assert_eq!(rejected(&peer, Some("windows"), true).len(), 2);
    }
}
