//! Профиль → отдельный запуск → связанные доказательства → частичный отчёт матрицы.
use crate::{
    devices::{self, finish, state},
    diagnostics,
};
use ferry_core::{
    control::{self, Request, Response},
    desk::Shell,
    device::{Fail, Status},
    jobs::{self, evidence::Action},
    mode::Mode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    command: String,
    #[serde(default)]
    shell: Shell,
    #[serde(default)]
    tools: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    platforms: BTreeMap<String, Spec>,
    #[serde(default = "default_timeout")]
    timeout: u32,
    #[serde(default)]
    ready_log: Option<String>,
    #[serde(default)]
    ready_port: Option<u16>,
    #[serde(default)]
    junit: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    window: Option<String>,
}
fn default_timeout() -> u32 {
    300
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    profiles: BTreeMap<String, Profile>,
}
fn validate(name: &str, p: &Profile) -> Result<(), Fail> {
    if name.is_empty()
        || name.len() > 40
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err(Fail::new("имя профиля: до 40 букв, цифр, - и _"));
    }
    if !(1..=3600).contains(&p.timeout) || p.artifacts.len() > 32 || p.platforms.is_empty() {
        return Err(Fail::new(
            "timeout 1..3600, до 32 артефактов, нужны platforms",
        ));
    }
    if p.ready_log
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.len() > 256)
        || p.ready_port == Some(0)
        || p.window.as_ref().is_some_and(|w| w.trim().is_empty())
    {
        return Err(Fail::new(
            "пустое условие готовности/окно или неверный порт",
        ));
    }
    for path in p.artifacts.iter().chain(p.junit.iter()) {
        if path.is_empty()
            || path.len() > 240
            || path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
            || path.contains(['\\', ':', '\0'])
        {
            return Err(Fail::new(
                "артефакты: относительные пути внутри {output}, без .. и обратных слешей",
            ));
        }
    }
    for (os, s) in &p.platforms {
        if !["macos", "windows", "all"].contains(&os.as_str())
            || s.command.is_empty()
            || s.command.len() > 32768
        {
            return Err(Fail::new(
                "platforms: macos, windows или all; нужна команда до 32 КиБ",
            ));
        }
        if (!p.artifacts.is_empty() || p.junit.is_some()) && !s.command.contains("{output}") {
            return Err(Fail::new(
                "команда должна писать отчёты в {output}, уникальный каталог этого запуска",
            ));
        }
        ferry_core::capabilities::validate(&ferry_core::capabilities::Request {
            shell: s.shell,
            tools: s.tools.clone(),
            desktop: false,
        })?;
    }
    Ok(())
}
pub(crate) fn send(request: Request, session: &str) -> Result<Response, Fail> {
    match control::call_session(request, session)? {
        Response::Error(e) => Err(e),
        r => Ok(r),
    }
}
fn job_result(
    device: &Option<String>,
    name: &str,
    cursor: Option<String>,
    wait: u32,
    session: &str,
) -> Result<jobs::JobResult, Fail> {
    match send(
        Request::JobResult {
            device: device.clone(),
            name: name.into(),
            cursor,
            max_bytes: 16384,
            wait,
        },
        session,
    )? {
        Response::JobResult(r) => Ok(r),
        _ => Err(Fail::new("нет результата задачи")),
    }
}
fn command(spec: &Spec, output: &str) -> String {
    let cmd = spec.command.replace("{output}", output);
    match spec.shell {
        Shell::Powershell => format!(
            "try {{ New-Item -ItemType Directory -Path '{output}' -ErrorAction Stop | Out-Null }} catch {{ Write-Error $_; exit 1 }}; {cmd}"
        ),
        Shell::Cmd => format!("mkdir {} && ({cmd})", output.replace('/', "\\")),
        _ => format!("mkdir -p '{output}' && (\n{cmd}\n)"),
    }
}
fn preflight(
    device: &Option<String>,
    status: &Status,
    spec: &Spec,
    p: &Profile,
    session: &str,
) -> Result<Value, Fail> {
    let version = status
        .version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>();
    if !version.is_ok_and(|v| v.len() == 3 && v.as_slice() >= [0, 1, 22].as_slice()) {
        return Err(Fail::new("check требует ferry 0.1.22 на устройстве"));
    }
    // start всегда может открыть окно, поэтому даже консольный профиль требует active.
    if status.mode != Mode::Active || !status.role.accepts_commands() {
        return Err(Fail::refused("устройство не принимает фоновые проверки"));
    }
    let request = ferry_core::capabilities::Request {
        shell: spec.shell,
        tools: spec.tools.clone(),
        desktop: p.window.is_some(),
    };
    let cap = match device {
        Some(d) => match send(
            Request::Capabilities {
                device: d.clone(),
                request,
            },
            session,
        )? {
            Response::Capabilities(c) => c,
            _ => return Err(Fail::new("нет возможностей устройства")),
        },
        None => ferry_core::capabilities::collect(request)?,
    };
    if !cap.shell_available
        || !cap.missing_tools.is_empty()
        || !cap.warnings.is_empty()
        || (p.window.is_some() && cap.locked != Some(false))
    {
        return Err(Fail::new(format!(
            "устройство не готово: {}",
            serde_json::to_string(&cap).unwrap_or_default()
        )));
    }
    Ok(json!(cap))
}
fn check_one(
    device: Option<String>,
    status: &Status,
    name: &str,
    p: &Profile,
    project: &Option<devices::Project>,
    home: bool,
) -> Value {
    let target = device.as_deref().unwrap_or("local");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session = format!("check:{}:{stamp}:{target}", std::process::id());
    let job = format!("check-{name}");
    let output = format!(".ferry-checks/{}-{stamp}", std::process::id());
    let mut row = json!({"device":target,"profile":name,"job":job,"state":"unavailable","status":status,"output_directory":output,"errors":[]});
    let mut errors = Vec::<String>::new();
    let mut lease = false;
    let mut run_id = None;
    let mut terminal = false;
    let mut junit_id = None;
    let result = (|| -> Result<(), Fail> {
        let spec = p
            .platforms
            .get(&status.os)
            .or_else(|| p.platforms.get("all"))
            .ok_or_else(|| Fail::new(format!("нет команды для {}", status.os)))?;
        row["capabilities"] = preflight(&device, status, spec, p, &session)?;
        // Долгая бронь отделяет параллельные check даже от той же сессии агента.
        send(
            Request::Desk {
                device: target.into(),
                action: "take".into(),
                minutes: p.timeout / 60 + 12,
            },
            &session,
        )?;
        lease = true;
        if let (Some(d), Some(project)) = (&device, project)
            && let Some((commit, tree)) = &project.snapshot
        {
            send(
                Request::Sync {
                    device: d.clone(),
                    root: project.repo.root.clone(),
                    project: project.repo.name.clone(),
                    commit: commit.clone(),
                    tree: tree.clone(),
                },
                &session,
            )?;
        }
        let previous = match &device {
            Some(d) => match send(Request::Jobs { device: d.clone() }, &session)? {
                Response::Jobs(j) => j,
                _ => return Err(Fail::new("нет списка задач")),
            },
            None => jobs::rows()?,
        };
        if previous
            .iter()
            .any(|j| j.name == job && j.state == "running")
        {
            return Err(Fail::new(
                "предыдущая проверка ещё работает; сначала result/kill",
            ));
        }
        let cmd = command(spec, &output);
        row["state"] = "starting".into();
        match &device {
            Some(d) => {
                send(
                    Request::StartShell {
                        device: d.clone(),
                        name: job.clone(),
                        cmd,
                        dir: devices::dir(project),
                        shell: spec.shell,
                    },
                    &session,
                )?;
            }
            None => {
                let cwd = if home {
                    ferry_core::home()?
                } else {
                    std::env::current_dir()?
                };
                let exe = std::env::current_exe()?;
                jobs::start_in_shell(&job, &cmd, &cwd, &[], &exe, spec.shell)?;
            }
        }
        let first = job_result(&device, &job, None, 0, &session)?;
        let run = first.run_id.clone();
        run_id = Some(run.clone());
        row["run_id"] = run.clone().into();
        row["state"] = "running".into();
        let deadline = Instant::now() + Duration::from_secs(p.timeout.into());
        let mut ready_log = p.ready_log.is_none();
        let mut ready_port = p.ready_port.is_none();
        let mut carry = String::new();
        let mut current = first;
        loop {
            if current.run_id != run {
                return Err(Fail::new(
                    "задачу перезапустили извне; чужой запуск не трогаем",
                ));
            }
            carry.push_str(&current.log);
            if let Some(want) = &p.ready_log {
                ready_log |= carry.contains(want);
            }
            if carry.len() > 1024 {
                let mut offset = carry.len() - 1024;
                while !carry.is_char_boundary(offset) {
                    offset += 1;
                }
                carry = carry[offset..].into();
            }
            if let Some(port) = p.ready_port
                && !ready_port
            {
                ready_port = match &device {
                    Some(d) => matches!(
                        send(
                            Request::PortOpen {
                                device: d.clone(),
                                port
                            },
                            &session
                        )?,
                        Response::Flag(true)
                    ),
                    None => std::net::TcpStream::connect_timeout(
                        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                        Duration::from_millis(100),
                    )
                    .is_ok(),
                };
            }
            row["ready"] = json!({"log":ready_log,"port":ready_port});
            if current.state != "running" {
                terminal = true;
                break;
            }
            if Instant::now() >= deadline {
                row["timed_out"] = true.into();
                diagnostics::evidence(&device, &job, &run, Action::Stop)?;
                current = job_result(&device, &job, None, 0, &session)?;
                terminal = current.run_id == run && current.state != "running";
                errors.push("превышен timeout; остановлен только этот run_id".into());
                break;
            }
            current = job_result(
                &device,
                &job,
                Some(current.cursor.clone()),
                if current.more { 0 } else { 1 },
                &session,
            )?;
        }
        if !ready_log || !ready_port {
            errors.push("условия готовности не выполнены".into());
        }
        row["result"] = diagnostics::evidence(
            &device,
            &job,
            &run,
            Action::Result {
                cursor: None,
                max_bytes: 8192,
            },
        )?;
        let mut artifacts = Vec::new();
        if terminal {
            for path in p.artifacts.iter().chain(p.junit.iter()) {
                match diagnostics::evidence(
                    &device,
                    &job,
                    &run,
                    Action::Add {
                        path: format!("{output}/{path}"),
                    },
                ) {
                    Ok(a) => {
                        if p.junit.as_ref() == Some(path) {
                            let id = a["artifact"]["id"]
                                .as_str()
                                .ok_or_else(|| Fail::new("нет id JUnit"))?;
                            junit_id = Some(id.to_string());
                            match diagnostics::evidence(
                                &device,
                                &job,
                                &run,
                                Action::Tests {
                                    artifact: id.into(),
                                },
                            ) {
                                Ok(t) => {
                                    if !tests_pass(&t) {
                                        errors.push(
                                            "JUnit: падения, пустой или неполный отчёт".into(),
                                        );
                                    }
                                    row["tests"] = t;
                                }
                                Err(e) => errors.push(format!("JUnit: {e}")),
                            }
                        }
                        artifacts.push(a["artifact"].clone());
                    }
                    Err(e) => errors.push(format!("артефакт {path}: {e}")),
                }
            }
        }
        row["artifacts"] = artifacts.into();
        row["state"] = if current.state == "succeeded" && errors.is_empty() {
            "succeeded"
        } else {
            "failed"
        }
        .into();
        Ok(())
    })();
    if let Err(e) = result {
        errors.push(e.to_string());
        if run_id.is_some() {
            row["state"] = "incomplete".into();
        }
    }
    if row["state"] != "succeeded"
        && let Some(run) = &run_id
    {
        let diagnosis = diagnostics::collect(
            &device,
            &job,
            run,
            p.window.as_deref(),
            junit_id.as_deref(),
            Some(&session),
        );
        match diagnostics::save(&diagnosis, "diagnostics") {
            Ok(file) => row["diagnostic_file"] = file.to_string_lossy().into_owned().into(),
            Err(e) => errors.push(format!("сохранение диагностики: {e}")),
        }
        row["diagnostics"] = diagnosis;
    }
    // При обрыве задача может продолжать работать; бронь истечёт сама, её не отдаём раньше.
    if lease && (terminal || run_id.is_none() && row["state"] == "unavailable") {
        if let Err(e) = send(
            Request::Desk {
                device: target.into(),
                action: "free".into(),
                minutes: 0,
            },
            &session,
        ) {
            errors.push(format!("снять бронь: {e}"));
        }
    } else if lease {
        row["lease_retained"] = true.into();
    }
    if !errors.is_empty() && row["state"] == "succeeded" {
        row["state"] = "failed".into();
    }
    row["errors"] = json!(errors);
    row
}
fn tests_pass(t: &Value) -> bool {
    t["tests"].as_u64().unwrap_or(0) > 0
        && t["failed"] == 0
        && t["errors"] == 0
        && t["warnings"].as_array().is_some_and(|a| a.is_empty())
}

