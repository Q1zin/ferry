//! Команды хаба — машины, где идёт разработка.
//!
//! Узлы — Host-блоки в `~/.ferry/ssh_config`; `ferry add` подключает его в `~/.ssh/config`
//! через Include, поэтому `ssh <узел>`, `scp` и `git push <узел>:…` работают и напрямую.
//!
//! Проект `…/<имя>` хаба живёт на узле в `~/ferry/<имя>`. `sync` кладёт туда снимок рабочего
//! дерева — с незакоммиченным и новыми файлами: снимок собирается во временном индексе и
//! коммитится вне веток, так что ветка и индекс здесь не меняются. Правки, сделанные на узле,
//! перед перезаписью сохраняются патчем в `~/.ferry/saved`, а `pull` приносит их сюда.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{exit_code, ferry_home, home, sh_quote, write_private};

const INCLUDE: &str = "Include ~/.ferry/ssh_config";

pub fn add(name: &str, target: &str) -> io::Result<i32> {
    let (user, addr) = target
        .split_once('@')
        .ok_or_else(|| io::Error::other("адрес узла — user@адрес"))?;
    // Всё это уходит в ssh_config как есть: пробел или перевод строки дописали бы свои опции.
    let alias = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    check("имя узла", name, alias)?;
    check("пользователь", user, |c| {
        !c.is_whitespace() && !c.is_control() && c != '"'
    })?;
    check("адрес", addr, |c| alias(c) || c == ':')?;

    let dir = ferry_home()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("ssh_config");
    let mut text = without_host(&fs::read_to_string(&path).unwrap_or_default(), name);
    // BatchMode: ssh не ждёт пароль, а сразу падает — для Claude и скриптов это важнее.
    // ControlMaster: одно соединение на узел, остальные команды идут без handshake.
    // RemoteForward: порт на узле → релей git этого хаба (см. relay.rs).
    let relay = dir.join("relay.sock");
    text.push_str(&format!(
        "Host {name}\n  HostName {addr}\n  User {user}\n  IdentityFile ~/.ssh/ferry\n  \
         IdentitiesOnly yes\n  BatchMode yes\n  StrictHostKeyChecking accept-new\n  \
         ServerAliveInterval 15\n  ControlMaster auto\n  ControlPath ~/.ferry/cm-%C\n  \
         ControlPersist 10m\n  RemoteForward 127.0.0.1:{} \"{}\"\n\n",
        crate::relay::NODE_PORT,
        relay.display()
    ));
    write_private(&path, &text)?;
    include_in_ssh_config()?;
    // Старое общее соединение живёт со старыми настройками — закрываем.
    let _ = Command::new("ssh")
        .args(["-O", "exit", name])
        .stderr(Stdio::null())
        .status();

    if !home()?.join(".ssh/ferry").exists() {
        eprintln!("ferry: нет ключа ~/.ssh/ferry — ssh-keygen -t ed25519 -f ~/.ssh/ferry -N ''");
    }
    let ok = Command::new("ssh")
        .args([name, "ferry desk true"])
        .stdin(Stdio::null())
        .status()?
        .success();
    if ok {
        eprintln!("ferry: узел {name} на связи, рабочий стол отвечает");
        Ok(0)
    } else {
        eprintln!("ferry: узел {name} записан, но проверка `ssh {name} ferry desk true` не прошла");
        Ok(1)
    }
}

pub fn ls() -> io::Result<i32> {
    let hosts = hosts()?;
    if hosts.is_empty() {
        eprintln!("ferry: узлов нет; добавить — ferry add <узел> <user@адрес>");
        return Ok(0);
    }
    let states: Vec<String> = thread::scope(|s| {
        let probes: Vec<_> = hosts
            .iter()
            .map(|(name, _)| s.spawn(move || probe(name)))
            .collect();
        probes
            .into_iter()
            .map(|p| p.join().unwrap_or_default())
            .collect()
    });
    for ((name, addr), state) in hosts.iter().zip(states) {
        println!("{name:<14} {addr:<16} {state}");
    }
    Ok(0)
}

pub fn sync(host: &str) -> io::Result<i32> {
    node(host)?;
    sync_repo(host, &repo()?.ok_or_else(not_in_repo)?)?;
    Ok(0)
}

