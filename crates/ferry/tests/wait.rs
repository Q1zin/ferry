//! `ferry wait --log` и `--port` целиком: настоящий бинарник `ferry`, сервер управления, агенты в
//! этом процессе и фоновые задачи. Задачи живут в `~/.ferry/jobs` — поэтому у процесса теста свой
//! HOME, иначе они легли бы в настоящий.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use ferry_net::{Agent, Options};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ferry-wait-{}-{name}", std::process::id()))
}

async fn agent(name: &str) -> Agent {
    let home = root(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let options = Options {
        internet: false,
        lan: false,
        home: home.clone(),
        port: None,
        supervisor: FERRY.into(),
        ..Options::new(home.join(".ferry"))
    };
    let agent = Agent::start(options).await.unwrap();
    agent.rename(None, name).await.unwrap();
    agent
}

async fn ferry(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(FERRY)
            .args(&args)
            .env("HOME", root("ctl"))
            .current_dir(root("ctl"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_log_lines_and_ports() {
    // До любых потоков: задачи устройства — в своём HOME, шелл — без профиля пользователя.
    let jobs = root("jobs");
    let _ = std::fs::remove_dir_all(&jobs);
    std::fs::create_dir_all(&jobs).unwrap();
    unsafe {
        std::env::set_var("HOME", &jobs);
        std::env::set_var("FERRY_SHELL", "/bin/sh");
    }

    let (ctl, win) = (agent("ctl").await, agent("win").await);
    ctl.learn(win.addr());
    win.learn(ctl.addr());
    ctl.pair(&win.open_pairing().ticket).await.unwrap();
    let start = Instant::now();
    while !ctl.snapshot().peers.iter().any(|p| p.status.is_some()) {
        assert!(start.elapsed() < Duration::from_secs(10), "win не на связи");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::spawn(ferry_net::control::serve(
        ctl.clone(),
        root("ctl").join(".ferry"),
    ));
    while !root("ctl").join(".ferry/app").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Строка появляется позже — wait печатает её целиком.
    let out = ferry(&[
        "start",
        "win",
        "srv",
        "--",
        "echo building; sleep 1; echo 'server ready on 4242'; sleep 30",
    ])
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = ferry(&["wait", "win", "--log", "srv", "ready on"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout).trim(), "server ready on 4242");
    // Уже в логе — сразу.
    let out = ferry(&["wait", "win", "--log", "srv", "building"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // Задача вышла, а текста так и не было — код 1 и почему.
    ferry(&["start", "win", "quit", "--", "echo nope; exit 3"]).await;
    let out = ferry(&["wait", "win", "--log", "quit", "ready"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("завершилась (код 3)"),
        "{}",
        text(&out.stderr)
    );
    // Не дождались — код 1 по таймауту, задача работает дальше.
    let started = Instant::now();
    let out = ferry(&["wait", "win", "--log", "srv", "never", "--timeout", "1"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("не появилось"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "ждал дольше таймаута"
    );
    assert_eq!(
        ferry(&["wait", "win", "--log", "srv", "x", "--gone"])
            .await
            .status
            .code(),
        Some(2)
    );
    ferry(&["kill", "win", "srv"]).await;

    // Порт: открыт на 127.0.0.1, только на [::1] (Vite на маке), закрылся, не открылся.
    let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = v4.local_addr().unwrap().port().to_string();
    let out = ferry(&["wait", "win", "--port", &port]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    drop(v4);
    let out = ferry(&["wait", "win", "--port", &port, "--gone"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = ferry(&["wait", "win", "--port", &port, "--timeout", "1"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("не открылся"),
        "{}",
        text(&out.stderr)
    );
    let v6 = std::net::TcpListener::bind("[::1]:0").unwrap();
    let port = v6.local_addr().unwrap().port().to_string();
    let out = ferry(&["wait", "win", "--port", &port, "--timeout", "3"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}
