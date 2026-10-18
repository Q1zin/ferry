//! Команды связанному устройству: оба агента в одном процессе, у каждого свой «дом».

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use ferry_net::agent::UpdateHook;
use ferry_net::proto::Output;
use ferry_net::{Device, Dir, Mode, Options, Role};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Весь вывод команды: stdout, stderr и код (или ошибка с кодом).
async fn run(device: &Device, cmd: &str, dir: Dir, desk: bool) -> (String, String, i32) {
    let mut rx = match device.run(cmd, dir, desk).await {
        Ok(rx) => rx,
        Err(fail) => return (String::new(), fail.message, fail.code),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    while let Some(event) = rx.recv().await.unwrap() {
        match event {
            Output::Out(data) => out.extend(data),
            Output::Err(data) => err.extend(data),
            Output::Exit(code) => return (lossy(&out), lossy(&err), code),
            Output::Fail(fail) => return (lossy(&out), fail.message, fail.code),
        }
    }
    panic!("вывод оборвался без кода выхода")
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", lossy(&out.stderr));
    lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn commands_stream_output_and_respect_role_and_mode() {
    let (a, b) = paired("ctl", "node").await;
    let node = a.device("node").unwrap();

    let (out, err, code) = run(&node, "echo out; echo err >&2; exit 3", Dir::Home, false).await;
    assert_eq!((out.as_str(), err.as_str(), code), ("out\n", "err\n", 3));
    let (out, ..) = run(&node, "pwd", Dir::Home, false).await;
    assert!(
        out.trim().ends_with("node/home"),
        "команда — в доме устройства: {out}"
    );

    // Режим «не трогать рабочий стол»: команды с окнами — нет, сборки — да.
    b.set_mode(Mode::NoDesktop).unwrap();
    let (_, err, code) = run(&node, "true", Dir::Home, true).await;
    assert_eq!(code, 75, "{err}");
    assert!(err.contains("не трогать рабочий стол"), "{err}");
    assert_eq!(run(&node, "true", Dir::Home, false).await.2, 0);
    // «Отключён» и «пульт» — ничего.
    b.set_mode(Mode::Paused).unwrap();
    assert_eq!(run(&node, "true", Dir::Home, false).await.2, 75);
    b.set_mode(Mode::Active).unwrap();
    b.set_role(None, Role::Console).await.unwrap();
    let (_, err, code) = run(&node, "true", Dir::Home, false).await;
    assert_eq!(code, 75);
    assert!(err.contains("пульт"), "{err}");
    b.set_role(None, Role::Node).await.unwrap();

    // Проекта ещё нет — понятная ошибка, а не «нет такого каталога».
    let project = Dir::Project {
        name: "app".into(),
        prefix: String::new(),
    };
    let (_, err, code) = run(&node, "true", project, false).await;
    assert_eq!(code, 1);
    assert!(err.contains("ferry sync"), "{err}");

    // Свойства устройства.
    let info = node.info().await.unwrap();
    assert!(info.iter().any(|(k, _)| k == "os"), "{info:?}");
}

#[tokio::test]
async fn update_goes_to_the_devices_own_updater() {
    let a = agent("upd-ctl").await;
    let hook: UpdateHook = Arc::new(|| Box::pin(async { Ok("ferry 0.1.0 → 0.1.1".to_string()) }));
    let options = Options {
        update: Some(hook),
        ..options("upd-node")
    };
    let b = agent_with("upd-node", options).await;
    pair(&a, &b).await;

    let node = a.device("upd-node").unwrap();
    assert_eq!(node.update().await.unwrap(), "ferry 0.1.0 → 0.1.1");
    // Сборке без updater (из исходников) обновляться нечем.
    let fail = b.device("upd-ctl").unwrap().update().await.unwrap_err();
    assert!(
        fail.message.contains("не обновляется сама"),
        "{}",
        fail.message
    );
    // Отключённое устройство издалека не обновляют.
    b.set_mode(Mode::Paused).unwrap();
    assert_eq!(node.update().await.unwrap_err().code, 75);
}

#[tokio::test]
async fn logs_give_the_tail_of_the_device_app_log() {
    let (a, b) = paired("logs-ctl", "logs-node").await;
    let node = a.device("logs-node").unwrap();
    let fail = node.logs(10).await.unwrap_err();
    assert!(
        fail.message.contains("ferry.log"),
        "лога ещё нет: {}",
        fail.message
    );

    let log = ferry_core::app_log(&path("logs-node"));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "one\ntwo\nthree\n").unwrap();
    assert_eq!(node.logs(2).await.unwrap(), "two\nthree");
    assert_eq!(
        a.logs_self(5).unwrap_err().code,
        1,
        "у этого устройства лога нет"
    );

    b.set_mode(Mode::Paused).unwrap();
    assert_eq!(node.logs(2).await.unwrap_err().code, 75);
}

#[tokio::test]
async fn journal_shows_who_did_what_here() {
    let (a, b) = paired("jr-ctl", "jr-node").await;
    let node = a.device("jr-node").unwrap();
    run(&node, "echo hi", Dir::Home, false).await;
    // Ввод на маке без экрана откажет — в журнал попадает сам запрос, текст — нет.
    let _ = node.input(vec!["type".into(), "пароль".into()]).await;
    let journal = b.snapshot().activity;
    let lines: Vec<_> = journal
        .iter()
        .map(|e| (e.who.as_str(), e.what.as_str(), e.desk))
        .collect();
    assert_eq!(
        lines,
        [
            ("jr-ctl", "команда: echo hi", false),
            ("jr-ctl", "ввод текста", true)
        ]
    );
    assert!(b.snapshot().controlled(ferry_core::now()).is_some());
    assert!(
        a.snapshot().activity.is_empty(),
        "журнал — только у того, с кем это делали"
    );
}

#[tokio::test]
async fn dropped_caller_stops_the_command() {
    let (a, b) = paired("ctl2", "node2").await;
    let node = a.device("node2").unwrap();
    let pidfile = path("node2").join("pid");
    let cmd = format!("echo $$ > '{}'; echo started; sleep 30", pidfile.display());
    let mut rx = node.run(&cmd, Dir::Home, false).await.unwrap();
    assert!(matches!(rx.recv().await.unwrap(), Some(Output::Out(_))));
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    drop(rx);
    let alive = || {
        Command::new("kill")
            .args(["-0", &pid])
            .status()
            .unwrap()
            .success()
    };
    for _ in 0..50 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive(), "команда {pid} пережила уход вызвавшего");
    drop(b);
}

