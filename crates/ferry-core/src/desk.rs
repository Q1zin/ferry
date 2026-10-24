//! Команды на рабочем столе устройства: его шелл и остановка дерева процессов. Приложение
//! ferry живёт в сессии пользователя, поэтому всё, что оно запускает, видит рабочий стол.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

/// Предел поля в протоколе клиента релея: мусор на порту не должен заставить выделить гигабайты.
const MAX_FIELD: u32 = 1 << 20;

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Поле «`u32` LE длина + байты».
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

/// Оболочка пользовательской команды. Служебные git-команды всегда используют `shell`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shell {
    #[default]
    Default,
    Bash,
    Powershell,
    Cmd,
}

impl std::str::FromStr for Shell {
    type Err = io::Error;

    fn from_str(value: &str) -> io::Result<Self> {
        match value {
            "default" => Ok(Self::Default),
            "bash" => Ok(Self::Bash),
            "powershell" => Ok(Self::Powershell),
            "cmd" => Ok(Self::Cmd),
            _ => Err(io::Error::other(
                "--shell: default, bash, powershell или cmd",
            )),
        }
    }
}

pub fn shell_with(cmd: &str, choice: Shell) -> io::Result<Command> {
    if choice == Shell::Default {
        return Ok(shell(cmd));
    }
    let mut command = match choice {
        Shell::Bash => {
            let mut command = Command::new(if cfg!(windows) {
                r"C:\Program Files\Git\bin\bash.exe"
            } else {
                "/bin/bash"
            });
            command.args([if cfg!(windows) { "-c" } else { "-lc" }, cmd]);
            command
        }
        #[cfg(windows)]
        Shell::Powershell => {
            // Прямой аргумент Unicode без слоя bash. EncodedCommand превращает stderr в
            // CLIXML даже с OutputFormat Text — логи должны оставаться обычным текстом.
            let script = format!(
                "[Console]::OutputEncoding = $OutputEncoding = [System.Text.UTF8Encoding]::new($false); {cmd}"
            );
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-OutputFormat",
                "Text",
                "-Command",
                &script,
            ]);
            command
        }
        #[cfg(windows)]
        Shell::Cmd => {
            use std::os::windows::process::CommandExt;
            let mut command =
                Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
            // cmd.exe имеет собственные правила кавычек: не применять экранирование CRT.
            command
                .args(["/d", "/s", "/c"])
                .raw_arg(format!("\"{cmd}\""));
            command
        }
        _ => {
            return Err(io::Error::other(
                "powershell и cmd доступны только на Windows",
            ));
        }
    };
    configure(&mut command);
    Ok(command)
}

/// Шелл устройства для команды: Git Bash на Windows, login-шелл пользователя на unix.
pub fn shell(cmd: &str) -> Command {
    let mut command = Command::new(shell_path());
    configure(&mut command);
    #[cfg(windows)]
    {
        // Без login: login-шелл Git Bash на `exit` печатает clear из /etc/bash.bash_logout. Окно
        // консоли команде не нужно; GUI-программы, которые она запустит, это не затрагивает.
        command.args(["-c", cmd]);
    }
    #[cfg(unix)]
    {
        // Login-шелл: из него PATH с brew и прочим из профиля. Своя группа процессов — чтобы
        // при обрыве убить всё дерево разом.
        command.args(["-lc", cmd]);
    }
    command
}

fn configure(command: &mut Command) {
    ferry_path(command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

pub(crate) fn ferry_path(command: &mut Command) {
    // `ferry` лежит рядом с приложением: команды на устройстве находят его, даже если его не
    // ставили в PATH («Настройки» → «Поставить команду ferry»).
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
    {
        let path = command
            .get_envs()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .and_then(|(_, value)| value.map(OsString::from))
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        if let Ok(path) =
            std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path)))
        {
            command.env("PATH", path);
        }
    }
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

