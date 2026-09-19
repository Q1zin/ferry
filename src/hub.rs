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
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{exit_code, ferry_home, home, write_private};

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
    text.push_str(&format!(
        "Host {name}\n  HostName {addr}\n  User {user}\n  IdentityFile ~/.ssh/ferry\n  \
         IdentitiesOnly yes\n  BatchMode yes\n  StrictHostKeyChecking accept-new\n  \
         ServerAliveInterval 15\n  ControlMaster auto\n  ControlPath ~/.ferry/cm-%C\n  \
         ControlPersist 10m\n\n"
    ));
    write_private(&path, &text)?;
    include_in_ssh_config()?;

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
    let dir = match repo()? {
        Some(repo) => {
            if sync {
                sync_repo(host, &repo)?;
            }
            format!("ferry/{}/{}", repo.name, repo.prefix)
        }
        None => ".".into(),
    };
    // Команда разбирается шеллом узла ровно один раз — и в обычном режиме, и через desk.
    let cmd = if desk {
        format!("ferry desk {}", sh_quote(cmd))
    } else {
        cmd.into()
    };
    let mut ssh = Command::new("ssh");
    if io::stdin().is_terminal() {
        ssh.arg("-t");
    }
    let remote = format!("cd {} && {cmd}", sh_quote(&dir));
    ssh.args([host, &remote]);
    // exec, а не дочерний процесс: сигнал по pid ferry (Ctrl-C, kill, таймаут вызывающего)
    // достаётся самому ssh, и канал на узле закрывается, а не висит за осиротевшим ssh.
    #[cfg(unix)]
    return Err(std::os::unix::process::CommandExt::exec(&mut ssh));
    #[cfg(not(unix))]
    Ok(exit_code(ssh.status()?))
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

pub fn update(host: &str, file: &str) -> io::Result<i32> {
    node(host)?;
    let os = text(Command::new("ssh").args([host, "uname -s"]))?;
    if !os.starts_with("MINGW") && !os.starts_with("MSYS") {
        return Err(io::Error::other(format!(
            "update пока умеет только Windows-узлы ({host}: {os})"
        )));
    }
    output(
        Command::new("scp")
            .arg("-q")
            .arg(file)
            .arg(format!("{host}:.ferry/bin/ferry.new.exe")),
    )?;
    // Запущенный exe не перезаписать, но переименовать можно. Демон перезапускает
    // планировщик — так он снова окажется на рабочем столе, а не в Session 0.
    const RESTART: &str = "set -e
cd ~/.ferry/bin
schtasks //end //tn ferry >/dev/null 2>&1 || true
mv -f ferry.exe ferry.old.exe
mv ferry.new.exe ferry.exe
rm -f ~/.ferry/desk
schtasks //run //tn ferry >/dev/null
for i in $(seq 50); do [ -f ~/.ferry/desk ] && break; sleep 0.2; done
rm -f ferry.old.exe 2>/dev/null || true
ferry desk 'echo ferry: daemon restarted'";
    remote_status(host, RESTART)
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
    eprintln!(
        "ferry: {host}:~/{dir} = рабочее дерево здесь ({})",
        &commit[..7]
    );
    Ok(())
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

/// Строка в одинарных кавычках для POSIX-шелла узла (там Git Bash или zsh).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Запустить, вернуть stdout; при ошибке — вывод команды в тексте ошибки.
fn output(command: &mut Command) -> io::Result<Vec<u8>> {
    let out = command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
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
