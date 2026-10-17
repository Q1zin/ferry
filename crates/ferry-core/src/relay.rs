//! Релей git, сторона устройства-узла и проверки стороны заказчика. У узла нет ключей от
//! приватных репозиториев, а git на нём ходит в них по SSH — ключами устройства, которое
//! запустило команду.
//!
//! Команды, которые прислало устройство, получают `GIT_SSH_COMMAND='<exe приложения> ssh'`,
//! `FERRY_RELAY='<порт> <токен>'` и `FERRY_TRUST` — доверенные хосты заказчика. Для доверенного
//! хоста [`ssh`] в сеть сам не идёт: он стучится на `127.0.0.1:<порт>` в приложение, а оно
//! гонит сессию к заказчику (`ferry-net`). Заказчик пропускает только чтение
//! (`git-upload-pack`, `git-upload-archive`) к своим доверенным хостам ([`allowed`]) и выполняет
//! команду своим ssh ([`ssh_command`]): ключ не покидает его. Недоверенные хосты — обычный ssh.
//!
//! Протокол клиента (поля «`u32` LE длина + байты»):
//! - клиент → приложение: токен, `user@host`, порт или пусто, команда, `GIT_PROTOCOL` или пусто;
//!   после «принято» — куски stdin, пустое поле — конец stdin;
//! - приложение → клиент: кадры «байт вида + поле»: 0 — принято, 1 — stdout, 2 — stderr,
//!   3 — код выхода (`i32` LE). Отказ — 2 и 3 без 0.

use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::thread;

use crate::desk::{read_field, write_field};
use crate::{exit_code, ferry_home, write_private};

/// `ferry ssh <аргументы ssh>` на узле. git зовёт его так:
/// `[-o SendEnv=GIT_PROTOCOL] [-p порт] [-4|-6] <user@host> <команда>`.
pub fn ssh(args: &[&str]) -> io::Result<i32> {
    let mut port = "";
    let mut rest = args;
    loop {
        match rest {
            ["-o", _, tail @ ..] => rest = tail,
            ["-p", p, tail @ ..] => {
                port = p;
                rest = tail;
            }
            [flag, tail @ ..] if flag.len() == 2 && flag.starts_with('-') => rest = tail,
            _ => break,
        }
    }
    let [target, command @ ..] = rest else {
        return plain_ssh(args);
    };
    let host = target.rsplit_once('@').map_or(*target, |(_, host)| host);
    let relay = std::env::var("FERRY_RELAY").unwrap_or_default();
    let trusted = std::env::var("FERRY_TRUST").unwrap_or_default();
    let (Some((relay_port, token)), true) =
        (relay.split_once(' '), trusted.split(' ').any(|t| t == host))
    else {
        return plain_ssh(args);
    };
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", relay_port.parse().unwrap_or(0))) else {
        eprintln!("ferry: релей недоступен — {host} обычным ssh");
        return plain_ssh(args);
    };

    let protocol = std::env::var("GIT_PROTOCOL").unwrap_or_default();
    let command = command.join(" ");
    for field in [token, *target, port, command.as_str(), protocol.as_str()] {
        write_field(&mut stream, field.as_bytes())?;
    }

    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    let mut started = false;
    loop {
        let mut kind = [0];
        if stream.read_exact(&mut kind).is_err() {
            if started {
                return Err(io::Error::other("релей оборвал соединение"));
            }
            // Релей не ответил. stdin ещё не тронут — можно и обычным ssh.
            eprintln!("ferry: релей не ответил — {host} обычным ssh");
            return plain_ssh(args);
        }
        let data = read_field(&mut stream)?;
        match kind[0] {
            0 => {
                started = true;
                let mut to_relay = stream.try_clone()?;
                thread::spawn(move || {
                    let mut buf = [0; 16384];
                    let mut stdin = io::stdin().lock();
                    while let Ok(n @ 1..) = stdin.read(&mut buf) {
                        if write_field(&mut to_relay, &buf[..n]).is_err() {
                            return;
                        }
                    }
                    let _ = write_field(&mut to_relay, &[]);
                });
            }
            1 => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            2 => stderr.write_all(&data)?,
            3 => {
                let code = data.get(..4).and_then(|b| b.try_into().ok());
                return Ok(code.map_or(255, i32::from_le_bytes));
            }
            other => return Err(io::Error::other(format!("релей прислал кадр {other}"))),
        }
    }
}

