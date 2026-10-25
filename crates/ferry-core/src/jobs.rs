//! Фоновые задачи на узле: команда на рабочем столе, которая живёт дольше вызова, — запущенное
//! приложение, dev-сервер, долгая сборка.
//!
//! `ferry job <имя> <команда>` (через `ferry desk`, то есть на рабочем столе) поднимает
//! отдельный процесс-надсмотрщик `ferry job-supervise <имя>` и сразу выходит. Надсмотрщик
//! запускает команду с выводом в лог, ждёт её и записывает код выхода. Поэтому задача
//! переживает и обрыв ssh, и перезапуск демона (`ferry update`, `ferry restart`).
//!
//! Всё состояние — файлы в `~/.ferry/jobs/<имя>/`: `cmd`, `cwd`, `started`, `pid`
//! (надсмотрщик), `child` (команда), `log`, `exit`, `killed`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::desk::{Shell, kill_tree, shell_with};
use crate::{exit_code, now};

mod result;
pub use result::{DEFAULT_BYTES, JobResult, MAX_BYTES, result_in};

/// На Windows установщик завершает ferry-app.exe и заменяет файлы приложения.
/// Надсмотрщик и его DLL живут отдельно: запущенную задачу обновление не трогает.
pub fn prepare_supervisor(state: &Path, source: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let root = state.join("runners");
        if source.starts_with(&root) && source.file_name().is_some_and(|n| n == "ferry-job.exe") {
            return Ok(source.to_path_buf());
        }
        cache_supervisor(&root, source)
    }
    #[cfg(not(windows))]
    {
        let _ = state;
        Ok(source.to_path_buf())
    }
}

