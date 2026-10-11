//! CLI → приложение: настоящий сервер управления и настоящий клиент из ядра.

use std::time::Duration;

use ferry_core::control::{self, Request, Response};
use ferry_net::{Agent, Options, Role};

#[tokio::test]
async fn cli_talks_to_the_app_with_its_token() {
    let dir = std::env::temp_dir().join(format!("ferry-control-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let agent = Agent::start(Options {
        internet: false,
        lan: false,
        home: dir.join("home"),
        ..Options::new(dir.clone())
    })
    .await
    .unwrap();
    agent.rename(None, "mac").await.unwrap();
    tokio::spawn(ferry_net::control::serve(agent.clone(), dir.clone()));
    while !dir.join("app").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let call = |request| {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || control::call_in(&dir, request).unwrap())
    };
    match call(Request::State).await.unwrap() {
        Response::State(snap) => assert_eq!(snap.me.name, "mac"),
        other => panic!("{other:?}"),
    }
    let set = Request::SetRole {
        device: None,
        role: Role::Node,
    };
    assert_eq!(call(set).await.unwrap(), Response::Done);
    assert_eq!(agent.snapshot().me.role, Role::Node);
    let unknown = Request::SetRole {
        device: Some("win-10".into()),
        role: Role::Node,
    };
    match call(unknown).await.unwrap() {
        Response::Error(e) => assert!(e.contains("нет устройства"), "{e}"),
        other => panic!("{other:?}"),
    }
    match call(Request::OpenPairing).await.unwrap() {
        Response::Pairing(info) => assert!(info.ticket.ends_with(&info.code)),
        other => panic!("{other:?}"),
    }

    // Без токена из ~/.ferry/app приложение не слушается.
    let app = std::fs::read_to_string(dir.join("app")).unwrap();
    let port = app.split_whitespace().next().unwrap();
    std::fs::write(dir.join("app"), format!("{port} 0000\n")).unwrap();
    match call(Request::State).await.unwrap() {
        Response::Error(e) => assert!(e.contains("токен"), "{e}"),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