fn plain_ssh(args: &[&str]) -> io::Result<i32> {
    Ok(exit_code(Command::new("ssh").args(args).status()?))
}

pub fn trust(host: Option<&str>) -> io::Result<i32> {
    let mut hosts = trusted()?;
    let Some(host) = host else {
        hosts.iter().for_each(|h| println!("{h}"));
        return Ok(0);
    };
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
    {
        return Err(io::Error::other(format!("недопустимый хост: {host:?}")));
    }
    if !hosts.iter().any(|h| h == host) {
        hosts.push(host.into());
        fs::create_dir_all(ferry_home()?)?;
        write_private(&ferry_home()?.join("trust"), &(hosts.join("\n") + "\n"))?;
    }
    eprintln!("ferry: git с {host} на устройствах пойдёт через ключи этой машины (только чтение)");
    Ok(0)
}

pub fn trusted() -> io::Result<Vec<String>> {
    trusted_in(&crate::ferry_home()?)
}

/// Доверенные хосты из `<dir>/trust`.
pub fn trusted_in(dir: &Path) -> io::Result<Vec<String>> {
    let text = std::fs::read_to_string(dir.join("trust")).unwrap_or_default();
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Можно ли выполнить ssh-команду узла ключами этой машины: хост доверен (`<dir>/trust`),
/// команда — только чтение, аргументы без сюрпризов.
/// Каждое поле уходит в аргументы ssh: здесь решается, не станет ли оно опцией.
pub fn allowed(
    dir: &Path,
    target: &str,
    port: &str,
    command: &str,
    protocol: &str,
) -> Result<(), String> {
    let (user, host) = target.rsplit_once('@').unwrap_or(("", target));
    let trusted = trusted_in(dir).map_err(|e| e.to_string())?;
    if !trusted.iter().any(|t| t == host) {
        return Err(format!(
            "{host} не в доверенных (на запустившем устройстве: ferry trust {host})"
        ));
    }
    let word = |s: &str| {
        !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    if !user.is_empty() && !word(user) {
        return Err(format!("недопустимый пользователь {user:?}"));
    }
    if !port.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("недопустимый порт {port:?}"));
    }
    if !protocol
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "=:._-".contains(c))
    {
        return Err(format!("недопустимый GIT_PROTOCOL {protocol:?}"));
    }
    // git присылает ровно `git-upload-pack '<путь>'`.
    let read_only = ["git-upload-pack ", "git-upload-archive "]
        .iter()
        .any(|verb| {
            command.strip_prefix(verb).is_some_and(|path| {
                path.len() > 2
                    && path.starts_with('\'')
                    && path.ends_with('\'')
                    && !path[1..path.len() - 1].contains('\'')
            })
        });
    if !read_only {
        return Err(format!(
            "релей пропускает только чтение (git-upload-pack, git-upload-archive), не {command:?}"
        ));
    }
    Ok(())
}

/// ssh этой машины для команды узла (её уже проверил [`allowed`]). `FERRY_RELAY_SSH` — подменить
/// ssh (тесты, нестандартный клиент).
pub fn ssh_command(target: &str, port: &str, command: &str, protocol: &str) -> Command {
    let program = std::env::var("FERRY_RELAY_SSH").unwrap_or_else(|_| "ssh".into());
    let mut ssh = Command::new(program);
    ssh.args(["-o", "BatchMode=yes"]);
    if !port.is_empty() {
        ssh.args(["-p", port]);
    }
    if !protocol.is_empty() {
        ssh.arg("-o").arg(format!("SetEnv=GIT_PROTOCOL={protocol}"));
    }
    ssh.args(["--", target, command]);
    ssh
}
