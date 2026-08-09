//! Релей git через хаб: у узла нет ключей от приватных репозиториев, а git на нём всё равно
//! ходит в них по SSH — ключами хаба.
//!
//! Команды, которые ferry запускает на узле, получают `GIT_SSH_COMMAND='ferry ssh'`. Для
//! доверенного хоста (`ferry trust <хост>`) `ferry ssh` в сеть сам не идёт: через обратный
//! проброс (RemoteForward из Host-блока узла → unix-сокет хаба) он просит релей хаба
//! выполнить ту же команду своим ssh и гоняет stdin/stdout/stderr. Ключ хаб не покидает.
//! Релей пропускает только чтение (`git-upload-pack`, `git-upload-archive`) и только к
//! доверенным хостам: скомпрометированный узел не сможет пушить от твоего имени. Другие
//! хосты, как и недоступный релей, — обычный ssh узла.
//!
//! Протокол (поля «`u32` LE длина + байты», как у демона):
//! - узел → хаб: токен, `user@host`, порт или пусто, команда, `GIT_PROTOCOL` или пусто;
//!   после «принято» — куски stdin, пустое поле — конец stdin;
//! - хаб → узел: кадры «байт вида + поле»: 0 — принято, 1 — stdout, 2 — stderr,
//!   3 — код выхода (`i32` LE). Отказ — 2 и 3 без 0.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::thread;

use crate::desk::{read_field, write_field};
use crate::exit_code;

/// Порт на узле, где sshd слушает обратный проброс к релею хаба.
pub const NODE_PORT: u16 = 47801;

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
        eprintln!("ferry: релей хаба недоступен — {host} обычным ssh");
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
                return Err(io::Error::other("релей хаба оборвал соединение"));
            }
            // Хаб на связи, но релей на нём не отвечает. stdin ещё не тронут — можно и так.
            eprintln!("ferry: релей хаба не ответил — {host} обычным ssh");
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

#[cfg(unix)]
pub use hub::{node_env, serve, trust, trusted};

#[cfg(not(unix))]
pub fn node_env() -> io::Result<String> {
    Ok(String::new())
}

#[cfg(not(unix))]
pub fn serve() -> io::Result<i32> {
    Err(io::Error::other(
        "релей хаба пока есть только на macOS и Linux",
    ))
}

#[cfg(not(unix))]
pub fn trust(_: Option<&str>) -> io::Result<i32> {
    serve()
}

#[cfg(not(unix))]
pub fn trusted() -> io::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(unix)]
mod hub {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::NODE_PORT;
    use crate::desk::{new_token, read_field, write_field};
    use crate::{exit_code, ferry_home, sh_quote, write_private};

    /// Сколько релей ждёт без единого соединения, прежде чем выйти; следующая команда
    /// ferry поднимет его снова.
    const IDLE: Duration = Duration::from_secs(30 * 60);

    pub fn socket() -> io::Result<PathBuf> {
        Ok(ferry_home()?.join("relay.sock"))
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
        eprintln!("ferry: git с {host} на узлах пойдёт через ключи этого хаба (только чтение)");
        Ok(0)
    }

    pub fn trusted() -> io::Result<Vec<String>> {
        let text = fs::read_to_string(ferry_home()?.join("trust")).unwrap_or_default();
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    fn token() -> io::Result<String> {
        let path = ferry_home()?.join("relay-token");
        let token = fs::read_to_string(&path).unwrap_or_default();
        if !token.trim().is_empty() {
            return Ok(token.trim().into());
        }
        let token = new_token();
        fs::create_dir_all(ferry_home()?)?;
        write_private(&path, &token)?;
        Ok(token)
    }

    /// Окружение для команды на узле: git ходит к доверенным хостам через релей хаба.
    /// Пусто, если доверенных хостов нет. Заодно поднимает релей.
    pub fn node_env() -> io::Result<String> {
        let hosts = trusted()?;
        if hosts.is_empty() {
            return Ok(String::new());
        }
        ensure_running()?;
        // cargo по умолчанию ходит в git через libgit2, мимо GIT_SSH_COMMAND.
        Ok(format!(
            "export GIT_SSH_COMMAND='ferry ssh' GIT_SSH_VARIANT=ssh \
             CARGO_NET_GIT_FETCH_WITH_CLI=true FERRY_RELAY={} FERRY_TRUST={}; ",
            sh_quote(&format!("{NODE_PORT} {}", token()?)),
            sh_quote(&hosts.join(" "))
        ))
    }

    fn ensure_running() -> io::Result<()> {
        let socket = socket()?;
        if UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        let mut relay = Command::new(std::env::current_exe()?);
        relay
            .arg("relay")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Своя группа: Ctrl-C в терминале, откуда его подняли, релей не трогает.
        std::os::unix::process::CommandExt::process_group(&mut relay, 0);
        relay.spawn()?;
        for _ in 0..50 {
            if UnixStream::connect(&socket).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(40));
        }
        eprintln!("ferry: релей не поднялся — git к доверенным хостам на узле пойдёт обычным ssh");
        Ok(())
    }

