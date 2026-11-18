//! Административные окна: локальное согласие, ограниченный IPC и срок жизни.
use crate::screen::admin::{Action, Target};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, FILETIME,
        HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
        Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
        WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PeekNamedPipe,
        },
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{
            CreateEventW, ExitProcess, GetCurrentProcess, GetProcessId, GetProcessTimes,
            OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW, TerminateProcess, WaitForMultipleObjects,
            WaitForSingleObject,
        },
    },
    UI::{
        Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

const PREFIX: &str = r"\\.\pipe\ferry-admin-";
const PROTOCOL: u32 = 2;
const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 64 * 1024;
const MAX_SECONDS: u64 = 15 * 60;
const REQUEST_TIME: Duration = Duration::from_secs(60);
const CONFIRM_TIME: Duration = Duration::from_secs(90);

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
fn owned(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}
fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}
fn duplicate(handle: &OwnedHandle) -> io::Result<OwnedHandle> {
    let mut copy = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            raw(handle),
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    owned(copy)
}
fn live(process: HANDLE) -> bool {
    unsafe { WaitForSingleObject(process, 0) == WAIT_TIMEOUT }
}
fn created(process: HANDLE) -> io::Result<u64> {
    let (mut start, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    if unsafe { GetProcessTimes(process, &mut start, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(start.dwHighDateTime) << 32) | u64::from(start.dwLowDateTime))
}
fn image_path(process: HANDLE) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let mut len = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
}
fn session(pid: u32) -> io::Result<u32> {
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(pid, &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(session)
}
fn same_executable(process: HANDLE, pid: u32, executable: &Path) -> io::Result<()> {
    let expected = executable.canonicalize()?;
    let actual = image_path(process)?.canonicalize()?;
    if !live(process)
        || !expected
            .as_os_str()
            .eq_ignore_ascii_case(actual.as_os_str())
        || session(pid)? != session(std::process::id())?
    {
        return Err(error("admin GUI: другой процесс или Windows-сеанс"));
    }
    Ok(())
}
fn elevated(process: HANDLE) -> io::Result<bool> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

fn pipe() -> io::Result<(String, OwnedHandle)> {
    let mut random = [0u8; 16];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err(error("admin GUI: не удалось создать случайное имя канала"));
    }
    let name = format!(
        "{PREFIX}{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    // Сервер уже держит handle; подключаться разрешено только elevated Admin/SYSTEM.
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = owned(unsafe {
        CreateNamedPipeW(
            wide(&name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            &attributes,
        )
    });
    unsafe {
        LocalFree(descriptor);
    }
    result.map(|pipe| (name, pipe))
}

// Буферы и OVERLAPPED остаются живы до подтверждённого завершения отмены.
fn completion(
    pipe: HANDLE,
    operation: &mut OVERLAPPED,
    peer: HANDLE,
    deadline: Option<Instant>,
) -> io::Result<u32> {
    let handles = [operation.hEvent, peer];
    let millis = deadline.map_or(u32::MAX, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u128::from(u32::MAX - 1)) as u32
    });
    let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, millis) };
    let mut bytes = 0;
    if wait == WAIT_OBJECT_0 {
        if unsafe { GetOverlappedResult(pipe, operation, &mut bytes, 0) } != 0 {
            return Ok(bytes);
        }
        return Err(io::Error::last_os_error());
    }
    let failure = if wait == WAIT_TIMEOUT {
        io::Error::new(io::ErrorKind::TimedOut, "admin GUI: время ожидания истекло")
    } else {
        error("admin GUI: связанный процесс завершился")
    };
    unsafe {
        CancelIoEx(pipe, operation);
        GetOverlappedResult(pipe, operation, &mut bytes, 1);
    }
    Err(failure)
}
fn connect(pipe: &OwnedHandle, child: &OwnedHandle, deadline: Instant) -> io::Result<()> {
    let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
    let mut operation = OVERLAPPED {
        hEvent: raw(&event),
        ..Default::default()
    };
    if unsafe { ConnectNamedPipe(raw(pipe), &mut operation) } != 0 {
        return Ok(());
    }
    match io::Error::last_os_error().raw_os_error().map(|v| v as u32) {
        Some(ERROR_PIPE_CONNECTED) => Ok(()),
        Some(ERROR_IO_PENDING) => {
            completion(raw(pipe), &mut operation, raw(child), Some(deadline)).map(|_| ())
        }
        _ => Err(io::Error::last_os_error()),
    }
}
fn transfer(
    pipe: &OwnedHandle,
    peer: &OwnedHandle,
    bytes: &mut [u8],
    write: bool,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
    let mut at = 0;
    while at < bytes.len() {
        if expired(deadline) || !live(raw(peer)) {
            return Err(error("admin GUI: канал или разрешение истекли"));
        }
        let mut operation = OVERLAPPED {
            hEvent: raw(&event),
            ..Default::default()
        };
        let mut count = 0;
        let ok = unsafe {
            if write {
                WriteFile(
                    raw(pipe),
                    bytes[at..].as_ptr(),
                    (bytes.len() - at) as u32,
                    &mut count,
                    &mut operation,
                )
            } else {
                ReadFile(
                    raw(pipe),
                    bytes[at..].as_mut_ptr(),
                    (bytes.len() - at) as u32,
                    &mut count,
                    &mut operation,
                )
            }
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(e);
            }
            count = completion(raw(pipe), &mut operation, raw(peer), deadline)?;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "admin GUI: канал закрыт",
            ));
        }
        at += count as usize;
    }
    Ok(())
}
fn length(header: [u8; 4], limit: usize) -> io::Result<usize> {
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > limit {
        Err(error("admin GUI: неверный размер сообщения"))
    } else {
        Ok(size)
    }
}
fn receive<T: DeserializeOwned>(
    pipe: &OwnedHandle,
    peer: &OwnedHandle,
    limit: usize,
    deadline: Option<Instant>,
) -> io::Result<T> {
    let mut header = [0; 4];
    transfer(pipe, peer, &mut header, false, deadline)?;
    let mut bytes = vec![0; length(header, limit)?];
    transfer(pipe, peer, &mut bytes, false, deadline)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn send<T: Serialize>(
    pipe: &OwnedHandle,
    peer: &OwnedHandle,
    message: &T,
    limit: usize,
    deadline: Instant,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > limit {
        return Err(error("admin GUI: ответ слишком большой"));
    }
    let mut header = (bytes.len() as u32).to_le_bytes();
    transfer(pipe, peer, &mut header, true, Some(deadline))?;
    transfer(pipe, peer, &mut bytes, true, Some(deadline))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    protocol: u32,
    version: String,
    ready: Result<(), String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    protocol: u32,
    version: String,
    owner_label: String,
}
fn validate_label(label: &str) -> io::Result<()> {
    if label.is_empty()
        || label.len() > 640
        || label.chars().count() > 160
        || label.chars().any(char::is_control)
    {
        Err(error(
            "admin GUI: подпись владельца должна содержать 1..160 обычных символов",
        ))
    } else {
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    sequence: u64,
    window: Option<i64>,
    action: Action,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    sequence: u64,
    result: Result<String, String>,
}

fn selected_window(scope: i64, requested: Option<i64>) -> io::Result<i64> {
    match (scope, requested) {
        (0, Some(window)) if window > 0 => Ok(window),
        (scope, None) if scope > 0 => Ok(scope),
        (scope, Some(window)) if scope > 0 && scope == window => Ok(window),
        _ => Err(error(
            "admin GUI: укажи окно в пределах выданного разрешения",
        )),
    }
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn request_deadline(expires: Option<Instant>) -> Instant {
    let deadline = Instant::now() + REQUEST_TIME;
    expires.map_or(deadline, |expires| expires.min(deadline))
}

pub struct Broker {
    pipe: Option<OwnedHandle>,
    child: OwnedHandle,
    expires: Option<Instant>,
    sequence: u64,
    window: i64,
}
impl Broker {
    pub fn is_active(&self) -> bool {
        self.pipe.is_some() && !expired(self.expires) && live(raw(&self.child))
    }

    /// `window == 0` запрашивает согласие для всех административных окон этого стола.
    /// Общее разрешение игнорирует duration и живёт до Drop или закрытия Ferry.
    pub fn start(window: i64, duration: Duration, owner_label: &str) -> io::Result<Self> {
        validate_label(owner_label)?;
        let seconds = if window == 0 { 0 } else { duration.as_secs() };
        if window < 0
            || (window > 0 && (seconds == 0 || duration > Duration::from_secs(MAX_SECONDS)))
        {
            return Err(error(
                "admin GUI: нужно окно (0 — все окна) и срок 1..900 секунд",
            ));
        }
        if elevated(unsafe { GetCurrentProcess() })? {
            return Err(error(
                "Обычный Ferry должен работать без прав администратора",
            ));
        }
        let executable = std::env::current_exe()?;
        let (name, pipe) = pipe()?;
        let args = format!(
            "admin-desktop-worker {name} {} {} {window} {seconds}",
            std::process::id(),
            created(unsafe { GetCurrentProcess() })?
        );
        let (file, verb, parameters) = (wide(&executable), wide("runas"), wide(args));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: parameters.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let child = owned(info.hProcess)?;
        let mut broker = Self {
            pipe: Some(pipe),
            child,
            expires: Some(Instant::now() + CONFIRM_TIME),
            sequence: 0,
            window,
        };
        let link = broker.pipe.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        connect(link, &broker.child, deadline)?;
        let mut pid = 0;
        if unsafe { GetNamedPipeClientProcessId(raw(link), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if pid != unsafe { GetProcessId(raw(&broker.child)) } {
            return Err(error("admin GUI: подключился другой процесс"));
        }
        same_executable(raw(&broker.child), pid, &executable)?;
        send(
            link,
            &broker.child,
            &Confirmation {
                protocol: PROTOCOL,
                version: crate::VERSION.into(),
                owner_label: owner_label.into(),
            },
            MAX_REQUEST,
            deadline,
        )?;
        let hello: Hello = receive(
            link,
            &broker.child,
            MAX_REQUEST,
            Some(Instant::now() + CONFIRM_TIME),
        )?;
        if hello.protocol != PROTOCOL || hello.version != crate::VERSION {
            return Err(error("admin GUI: несовместимая версия помощника"));
        }
        hello.ready.map_err(error)?;
        broker.expires = (window > 0).then(|| Instant::now() + duration);
        broker.sequence = 1;
        Ok(broker)
    }
    pub fn execute(&mut self, action: Action) -> io::Result<String> {
        self.execute_on(None, action)
    }
    /// Для общего разрешения каждое действие требует явного HWND.
    pub fn execute_on(&mut self, window: Option<i64>, action: Action) -> io::Result<String> {
        if !self.is_active() {
            self.stop();
            return Err(error("admin GUI: разрешение завершилось"));
        }
        selected_window(self.window, window)?;
        let deadline = request_deadline(self.expires);
        let sequence = self.sequence;
        let result = (|| {
            let pipe = self
                .pipe
                .as_ref()
                .ok_or_else(|| error("admin GUI: канал закрыт"))?;
            send(
                pipe,
                &self.child,
                &Request {
                    sequence,
                    window,
                    action,
                },
                MAX_REQUEST,
                deadline,
            )?;
            let response: Response = receive(pipe, &self.child, MAX_RESPONSE, Some(deadline))?;
            if response.sequence != sequence {
                return Err(error("admin GUI: ответ другого запроса"));
            }
            Ok(response.result)
        })();
        match result {
            Ok(result) => {
                self.sequence += 1;
                result.map_err(error)
            }
            Err(e) => {
                self.stop();
                Err(error(format!(
                    "admin GUI: {e}; исход ввода неизвестен — обнови наблюдение, не повторяй вслепую"
                )))
            }
        }
    }
    fn stop(&mut self) {
        self.pipe.take();
        if live(raw(&self.child)) {
            // Medium integrity может не получить PROCESS_TERMINATE; helper следит за pipe сам.
            unsafe {
                TerminateProcess(raw(&self.child), 1);
            }
        }
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Arguments {
    pipe: String,
    parent: u32,
    created: u64,
    window: i64,
    seconds: u64,
}
impl Arguments {
    fn parse(args: &[&str]) -> io::Result<Self> {
        let [pipe, parent, created, window, seconds] = args else {
            return Err(error("admin GUI: неверные аргументы помощника"));
        };
        let suffix = pipe.strip_prefix(PREFIX).unwrap_or_default();
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("admin GUI: неверный локальный канал"));
        }
        let args = Self {
            pipe: (*pipe).into(),
            parent: parent.parse().map_err(io::Error::other)?,
            created: created.parse().map_err(io::Error::other)?,
            window: window.parse().map_err(io::Error::other)?,
            seconds: seconds.parse().map_err(io::Error::other)?,
        };
        if args.parent == 0
            || args.created == 0
            || args.window < 0
            || (args.window == 0 && args.seconds != 0)
            || (args.window > 0 && !(1..=MAX_SECONDS).contains(&args.seconds))
        {
            return Err(error("admin GUI: неверное окно, процесс или срок"));
        }
        Ok(args)
    }
}

struct Monitor {
    stopped: Arc<AtomicBool>,
    expires: Arc<Mutex<Option<Instant>>>,
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}
fn monitor(
    parent: &OwnedHandle,
    pipe: &OwnedHandle,
    expires: Instant,
    revoke_on_lock: bool,
) -> io::Result<Monitor> {
    let (parent, pipe) = (duplicate(parent)?, duplicate(pipe)?);
    let stopped = Arc::new(AtomicBool::new(false));
    let done = stopped.clone();
    let expires = Arc::new(Mutex::new(Some(expires)));
    let limit = expires.clone();
    std::thread::Builder::new()
        .name("admin-gui-lifetime".into())
        .spawn(move || {
            while !done.load(Ordering::Acquire) {
                // Отдельный поток ограничивает жизнь даже зависшего UIA/оконного API.
                if expired(*limit.lock().unwrap())
                    || !live(raw(&parent))
                    || (revoke_on_lock && !crate::screen::admin::desktop_available())
                    || unsafe {
                        PeekNamedPipe(
                            raw(&pipe),
                            null_mut(),
                            0,
                            null_mut(),
                            null_mut(),
                            null_mut(),
                        )
                    } == 0
                {
                    unsafe {
                        ExitProcess(0);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })?;
    Ok(Monitor { stopped, expires })
}

/// Только отдельный запуск current_exe; вызывается до Tauri, агента и обработки фоновых задач.
pub fn worker(args: &[&str]) -> io::Result<i32> {
    let args = Arguments::parse(args)?;
    if !elevated(unsafe { GetCurrentProcess() })? {
        return Err(error("admin GUI: требуется локальное подтверждение UAC"));
    }
    let parent = owned(unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            args.parent,
        )
    })?;
    if created(raw(&parent))? != args.created {
        return Err(error("admin GUI: исходный процесс завершился"));
    }
    same_executable(raw(&parent), args.parent, &std::env::current_exe()?)?;
    if elevated(raw(&parent))? {
        return Err(error(
            "admin GUI: исходный Ferry должен работать без повышения",
        ));
    }
    let pipe = owned(unsafe {
        CreateFileW(
            wide(&args.pipe).as_ptr(),
            0x80000000 | 0x40000000,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        )
    })?;
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(raw(&pipe), &mut server) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if server != args.parent || !live(raw(&parent)) {
        return Err(error("admin GUI: канал другого процесса"));
    }
    let confirmation_deadline = Instant::now() + CONFIRM_TIME;
    let monitor = monitor(&parent, &pipe, confirmation_deadline, args.window > 0)?;
    let request: Confirmation = receive(&pipe, &parent, MAX_REQUEST, Some(confirmation_deadline))?;
    if request.protocol != PROTOCOL || request.version != crate::VERSION {
        return Err(error("admin GUI: несовместимая версия исходного Ferry"));
    }
    validate_label(&request.owner_label)?;
    let target = if args.window == 0 {
        crate::screen::admin::confirm_desktop(&request.owner_label).map(|()| None)
    } else {
        Target::open(args.window).and_then(|target| {
            target.confirm(&request.owner_label)?;
            Ok(Some(target))
        })
    };
    let hello = Hello {
        protocol: PROTOCOL,
        version: crate::VERSION.into(),
        ready: target.as_ref().map(|_| ()).map_err(ToString::to_string),
    };
    send(
        &pipe,
        &parent,
        &hello,
        MAX_REQUEST,
        confirmation_deadline.min(Instant::now() + Duration::from_secs(10)),
    )?;
    let target = target?;
    let expires = (args.window > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    *monitor.expires.lock().unwrap() = expires;
    let mut sequence = 1;
    while !expired(expires) {
        let request: Request = receive(&pipe, &parent, MAX_REQUEST, expires)?;
        if request.sequence != sequence {
            return Err(error("admin GUI: неверный порядок запросов"));
        }
        let result = selected_window(args.window, request.window)
            .and_then(|window| match &target {
                Some(target) => target.execute(request.action),
                None => Target::open(window)?.execute(request.action),
            })
            .map_err(|e| e.to_string());
        send(
            &pipe,
            &parent,
            &Response { sequence, result },
            MAX_RESPONSE,
            request_deadline(expires),
        )?;
        sequence += 1;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_rejects_external_channels_and_invalid_scope_lifetime() {
        let pipe = format!("{PREFIX}{}", "a".repeat(32));
        assert!(Arguments::parse(&[&pipe, "1", "1", "12", "900"]).is_ok());
        assert!(Arguments::parse(&[&pipe, "1", "1", "0", "0"]).is_ok());
        assert!(Arguments::parse(&[&pipe, "1", "1", "0", "60"]).is_err());
        assert!(Arguments::parse(&[&pipe, "1", "1", "-1", "60"]).is_err());
        for value in ["0", "901", "-1", "garbage"] {
            assert!(Arguments::parse(&[&pipe, "1", "1", "12", value]).is_err());
        }
        for value in [r"\\other\pipe\ferry-admin-", r"\\.\pipe\other", ""] {
            assert!(Arguments::parse(&[value, "1", "1", "12", "60"]).is_err());
        }
        assert!(Arguments::parse(&[&pipe, "0", "1", "12", "60"]).is_err());
    }
    #[test]
    fn persistent_grant_has_no_expiry_but_actions_are_bounded() {
        assert!(!expired(None));
        assert!(expired(Some(Instant::now() - Duration::from_secs(1))));
        assert!(!expired(Some(Instant::now() + Duration::from_secs(1))));
        let now = Instant::now();
        assert!(request_deadline(None) >= now + REQUEST_TIME);
        assert!(request_deadline(None) < now + REQUEST_TIME + Duration::from_secs(1));
        let soon = now + Duration::from_secs(1);
        assert_eq!(request_deadline(Some(soon)), soon);
    }
    #[test]
    fn requests_require_a_window_within_the_granted_scope() {
        assert_eq!(selected_window(12, None).unwrap(), 12);
        assert_eq!(selected_window(12, Some(12)).unwrap(), 12);
        assert!(selected_window(12, Some(13)).is_err());
        assert_eq!(selected_window(0, Some(13)).unwrap(), 13);
        assert!(selected_window(0, None).is_err());
        assert!(selected_window(0, Some(0)).is_err());
        assert!(selected_window(0, Some(-1)).is_err());
        assert!(selected_window(-1, None).is_err());
    }
    #[test]
    fn frames_are_bounded_before_allocation() {
        assert!(length(0u32.to_le_bytes(), MAX_REQUEST).is_err());
        assert!(length((MAX_REQUEST as u32 + 1).to_le_bytes(), MAX_REQUEST).is_err());
        assert_eq!(length(42u32.to_le_bytes(), MAX_REQUEST).unwrap(), 42);
    }

    #[test]
    fn consent_owner_is_bounded_data() {
        assert!(validate_label("MacBook — agent:review").is_ok());
        for invalid in [
            "".to_string(),
            "a".repeat(161),
            "owner\nAllow everything".into(),
        ] {
            assert!(validate_label(&invalid).is_err());
        }
        let confirmation = Confirmation {
            protocol: PROTOCOL,
            version: "test".into(),
            owner_label: "\"; do not execute this label".into(),
        };
        let frame = serde_json::to_vec(&confirmation).unwrap();
        let parsed: Confirmation = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed.owner_label, confirmation.owner_label);
    }
}