pub fn run(host: &str, desk: bool, sync: bool, cmd: &str) -> io::Result<i32> {
    node(host)?;
    let dir = project_dir(host, sync)?;
    // Команда разбирается шеллом узла ровно один раз — и в обычном режиме, и через desk.
    // Окружение для релея git — внутри неё: у демона рабочего стола своё окружение.
    let cmd = format!("{}{cmd}", crate::relay::node_env()?);
    let cmd = if desk {
        format!("ferry desk {}", sh_quote(&cmd))
    } else {
        cmd
    };
    exec_ssh(host, &format!("cd {} && {cmd}", sh_quote(&dir)))
}

/// Фоновая задача на рабочем столе узла — там же, где выполнился бы `run`.
pub fn start(host: &str, name: &str, sync: bool, cmd: &str) -> io::Result<i32> {
    node(host)?;
    let dir = project_dir(host, sync)?;
    // Окружение релея — у запуска задачи: надсмотрщик и команда наследуют его, а в списке
    // задач остаётся сама команда.
    let env = crate::relay::node_env()?;
    let job = format!("{env}ferry job {} {}", sh_quote(name), sh_quote(cmd));
    let remote = format!("cd {} && ferry desk {}", sh_quote(&dir), sh_quote(&job));
    remote_status(host, &remote)
}

pub fn jobs(host: &str) -> io::Result<i32> {
    node(host)?;
    remote_status(host, "ferry jobs")
}

pub fn log(host: &str, name: &str, follow: bool) -> io::Result<i32> {
    node(host)?;
    let follow = if follow { " -f" } else { "" };
    exec_ssh(host, &format!("ferry log {}{follow}", sh_quote(name)))
}

pub fn kill(host: &str, name: &str) -> io::Result<i32> {
    node(host)?;
    remote_status(host, &format!("ferry kill {}", sh_quote(name)))
}

/// Каталог на узле, где выполнять команду: тот же подкаталог проекта (после sync) или дом.
fn project_dir(host: &str, sync: bool) -> io::Result<String> {
    let Some(repo) = repo()? else {
        return Ok(".".into());
    };
    if sync {
        sync_repo(host, &repo)?;
    }
    Ok(format!("ferry/{}/{}", repo.name, repo.prefix))
}

/// ssh вместо себя: сигнал по pid ferry (Ctrl-C, kill, таймаут вызывающего) достаётся
/// самому ssh, и канал на узле закрывается, а не висит за осиротевшим ssh.
fn exec_ssh(host: &str, remote: &str) -> io::Result<i32> {
    let mut ssh = Command::new("ssh");
    if io::stdin().is_terminal() {
        ssh.arg("-t");
    }
    ssh.args([host, remote]);
    #[cfg(unix)]
    return Err(std::os::unix::process::CommandExt::exec(&mut ssh));
    #[cfg(not(unix))]
    Ok(exit_code(ssh.status()?))
}

/// Без аргументов — открытые туннели; с узлом и портом — открыть (или закрыть, `close`).
/// Порт тот же с обеих сторон: CDP отдаёт в `/json/version` ws-адрес со своим портом.
/// Каждый туннель — своё фоновое ssh-соединение с сокетом `~/.ferry/tun-<узел>-<порт>`:
/// живёт, пока не закроешь или узел не пропадёт, и не зависит от общего ControlMaster.
pub fn tunnel(target: Option<(&str, &str)>, close: bool) -> io::Result<i32> {
    let Some((host, port)) = target else {
        return tunnels();
    };
    node(host)?;
    let port: u16 = port
        .parse()
        .map_err(|_| io::Error::other(format!("порт — число, а не {port:?}")))?;
    let socket = ferry_home()?.join(format!("tun-{host}-{port}"));
    let log = socket.with_extension("log");
    let open = tunnel_open(host, &socket);
    if close {
        if open {
            let _ = Command::new("ssh")
                .arg("-S")
                .arg(&socket)
                .args(["-O", "exit", host])
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_file(&log);
        eprintln!("ferry: туннель {host}:{port} закрыт");
        return Ok(0);
    }
    if open {
        eprintln!("ferry: туннель уже открыт: 127.0.0.1:{port} → {host}:{port}");
        return Ok(0);
    }
    if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        return Err(io::Error::other(format!(
            "порт {port} здесь занят — запусти на узле на другом порту"
        )));
    }
    // -E: ssh -f не закрывает stdio, поэтому ошибки — в файл, а не в наш stderr.
    // LogLevel=ERROR: молчим о RemoteForward релея, который уже держит общее соединение.
    let ok = Command::new("ssh")
        .args([
            "-f",
            "-N",
            "-M",
            "-o",
            "ControlPersist=no",
            "-o",
            "LogLevel=ERROR",
        ])
        .arg("-S")
        .arg(&socket)
        .arg("-E")
        .arg(&log)
        .arg("-L")
        .arg(format!("127.0.0.1:{port}:127.0.0.1:{port}"))
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();
    if !ok || !tunnel_open(host, &socket) {
        let why = fs::read_to_string(&log).unwrap_or_default();
        return Err(io::Error::other(format!(
            "туннель не открылся: {}",
            why.trim()
        )));
    }
    eprintln!("ferry: 127.0.0.1:{port} → {host}:{port}");
    Ok(0)
}

