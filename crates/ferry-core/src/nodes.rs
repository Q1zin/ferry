//! Реестр узлов хаба — Host-блоки в `~/.ferry/ssh_config`. `register` подключает файл в
//! `~/.ssh/config` через Include, поэтому `ssh <узел>`, `scp` и `git push <узел>:…` работают и
//! напрямую.
//!
//! Пауза на хабе — строка-комментарий `# ferry: paused` в блоке: ssh её не видит, ferry
//! перестаёт трогать узел (ноутбук унесли домой). Это не то же самое, что режим узла
//! ([`crate::mode`]): режим выбирают на самом узле, паузу — здесь.

use std::fs;
use std::io::{self, Write};
use std::process::{Command, Stdio};

use crate::mode::Mode;
use crate::{ferry_home, home, write_private};

const INCLUDE: &str = "Include ~/.ferry/ssh_config";
const PAUSED: &str = "# ferry: paused";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub addr: String,
    pub user: String,
    /// Пауза на хабе: ferry узел не трогает.
    pub paused: bool,
}

pub fn list() -> io::Result<Vec<Node>> {
    Ok(parse(&fs::read_to_string(config()?).unwrap_or_default()))
}

/// Узел записан через `ferry add`: заодно имя не превратится в опцию ssh.
pub fn get(name: &str) -> io::Result<Node> {
    list()?.into_iter().find(|n| n.name == name).ok_or_else(|| {
        io::Error::other(format!(
            "нет узла {name:?}; список — ferry ls, добавить — ferry add"
        ))
    })
}

/// Узел, который можно трогать: записан и не на паузе на хабе.
pub fn active(name: &str) -> io::Result<Node> {
    let node = get(name)?;
    if node.paused {
        return Err(io::Error::other(format!(
            "{name} на паузе на хабе — ferry resume {name}"
        )));
    }
    Ok(node)
}

/// Записать узел (или переписать его блок заново).
pub fn register(name: &str, user: &str, addr: &str) -> io::Result<()> {
    validate_name(name)?;
    // Всё это уходит в ssh_config как есть: пробел или перевод строки дописали бы свои опции.
    check("пользователь", user, |c| {
        !c.is_whitespace() && !c.is_control() && c != '"'
    })?;
    check("адрес", addr, |c| {
        c.is_ascii_alphanumeric() || "._-:".contains(c)
    })?;

    let dir = ferry_home()?;
    fs::create_dir_all(&dir)?;
    let path = config()?;
    let mut text = edit_block(
        &fs::read_to_string(&path).unwrap_or_default(),
        name,
        |_| vec![],
    );
    // BatchMode: ssh не ждёт пароль, а сразу падает — для Claude и скриптов это важнее.
    // ControlMaster: одно соединение на узел, остальные команды идут без handshake.
    // RemoteForward: порт на узле → релей git этого хаба (см. relay.rs).
    // HostKeyAlias: ключ узла — под его именем; сменит адрес — ferry find узнает его по ключу.
    text.push_str(&format!(
        "Host {name}\n  HostName {addr}\n  HostKeyAlias {name}\n  User {user}\n  \
         IdentityFile ~/.ssh/ferry\n  IdentitiesOnly yes\n  BatchMode yes\n  \
         StrictHostKeyChecking accept-new\n  ServerAliveInterval 15\n  ControlMaster auto\n  \
         ControlPath ~/.ferry/cm-%C\n  ControlPersist 10m\n  RemoteForward 127.0.0.1:{} \"{}\"\n\n",
        crate::relay::NODE_PORT,
        dir.join("relay.sock").display()
    ));
    write_private(&path, &text)?;
    include_in_ssh_config()?;
    // Старое общее соединение живёт со старыми настройками — закрываем.
    close_master(name);
    Ok(())
}

pub fn set_paused(name: &str, paused: bool) -> io::Result<()> {
    get(name)?;
    rewrite(|text| with_pause(text, name, paused))?;
    if paused {
        close_master(name);
    }
    Ok(())
}

/// Новое имя узла: блок, алиас ключа и записи в known_hosts.
pub fn rename(old: &str, new: &str) -> io::Result<()> {
    get(old)?;
    validate_name(new)?;
    if get(new).is_ok() {
        return Err(io::Error::other(format!("узел {new} уже есть")));
    }
    close_master(old);
    rewrite(|text| renamed(text, old, new))?;
    // Ключ узла — под новым именем, старые записи — прочь.
    let keys = known_keys(old);
    if !keys.is_empty() {
        let path = home()?.join(".ssh/known_hosts");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        for key in keys {
            writeln!(file, "{new} {key}")?;
        }
        forget_keys(old);
    }
    Ok(())
}

/// Убрать узел с хаба. ferry на самом узле остаётся.
pub fn remove(name: &str) -> io::Result<()> {
    get(name)?;
    close_master(name);
    rewrite(|text| edit_block(text, name, |_| vec![]))?;
    forget_keys(name);
    Ok(())
}

/// Новый адрес узла и `HostKeyAlias` сразу за ним (прежний, где бы ни стоял, убирается —
/// так он ровно один).
pub fn set_hostname(name: &str, ip: &str) -> io::Result<()> {
    rewrite(|text| with_hostname(text, name, ip))?;
    // Общее соединение, если вдруг живо, — к старому адресу.
    close_master(name);
    Ok(())
}

/// Имя узла: им называют блок в ssh_config и пишут его в командах.
pub fn validate_name(name: &str) -> io::Result<()> {
    check("имя узла", name, |c| {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    })?;
    if Mode::parse(name).is_some() {
        return Err(io::Error::other(format!(
            "{name:?} — имя режима, назови узел иначе"
        )));
    }
    Ok(())
}

