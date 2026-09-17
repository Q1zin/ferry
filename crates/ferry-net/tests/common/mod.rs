//! Общее для тестов: агенты в одном процессе, без интернета и mDNS, адреса друг друга им дают
//! напрямую. У каждого — свой каталог и свой «дом» во временном каталоге.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferry_net::{Agent, Options, Snapshot};

pub fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ferry-net-{}-{name}", std::process::id()))
}

pub fn dir(name: &str) -> PathBuf {
    let _ = std::fs::remove_dir_all(path(name));
    path(name)
}

pub fn options(name: &str) -> Options {
    let dir = path(name);
    Options {
        internet: false,
        lan: false,
        home: dir.join("home"),
        port: None,
        ..Options::new(dir)
    }
}

pub async fn agent(name: &str) -> Agent {
    agent_with(name, options(name)).await
}

pub async fn agent_with(name: &str, options: Options) -> Agent {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let _ = std::fs::remove_dir_all(path(name));
    std::fs::create_dir_all(path(name).join("home")).unwrap();
    let agent = Agent::start(options).await.unwrap();
    agent.rename(None, name).await.unwrap();
    agent
}

/// Дождаться, пока снимок агента станет таким, как нужно.
pub async fn until(agent: &Agent, what: &str, ok: impl Fn(&Snapshot) -> bool) -> Duration {
    let start = Instant::now();
    while !ok(&agent.snapshot()) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "не дождались: {what}\n{:#?}",
            agent.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    start.elapsed()
}

pub async fn paired(a: &str, b: &str) -> (Agent, Agent) {
    let (a, b) = (agent(a).await, agent(b).await);
    pair(&a, &b).await;
    (a, b)
}

pub async fn pair(a: &Agent, b: &Agent) {
    a.learn(b.addr());
    b.learn(a.addr());
    let ticket = b.open_pairing().ticket;
    let peer = a.pair(&ticket).await.unwrap();
    assert_eq!(peer.name, b.snapshot().me.name);
    assert!(b.snapshot().pairing.is_none(), "окно связывания закрылось");
    until(a, "a видит b", |s| {
        s.peers.iter().any(|p| p.online && p.status.is_some())
    })
    .await;
    until(b, "b видит a", |s| {
        s.peers.iter().any(|p| p.online && p.status.is_some())
    })
    .await;
}
