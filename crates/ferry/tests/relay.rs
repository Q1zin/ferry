//! Релей git: `ferry ssh` «узла» → мост TCP → unix-сокет (вместо RemoteForward) → `ferry relay`
//! «хаба» → подменённый ssh. Потоки, код выхода, отказы и откат на обычный ssh.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

struct Hub {
    relay: Child,
    home: PathBuf,
    port: u16,
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.relay.kill();
        let _ = self.relay.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn hub(name: &str) -> Hub {
    let home = std::env::temp_dir().join(format!("ferry-relay-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join(".ferry")).unwrap();
    fs::write(home.join(".ferry/trust"), "git.test\n").unwrap();
    // Вместо ssh хаба: аргументы — в stderr, stdin заглавными — в stdout, код 3.
    let fake = home.join("fake-ssh");
    script(&fake, "echo \"args: $*\" >&2\ntr a-z A-Z\nexit 3");
    let relay = Command::new(FERRY)
        .arg("relay")
        .env("HOME", &home)
        .env("FERRY_RELAY_SSH", &fake)
        .spawn()
        .unwrap();

    let socket = home.join(".ferry/relay.sock");
    let start = Instant::now();
    while UnixStream::connect(&socket).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "релей не поднялся"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Вместо RemoteForward узла — мост TCP → unix-сокет.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let unix = UnixStream::connect(&socket).unwrap();
            let (mut tcp_in, mut unix_out) = (tcp.try_clone().unwrap(), unix.try_clone().unwrap());
            thread::spawn(move || {
                let _ = std::io::copy(&mut tcp_in, &mut unix_out);
                let _ = unix_out.shutdown(Shutdown::Write);
            });
            let (mut unix_in, mut tcp_out) = (unix, tcp);
            thread::spawn(move || {
                let _ = std::io::copy(&mut unix_in, &mut tcp_out);
                let _ = tcp_out.shutdown(Shutdown::Write);
            });
        }
    });
    Hub { relay, home, port }
}

/// `ferry ssh` на «узле», которому разрешено ходить через релей к `trust`.
fn node_ssh(hub: &Hub, port: u16, trust: &str, args: &[&str], stdin: &str) -> Output {
    let token = fs::read_to_string(hub.home.join(".ferry/relay-token")).unwrap();
    // Обычный ssh узла — тоже подменный: виден откат мимо релея.
    let bin = hub.home.join("node-bin");
    fs::create_dir_all(&bin).unwrap();
    script(&bin.join("ssh"), "echo \"plain ssh $*\"");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut child = Command::new(FERRY)
        .arg("ssh")
        .args(args)
        .env("PATH", path)
        .env("FERRY_RELAY", format!("{port} {}", token.trim()))
        .env("FERRY_TRUST", trust)
        .env("GIT_PROTOCOL", "version=2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn fetch_goes_through_the_hubs_ssh() {
    let hub = hub("fetch");
    let args = [
        "-o",
        "SendEnv=GIT_PROTOCOL",
        "-p",
        "2222",
        "git@git.test",
        "git-upload-pack 'group/repo.git'",
    ];
    let out = node_ssh(&hub, hub.port, "git.test", &args, "want abc\n");
    assert_eq!(text(&out.stdout), "WANT ABC\n");
    assert_eq!(
        text(&out.stderr),
        "args: -o BatchMode=yes -p 2222 -o SetEnv=GIT_PROTOCOL=version=2 -- \
         git@git.test git-upload-pack 'group/repo.git'\n"
    );
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn hub_refuses_anything_but_reading_from_trusted_hosts() {
    let hub = hub("refuse");
    let cases: [(&[&str], &str); 5] = [
        (
            &["git@git.test", "git-receive-pack 'r.git'"],
            "только чтение",
        ),
        (&["git@git.test", "sh -c id"], "только чтение"),
        (
            &["git@git.test", "git-upload-pack 'a'; id; 'b'"],
            "только чтение",
        ),
        (
            &["-oProxyCommand=id@git.test", "git-upload-pack 'r'"],
            "пользователь",
        ),
        // Узел считает хост доверенным, а хаб — нет: решает хаб.
        (&["git@evil.test", "git-upload-pack 'r'"], "не в доверенных"),
    ];
    for (args, why) in cases {
        let out = node_ssh(&hub, hub.port, "git.test evil.test", args, "");
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out.stderr).contains(why),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn untrusted_host_or_unreachable_relay_falls_back_to_plain_ssh() {
    let hub = hub("fallback");
    let args = ["git@github.com", "git-upload-pack 'r'"];
    let out = node_ssh(&hub, hub.port, "git.test", &args, "");
    assert_eq!(
        text(&out.stdout),
        "plain ssh git@github.com git-upload-pack 'r'\n"
    );

    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let args = ["git@git.test", "git-upload-pack 'r'"];
    let out = node_ssh(&hub, closed, "git.test", &args, "");
    assert_eq!(
        text(&out.stdout),
        "plain ssh git@git.test git-upload-pack 'r'\n"
    );
    assert!(text(&out.stderr).contains("недоступен"));
}
