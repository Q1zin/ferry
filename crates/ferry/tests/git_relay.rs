//! Релей git через устройство, запустившее команду: git на «узле» зовёт `ferry ssh`, сессия
//! уходит по iroh к «пульту», тот проверяет и выполняет её своим ssh (здесь — подменённым).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferry_net::proto::Output;
use ferry_net::{Agent, Device, Dir, Options};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ferry-git-relay-{}-{name}", std::process::id()))
}

async fn agent(name: &str) -> Agent {
    let home = root(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ferry")).unwrap();
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

async fn run(device: &Device, cmd: &str) -> (String, String, i32) {
    let mut rx = device.run(cmd, Dir::Home, false).await.unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    while let Some(event) = rx.recv().await.unwrap() {
        match event {
            Output::Out(d) => out.extend(d),
            Output::Err(d) => err.extend(d),
            Output::Exit(code) => {
                return (
                    String::from_utf8_lossy(&out).into(),
                    String::from_utf8_lossy(&err).into(),
                    code,
                );
            }
            Output::Fail(fail) => panic!("{fail}"),
        }
    }
    panic!("без кода выхода")
}

#[test]
fn git_on_the_node_uses_the_callers_keys_read_only() {
    // ssh пульта — скрипт: печатает аргументы и stdin. Переменную — до потоков рантайма.
    let fake = root("fake-ssh");
    let log = root("fake-ssh.log");
    let script = format!(
        "#!/bin/sh\necho \"ssh $*\" >> '{}'\necho \"ssh $*\"\ncat\n",
        log.display()
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe { std::env::set_var("FERRY_RELAY_SSH", &fake) };

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (ctl, node) = (agent("ctl").await, agent("node").await);
        std::fs::write(root("ctl").join(".ferry/trust"), "git.test\n").unwrap();
        ctl.learn(node.addr());
        node.learn(ctl.addr());
        ctl.pair(&node.open_pairing().ticket).await.unwrap();
        let start = Instant::now();
        let device = loop {
            if let Ok(device) = ctl.device("node") {
                break device;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "узел не вышел на связь"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        // Окружение команды: доверенные хосты пульта и ssh через ferry.
        let (out, ..) = run(
            &device,
            "echo \"$FERRY_TRUST|$GIT_SSH_COMMAND|$CARGO_NET_GIT_FETCH_WITH_CLI\"",
        )
        .await;
        assert_eq!(out.trim(), format!("git.test|'{FERRY}' ssh|true"));

        // Чтение — ssh пульта, с аргументами git и его stdin. Зовём так же, как git.
        let ssh = "sh -c \"$GIT_SSH_COMMAND \\\"\\$@\\\"\" ssh";
        let fetch = format!("echo want-refs | {ssh} git@git.test \"git-upload-pack '/repo.git'\"");
        let (out, err, code) = run(&device, &fetch).await;
        assert_eq!(code, 0, "{err}");
        assert!(
            out.contains("ssh -o BatchMode=yes -- git@git.test git-upload-pack '/repo.git'"),
            "{out}"
        );
        assert!(
            out.contains("want-refs"),
            "stdin дошёл до ssh пульта: {out}"
        );

        // Запись — нет: скомпрометированный узел не пушит от имени пульта.
        let push = format!("{ssh} git@git.test \"git-receive-pack '/repo.git'\" </dev/null");
        let (out, err, code) = run(&device, &push).await;
        assert_eq!(code, 1, "{out}");
        assert!(err.contains("только чтение"), "{err}");

        // Настоящий git на узле идёт через релей: подменённый ssh пульта его увидел.
        let _ = std::fs::remove_file(&log);
        run(
            &device,
            "git ls-remote ssh://git@git.test/repo.git 2>/dev/null; true",
        )
        .await;
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            seen.contains("git@git.test") && seen.contains("git-upload-pack '/repo.git'"),
            "{seen}"
        );
    });
}
