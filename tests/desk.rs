//! Демон и клиент целиком: вывод и код выхода, фоновые процессы, обрыв соединения.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

struct Daemon {
    child: Child,
    home: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn daemon(name: &str) -> Daemon {
    let home = std::env::temp_dir().join(format!("ferry-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let child = Command::new(FERRY)
        .arg("daemon")
        .env("HOME", &home)
        .env("FERRY_SHELL", "/bin/sh")
        .spawn()
        .unwrap();
    let daemon = Daemon { child, home };
    let start = Instant::now();
    while !daemon.home.join(".ferry/desk").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "демон не записал ~/.ferry/desk"
        );
        sleep(Duration::from_millis(20));
    }
    daemon
}

fn desk(daemon: &Daemon, cmd: &str) -> Command {
    let mut command = Command::new(FERRY);
    command
        .args(["desk", cmd])
        .env("HOME", &daemon.home)
        .current_dir(&daemon.home);
    command
}

#[test]
fn output_cwd_and_exit_code() {
    let d = daemon("out");
    let out = desk(&d, "pwd; echo out; echo err >&2; exit 7")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    let text = String::from_utf8(out.stdout).unwrap();
    let (cwd, rest) = text.split_once('\n').unwrap();
    assert!(
        cwd.ends_with(d.home.file_name().unwrap().to_str().unwrap()),
        "{text:?}"
    );
    assert_eq!(rest, "out\nerr\n");
}

#[test]
fn background_process_does_not_hold_the_call() {
    let d = daemon("bg");
    let start = Instant::now();
    let out = desk(&d, "sleep 5 & echo started").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "started\n");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn disconnect_kills_the_command() {
    let d = daemon("kill");
    let mut client = desk(&d, "echo $$; exec sleep 30")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pid = String::new();
    BufReader::new(client.stdout.take().unwrap())
        .read_line(&mut pid)
        .unwrap();
    client.kill().unwrap();
    client.wait().unwrap();

    let alive = || {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    let start = Instant::now();
    while alive() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "команда пережила обрыв"
        );
        sleep(Duration::from_millis(50));
    }
}