fn tunnel_open(host: &str, socket: &Path) -> bool {
    socket.exists()
        && Command::new("ssh")
            .arg("-S")
            .arg(socket)
            .args(["-O", "check", host])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn tunnels() -> io::Result<i32> {
    let mut found = false;
    for entry in fs::read_dir(ferry_home()?)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((host, port)) = name
            .strip_prefix("tun-")
            .filter(|n| !n.ends_with(".log"))
            .and_then(|n| n.rsplit_once('-'))
        else {
            continue;
        };
        if tunnel_open(host, &entry.path()) {
            println!("127.0.0.1:{port} → {host}:{port}");
            found = true;
        }
    }
    if !found {
        eprintln!("ferry: туннелей нет; открыть — ferry tunnel <узел> <порт>");
    }
    Ok(0)
}

pub fn pull(host: &str) -> io::Result<i32> {
    node(host)?;
    let repo = repo()?.ok_or_else(not_in_repo)?;
    let changes = node_changes(host, &repo, "")?;
    if changes.is_empty() {
        eprintln!("ferry: на {host} нет правок поверх снимка");
        return Ok(0);
    }
    let saved = save_patch(host, &repo, &changes)?;
    let applied = Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["apply", "--binary"])
        .arg(&saved)
        .status()?
        .success();
    if applied {
        eprintln!(
            "ferry: правки с {host} применены здесь (копия: {})",
            saved.display()
        );
        Ok(0)
    } else {
        eprintln!(
            "ferry: правки с {host} не легли чисто; патч: {} — попробуй git apply --3way",
            saved.display()
        );
        Ok(1)
    }
}

/// Файлы и каталоги отсюда — на то же место в проекте на узле.
pub fn put(host: &str, paths: &[&str]) -> io::Result<i32> {
    node(host)?;
    let here = here()?;
    let mut rels = Vec::new();
    for path in paths {
        let full = fs::canonicalize(path).map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        let rel = full
            .strip_prefix(&here.local)
            .map_err(|_| io::Error::other(format!("{path}: вне {}", here.local.display())))?;
        if rel.as_os_str().is_empty() {
            return Err(io::Error::other(
                "весь проект целиком — это sync; назови файлы",
            ));
        }
        rels.push(rel.to_path_buf());
    }
    send(host, &here.local, &rels, &here.remote)?;
    let rels: Vec<_> = rels.iter().map(|r| r.display().to_string()).collect();
    eprintln!("ferry: {} → {host}:~/{}", rels.join(", "), here.remote);
    Ok(0)
}