pub fn run(args: &[&str]) -> io::Result<i32> {
    let [name, tail @ ..] = args else {
        return crate::usage();
    };
    let mut rest = tail;
    let (mut file, mut targets, mut parallel, mut home, mut no_sync) =
        ("ferry.checks.json", "local", 2usize, false, false);
    while !rest.is_empty() {
        match rest {
            ["--file", p, tail @ ..] => {
                file = p;
                rest = tail;
            }
            ["--devices", d, tail @ ..] => {
                targets = d;
                rest = tail;
            }
            ["--parallel", n, tail @ ..] => {
                parallel = n.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            ["--home", tail @ ..] => {
                home = true;
                rest = tail;
            }
            ["--no-sync", tail @ ..] => {
                no_sync = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    if !(1..=8).contains(&parallel) {
        return Err(io::Error::other("--parallel: 1..8"));
    }
    finish((|| {
        if std::fs::metadata(file)?.len() > 128 * 1024 {
            return Err(Fail::new("профили больше 128 КиБ"));
        }
        let config: Config =
            serde_json::from_slice(&std::fs::read(file)?).map_err(|e| Fail::new(e.to_string()))?;
        let profile = config
            .profiles
            .get(*name)
            .ok_or_else(|| Fail::new("нет такого профиля"))?;
        validate(name, profile)?;
        let snapshot = state()?;
        let mut selected: Vec<(Option<String>, Status)> = Vec::new();
        let mut rows = Vec::new();
        for target in targets.split(',') {
            if target == "local" {
                if !selected.iter().any(|(d, _)| d.is_none()) {
                    selected.push((None, snapshot.me.clone()));
                }
                continue;
            }
            let peers: Vec<_> = match target {
                "all" => snapshot.peers.iter().collect(),
                os if os.starts_with("os:") => snapshot
                    .peers
                    .iter()
                    .filter(|p| p.status.as_ref().is_some_and(|s| s.os == os[3..]))
                    .collect(),
                d => match snapshot.find(d) {
                    Ok(p) => vec![p],
                    Err(e) => {
                        rows.push(json!({"device":d,"state":"unavailable","errors":[e]}));
                        continue;
                    }
                },
            };
            for peer in peers {
                if selected
                    .iter()
                    .any(|(d, _)| d.as_deref() == Some(&peer.name))
                {
                    continue;
                }
                if peer.online
                    && let Some(status) = &peer.status
                {
                    selected.push((Some(peer.name.clone()), status.clone()));
                } else {
                    rows.push(json!({"device":peer.name,"state":"unavailable","errors":["нет связи или статуса"]}));
                }
            }
        }
        let project = if selected.iter().any(|(d, _)| d.is_some()) {
            devices::project(!no_sync, home)?
        } else {
            None
        };
        let next = std::sync::atomic::AtomicUsize::new(0);
        let completed = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..parallel.min(selected.len()) {
                let (next, completed, selected, project) = (&next, &completed, &selected, &project);
                scope.spawn(move||loop {
                    let index=next.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                    let Some((d,status))=selected.get(index) else {break;};
                    let mut row=std::panic::catch_unwind(||check_one(d.clone(),status,name,profile,project,home)).unwrap_or_else(|_|json!({"device":d,"state":"incomplete","errors":["проверка прервалась"]}));
                    match diagnostics::save(&row,"checks"){Ok(p)=>row["report_file"]=p.to_string_lossy().into_owned().into(),Err(e)=>row["save_error"]=e.to_string().into()}
                    eprintln!("ferry check [{}]: {}",d.as_deref().unwrap_or("local"),row["state"].as_str().unwrap_or("incomplete"));
                    completed.lock().unwrap().push(row);
                });
            }
        });
        rows.extend(completed.into_inner().unwrap());
        rows.sort_by(|a, b| a["device"].as_str().cmp(&b["device"].as_str()));
        let succeeded = rows.iter().filter(|r| r["state"] == "succeeded").count();
        let ok = !rows.is_empty()
            && succeeded == rows.len()
            && rows.iter().all(|r| r["save_error"].is_null());
        let mut value = json!({"profile":name,"state":if ok{"succeeded"}else{"failed"},"succeeded":succeeded,"total":rows.len(),"results":rows});
        let file = diagnostics::save(&value, "checks")?;
        value["report_file"] = file.to_string_lossy().into_owned().into();
        println!("{value}");
        Ok(i32::from(!ok))
    })())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_reject_stale_outputs_and_unknown_fields() {
        let mut p: Profile = serde_json::from_value(
            json!({"platforms":{"all":{"command":"test"}},"junit":"junit.xml"}),
        )
        .unwrap();
        assert!(validate("smoke", &p).is_err());
        p.platforms.get_mut("all").unwrap().command = "test --output {output}".into();
        assert!(validate("smoke", &p).is_ok());
        p.junit = Some("../old.xml".into());
        assert!(validate("smoke", &p).is_err());
        assert!(serde_json::from_value::<Profile>(json!({"platforms":{},"timout":3})).is_err());
        assert!(!tests_pass(
            &json!({"tests":0,"failed":0,"errors":0,"warnings":[]})
        ));
        assert!(!tests_pass(
            &json!({"tests":1,"failed":0,"errors":0,"warnings":["mismatch"]})
        ));
        assert!(tests_pass(
            &json!({"tests":1,"failed":0,"errors":0,"warnings":[]})
        ));
    }
}
