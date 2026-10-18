//! CLI → приложение → устройства целиком: настоящий бинарник `ferry`, сервер управления и агенты
//! в этом процессе (без интернета и mDNS). У «приложения» свой HOME — там его `~/.ferry/app`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use ferry_net::{Agent, Mode, Options};

const FERRY: &str = env!("CARGO_BIN_EXE_ferry");

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ferry-cli-{}-{name}", std::process::id()))
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
        ..Options::new(home.join(".ferry"))
    };
    let agent = Agent::start(options).await.unwrap();
    agent.rename(None, name).await.unwrap();
    agent
}

async fn pair(controller: &Agent, node: &Agent) {
    controller.learn(node.addr());
    node.learn(controller.addr());
    controller.pair(&node.open_pairing().ticket).await.unwrap();
    let id = node.id().to_string();
    let start = Instant::now();
    while !controller
        .snapshot()
        .peers
        .iter()
        .any(|p| p.id == id && p.status.is_some())
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "устройство не вышло на связь"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `ferry …` от имени «приложения» `controller`, из каталога `cwd`.
async fn ferry(cwd: &Path, args: &[&str]) -> Output {
    ferry_as("", cwd, args).await
}

/// То же от сессии `session` (`FERRY_AGENT`); пустая — как вызвал тест.
async fn ferry_as(session: &str, cwd: &Path, args: &[&str]) -> Output {
    let (session, cwd, args) = (
        session.to_string(),
        cwd.to_path_buf(),
        args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
    );
    tokio::task::spawn_blocking(move || {
        Command::new(FERRY)
            .args(&args)
            .env("HOME", root("ctl"))
            .env("FERRY_AGENT", session)
            .current_dir(cwd)
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
async fn cli_drives_devices_through_the_app() {
    let ctl = agent("ctl").await;
    let (win, mac) = (agent("win").await, agent("mac").await);
    pair(&ctl, &win).await;
    pair(&ctl, &mac).await;
    tokio::spawn(ferry_net::control::serve(
        ctl.clone(),
        root("ctl").join(".ferry"),
    ));
    while !root("ctl").join(".ferry/app").exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let outside = root("ctl");

    let out = ferry(&outside, &["devices"]).await;
    let list = text(&out.stdout);
    assert!(
        list.contains("win") && list.contains("mac") && list.contains("напрямую"),
        "{list}"
    );

    // --json: устройства — одной строкой, свойства — строкой на устройство, без подписей.
    let out = ferry(&outside, &["devices", "--json"]).await;
    let view: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = view["peers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"win") && names.contains(&"mac"), "{view}");
    let out = ferry(&outside, &["info", "win,mac", "--json"]).await;
    let lines: Vec<serde_json::Value> = text(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{}", text(&out.stdout));
    assert!(
        lines.iter().all(|l| l["info"]["os"].is_string()),
        "{lines:?}"
    );

    // wait только читает список окон: окна нет — с --gone сразу 0, без — 1 по таймауту.
    let missing = "ferry-нет-такого-окна";
    let out = ferry(&outside, &["wait", "win", "--window", missing, "--gone"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let out = ferry(
        &outside,
        &["wait", "win", "--window", missing, "--timeout", "1"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("не появилось"),
        "{}",
        text(&out.stderr)
    );

    // Одна цель: вывод как есть, код — устройства. Вне репозитория — в доме устройства.
    let out = ferry(&outside, &["run", "win", "--", "echo hi; pwd; exit 4"]).await;
    assert_eq!(out.status.code(), Some(4), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(said.starts_with("hi\n"), "{said}");
    assert!(
        said.contains(&root("win").display().to_string()),
        "в доме устройства: {said}"
    );

    // Все сразу: параллельно, строки подписаны, код — первый ненулевой, в конце сводка.
    let out = ferry(
        &outside,
        &["run", "all", "--", "echo from $(basename \"$PWD\")"],
    )
    .await;
    let said = text(&out.stdout);
    assert!(
        said.contains("[win] from") && said.contains("[mac] from"),
        "{said}"
    );
    assert!(
        text(&out.stderr).contains("win — 0") && text(&out.stderr).contains("mac — 0"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));

    // Отказ одного устройства — его код (75), остальные работают.
    mac.set_mode(Mode::NoDesktop).unwrap();
    let out = ferry(&outside, &["run", "win,mac", "--desk", "--", "echo ok"]).await;
    assert_eq!(out.status.code(), Some(75), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("[win] ok"));
    assert!(
        text(&out.stderr).contains("[mac] ferry: на mac режим «не трогать рабочий стол»"),
        "{}",
        text(&out.stderr)
    );
    // Новый ввод доходит до устройства, но без рабочего стола — отказ, а не настоящий клик: в
    // тестах ввода нет. Текст для буфера обмена в журнал не попадает.
    let actions: [&[&str]; 7] = [
        &["text", "mac"],
        &["click", "mac", "--text", "Готово"],
        &["drag", "mac", "1", "2", "3", "4", "--hold", "shift"],
        &["click", "mac", "1", "2", "--hold", "shift"],
        &["key", "mac", "a", "--for", "1"],
        &["clip", "mac"],
        &["clip", "mac", "секрет-буфера"],
    ];
    for args in actions {
        let out = ferry(&outside, args).await;
        assert_eq!(
            out.status.code(),
            Some(75),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    let journal: Vec<String> = mac
        .snapshot()
        .activity
        .into_iter()
        .map(|a| a.what)
        .collect();
    assert!(
        journal.iter().any(|w| w == "буфер обмена: записать"),
        "{journal:?}"
    );
    assert!(
        journal.iter().all(|w| !w.contains("секрет-буфера")),
        "{journal:?}"
    );
    mac.set_mode(Mode::Active).unwrap();

    // Бронь стола: --desk занял win на эту сессию; у другой сессии — отказ с подсказкой, команды
    // без стола ей можно. Своя снимается просто, чужая — только --force.
    let out = ferry(&outside, &["desk", "win"]).await;
    assert!(
        text(&out.stdout).starts_with("win: занят: ctl"),
        "{}",
        text(&out.stdout)
    );
    let out = ferry(&outside, &["desk", "win", "free"]).await;
    assert_eq!(
        text(&out.stdout),
        "win: свободен\n",
        "{}",
        text(&out.stderr)
    );
    let out = ferry_as("a", &outside, &["run", "win", "--desk", "--", "echo a"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    // Рамка управления: команда кончилась, но рамка ещё горит — и от действия, а не от брони.
    assert_eq!(win.acting().as_deref(), Some("ctl"));
    let out = ferry_as("b", &outside, &["run", "win", "--desk", "--", "echo b"]).await;
    assert_eq!(out.status.code(), Some(75), "{}", text(&out.stderr));
    let said = text(&out.stderr);
    assert!(
        said.contains("рабочий стол win занят: ctl (a) — ещё 1")
            && said.contains("ferry desk win free --force"),
        "{said}"
    );
    assert!(text(&out.stdout).is_empty(), "команда не запускалась");
    // Ввода в тестах нет: сломайся бронь — клик упрётся в режим, а не в экран.
    win.set_mode(Mode::NoDesktop).unwrap();
    let out = ferry_as("b", &outside, &["click", "win", "1", "2"]).await;
    assert!(
        text(&out.stderr).contains("занят: ctl (a)"),
        "{}",
        text(&out.stderr)
    );
    win.set_mode(Mode::Active).unwrap();
    let out = ferry_as("b", &outside, &["run", "win", "--", "echo b"]).await;
    assert_eq!(out.status.code(), Some(0), "без стола — можно");
    let out = ferry_as("b", &outside, &["desk", "win", "free"]).await;
    assert_eq!(out.status.code(), Some(75));
    let out = ferry_as("a", &outside, &["desk", "win", "free"]).await;
    assert_eq!(text(&out.stdout), "win: свободен\n");
    let out = ferry_as("b", &outside, &["desk", "win", "take", "5"]).await;
    assert_eq!(text(&out.stdout), "win: занят тобой — ещё 300 с\n");
    let out = ferry_as("a", &outside, &["desk", "win", "free", "--force"]).await;
    assert_eq!(text(&out.stdout), "win: свободен — бронь ctl (b) снята\n");

    // В репозитории — сначала снимок, потом команда в том же подкаталоге проекта.
    let repo = root("ctl").join("app");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["init", "-q"]);
    std::fs::write(repo.join("src/main.txt"), "from controller\n").unwrap();
    let out = ferry(&repo.join("src"), &["run", "win", "--", "cat main.txt"]).await;
    assert_eq!(
        text(&out.stdout),
        "from controller\n",
        "{}",
        text(&out.stderr)
    );
    assert!(
        text(&out.stderr).contains("рабочее дерево здесь"),
        "{}",
        text(&out.stderr)
    );
    assert!(root("win").join("ferry/app/src/main.txt").exists());

    // doctor из репозитория: проверки идут шеллом устройства.
    let out = ferry(&repo, &["doctor", "win"]).await;
    let said = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{said}{}", text(&out.stderr));
    assert!(
        said.contains("✓ git") && said.contains("свободно на диске"),
        "{said}"
    );

    // Убили CLI посреди команды — команда на устройстве останавливается.
    let pidfile = root("win").join("pid");
    let cmd = format!("echo $$ > '{}'; sleep 30", pidfile.display());
    let mut cli = Command::new(FERRY)
        .args(["run", "win", "--no-sync", "--", &cmd])
        .env("HOME", root("ctl"))
        .current_dir(&outside)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !pidfile.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "команда не запустилась"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    cli.kill().unwrap();
    cli.wait().unwrap();
    let alive = || {
        Command::new("kill")
            .args(["-0", &pid])
            .status()
            .unwrap()
            .success()
    };
    let start = Instant::now();
    while alive() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "команда {pid} пережила убитый CLI"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Падения: такой программы нет — так и сказано, код 0.
    let out = ferry(&outside, &["crashes", "win", "ferry-нет-такой-программы"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("падений у «ferry-нет-такой-программы» нет"));
    let out = ferry(
        &outside,
        &["crashes", "win", "ferry-нет-такой-программы", "--last"],
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // Из репозитория без снимка на устройстве: --no-sync подсказывает --home, а --home —
    // в доме устройства и без снимка, как вне репозитория.
    let repo = root("ctl").join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(git.success());
    let out = ferry(&repo, &["run", "win", "--no-sync", "--", "pwd"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--home"),
        "{}",
        text(&out.stderr)
    );
    let out = ferry(&repo, &["run", "win", "--home", "--", "pwd"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    // /var на маке — ссылка на /private/var.
    let said = text(&out.stdout);
    assert!(
        said.trim().ends_with(&root("win").display().to_string()),
        "в доме устройства: {said}"
    );
    assert!(!root("win").join("ferry/proj").exists(), "снимок не клали");

    // MCP: те же команды через `ferry mcp` — JSON-RPC строками, ответ по строке на запрос.
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ferry","arguments":{"args":["devices","--json"]}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ferry","arguments":{"args":["run","win","--","echo через mcp; exit 3"]}}}"#,
    ];
    let mut input = requests.join("\n");
    input.push('\n');
    let replies = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut mcp = Command::new(FERRY)
            .arg("mcp")
            .env("HOME", root("ctl"))
            .current_dir(root("ctl"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        mcp.stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        String::from_utf8(mcp.wait_with_output().unwrap().stdout).unwrap()
    })
    .await
    .unwrap();
    let replies: Vec<serde_json::Value> = replies
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), 3, "на уведомление не отвечают: {replies:?}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "ferry");
    let listed = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        listed.contains("\"win\"") && listed.contains("(код 0)"),
        "{listed}"
    );
    let ran = &replies[2]["result"];
    assert_eq!(ran["isError"], true);
    let said = ran["content"][0]["text"].as_str().unwrap();
    assert!(
        said.contains("через mcp") && said.ends_with("(код 3)"),
        "{said}"
    );

    // Нет такого устройства — понятная ошибка.
    let out = ferry(&outside, &["run", "nope", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("нет устройства"),
        "{}",
        text(&out.stderr)
    );

    // forget — только здесь: mac пропал из списка, win остался, сам mac об этом не знает.
    let out = ferry(&outside, &["forget", "mac"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let names: Vec<String> = ctl.snapshot().peers.into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["win"]);
    assert_eq!(mac.snapshot().peers.len(), 1);
    let out = ferry(&outside, &["forget", "mac"]).await;
    assert_eq!(out.status.code(), Some(1), "второй раз — нет такого");
}