/// Файл или каталог с узла — сюда (в `dest`, по умолчанию в текущий каталог). Путь на узле —
/// относительно того же места в проекте, `~/…` — от домашнего каталога, или абсолютный.
pub fn get(host: &str, path: &str, dest: Option<&str>) -> io::Result<i32> {
    node(host)?;
    let remote = match path.strip_prefix("~/") {
        // ssh начинает в домашнем каталоге — `~/x` это просто `x`.
        Some(rest) => rest.to_string(),
        None if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') => {
            path.replace('\\', "/")
        }
        None => {
            let here = here()?;
            format!("{}/{}{path}", here.remote, here.prefix)
        }
    };
    let remote = remote.trim_end_matches('/');
    let (dir, name) = remote.rsplit_once('/').unwrap_or((".", remote));
    if name.is_empty() || name == "." || name == ".." {
        return Err(io::Error::other(format!("{path}: назови файл или каталог")));
    }
    let dir = if dir.is_empty() { "/" } else { dir };
    let dest = PathBuf::from(dest.unwrap_or("."));
    fs::create_dir_all(&dest)?;

    let pack = format!("tar -C {} -cf - {}", sh_quote(dir), sh_quote(name));
    let mut ssh = Command::new("ssh")
        .args([host, &pack])
        .stdout(Stdio::piped())
        .spawn()?;
    let unpacked = Command::new("tar")
        .arg("-C")
        .arg(&dest)
        .args(["-xf", "-"])
        .stdin(ssh.stdout.take().unwrap())
        .status()?;
    // Почему не вышло, оба tar уже написали в stderr.
    if !ssh.wait()?.success() || !unpacked.success() {
        return Ok(1);
    }
    eprintln!("ferry: {host}:{path} → {}", dest.join(name).display());
    Ok(0)
}

pub fn shot(host: &str, file: Option<&str>) -> io::Result<i32> {
    node(host)?;
    let file = match file {
        Some(file) => PathBuf::from(file),
        None => {
            let dir = ferry_home()?.join("shots");
            fs::create_dir_all(&dir)?;
            dir.join(format!("{host}.png"))
        }
    };
    output(Command::new("ssh").args([host, "ferry desk 'ferry screenshot .ferry/shot.png'"]))?;
    output(
        Command::new("scp")
            .arg("-q")
            .arg(format!("{host}:.ferry/shot.png"))
            .arg(&file),
    )?;
    println!("{}", file.display());
    Ok(0)
}

pub fn stop(host: &str) -> io::Result<i32> {
    node(host)?;
    remote_status(host, "ferry stop")
}

/// ssh-команда без stdin: ей нечего с него читать, а в фоне (`ferry stop &`) ssh,
/// читающий терминал, встал бы по SIGTTIN.
fn remote_status(host: &str, cmd: &str) -> io::Result<i32> {
    let status = Command::new("ssh")
        .args([host, cmd])
        .stdin(Stdio::null())
        .status()?;
    Ok(exit_code(status))
}

pub fn doctor(host: &str) -> io::Result<i32> {
    node(host)?;
    let repo = repo()?;
    crate::doctor::run(
        host,
        repo.as_ref().map(|r| (r.root.as_path(), r.name.as_str())),
    )
}

pub fn update(host: &str, file: &str) -> io::Result<i32> {
    windows_node(host)?;
    output(
        Command::new("scp")
            .arg("-q")
            .arg(file)
            .arg(format!("{host}:.ferry/bin/ferry.new.exe")),
    )?;
    // Запущенный exe не перезаписать, но переименовать можно.
    let swap = "cd ~/.ferry/bin && schtasks //end //tn ferry >/dev/null 2>&1; \
                mv -f ferry.exe ferry.old.exe && mv ferry.new.exe ferry.exe";
    let cleanup = "rc=$?; rm -f ~/.ferry/bin/ferry.old.exe 2>/dev/null; exit $rc";
    remote_status(host, &format!("{swap} && {RESTART}; {cleanup}"))
}

/// Перезапустить демон: он снова прочитает окружение (новый PATH и т. п.).
pub fn restart(host: &str) -> io::Result<i32> {
    windows_node(host)?;
    remote_status(host, RESTART)
}

/// Демон перезапускает планировщик — так он снова окажется на рабочем столе, а не в Session 0.
/// Ждём до минуты: новый ferry.exe с незнакомым хешем Defender перед первым запуском
/// отправляет на облачную проверку («block at first sight»), это бывает дольше 10 секунд.
const RESTART: &str = "schtasks //end //tn ferry >/dev/null 2>&1; rm -f ~/.ferry/desk \
    && schtasks //run //tn ferry >/dev/null \
    && for i in $(seq 300); do [ -f ~/.ferry/desk ] && break; sleep 0.2; done \
    && ferry desk 'echo ferry: daemon restarted'";

