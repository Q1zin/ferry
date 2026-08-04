//! Демон рабочего стола и клиент к нему.
//!
//! По SSH команды на Windows попадают в Session 0, где нет рабочего стола: окна не видны,
//! фокуса нет. Демон запускает планировщик при входе пользователя, то есть в его
//! интерактивной сессии, а `ferry desk`, вызванный по SSH, выполняет команду через него.
//!
//! Протокол — TCP на 127.0.0.1, порт и токен лежат в `~/.ferry/desk`:
//! - запрос — три поля «`u32` LE длина + байты»: токен, рабочий каталог, команда для shell;
//! - ответ — куски общего stdout+stderr в том же формате, затем пустое поле и `i32` LE код выхода.
//!
//! Клиент после запроса ничего не шлёт. Если соединение закрылось раньше, чем команда
//! завершилась, демон убивает её дерево процессов.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// Предел поля запроса: мусор на порту не должен заставить демон выделить гигабайты.
const MAX_FIELD: u32 = 1 << 20;

/// Сколько ждать хвост вывода после выхода команды. Запущенные ею фоновые процессы держат
/// pipe открытым (как и у ssh), их конца не ждём.
const TAIL: Duration = Duration::from_millis(500);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn daemon() -> io::Result<i32> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let token = new_token();
    write_state(&format!("{} {token}\n", listener.local_addr()?.port()))?;
    detach_console();
    let token: Arc<str> = token.into();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let token = token.clone();
        thread::spawn(move || {
            let _ = serve(stream, &token);
        });
    }
    Ok(0)
}

pub fn run(cmd: &str) -> io::Result<i32> {
    let not_running = |e: io::Error| {
        io::Error::other(format!(
            "демон рабочего стола не запущен ({e}); на Windows: schtasks /run /tn ferry"
        ))
    };
    let state = std::fs::read_to_string(state_path()?).map_err(not_running)?;
    let (port, token) = state
        .trim()
        .split_once(' ')
        .and_then(|(port, token)| Some((port.parse::<u16>().ok()?, token)))
        .ok_or_else(|| io::Error::other("испорчен ~/.ferry/desk"))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(not_running)?;

    let cwd = std::env::current_dir()?;
    let cwd = cwd.to_string_lossy();
    for field in [token.as_bytes(), cwd.as_bytes(), cmd.as_bytes()] {
        write_field(&mut stream, field)?;
    }

    let mut stdout = io::stdout().lock();
    loop {
        let chunk = read_field(&mut stream)
            .map_err(|e| io::Error::other(format!("демон оборвал соединение: {e}")))?;
        if chunk.is_empty() {
            break;
        }
        stdout.write_all(&chunk)?;
        stdout.flush()?;
    }
    let mut code = [0; 4];
    stream.read_exact(&mut code)?;
    Ok(i32::from_le_bytes(code))
}

fn serve(stream: TcpStream, token: &str) -> io::Result<()> {
    let mut input = &stream;
    if read_field(&mut input)? != token.as_bytes() {
        return Ok(());
    }
    let cwd = String::from_utf8_lossy(&read_field(&mut input)?).into_owned();
    let cmd = String::from_utf8_lossy(&read_field(&mut input)?).into_owned();

    // Ответ пишут двое — насос вывода и итоговый код; `None` — ответ уже закрыт.
    let out = Arc::new(Mutex::new(Some(stream.try_clone()?)));
    let (reader, writer) = io::pipe()?;
    let mut command = shell(&cmd);
    command
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let spawned = command.spawn();
    // Копии пишущего конца живут в `command`: пока он жив, насос не дождётся EOF.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let msg = format!("ferry: не удалось запустить команду в {cwd}: {e}\n");
            return finish(&out, msg.as_bytes(), 127);
        }
    };

    let (tail_tx, tail_rx) = mpsc::channel();
    let pump_out = out.clone();
    thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0; 8192];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            let mut out = pump_out.lock().unwrap();
            let Some(stream) = out.as_mut() else { break };
            if write_field(stream, &buf[..n]).is_err() {
                break;
            }
        }
        let _ = tail_tx.send(());
    });

    let done = Arc::new(AtomicBool::new(false));
    let watch_done = done.clone();
    let watch = stream.try_clone()?;
    let pid = child.id();
    thread::spawn(move || {
        // Клиент больше ничего не шлёт: чтение вернётся только при закрытии соединения.
        let _ = (&watch).read(&mut [0]);
        if !watch_done.load(Ordering::SeqCst) {
            kill_tree(pid);
        }
    });

    let status = child.wait()?;
    done.store(true, Ordering::SeqCst);
    let _ = tail_rx.recv_timeout(TAIL);
    finish(&out, &[], exit_code(status))
}

fn finish(out: &Mutex<Option<TcpStream>>, msg: &[u8], code: i32) -> io::Result<()> {
    let Some(mut stream) = out.lock().unwrap().take() else {
        return Ok(());
    };
    if !msg.is_empty() {
        write_field(&mut stream, msg)?;
    }
    write_field(&mut stream, &[])?;
    stream.write_all(&code.to_le_bytes())
}

fn read_field(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FIELD {
        return Err(io::Error::other("слишком длинное поле"));
    }
    let mut buf = vec![0; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_field(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)
}

fn state_path() -> io::Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).ok_or_else(|| io::Error::other(format!("не задан {var}")))?;
    Ok(PathBuf::from(home).join(".ferry").join("desk"))
}

fn write_state(text: &str) -> io::Result<()> {
    let path = state_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // На Windows профиль и так доступен только владельцу.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(text.as_bytes())
}

// ponytail: токен из ключей SipHash, которые std берёт у ОС, а не из CSPRNG; для сокета
// на 127.0.0.1 этого хватает, понадобится строже — крейт getrandom.
fn new_token() -> String {
    let state = RandomState::new();
    (0..2u64)
        .map(|i| format!("{:016x}", state.hash_one(i)))
        .collect()
}

fn shell(cmd: &str) -> Command {
    let mut command = Command::new(shell_path());
    command.args(["-lc", cmd]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Без CHERE_INVOKING login-профиль Git Bash уходит в $HOME. Окно консоли команде не
        // нужно; GUI-программы, которые она запустит, это не затрагивает.
        command
            .env("CHERE_INVOKING", "1")
            .creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Своя группа процессов — чтобы при обрыве убить всё дерево разом.
        command.process_group(0);
    }
    command
}

fn shell_path() -> OsString {
    if let Some(shell) = std::env::var_os("FERRY_SHELL") {
        return shell;
    }
    if cfg!(windows) {
        r"C:\Program Files\Git\bin\bash.exe".into()
    } else {
        std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())
    }
}

// ponytail: taskkill /t идёт по ссылкам на родителя, поэтому процессы, чей родитель уже
// вышел, переживут обрыв; если начнут мешать — Job Object с KILL_ON_JOB_CLOSE.
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/t", "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .status();
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(255)
}

/// Планировщик запускает консольный exe с окном консоли — отцепляемся, и окно закрывается.
#[cfg(windows)]
fn detach_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    unsafe { FreeConsole() };
}

#[cfg(unix)]
fn detach_console() {}