#[cfg(any(windows, test))]
fn cache_supervisor(root: &Path, source: &Path) -> io::Result<PathBuf> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut files = vec![("ferry-job.exe", fs::read(source)?)];
    // GNU-сборка приложения загружает эту DLL ещё до разбора job-supervise.
    let dll = source.with_file_name("WebView2Loader.dll");
    match fs::read(dll) {
        Ok(bytes) => files.push(("WebView2Loader.dll", bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut hash = DefaultHasher::new();
    files.hash(&mut hash);
    let dir = root.join(format!("{:016x}", hash.finish()));
    fs::create_dir_all(&dir)?;
    for (name, bytes) in files {
        let path = dir.join(name);
        match fs::read(&path) {
            // Хеш — только имя кеша; перед запуском сверяем все байты.
            Ok(saved) if saved == bytes => continue,
            Ok(_) => {
                return Err(io::Error::other(format!(
                    "повреждён надсмотрщик: {}",
                    path.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Err(e) = crate::write_private_bytes(&path, &bytes) {
            // Другой запуск мог уже положить тот же файл и запустить его: Windows не даст
            // заменить работающий exe, но готовая побайтно совпадающая копия нам подходит.
            if fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                return Err(e);
            }
        }
    }
    // shortcut: старые сборки остаются в кеше; удалять можно лишь после завершения всех
    // использующих их задач, включая готовящиеся к запуску в другом процессе.
    Ok(dir.join("ferry-job.exe"))
}

/// Запустить задачу; задача с тем же именем, если ещё работает, перезапускается.
pub fn start(name: &str, cmd: &str) -> io::Result<i32> {
    let exe = std::env::current_exe()?;
    println!(
        "{}",
        start_in(name, cmd, &std::env::current_dir()?, &[], &exe)?
    );
    Ok(0)
}

/// Запустить задачу в каталоге `cwd` с добавкой к окружению. `supervisor` — бинарник, который
/// понимает `job-supervise <имя>`: `ferry` или приложение.
pub fn start_in(
    name: &str,
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    supervisor: &Path,
) -> io::Result<String> {
    start_in_shell(name, cmd, cwd, env, supervisor, Shell::Default)
}

pub fn start_in_shell(
    name: &str,
    cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    supervisor: &Path,
    shell: Shell,
) -> io::Result<String> {
    // Отклонить неподдерживаемую оболочку до остановки существующей задачи.
    shell_with(cmd, shell)?;
    let dir = dir(name)?;
    let supervisor = prepare_supervisor(&crate::ferry_home()?, supervisor)?;
    if running(&dir) {
        stop_job(name)?;
    }
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("run-id"), result::new_run_id())?;
    fs::write(dir.join("cmd"), cmd)?;
    fs::write(dir.join("shell"), serde_json::to_vec(&shell)?)?;
    fs::write(dir.join("cwd"), cwd.to_string_lossy().as_bytes())?;
    fs::write(dir.join("started"), now().to_string())?;
    fs::File::create(dir.join("log"))?;

    let mut supervisor = Command::new(supervisor);
    supervisor
        .args(["job-supervise", name])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // После переноса exe в runners его собственный каталог больше не содержит CLI.
    crate::desk::ferry_path(&mut supervisor);
    detach(&mut supervisor);
    let pid = supervisor.spawn()?.id();
    fs::write(dir.join("pid"), pid.to_string())?;
    Ok(format!("ferry: задача {name} запущена"))
}

/// `ferry job-supervise <имя>`: запустить команду задачи, дождаться, записать код выхода.
pub fn supervise(name: &str) -> io::Result<i32> {
    let dir = dir(name)?;
    // Держать до конца: по ней [`running`] знает, что надсмотрщик жив.
    let lock = fs::File::create(dir.join("lock"))?;
    lock.lock()?;
    let cmd = fs::read_to_string(dir.join("cmd"))?;
    let cwd = fs::read_to_string(dir.join("cwd"))?;
    // Лог создан пустым при запуске, пишет только надсмотрщик — хватит обычной записи.
    // Дозапись не годится: на Windows такой дескриптор без FILE_WRITE_DATA, и MSYS-bash
    // в него молча не пишет.
    let log = fs::OpenOptions::new().write(true).open(dir.join("log"))?;
    let choice = match fs::read(dir.join("shell")) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Shell::Default,
        Err(e) => return Err(e),
    };
    let mut command = shell_with(&cmd, choice)?;
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let code = match command.spawn() {
        Ok(mut child) => {
            let _tree = crate::desk::Tree::track(child.id());
            fs::write(dir.join("child"), child.id().to_string())?;
            exit_code(child.wait()?)
        }
        Err(e) => {
            writeln!(&log, "ferry: не удалось запустить: {e}")?;
            127
        }
    };
    crate::write_private(&dir.join("exit"), &code.to_string())?;
    drop(lock);
    Ok(code)
}

/// Задача узла — для `ferry jobs` и приложения.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub name: String,
    /// `running`, `exit:<код>`, `killed` или `lost` (надсмотрщик умер, не записав код).
    pub state: String,
    /// Когда запущена, секунды Unix.
    pub started: Option<u64>,
    /// Первая строка команды.
    pub cmd: String,
}

impl Job {
    pub fn label(&self) -> String {
        label(&self.state)
    }

    pub fn running(&self) -> bool {
        self.state == "running"
    }

    pub fn ago(&self) -> String {
        self.started
            .map(|t| ago(now().saturating_sub(t)))
            .unwrap_or_default()
    }
}

/// Задачи этой машины по имени.
pub fn rows() -> io::Result<Vec<Job>> {
    let mut names: Vec<String> = match fs::read_dir(crate::ferry_home()?.join("jobs")) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    let mut jobs = Vec::new();
    for name in names {
        let dir = dir(&name)?;
        let cmd = read(&dir, "cmd").unwrap_or_default();
        jobs.push(Job {
            state: code(&dir),
            started: read(&dir, "started").and_then(|t| t.parse().ok()),
            cmd: cmd.lines().next().unwrap_or_default().replace('\t', " "),
            name,
        });
    }
    Ok(jobs)
}

/// `ferry jobs [--tsv]`: таблица для человека или строки `имя\tсостояние\tзапущена\tкоманда`.
pub fn list(tsv: bool) -> io::Result<i32> {
    let jobs = rows()?;
    if tsv {
        for job in jobs {
            let started = job.started.map(|s| s.to_string()).unwrap_or_default();
            println!("{}\t{}\t{started}\t{}", job.name, job.state, job.cmd);
        }
        return Ok(0);
    }
    if jobs.is_empty() {
        eprintln!("ferry: задач нет");
    }
    for job in jobs {
        let cmd: String = job.cmd.chars().take(70).collect();
        println!(
            "{:<14} {:<22} {:<12} {cmd}",
            job.name,
            job.label(),
            job.ago()
        );
    }
    Ok(0)
}

pub fn parse_tsv(text: &str) -> Vec<Job> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            Some(Job {
                name: fields.next()?.to_string(),
                state: fields.next()?.to_string(),
                started: fields.next()?.parse().ok(),
                cmd: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Лог задачи; с `follow` — дописывать новое, пока задача работает, и выйти с её кодом.
pub fn log(name: &str, follow: bool) -> io::Result<i32> {
    let dir = existing(name)?;
    let mut file = fs::File::open(dir.join("log"))?;
    let mut out = io::stdout().lock();
    loop {
        // Ещё работает — смотрим до чтения: строки, дописанные перед выходом, тоже попадут.
        let alive = running(&dir);
        io::copy(&mut file, &mut out)?;
        out.flush()?;
        if !follow {
            return Ok(0);
        }
        if !alive {
            eprintln!("ferry: задача {name}: {}", state(&dir));
            let code = read(&dir, "exit").and_then(|c| c.parse().ok());
            return Ok(code.unwrap_or(1));
        }
        thread::sleep(Duration::from_millis(300));
    }
}

pub fn kill(name: &str) -> io::Result<i32> {
    eprintln!("{}", stop_job(name)?);
    Ok(0)
}

/// Остановить задачу; ответ — что вышло, для человека.
pub fn stop_job(name: &str) -> io::Result<String> {
    let dir = existing(name)?;
    if !running(&dir) {
        return Ok(format!(
            "ferry: задача {name} уже не работает: {}",
            state(&dir)
        ));
    }
    fs::write(dir.join("killed"), "")?;
    // Команда в своей группе (unix) или дереве (Windows); надсмотрщик запишет её код.
    // Если надсмотрщик ещё не успел её запустить — гасим его самого.
    let pid = read(&dir, "child").or_else(|| read(&dir, "pid"));
    // Код taskkill не показатель: в дереве вроде Chrome часть детей умирает сама, пока его
    // обходят, и он отвечает «не всё нашлось». Решает то, работает ли задача после.
    let killed = pid.and_then(|p| p.parse().ok()).map(kill_tree);
    for _ in 0..50 {
        if !running(&dir) {
            return Ok(format!("ferry: задача {name} остановлена"));
        }
        thread::sleep(Duration::from_millis(100));
    }
    let why = match killed {
        Some(Err(e)) => e.to_string(),
        _ => "всё ещё работает".into(),
    };
    Err(io::Error::other(format!(
        "задача {name} не остановилась: {why}"
    )))
}

/// Остановить все работающие задачи — часть `ferry stop`.
pub fn kill_all() -> io::Result<usize> {
    let Ok(entries) = fs::read_dir(crate::ferry_home()?.join("jobs")) else {
        return Ok(0);
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        if running(&entry.path()) {
            stop_job(&entry.file_name().to_string_lossy())?;
            killed += 1;
        }
    }
    Ok(killed)
}

/// Лог задачи и её состояние: работает ли, код выхода, подпись — для слежения за логом.
pub fn log_file(name: &str) -> io::Result<PathBuf> {
    Ok(existing(name)?.join("log"))
}

pub fn progress(name: &str) -> io::Result<(bool, Option<i32>, String)> {
    let dir = existing(name)?;
    let exit = read(&dir, "exit").and_then(|c| c.parse().ok());
    Ok((running(&dir), exit, state(&dir)))
}

fn state(dir: &Path) -> String {
    label(&code(dir))
}

fn code(dir: &Path) -> String {
    match (read(dir, "killed"), read(dir, "exit")) {
        (Some(_), _) => "killed".into(),
        (None, Some(code)) => format!("exit:{code}"),
        (None, None) if running(dir) => "running".into(),
        (None, None) => "lost".into(),
    }
}

fn label(code: &str) -> String {
    match code {
        "running" => "работает".into(),
        "killed" => "остановлена".into(),
        // Надсмотрщик умер, не записав код: выход из системы, его убили вручную.
        "lost" => "пропала".into(),
        other => match other.strip_prefix("exit:") {
            Some(code) => format!("завершилась, код {code}"),
            None => other.into(),
        },
    }
}

/// Работает ли задача. Надсмотрщик всю жизнь держит блокировку файла `lock`, и ОС снимает её
/// с его смертью — так его не спутать с чужим процессом, которому достался тот же PID
/// (надсмотрщика убили вместе с приложением при обновлении). Пока надсмотрщик жив, и PID
/// команды не переходит другому: он держит её процесс.
fn running(dir: &Path) -> bool {
    if dir.join("exit").exists() {
        return false;
    }
    match fs::File::open(dir.join("lock")) {
        Ok(lock) => match lock.try_lock() {
            Ok(()) => false,
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(_)) => false,
        },
        // ponytail: блокировки нет у задач версий до 0.1.11 и у только что запущенной — тогда по
        // PID, но только если это процесс ferry; чужой процесс с тем же PID задачей не считается.
        Err(_) => read(dir, "pid")
            .and_then(|p| p.parse().ok())
            .is_some_and(ferry_process),
    }
}

/// Есть ли процесс `pid` и это ли ferry (`ferry` или `ferry-app` — они и бывают надсмотрщиками).
fn ferry_process(pid: u32) -> bool {
    process_name(pid)
        .is_some_and(|name| matches!(name.as_str(), "ferry" | "ferry-app" | "ferry-job"))
}

/// Имя программы процесса: без пути и `.exe`, в нижнем регистре.
#[cfg(windows)]
fn process_name(pid: u32) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new("tasklist")
        .args(["/fi", &format!("PID eq {pid}"), "/fo", "csv", "/nh"])
        .creation_flags(crate::desk::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    // `"ferry-app.exe","1234",…`; нет такого — строка-сообщение без кавычек.
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains(&format!("\"{pid}\"")))?;
    let name = line.split("\",\"").next()?.trim_start_matches('"');
    Some(name.to_lowercase().trim_end_matches(".exe").to_string())
}

#[cfg(unix)]
fn process_name(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let name = comm.rsplit('/').next()?.to_lowercase();
    (!name.is_empty()).then_some(name)
}

/// Надсмотрщик — сам по себе: своя группа процессов, своя скрытая консоль на Windows.
/// И без унаследованных дескрипторов `ferry desk`: иначе вызов desk ждал бы его конца.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        unsafe { SetHandleInformation(GetStdHandle(which as u32), HANDLE_FLAG_INHERIT, 0) };
    }
    command.creation_flags(crate::desk::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    std::os::unix::process::CommandExt::process_group(command, 0);
}

fn dir(name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    Ok(crate::ferry_home()?.join("jobs").join(name))
}

fn validate_name(name: &str) -> io::Result<()> {
    let alias = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    // Имена переносимы на Windows; точка в конце и имена устройств там имеют особый смысл.
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix)
                .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        });
    if name.is_empty()
        || name.starts_with('-')
        || name.ends_with('.')
        || !name.chars().all(alias)
        || reserved
    {
        return Err(io::Error::other(format!(
            "недопустимое имя задачи: {name:?}"
        )));
    }
    Ok(())
}