fn windows_node(host: &str) -> io::Result<()> {
    node(host)?;
    let os = text(Command::new("ssh").args([host, "uname -s"]))?;
    if os.starts_with("MINGW") || os.starts_with("MSYS") {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "пока умею только Windows-узлы ({host}: {os})"
    )))
}

struct Repo {
    root: PathBuf,
    git_dir: PathBuf,
    /// Имя каталога репозитория — оно же каталог на узле: `~/ferry/<name>`.
    name: String,
    /// Где мы внутри репозитория: `src-tauri/` или пусто.
    prefix: String,
}

fn repo() -> io::Result<Option<Repo>> {
    let out = Command::new("git")
        .args([
            "rev-parse",
            "--show-toplevel",
            "--absolute-git-dir",
            "--show-prefix",
        ])
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let out = String::from_utf8_lossy(&out.stdout);
    let mut lines = out.lines();
    let (Some(root), Some(git_dir)) = (lines.next(), lines.next()) else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(Repo {
        name,
        root,
        git_dir: git_dir.into(),
        prefix: lines.next().unwrap_or_default().into(),
    }))
}

/// Откуда считаются пути: корень репозитория (или текущий каталог вне его), его место на
/// узле и подкаталог, в котором мы сейчас.
struct Here {
    local: PathBuf,
    remote: String,
    prefix: String,
}

fn here() -> io::Result<Here> {
    Ok(match repo()? {
        Some(repo) => Here {
            local: repo.root.canonicalize()?,
            remote: format!("ferry/{}", repo.name),
            prefix: repo.prefix,
        },
        None => Here {
            local: std::env::current_dir()?.canonicalize()?,
            remote: ".".into(),
            prefix: String::new(),
        },
    })
}

fn not_in_repo() -> io::Error {
    io::Error::other("здесь нет git-репозитория — синхронизировать нечего")
}

fn sync_repo(host: &str, repo: &Repo) -> io::Result<()> {
    let (commit, tree) = snapshot(repo)?;
    let changes = node_changes(host, repo, &tree)?;
    if !changes.is_empty() {
        let saved = save_patch(host, repo, &changes)?;
        eprintln!(
            "ferry: на {host} были правки, которых нет здесь; сохранил: {} (вернуть: git apply <патч>)",
            saved.display()
        );
    }
    let dir = format!("ferry/{}", repo.name);
    // --no-verify: pre-push хуки проекта (линтеры, тесты) к доставке на узел отношения не имеют.
    output(
        Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["push", "-q", "-f", "--no-verify"])
            .arg(format!("{host}:{dir}"))
            .arg(format!("{commit}:refs/ferry/snapshot")),
    )?;
    let checkout = format!(
        "cd {} && git checkout -q -f --detach {commit}",
        sh_quote(&dir)
    );
    output(Command::new("ssh").args([host, &checkout]))?;

    // Файлы мимо git (`.env`) — из настроек проекта на хабе.
    let mut extra = Vec::new();
    for file in crate::project::load(&repo.name)?.files {
        if repo.root.join(&file).exists() {
            extra.push(PathBuf::from(file));
        } else {
            eprintln!("ferry: {file} из files проекта здесь нет — не везу");
        }
    }
    if !extra.is_empty() {
        send(host, &repo.root, &extra, &dir)?;
    }
    let extra: Vec<_> = extra.iter().map(|f| f.display().to_string()).collect();
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" + {}", extra.join(", "))
    };
    eprintln!(
        "ferry: {host}:~/{dir} = рабочее дерево здесь ({}){extra}",
        &commit[..7]
    );
    Ok(())
}

/// Файлы `rels` (пути относительно `base`) — в каталог `dir` на узле: один tar через ssh.
fn send(host: &str, base: &Path, rels: &[PathBuf], dir: &str) -> io::Result<()> {
    let mut tar = Command::new("tar")
        // Без ._-файлов и xattrs macOS: tar на узле о них только ругается.
        .env("COPYFILE_DISABLE", "1")
        .args(["--no-xattrs", "-cf", "-", "-C"])
        .arg(base)
        .args(rels)
        .stdout(Stdio::piped())
        .spawn()?;
    let unpack = format!("mkdir -p {dir} && tar -C {dir} -xf -", dir = sh_quote(dir));
    let unpacked = output(
        Command::new("ssh")
            .args([host, &unpack])
            .stdin(tar.stdout.take().unwrap()),
    );
    if !tar.wait()?.success() {
        return Err(io::Error::other("tar здесь не собрал архив"));
    }
    unpacked.map(drop)
}