    /// `ferry relay`: слушать `~/.ferry/relay.sock`, пока есть работа.
    pub fn serve() -> io::Result<i32> {
        let socket = socket()?;
        fs::create_dir_all(ferry_home()?)?;
        if UnixStream::connect(&socket).is_ok() {
            return Ok(0);
        }
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let token: Arc<str> = token()?.into();

        let active = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Instant::now()));
        let (watch_active, watch_last) = (active.clone(), last.clone());
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(60));
                let idle = watch_last.lock().unwrap().elapsed() > IDLE;
                if watch_active.load(Ordering::SeqCst) == 0 && idle {
                    let _ = fs::remove_file(&socket);
                    std::process::exit(0);
                }
            }
        });

        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            active.fetch_add(1, Ordering::SeqCst);
            let (token, active, last) = (token.clone(), active.clone(), last.clone());
            thread::spawn(move || {
                let _ = relay(stream, &token);
                *last.lock().unwrap() = Instant::now();
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Ok(0)
    }

    fn relay(stream: UnixStream, token: &str) -> io::Result<()> {
        let mut input = &stream;
        if read_field(&mut input)? != token.as_bytes() {
            return Ok(());
        }
        let mut field = || read_field(&mut input).map(|f| String::from_utf8_lossy(&f).into_owned());
        let (target, port, command, protocol) = (field()?, field()?, field()?, field()?);

        let out = Mutex::new(stream.try_clone()?);
        let send = |kind: u8, data: &[u8]| -> io::Result<()> {
            let mut out = out.lock().unwrap();
            out.write_all(&[kind])?;
            write_field(&mut *out, data)
        };
        if let Err(why) = allowed(&target, &port, &command, &protocol) {
            send(2, format!("ferry relay: {why}\n").as_bytes())?;
            return send(3, &1i32.to_le_bytes());
        }

        // FERRY_RELAY_SSH — подменить ssh (тесты, нестандартный клиент).
        let program = std::env::var("FERRY_RELAY_SSH").unwrap_or_else(|_| "ssh".into());
        let mut ssh = Command::new(program);
        ssh.args(["-o", "BatchMode=yes"]);
        if !port.is_empty() {
            ssh.args(["-p", &port]);
        }
        if !protocol.is_empty() {
            ssh.arg("-o").arg(format!("SetEnv=GIT_PROTOCOL={protocol}"));
        }
        ssh.args(["--", &target, &command])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match ssh.spawn() {
            Ok(child) => child,
            Err(e) => {
                send(2, format!("ferry relay: ssh на хабе: {e}\n").as_bytes())?;
                return send(3, &127i32.to_le_bytes());
            }
        };
        send(0, &[])?;

        // stdin — отдельно и без ожидания: клиент может держать его открытым до кода выхода.
        let mut stdin = child.stdin.take().unwrap();
        let mut from_node = stream.try_clone()?;
        thread::spawn(move || {
            while let Ok(chunk) = read_field(&mut from_node) {
                if chunk.is_empty() || stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        thread::scope(|s| {
            s.spawn(|| pump(&mut stdout, 1, &send));
            s.spawn(|| pump(&mut stderr, 2, &send));
        });
        let code = exit_code(child.wait()?);
        send(3, &code.to_le_bytes())?;
        // Отпускает поток stdin, если клиент так и не закрыл свой.
        stream.shutdown(std::net::Shutdown::Both)
    }

    fn pump(from: &mut impl Read, kind: u8, send: &impl Fn(u8, &[u8]) -> io::Result<()>) {
        let mut buf = [0; 16384];
        while let Ok(n @ 1..) = from.read(&mut buf) {
            if send(kind, &buf[..n]).is_err() {
                return;
            }
        }
    }

    /// Каждое поле уходит в аргументы ssh хаба: здесь решается, не станет ли оно опцией.
    fn allowed(target: &str, port: &str, command: &str, protocol: &str) -> Result<(), String> {
        let (user, host) = target.rsplit_once('@').unwrap_or(("", target));
        let trusted = trusted().map_err(|e| e.to_string())?;
        if !trusted.iter().any(|t| t == host) {
            return Err(format!(
                "{host} не в доверенных (на хабе: ferry trust {host})"
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
}