#[tokio::test]
async fn sync_puts_the_working_tree_and_keeps_node_edits() {
    let (a, b) = paired("ctl3", "node3").await;
    let node = a.device("node3").unwrap();
    let home = path("node3").join("home");

    // Проект здесь: коммит, правка, новый файл и .env мимо git.
    let root = path("ctl3").join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(root.join("src/main.txt"), "v1\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("src/main.txt"), "v2\n").unwrap();
    std::fs::write(root.join("new.txt"), "new\n").unwrap();
    std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
    let repo = ferry_core::sync::repo_at(&root).unwrap().unwrap();

    let (commit, tree) = ferry_core::sync::snapshot(&repo).unwrap();
    let said = node
        .sync(&root, "app", &commit, &tree, &[".env".into()])
        .await
        .unwrap();
    assert!(
        said.contains("рабочее дерево здесь") && said.contains(".env"),
        "{said}"
    );
    let there = home.join("ferry/app");
    assert_eq!(
        std::fs::read_to_string(there.join("src/main.txt")).unwrap(),
        "v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(there.join("new.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(there.join(".env")).unwrap(),
        "KEY=1\n"
    );

    // Команда — в том же подкаталоге проекта.
    let src = Dir::Project {
        name: "app".into(),
        prefix: "src/".into(),
    };
    assert_eq!(
        run(&node, "cat main.txt", src.clone(), false).await.0,
        "v2\n"
    );

    // Второй снимок: едет только разница, удалённое удаляется.
    std::fs::write(root.join("src/main.txt"), "v3\n").unwrap();
    std::fs::remove_file(root.join("new.txt")).unwrap();
    let (commit2, tree2) = ferry_core::sync::snapshot(&repo).unwrap();
    node.sync(&root, "app", &commit2, &tree2, &[])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(there.join("src/main.txt")).unwrap(),
        "v3\n"
    );
    assert!(!there.join("new.txt").exists());

    // Правка на устройстве: тот же снимок ещё раз — правка сохранена патчем и откачена.
    std::fs::write(there.join("src/main.txt"), "edited on node\n").unwrap();
    let said = node
        .sync(&root, "app", &commit2, &tree2, &[])
        .await
        .unwrap();
    assert!(said.contains("были правки"), "{said}");
    assert_eq!(
        std::fs::read_to_string(there.join("src/main.txt")).unwrap(),
        "v3\n"
    );

    // pull — правки с устройства сюда.
    std::fs::write(there.join("src/main.txt"), "from node\n").unwrap();
    let said = node.pull(&root, "app").await.unwrap();
    assert!(said.contains("применены"), "{said}");
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.txt")).unwrap(),
        "from node\n"
    );

    // put и get: файл туда, каталог обратно.
    std::fs::create_dir_all(root.join("conf")).unwrap();
    std::fs::write(root.join("conf/local.toml"), "x = 1\n").unwrap();
    let project = Dir::Project {
        name: "app".into(),
        prefix: String::new(),
    };
    node.put(&root, &["conf".into()], project.clone())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(there.join("conf/local.toml")).unwrap(),
        "x = 1\n"
    );
    run(
        &node,
        "mkdir -p report && echo '<h1>ok</h1>' > report/index.html",
        project.clone(),
        false,
    )
    .await;
    let dest = path("ctl3").join("got");
    let got = node.get("report", project.clone(), &dest).await.unwrap();
    assert_eq!(got, ["report"]);
    assert_eq!(
        std::fs::read_to_string(dest.join("report/index.html")).unwrap(),
        "<h1>ok</h1>\n"
    );
    let err = node.get("missing", project, &dest).await.unwrap_err();
    assert!(err.message.contains("нет такого"), "{err}");
    drop(b);
}

#[tokio::test]
async fn tunnel_carries_bytes_both_ways() {
    let (a, b) = paired("ctl4", "node4").await;
    let node = a.device("node4").unwrap();
    // Эхо-сервер «на устройстве».
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = socket.read(&mut buf).await.unwrap();
        socket.write_all(&buf[..n]).await.unwrap();
    });
    let (tx, mut rx) = node.tunnel(port).await.unwrap();
    tx.send(b"ping".to_vec()).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), Some(b"ping".to_vec()));
    drop(b);
}