fn config() -> io::Result<std::path::PathBuf> {
    Ok(ferry_home()?.join("ssh_config"))
}

fn rewrite(edit: impl FnOnce(&str) -> String) -> io::Result<()> {
    let path = config()?;
    let text = fs::read_to_string(&path)?;
    write_private(&path, &edit(&text))
}

fn with_hostname(text: &str, name: &str, ip: &str) -> String {
    edit_block(text, name, |line| {
        let key = line.trim_start();
        if key.starts_with("HostKeyAlias ") {
            vec![]
        } else if key.starts_with("HostName ") {
            vec![format!("  HostName {ip}"), format!("  HostKeyAlias {name}")]
        } else {
            vec![line.into()]
        }
    })
}

fn with_pause(text: &str, name: &str, paused: bool) -> String {
    edit_block(text, name, |line| {
        if line.trim() == PAUSED {
            vec![]
        } else if paused && line.starts_with("Host ") {
            vec![line.into(), format!("  {PAUSED}")]
        } else {
            vec![line.into()]
        }
    })
}

fn renamed(text: &str, old: &str, new: &str) -> String {
    edit_block(text, old, |line| {
        if line.starts_with("Host ") {
            vec![format!("Host {new}")]
        } else if line.trim_start().starts_with("HostKeyAlias ") {
            vec![format!("  HostKeyAlias {new}")]
        } else {
            vec![line.into()]
        }
    })
}

/// ssh_config с правкой блока `name`: `edit` получает каждую строку блока (с `Host …`) и
/// возвращает, чем её заменить; пусто — убрать.
fn edit_block(text: &str, name: &str, mut edit: impl FnMut(&str) -> Vec<String>) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if let Some(host) = line.strip_prefix("Host ") {
            inside = host.trim() == name;
        }
        let lines = if inside {
            edit(line)
        } else {
            vec![line.into()]
        };
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn parse(text: &str) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Host ") {
            nodes.push(Node {
                name: name.trim().into(),
                ..Node::default()
            });
            continue;
        }
        let Some(node) = nodes.last_mut() else {
            continue;
        };
        if let Some(addr) = line.strip_prefix("HostName ") {
            node.addr = addr.trim().into();
        } else if let Some(user) = line.strip_prefix("User ") {
            node.user = user.trim().into();
        } else if line == PAUSED {
            node.paused = true;
        }
    }
    nodes
}

/// Ключи хоста из known_hosts по имени: `тип base64`.
pub(crate) fn known_keys(name: &str) -> Vec<String> {
    let Ok(out) = Command::new("ssh-keygen").args(["-F", name]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(
            |line| match line.split_whitespace().collect::<Vec<_>>()[..] {
                [_, kind, key, ..] => Some(format!("{kind} {key}")),
                _ => None,
            },
        )
        .collect()
}

fn forget_keys(name: &str) {
    let _ = Command::new("ssh-keygen")
        .args(["-R", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn close_master(name: &str) {
    let _ = Command::new("ssh")
        .args(["-O", "exit", name])
        .stderr(Stdio::null())
        .status();
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

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "Host a\n  HostName 1.1.1.1\n  User u\n\nHost b\n  HostName 2.2.2.2\n";

    #[test]
    fn parses_nodes_with_pause() {
        let text =
            "Host a\n  # ferry: paused\n  HostName 1.1.1.1\n  User u\nHost b\n  HostName 2.2.2.2\n";
        let nodes = parse(text);
        assert_eq!(nodes.len(), 2);
        assert_eq!(
            (
                nodes[0].addr.as_str(),
                nodes[0].user.as_str(),
                nodes[0].paused
            ),
            ("1.1.1.1", "u", true)
        );
        assert_eq!(
            (nodes[1].addr.as_str(), nodes[1].paused),
            ("2.2.2.2", false)
        );
    }

    #[test]
    fn removing_touches_only_its_block() {
        assert_eq!(
            edit_block(TEXT, "a", |_| vec![]),
            "Host b\n  HostName 2.2.2.2\n"
        );
        assert_eq!(edit_block(TEXT, "c", |_| vec![]), TEXT);
    }

    #[test]
    fn new_address_keeps_exactly_one_alias_next_to_it() {
        let moved = with_hostname(TEXT, "a", "9.9.9.9");
        assert_eq!(
            moved,
            "Host a\n  HostName 9.9.9.9\n  HostKeyAlias a\n  User u\n\nHost b\n  HostName 2.2.2.2\n"
        );
        assert_eq!(
            with_hostname(&moved, "a", "8.8.8.8"),
            moved.replace("9.9.9.9", "8.8.8.8")
        );
        // Алиас в конце блока (так писала первая версия поиска) встаёт на место.
        let tail = "Host a\n  HostName 1.1.1.1\n  User u\n\n  HostKeyAlias a\n";
        assert_eq!(
            with_hostname(tail, "a", "9.9.9.9"),
            "Host a\n  HostName 9.9.9.9\n  HostKeyAlias a\n  User u\n\n"
        );
    }

    #[test]
    fn pause_and_rename_round_trip() {
        let paused = with_pause(TEXT, "a", true);
        assert!(parse(&paused)[0].paused && !parse(&paused)[1].paused);
        assert_eq!(
            with_pause(&paused, "a", true),
            paused,
            "второй раз не дублирует"
        );
        assert_eq!(with_pause(&paused, "a", false), TEXT);

        let aliased = with_hostname(TEXT, "a", "1.1.1.1");
        let renamed = renamed(&aliased, "a", "c");
        assert_eq!(
            renamed,
            aliased
                .replace("Host a", "Host c")
                .replace("HostKeyAlias a", "HostKeyAlias c")
        );
        assert_eq!(parse(&renamed)[0].name, "c");
    }
}
