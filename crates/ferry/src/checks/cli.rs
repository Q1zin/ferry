use super::*;
use journal::{Operation, Project};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

fn spawn(root: &Path, id: &str, collect_only: bool) -> Result<(), Fail> {
    if journal::active(root)? {
        return Ok(());
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("worker.log"))?;
    let executable =
        jobs::prepare_supervisor(&ferry_core::ferry_home()?, &std::env::current_exe()?)?;
    let mut command = Command::new(executable);
    command
        .args(["check", "worker", id])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if collect_only {
        command.arg("--collect-only");
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x08000000 | 0x00000200);
    let mut child = command.spawn()?;
    for _ in 0..100 {
        if journal::active(root)? || child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Fail::new(
        "worker не подтвердил запуск; check status/resume покажет состояние",
    ))
}

fn output(root: &Path, wait: bool) -> Result<i32, Fail> {
    loop {
        let report = journal::report(root)?;
        let active = report["worker_active"] == true;
        if !wait || !active {
            journal::write(&root.join("report.json"), &report)?;
            println!("{report}");
            return Ok(i32::from(!matches!(
                report["state"].as_str(),
                Some("succeeded" | "running" | "cancelling")
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

type Selected = Vec<(Option<String>, Option<Status>)>;
fn targets(selector: &str) -> Result<Selected, Fail> {
    let snapshot = state()?;
    let mut selected: Vec<(Option<String>, Option<Status>)> = Vec::new();
    for target in selector.split(',') {
        if target == "local" {
            if !selected.iter().any(|(d, _)| d.is_none()) {
                selected.push((None, Some(snapshot.me.clone())));
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
            name => match snapshot.find(name) {
                Ok(p) => vec![p],
                Err(_) => {
                    selected.push((Some(name.into()), None));
                    continue;
                }
            },
        };
        for peer in peers {
            if !selected
                .iter()
                .any(|(d, _)| d.as_deref() == Some(peer.name.as_str()))
            {
                selected.push((
                    Some(peer.name.clone()),
                    peer.online.then(|| peer.status.clone()).flatten(),
                ));
            }
        }
    }
    if selected.is_empty() || selected.len() > 64 {
        return Err(Fail::new("check: нужны 1..64 устройства"));
    }
    Ok(selected)
}

fn same_request(a: &Operation, b: &Operation) -> bool {
    a.profile_name == b.profile_name
        && a.profile == b.profile
        && a.cwd == b.cwd
        && a.isolated == b.isolated
        && a.targets
            .iter()
            .map(|(d, _)| d)
            .eq(b.targets.iter().map(|(d, _)| d))
        && a.project.as_ref().map(|p| {
            (
                &p.root,
                &p.prefix,
                p.snapshot.as_ref().map(|(_, tree)| tree),
            )
        }) == b.project.as_ref().map(|p| {
            (
                &p.root,
                &p.prefix,
                p.snapshot.as_ref().map(|(_, tree)| tree),
            )
        })
}

pub fn run(args: &[&str]) -> io::Result<i32> {
    if let [
        action @ ("status" | "resume" | "collect" | "cancel" | "worker"),
        id,
        tail @ ..,
    ] = args
    {
        let (detach, collect_only) = match tail {
            [] => (false, false),
            ["--detach"] if *action != "worker" => (true, false),
            ["--collect-only"] if *action == "worker" => (false, true),
            _ => return crate::usage(),
        };
        return finish((|| {
            let root = journal::root(id)?;
            let op: Operation = journal::read(&root.join("operation.json"))?;
            if *action == "worker" {
                return engine::worker(&root, &op, collect_only);
            }
            if *action == "cancel" {
                if journal::terminal(journal::report(&root)?["state"].as_str().unwrap_or("")) {
                    return output(&root, false);
                }
                ferry_core::write_private(&root.join("cancel"), "cancel requested")?;
            }
            if *action != "status" {
                spawn(&root, id, *action == "collect")?;
            }
            output(&root, *action != "status" && !detach)
        })());
    }
    let [name, tail @ ..] = args else {
        return crate::usage();
    };
    let mut rest = tail;
    let (
        mut file,
        mut selector,
        mut parallel,
        mut home,
        mut no_sync,
        mut detached,
        mut isolated,
        mut id,
    ) = (
        "ferry.checks.json",
        "local",
        2usize,
        false,
        false,
        false,
        false,
        None,
    );
    while !rest.is_empty() {
        match rest {
            ["--file", p, tail @ ..] => {
                file = p;
                rest = tail;
            }
            ["--devices", d, tail @ ..] => {
                selector = d;
                rest = tail;
            }
            ["--parallel", n, tail @ ..] => {
                parallel = n.parse().map_err(io::Error::other)?;
                rest = tail;
            }
            ["--id", n, tail @ ..] => {
                id = Some(*n);
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
            ["--detach", tail @ ..] => {
                detached = true;
                rest = tail;
            }
            ["--isolated", tail @ ..] => {
                isolated = true;
                rest = tail;
            }
            _ => return crate::usage(),
        }
    }
    finish((|| {
        if !(1..=8).contains(&parallel) {
            return Err(Fail::new("--parallel: 1..8"));
        }
        if fs::metadata(file)?.len() > 128 * 1024 {
            return Err(Fail::new("профили больше 128 КиБ"));
        }
        let config: Config = serde_json::from_slice(&fs::read(file)?).map_err(Fail::new)?;
        let profile = config
            .profiles
            .get(*name)
            .ok_or_else(|| Fail::new("нет такого профиля"))?
            .clone();
        validate(name, &profile)?;
        let selected = targets(selector)?;
        let project = devices::project(!no_sync, home)?.map(|p| Project {
            root: p.repo.root,
            name: p.repo.name,
            prefix: p.repo.prefix,
            snapshot: p.snapshot,
        });
        // Изоляция обязательна для новых lifecycle-профилей внутри git-проекта.
        let isolated = isolated
            || (project.is_some()
                && profile
                    .platforms
                    .values()
                    .any(|s| s.launch.is_some() || s.build.is_some()));
        if isolated && project.as_ref().is_none_or(|p| p.snapshot.is_none()) {
            return Err(Fail::new("изоляция требует git-проект и синхронизацию"));
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| format!("c-{:x}-{stamp:x}", std::process::id()));
        let root = journal::root(&id)?;
        fs::create_dir_all(&root)?;
        let init = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("init.lock"))?;
        init.lock()?;
        let op = Operation {
            id: id.clone(),
            profile_name: (*name).into(),
            profile,
            cwd: if home {
                ferry_core::home()?
            } else {
                std::env::current_dir()?
            },
            project,
            isolated,
            targets: selected,
            parallel,
            created: ferry_core::now(),
        };
        let path = root.join("operation.json");
        if path.exists() {
            let saved: Operation = journal::read(&path)?;
            if !same_request(&saved, &op) {
                return Err(Fail::new(
                    "--id уже связан с другим профилем, снимком или устройствами; для продолжения check resume ID",
                ));
            }
        } else {
            journal::write(&path, &op)?;
        }
        drop(init);
        spawn(&root, &id, false)?;
        output(&root, !detached)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_id_cannot_change_effects() {
        let mut a = Operation {
            id: "x".into(),
            profile_name: "smoke".into(),
            profile: serde_json::from_value(json!({"platforms":{"all":{"command":"true"}}}))
                .unwrap(),
            cwd: "/tmp".into(),
            project: None,
            isolated: false,
            targets: vec![(None, None)],
            parallel: 1,
            created: 1,
        };
        let b = a.clone();
        a.created = 2;
        assert!(same_request(&a, &b));
        a.profile.platforms.get_mut("all").unwrap().command = "false".into();
        assert!(!same_request(&a, &b));
    }
}