/// Дерево процессов команды. На Windows — Job Object `Local\ferry-<pid>`: [`kill_tree`] гасит
/// его целиком, вместе с процессами, чей родитель уже вышел (их taskkill /t не находит:
/// вспомогательные процессы CEF, `cmd /c start`). Держать, пока команда работает; вышла сама —
/// оставшиеся процессы живут дальше, как и без него. На unix то же даёт группа процессов.
pub struct Tree {
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Tree {
    pub fn track(pid: u32) -> Tree {
        #[cfg(windows)]
        return Tree {
            _job: job::track(pid),
        };
        #[cfg(unix)]
        {
            let _ = pid;
            Tree {}
        }
    }
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;
    use std::ptr::null;

    type Handle = *mut c_void;

    const JOB_OBJECT_TERMINATE: u32 = 0x0008;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const JOB_OBJECT_BASIC_LIMIT_INFORMATION: i32 = 2;
    const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0800;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_processes: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn OpenJobObjectW(access: u32, inherit: i32, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    fn name(pid: u32) -> Vec<u16> {
        format!("Local\\ferry-{pid}")
            .encode_utf16()
            .chain([0])
            .collect()
    }

    pub struct Job(Handle);

    // Хэндл ядра можно закрыть из любого потока.
    unsafe impl Send for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn track(pid: u32) -> Option<Job> {
        unsafe {
            let job = CreateJobObjectW(null(), name(pid).as_ptr());
            if job.is_null() {
                return None;
            }
            let job = Job(job);
            // Кто сам просит выйти из job (так умеет Chromium), пусть выходит, а не падает с
            // «отказано в доступе»; такие процессы и раньше не гасились.
            let limits = BasicLimits {
                flags: JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                ..Default::default()
            };
            let info = (&limits as *const BasicLimits).cast();
            let size = size_of::<BasicLimits>() as u32;
            SetInformationJobObject(job.0, JOB_OBJECT_BASIC_LIMIT_INFORMATION, info, size);
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return None;
            }
            // ponytail: процесс уже идёт; то, что он успел запустить до этой строки, в job не
            // попадёт. Шеллу на старт нужны десятки миллисекунд — хватает; если не хватит —
            // запуск с CREATE_SUSPENDED.
            let assigned = AssignProcessToJobObject(job.0, process) != 0;
            CloseHandle(process);
            assigned.then_some(job)
        }
    }

    /// Погасить всё дерево команды; `false` — job нет (команда вышла или запущена не так).
    pub fn terminate(pid: u32) -> bool {
        unsafe {
            let job = OpenJobObjectW(JOB_OBJECT_TERMINATE, 0, name(pid).as_ptr());
            if job.is_null() {
                return false;
            }
            let done = TerminateJobObject(job, 1) != 0;
            CloseHandle(job);
            done
        }
    }
}

/// Остановить команду вместе со всем, что она запустила.
pub fn kill_tree(pid: u32) -> io::Result<()> {
    #[cfg(windows)]
    if job::terminate(pid) {
        return Ok(());
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn commands_find_ferry_next_to_the_app() {
        let dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let out = shell("printf %s \"$PATH\"").output().unwrap();
        let path = String::from_utf8(out.stdout).unwrap();
        assert!(
            std::env::split_paths(&path).any(|p| p == dir),
            "{} нет в PATH команды: {path}",
            dir.display()
        );
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn ferry_path_preserves_the_commands_environment() {
        let custom = std::env::temp_dir().join("ferry-custom-path");
        let mut command = Command::new("unused");
        command.env("PATH", &custom);
        ferry_path(&mut command);
        let path = command
            .get_envs()
            .find(|(k, _)| k == &"PATH")
            .unwrap()
            .1
            .unwrap();
        let dirs = std::env::split_paths(path).collect::<Vec<_>>();
        assert_eq!(
            dirs,
            [
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf(),
                custom
            ]
        );
    }

    #[test]
    fn explicit_bash_preserves_script_and_exit_code() {
        let output = shell_with(
            "[[ -n $BASH_VERSION ]] || exit 9; printf '%s' 'a \"b\" $HOME Привет'; exit 7",
            Shell::Bash,
        )
        .unwrap()
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "a \"b\" $HOME Привет"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_shells_are_rejected_on_unix() {
        assert!(shell_with("echo wrong", Shell::Powershell).is_err());
        assert!(shell_with("echo wrong", Shell::Cmd).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_shells_preserve_quotes_unicode_and_exit_codes() {
        let output = shell_with(
            "[Console]::Write('a \"b\" $HOME Привет'); exit 7",
            Shell::Powershell,
        )
        .unwrap()
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(7), "{:?}", output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "a \"b\" $HOME Привет"
        );
        let error = shell_with("Write-Error 'ошибка проверки'; exit 6", Shell::Powershell)
            .unwrap()
            .output()
            .unwrap();
        assert_eq!(error.status.code(), Some(6));
        let stderr = String::from_utf8(error.stderr).unwrap();
        assert!(stderr.contains("ошибка проверки"), "{stderr}");
        assert!(!stderr.contains("CLIXML"), "{stderr}");
        let output = shell_with("echo ferry-shell&exit /b 9", Shell::Cmd)
            .unwrap()
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "ferry-shell"
        );
        let output = shell_with(
            r#""C:\Windows\System32\cmd.exe" /d /c "echo quoted path""#,
            Shell::Cmd,
        )
        .unwrap()
        .output()
        .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "quoted path"
        );
    }
}