/// Снимок рабочего дерева: коммит вне веток поверх HEAD и его дерево. Автор и даты
/// фиксированы, поэтому тот же снимок даёт тот же хеш и повторный push пуст.
fn snapshot(repo: &Repo) -> io::Result<(String, String)> {
    let index = repo.git_dir.join("ferry-index");
    // Копия индекса: git переиспользует его stat-кэш и не перечитывает всё дерево.
    if fs::copy(repo.git_dir.join("index"), &index).is_err() {
        let _ = fs::remove_file(&index);
    }
    let git = || {
        let mut git = Command::new("git");
        git.arg("-C").arg(&repo.root).env("GIT_INDEX_FILE", &index);
        git
    };
    output(git().args(["add", "-A"]))?;
    let tree = text(git().arg("write-tree"))?;
    let mut commit = git();
    commit.args(["commit-tree", &tree, "-m", "ferry snapshot"]);
    if let Ok(head) = text(git().args(["rev-parse", "--verify", "-q", "HEAD"])) {
        commit.args(["-p", &head]);
    }
    for who in ["AUTHOR", "COMMITTER"] {
        commit
            .env(format!("GIT_{who}_NAME"), "ferry")
            .env(format!("GIT_{who}_EMAIL"), "ferry@localhost")
            .env(format!("GIT_{who}_DATE"), "@0 +0000");
    }
    Ok((text(&mut commit)?, tree))
}

/// Правки на узле поверх его последнего снимка — патч, или пусто, если их нет или они уже
/// совпадают с деревом `keep` (новым снимком). Заодно заводит репозиторий на узле.
fn node_changes(host: &str, repo: &Repo, keep: &str) -> io::Result<Vec<u8>> {
    let dir = sh_quote(&format!("ferry/{}", repo.name));
    let keep = sh_quote(keep);
    // autocrlf=false — файлы на узле байт в байт как здесь; longpaths — для node_modules.
    let script = format!(
        "set -e
mkdir -p ferry
[ -d {dir}/.git ] || git init -q {dir}
cd {dir}
git config core.autocrlf false
git config core.longpaths true
git rev-parse -q --verify HEAD >/dev/null || exit 0
export GIT_INDEX_FILE=.git/ferry-index
cp .git/index $GIT_INDEX_FILE
git add -A
t=$(git write-tree)
[ $t = $(git rev-parse 'HEAD^{{tree}}') ] || [ $t = {keep} ] || git diff --cached --binary HEAD"
    );
    output(Command::new("ssh").args([host, &script]))
}

fn save_patch(host: &str, repo: &Repo, patch: &[u8]) -> io::Result<PathBuf> {
    let dir = ferry_home()?.join("saved");
    fs::create_dir_all(&dir)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = dir.join(format!("{host}-{}-{now}.patch", repo.name));
    fs::write(&path, patch)?;
    Ok(path)
}

/// Узлы из `~/.ferry/ssh_config`: имя и адрес.
fn hosts() -> io::Result<Vec<(String, String)>> {
    let text = fs::read_to_string(ferry_home()?.join("ssh_config")).unwrap_or_default();
    let mut hosts: Vec<(String, String)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Host ") {
            hosts.push((name.trim().into(), String::new()));
        } else if let (Some(addr), Some(host)) = (line.strip_prefix("HostName "), hosts.last_mut())
        {
            host.1 = addr.trim().into();
        }
    }
    Ok(hosts)
}

/// Узел должен быть записан через `ferry add`: заодно имя не превратится в опцию ssh.
fn node(name: &str) -> io::Result<()> {
    if hosts()?.iter().any(|(host, _)| host == name) {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "нет узла {name:?}; список — ferry ls, добавить — ferry add"
    )))
}

