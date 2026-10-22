//! Фоновые задачи узла целиком: запуск, лог, ожидание с кодом, остановка, перезапуск.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

struct Node {
    home: PathBuf,
}

impl Node {
    fn new(name: &str) -> Node {
        let home = std::env::temp_dir().join(format!("ferry-jobs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        Node { home }
    }

    fn ferry(&self, args: &[&str]) -> Output {
        Command::new(FERRY)
            .args(args)
            .env("HOME", &self.home)
            .env("FERRY_SHELL", "/bin/sh")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn jobs(&self) -> String {
        String::from_utf8(self.ferry(&["jobs"]).stdout).unwrap()
    }

    fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) {
        let start = Instant::now();
        while !done(&self.jobs()) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "не дождались: {what}\n{}",
                self.jobs()
            );
            sleep(Duration::from_millis(50));
        }
    }

    fn file(&self, job: &str, file: &str) -> String {
        fs::read_to_string(self.home.join(".ferry/jobs").join(job).join(file)).unwrap()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.ferry(&["stop"]);
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn invalid_job_names_cannot_touch_state_or_other_jobs() {
    let node = Node::new("invalid");
    let state = node.home.join(".ferry");
    fs::create_dir_all(state.join("jobs/keep")).unwrap();
    for path in ["key", "device.json", "jobs/keep/cmd", "jobs/keep/log"] {
        fs::write(state.join(path), "keep").unwrap();
    }
    for name in [
        "",
        ".",
        "..",
        "../keep",
        "/tmp/escape",
        "a\\b",
        "CON",
        "nul.txt",
        "COM1",
        "Lpt9.log",
        "app.",
    ] {
        for args in [
            vec!["job", name, "true"],
            vec!["log", name],
            vec!["kill", name],
            vec!["job-supervise", name],
        ] {
            let out = node.ferry(&args);
            assert!(!out.status.success(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("недопустимое имя задачи"),
                "{args:?}: {out:?}"
            );
        }
        for path in ["key", "device.json", "jobs/keep/cmd", "jobs/keep/log"] {
            assert_eq!(fs::read_to_string(state.join(path)).unwrap(), "keep");
        }
    }
}

#[test]
fn job_runs_in_background_and_records_its_exit() {
    let node = Node::new("exit");
    let start = Instant::now();
    assert!(
        node.ferry(&["job", "t", "echo hi; pwd; sleep 1; exit 3"])
            .status
            .success()
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "запуск ждал задачу"
    );
    assert!(node.jobs().contains("работает"));

    node.wait_until("код 3", |jobs| jobs.contains("завершилась, код 3"));
    let log = String::from_utf8(node.ferry(&["log", "t"]).stdout).unwrap();
    let (first, cwd) = log.trim_end().split_once('\n').unwrap();
    assert_eq!(first, "hi");
    assert!(
        cwd.ends_with(node.home.file_name().unwrap().to_str().unwrap()),
        "{cwd}"
    );
}

#[test]
fn log_follow_waits_for_the_end_and_returns_the_jobs_code() {
    let node = Node::new("follow");
    node.ferry(&["job", "t", "echo a; sleep 1; echo b; exit 4"]);
    let out = node.ferry(&["log", "t", "-f"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "a\nb\n");
    assert_eq!(out.status.code(), Some(4));
}

#[test]
fn kill_stops_the_command_and_restart_replaces_it() {
    let node = Node::new("kill");
    node.ferry(&["job", "t", "exec sleep 30"]);
    node.wait_until("child", |_| node.home.join(".ferry/jobs/t/child").exists());
    let first = node.file("t", "child");

    // То же имя ещё раз — старая команда гаснет, новая работает.
    node.ferry(&["job", "t", "exec sleep 30"]);
    assert!(!alive(&first), "старая команда пережила перезапуск");
    node.wait_until("новый child", |_| {
        node.home.join(".ferry/jobs/t/child").exists()
    });
    let second = node.file("t", "child");
    assert!(alive(&second));

    assert!(node.ferry(&["kill", "t"]).status.success());
    assert!(node.jobs().contains("остановлена"), "{}", node.jobs());
    assert!(!alive(&second), "команда пережила kill");
}

#[test]
fn a_stranger_with_the_old_pid_is_not_the_job() {
    // Надсмотрщика убили (так бывает при обновлении приложения), а его PID и PID команды
    // достались другому процессу: задача не работает, и ни kill, ни перезапуск того не трогают.
    let node = Node::new("stale");
    node.ferry(&["job", "t", "exec sleep 30"]);
    node.wait_until("child", |_| node.home.join(".ferry/jobs/t/child").exists());
    for file in ["pid", "child"] {
        let pid = node.file("t", file);
        Command::new("kill")
            .args(["-9", pid.trim()])
            .status()
            .unwrap();
    }
    let mut stranger = Command::new("sleep").arg("30").spawn().unwrap();
    let job = node.home.join(".ferry/jobs/t");
    for file in ["pid", "child"] {
        fs::write(job.join(file), stranger.id().to_string()).unwrap();
    }
    node.wait_until("пропала", |jobs| jobs.contains("пропала"));
    node.ferry(&["kill", "t"]);
    assert!(alive(&stranger.id().to_string()), "kill убил чужой процесс");

    // Задача старой версии — без блокировки: решает PID, но чужой процесс — не ferry.
    fs::remove_file(job.join("lock")).unwrap();
    assert!(node.jobs().contains("пропала"), "{}", node.jobs());
    node.ferry(&["job", "t", "true"]);
    assert!(
        alive(&stranger.id().to_string()),
        "перезапуск убил чужой процесс"
    );
    node.wait_until("новая отработала", |jobs| {
        jobs.contains("код 0")
    });
    stranger.kill().unwrap();
    stranger.wait().unwrap();
}
