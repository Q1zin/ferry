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
//! Пустая команда — `ferry stop`: демон убивает деревья всех команд, которые сейчас выполняет.
//!
//! Клиент после запроса ничего не шлёт. Если соединение закрылось раньше, чем команда
//! завершилась, демон убивает её дерево процессов.

use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::exit_code;

/// pid команд, которые демон выполняет прямо сейчас, — для `ferry stop`.
type Jobs = Arc<Mutex<HashSet<u32>>>;

/// Предел поля запроса: мусор на порту не должен заставить демон выделить гигабайты.
const MAX_FIELD: u32 = 1 << 20;

/// Сколько ждать хвост вывода после выхода команды. Запущенные ею фоновые процессы держат
/// pipe открытым (как и у ssh), их конца не ждём.
const TAIL: Duration = Duration::from_millis(500);

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn daemon() -> io::Result<i32> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let token = new_token();
    write_state(&format!("{} {token}\n", listener.local_addr()?.port()))?;
    detach_console();
    let token: Arc<str> = token.into();
    let jobs = Jobs::default();
    let accept = move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (token, jobs) = (token.clone(), jobs.clone());
            thread::spawn(move || {
                let _ = serve(stream, &token, &jobs);
            });
        }
    };
    // На Windows главный поток отдаём иконке в трее (её цикл сообщений), команды — в фоне.
    if cfg!(windows) {
        thread::spawn(accept);
        crate::tray::run();
    } else {
        accept();
    }
    Ok(0)
}

/// Узловой `ferry stop`: фоновые задачи и команды, которые сейчас выполняет демон.
pub fn stop() -> io::Result<i32> {
    let jobs = crate::jobs::kill_all()?;
    if jobs > 0 {
        println!("ferry: остановлено задач: {jobs}");
    }
    run("")
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

    // Обрыв ssh гасит команду не отсюда: когда хабовый ssh умирает, sshd на узле замечает
    // мёртвого клиента (ClientAliveInterval, ставит установщик) и сносит сессию — этот
    // процесс умирает, сокет демона закрывается, и демон убивает дерево команды.
    // Следить за EOF на stdin нельзя: при `ssh host 'ferry desk ...' <<heredoc` или с любым
    // конечным stdin он закроется, пока команда ещё работает, — как и обычный ssh, концом
    // stdin команду не прерываем.
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

fn serve(stream: TcpStream, token: &str, jobs: &Jobs) -> io::Result<()> {
    let mut input = &stream;
    if read_field(&mut input)? != token.as_bytes() {
        return Ok(());
    }
    let cwd = String::from_utf8_lossy(&read_field(&mut input)?).into_owned();
    let cmd = String::from_utf8_lossy(&read_field(&mut input)?).into_owned();

    // Ответ пишут двое — насос вывода и итоговый код; `None` — ответ уже закрыт.
    let out = Arc::new(Mutex::new(Some(stream.try_clone()?)));

    if cmd.is_empty() {
        let pids: Vec<u32> = jobs.lock().unwrap().iter().copied().collect();
        let errors: Vec<_> = pids.iter().map(|&pid| (pid, kill_tree(pid))).collect();
        // Код taskkill не показатель (часть дерева умирает сама, пока его обходят) — провал
        // только у тех, кто через пару секунд всё ещё числится выполняемым.
        for _ in 0..20 {
            if pids.iter().all(|pid| !jobs.lock().unwrap().contains(pid)) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let running = jobs.lock().unwrap().clone();
        let failed: Vec<String> = errors
            .into_iter()
            .filter(|(pid, _)| running.contains(pid))
            .map(|(pid, result)| match result {
                Err(e) => format!("{pid}: {e}"),
                Ok(()) => format!("{pid}: всё ещё работает"),
            })
            .collect();
        if failed.is_empty() {
            let msg = format!("ferry: остановлено команд: {}\n", pids.len());
            return finish(&out, msg.as_bytes(), 0);
        }
        let msg = format!(
            "ferry: остановлено {} из {}; не удалось — {}\n",
            pids.len() - failed.len(),
            pids.len(),
            failed.join(", ")
        );
        return finish(&out, msg.as_bytes(), 1);
    }

    let mode = crate::mode::current();
    if !mode.allows_desktop() {
        let msg = format!(
            "ferry: рабочий стол сейчас трогать нельзя — на узле режим «{}»\n",
            mode.label()
        );
        return finish(&out, msg.as_bytes(), crate::mode::REFUSED);
    }

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
    jobs.lock().unwrap().insert(pid);
    thread::spawn(move || {
        // Клиент больше ничего не шлёт: чтение вернётся только при закрытии соединения.
        let _ = (&watch).read(&mut [0]);
        if !watch_done.load(Ordering::SeqCst) {
            let _ = kill_tree(pid);
        }
    });

    let status = child.wait();
    // Сразу после выхода: иначе stop мог бы убить чужой процесс, получивший тот же pid.
    jobs.lock().unwrap().remove(&pid);
    done.store(true, Ordering::SeqCst);
    let status = status?;
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

pub fn read_field(r: &mut impl Read) -> io::Result<Vec<u8>> {
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

pub fn write_field(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes())?;
    w.write_all(data)
}

fn state_path() -> io::Result<PathBuf> {
    Ok(crate::ferry_home()?.join("desk"))
}

fn write_state(text: &str) -> io::Result<()> {
    let path = state_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    crate::write_private(&path, text)
}

// ponytail: токен из ключей SipHash, которые std берёт у ОС, а не из CSPRNG; для сокета
// на 127.0.0.1 этого хватает, понадобится строже — крейт getrandom.
pub(crate) fn new_token() -> String {
    let state = RandomState::new();
    (0..2u64)
        .map(|i| format!("{:016x}", state.hash_one(i)))
        .collect()
}

pub fn shell(cmd: &str) -> Command {
    let mut command = Command::new(shell_path());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Без login, как и sshd: login-шелл Git Bash на `exit` печатает clear из
        // /etc/bash.bash_logout. Окно консоли команде не нужно; GUI-программы, которые она
        // запустит, это не затрагивает.
        command.args(["-c", cmd]).creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Login-шелл: из него PATH с brew и прочим из профиля. Своя группа процессов — чтобы
        // при обрыве убить всё дерево разом.
        command.args(["-lc", cmd]).process_group(0);
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
pub fn kill_tree(pid: u32) -> io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("taskkill");
        command
            .args(["/pid", &pid.to_string(), "/t", "/f"])
            .creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("kill");
        command.args(["-TERM", "--", &format!("-{pid}")]);
        command
    };
    // Все три потока — явно: после FreeConsole у демона нет годных std-дескрипторов, и
    // унаследованный stdin валит spawn с «неверный дескриптор».
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(status.to_string()))
    }
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