fn probe(host: &str) -> String {
    let out = Command::new("ssh")
        .args(["-o", "ConnectTimeout=3", host])
        .arg("uname -s; ferry desk true >/dev/null 2>&1 && echo desk")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return "нет ssh".into();
    };
    let out = String::from_utf8_lossy(&out.stdout);
    let mut lines = out.lines();
    match (lines.next(), lines.next()) {
        (None, _) => "нет связи".into(),
        (Some(os), Some("desk")) => format!("{os}, рабочий стол отвечает"),
        (Some(os), _) => format!("{os}, рабочий стол НЕ отвечает"),
    }
}

/// Текст ssh_config без Host-блока `name` — от строки `Host name` до следующей `Host`.
fn without_host(text: &str, name: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    for line in text.lines() {
        if let Some(host) = line.strip_prefix("Host ") {
            skip = host.trim() == name;
        }
        if !skip {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn include_in_ssh_config() -> io::Result<()> {
    let dir = home()?.join(".ssh");
    let path = dir.join("config");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|line| line.trim() == INCLUDE) {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    // В самое начало: после первого Host строка попала бы в его условную область.
    write_private(&path, &format!("{INCLUDE}\n\n{text}"))?;
    eprintln!(
        "ferry: в начало ~/.ssh/config добавлено `{INCLUDE}` — узлы доступны как `ssh <узел>`"
    );
    Ok(())
}

fn check(what: &str, value: &str, allowed: impl Fn(char) -> bool) -> io::Result<()> {
    if value.is_empty() || value.starts_with('-') || !value.chars().all(allowed) {
        return Err(io::Error::other(format!(
            "недопустимое значение ({what}): {value:?}"
        )));
    }
    Ok(())
}

/// Запустить, вернуть stdout; при ошибке — вывод команды в тексте ошибки.
/// stdin — пустой, если вызывающий не задал свой (так устроен `Command::output`).
fn output(command: &mut Command) -> io::Result<Vec<u8>> {
    let out = command.stderr(Stdio::piped()).output()?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let said = [&out.stderr, &out.stdout]
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .join("\n");
    Err(io::Error::other(format!(
        "{} {}: {}",
        command.get_program().to_string_lossy(),
        command
            .get_args()
            .next()
            .map(|a| a.to_string_lossy())
            .unwrap_or_default(),
        said.trim()
    )))
}

fn text(command: &mut Command) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&output(command)?).trim_end().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_the_remote_shell() {
        let cmd = r#"printf '%s|' "a b" it's $HOME `x`"#;
        let out = Command::new("sh")
            .args(["-c", &format!("printf %s {}", sh_quote(cmd))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), cmd);
    }

    #[test]
    fn re_adding_a_host_replaces_its_block() {
        let text = "Host a\n  HostName 1\n\nHost b\n  HostName 2\n\n";
        assert_eq!(without_host(text, "a"), "Host b\n  HostName 2\n\n");
        assert_eq!(without_host(text, "b"), "Host a\n  HostName 1\n\n");
        assert_eq!(without_host(text, "c"), text);
    }

    /// Снимок берёт незакоммиченное и новые файлы, детерминирован и не трогает ветку и индекс.
    #[test]
    fn snapshot_takes_the_worktree_and_leaves_the_repo_alone() {
        let root = std::env::temp_dir().join(format!("ferry-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        fs::write(root.join("a.txt"), "1\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let head = git(&["rev-parse", "HEAD"]);

        fs::write(root.join("a.txt"), "2\n").unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        fs::write(root.join("ignored"), "x\n").unwrap();
        let repo = Repo {
            git_dir: PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])),
            root: root.clone(),
            name: "r".into(),
            prefix: String::new(),
        };
        let (commit, tree) = snapshot(&repo).unwrap();

        assert_eq!(snapshot(&repo).unwrap(), (commit.clone(), tree.clone()));
        assert_eq!(git(&["show", &format!("{commit}:a.txt")]), "2");
        assert_eq!(git(&["show", &format!("{commit}:new.txt")]), "new");
        assert_eq!(git(&["rev-parse", &format!("{commit}^")]), head);
        assert!(
            git(&["ls-tree", "--name-only", &tree])
                .lines()
                .all(|f| f != "ignored")
        );
        assert_eq!(git(&["rev-parse", "HEAD"]), head);
        assert_eq!(git(&["diff", "--cached", "--name-only"]), "");
        assert_eq!(
            git(&["ls-files", "--others", "--exclude-standard"]),
            "new.txt"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