fn existing(name: &str) -> io::Result<PathBuf> {
    let dir = dir(name)?;
    if dir.join("cmd").exists() {
        return Ok(dir);
    }
    Err(io::Error::other(format!(
        "нет задачи {name:?}; список — ferry jobs"
    )))
}

fn read(dir: &Path, file: &str) -> Option<String> {
    fs::read_to_string(dir.join(file))
        .ok()
        .map(|s| s.trim().to_string())
}

pub(crate) fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} с назад"),
        60..3600 => format!("{} мин назад", secs / 60),
        3600..172_800 => format!("{} ч назад", secs / 3600),
        _ => format!("{} дн назад", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisors_keep_their_build_and_verify_cached_bytes() {
        let root = std::env::temp_dir().join(format!("ferry-runners-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("ferry-app.exe");
        let dll = root.join("WebView2Loader.dll");
        let cache = root.join("runners");
        fs::write(&source, b"old exe").unwrap();
        fs::write(&dll, b"old dll").unwrap();
        let old = cache_supervisor(&cache, &source).unwrap();
        assert_eq!(cache_supervisor(&cache, &source).unwrap(), old);
        fs::write(&source, b"new exe").unwrap();
        let new = cache_supervisor(&cache, &source).unwrap();
        assert_ne!(new, old);
        fs::write(&dll, b"new dll").unwrap();
        let newer = cache_supervisor(&cache, &source).unwrap();
        assert_ne!(newer, new);
        fs::remove_file(&source).unwrap();
        fs::remove_file(&dll).unwrap();
        assert_eq!(fs::read(&old).unwrap(), b"old exe");
        assert_eq!(
            fs::read(old.with_file_name("WebView2Loader.dll")).unwrap(),
            b"old dll"
        );
        fs::write(&source, b"new exe").unwrap();
        fs::write(&dll, b"new dll").unwrap();
        fs::write(&newer, b"broken").unwrap();
        assert!(cache_supervisor(&cache, &source).is_err());
        assert_eq!(fs::read(&newer).unwrap(), b"broken");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tsv_round_trips_jobs() {
        let text = "app\trunning\t1792300000\t./app.exe --port 9322\nbuild\texit:3\t1792300100\tcargo build\nold\tkilled\t\tsleep 300\n";
        let jobs = parse_tsv(text);
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].cmd, "./app.exe --port 9322");
        assert!(jobs[0].running());
        assert_eq!(jobs[1].label(), "завершилась, код 3");
        assert_eq!(
            (jobs[2].started, jobs[2].label().as_str()),
            (None, "остановлена")
        );
    }
}
