//! Подтверждённый ввод в окна приложений с повышенными правами на Windows.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum Action {
    Probe,
    Focus,
    Move { x: i32, y: i32 },
    Click { x: i32, y: i32, right: bool },
    Scroll { clicks: i32 },
    Key { combo: String },
    Type { text: String },
}

#[cfg(windows)]
pub use windows::{Target, candidates, confirm_desktop, desktop_available};

#[cfg(any(windows, test))]
fn denied(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

#[cfg(any(windows, test))]
fn consent_text(text: &str) -> String {
    text.chars()
        .filter(|&ch| {
            !ch.is_control()
                && !matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(160)
        .collect()
}

#[cfg(any(windows, test))]
fn check_combo(combo: &str) -> std::io::Result<()> {
    if combo.len() > 64 {
        return Err(denied("слишком длинная комбинация клавиш"));
    }
    let keys = super::combo(combo)?;
    let (last, modifiers) = keys.split_last().ok_or_else(|| denied("нет клавиши"))?;
    let modifier = |vk| matches!(vk, 0x10 | 0x11);
    if modifiers.iter().all(|&(vk, _)| modifier(vk))
        && !matches!(last.0, 0x10..=0x12 | 0x5b | 0x5c)
        && !(last.0 == 0x1b && modifiers.iter().any(|&(vk, _)| vk == 0x11))
    {
        Ok(())
    } else {
        Err(denied("комбинация может переключить окно или рабочий стол"))
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::path::Path;
    use std::ptr::{from_mut, null_mut};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetGUIThreadInfo};

    use super::{Action, check_combo, consent_text, denied};
    use crate::screen::{self, INPUT_LOCK, Window, win};

    const GA_ROOT: u32 = 2;
    const DESKTOP_READOBJECTS: u32 = 1;
    const UOI_NAME: i32 = 2;
    const UOI_IO: i32 = 6;
    const DWMWA_CLOAKED: u32 = 14;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_TIMEOUT: u32 = 258;
    const TOKEN_QUERY: u32 = 8;
    const TOKEN_INTEGRITY_LEVEL: i32 = 25;
    const SECURITY_MANDATORY_HIGH_RID: u32 = 0x3000;
    const SECURITY_MANDATORY_SYSTEM_RID: u32 = 0x4000;

    #[repr(C)]
    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MandatoryLabel {
        sid: *mut c_void,
        attributes: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> isize;
        fn CloseDesktop(desktop: isize) -> i32;
        fn GetProcessWindowStation() -> isize;
        fn GetThreadDesktop(thread: u32) -> isize;
        fn GetUserObjectInformationW(
            object: isize,
            index: i32,
            info: *mut c_void,
            bytes: u32,
            needed: *mut u32,
        ) -> i32;
        fn EnumWindows(callback: extern "system" fn(isize, isize) -> i32, data: isize) -> i32;
        fn GetWindowThreadProcessId(window: isize, pid: *mut u32) -> u32;
        fn GetAncestor(window: isize, flags: u32) -> isize;
        fn IsWindowVisible(window: isize) -> i32;
        fn IsIconic(window: isize) -> i32;
        fn GetWindowTextW(window: isize, text: *mut u16, max: i32) -> i32;
        fn GetWindowRect(window: isize, rect: *mut Rect) -> i32;
        fn GetForegroundWindow() -> isize;
        fn GetAsyncKeyState(key: i32) -> i16;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
        fn WindowFromPoint(point: Point) -> isize;
        fn MessageBoxW(window: isize, text: *const u16, title: *const u16, flags: u32) -> i32;
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmGetWindowAttribute(window: isize, attr: u32, data: *mut c_void, size: u32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> RawHandle;
        fn GetCurrentThreadId() -> u32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> RawHandle;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
        fn WaitForSingleObject(handle: RawHandle, timeout: u32) -> u32;
        fn QueryFullProcessImageNameW(
            process: RawHandle,
            flags: u32,
            text: *mut u16,
            len: *mut u32,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: RawHandle, access: u32, token: *mut RawHandle) -> i32;
        fn GetTokenInformation(
            token: RawHandle,
            kind: i32,
            info: *mut c_void,
            bytes: u32,
            needed: *mut u32,
        ) -> i32;
        fn GetSidSubAuthorityCount(sid: *mut c_void) -> *mut u8;
        fn GetSidSubAuthority(sid: *mut c_void, index: u32) -> *mut u32;
    }

    fn session_of(pid: u32) -> io::Result<u32> {
        let mut session = 0;
        if unsafe { ProcessIdToSessionId(pid, &mut session) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(session)
    }

    fn object_named(handle: isize, expected: &str) -> bool {
        let mut name = [0u16; 64];
        let mut needed = 0;
        handle != 0
            && unsafe {
                GetUserObjectInformationW(
                    handle,
                    UOI_NAME,
                    name.as_mut_ptr().cast(),
                    size_of_val(&name) as u32,
                    &mut needed,
                )
            } != 0
            && name
                .split(|&unit| unit == 0)
                .next()
                .is_some_and(|value| String::from_utf16_lossy(value).eq_ignore_ascii_case(expected))
    }

    fn default_input() -> io::Result<()> {
        if !object_named(unsafe { GetProcessWindowStation() }, "WinSta0")
            || !object_named(unsafe { GetThreadDesktop(GetCurrentThreadId()) }, "Default")
        {
            return Err(denied("нужна интерактивная сессия Windows"));
        }
        let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
        if desktop != 0 {
            let mut active = 0i32;
            let default = object_named(desktop, "Default")
                && unsafe {
                    GetUserObjectInformationW(
                        desktop,
                        UOI_IO,
                        from_mut(&mut active).cast(),
                        size_of::<i32>() as u32,
                        null_mut(),
                    )
                } != 0
                && active != 0;
            unsafe { CloseDesktop(desktop) };
            if default {
                return Ok(());
            }
        }
        Err(denied("активен защищённый рабочий стол Windows"))
    }

    pub fn desktop_available() -> bool {
        default_input().is_ok()
    }

    fn window_owner(window: isize) -> (u32, u32) {
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        (thread, pid)
    }

    fn visible(window: isize) -> bool {
        let mut cloaked = 0u32;
        unsafe {
            IsWindowVisible(window) != 0
                && DwmGetWindowAttribute(
                    window,
                    DWMWA_CLOAKED,
                    from_mut(&mut cloaked).cast(),
                    size_of::<u32>() as u32,
                ) >= 0
                && cloaked == 0
        }
    }

    fn integrity(process: RawHandle) -> io::Result<u32> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut needed = 0;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TOKEN_INTEGRITY_LEVEL,
                null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TOKEN_INTEGRITY_LEVEL,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // Windows записывает TOKEN_MANDATORY_LABEL и SID в этот выровненный буфер.
        let sid = unsafe { (*buffer.as_ptr().cast::<MandatoryLabel>()).sid };
        if sid.is_null() {
            return Err(denied("Windows не вернула уровень целостности"));
        }
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        if count == 0 {
            return Err(denied("Windows не вернула уровень целостности"));
        }
        Ok(unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) })
    }

    /// Сохраняем дескриптор именно того процесса, чьё окно одобрено.
    pub struct Target {
        window: isize,
        thread: u32,
        pid: u32,
        process: OwnedHandle,
    }

    impl Target {
        pub fn open(window: i64) -> io::Result<Self> {
            default_input()?;
            let window = isize::try_from(window).map_err(|_| denied("неверный HWND"))?;
            Self::open_on_desktop(window, session_of(std::process::id())?)
        }

        fn open_on_desktop(window: isize, session: u32) -> io::Result<Self> {
            let (thread, pid) = window_owner(window);
            if thread == 0
                || pid == 0
                || unsafe { GetAncestor(window, GA_ROOT) } != window
                || !visible(window)
                || !object_named(unsafe { GetThreadDesktop(thread) }, "Default")
                || session_of(pid)? != session
            {
                return Err(denied("нужно видимое окно в этой сессии Windows"));
            }
            let raw =
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let process = unsafe { OwnedHandle::from_raw_handle(raw) };
            if !(SECURITY_MANDATORY_HIGH_RID..SECURITY_MANDATORY_SYSTEM_RID)
                .contains(&integrity(raw)?)
            {
                return Err(denied(
                    "нужно окно приложения, запущенного от администратора",
                ));
            }
            let target = Self {
                window,
                thread,
                pid,
                process,
            };
            target.check(false)?;
            Ok(target)
        }

        fn check(&self, focused: bool) -> io::Result<()> {
            if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } != WAIT_TIMEOUT
                || window_owner(self.window) != (self.thread, self.pid)
                || !visible(self.window)
            {
                return Err(denied("разрешённое окно закрылось или сменило процесс"));
            }
            if focused && unsafe { GetForegroundWindow() } != self.window {
                return Err(denied("разрешённое окно потеряло фокус"));
            }
            Ok(())
        }

        fn description(&self) -> io::Result<Window> {
            let (left, top, _, _) = win::desktop();
            let mut rect = Rect::default();
            if unsafe { GetWindowRect(self.window, &mut rect) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut title = [0u16; 512];
            let len =
                unsafe { GetWindowTextW(self.window, title.as_mut_ptr(), title.len() as i32) };
            let mut path = [0u16; 1024];
            let mut size = path.len() as u32;
            if unsafe {
                QueryFullProcessImageNameW(
                    self.process.as_raw_handle(),
                    0,
                    path.as_mut_ptr(),
                    &mut size,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let path = String::from_utf16_lossy(&path[..size as usize]);
            let process = Path::new(&path)
                .file_name()
                .ok_or_else(|| denied("не удалось распознать приложение окна"))?;
            Ok(Window {
                id: self.window as i64,
                process: process.to_string_lossy().into_owned(),
                pid: i64::from(self.pid),
                title: String::from_utf16_lossy(&title[..len as usize]),
                rect: (unsafe { IsIconic(self.window) } == 0).then_some((
                    i64::from(rect.left) - i64::from(left),
                    i64::from(rect.top) - i64::from(top),
                    i64::from(rect.right) - i64::from(rect.left),
                    i64::from(rect.bottom) - i64::from(rect.top),
                )),
                focused: unsafe { GetForegroundWindow() } == self.window,
            })
        }

        fn owns(&self, window: isize) -> bool {
            window != 0
                && (window == self.window || unsafe { GetAncestor(window, GA_ROOT) } == self.window)
                && window_owner(window).1 == self.pid
        }

        fn check_point(&self, point: Point) -> io::Result<()> {
            if self.owns(unsafe { WindowFromPoint(point) }) {
                return Ok(());
            }
            Err(denied("точка вне разрешённого окна"))
        }

        fn check_input(&self) -> io::Result<()> {
            default_input()?;
            self.check(true)?;
            let mut info = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetGUIThreadInfo(0, &mut info) } == 0
                || info.hwndActive as isize != self.window
                || !self.owns(info.hwndFocus as isize)
                || (!info.hwndCapture.is_null() && !self.owns(info.hwndCapture as isize))
            {
                return Err(denied("клавиатура или мышь направлены за пределы окна"));
            }
            // SendInput не сбрасывает уже нажатые кнопки и модификаторы.
            let user_keys = [1, 2, 4, 5, 6, 0x10, 0x11, 0x12, 0x5b, 0x5c];
            if user_keys
                .into_iter()
                .any(|key| unsafe { GetAsyncKeyState(key) } < 0)
            {
                return Err(denied("удерживается кнопка мыши или клавиша-модификатор"));
            }
            Ok(())
        }

        fn pointer(&self, x: i32, y: i32, buttons: Option<[u32; 2]>) -> io::Result<String> {
            let (left, top, width, height) = win::desktop();
            if !(0..width).contains(&x) || !(0..height).contains(&y) {
                return Err(denied("точка вне экрана"));
            }
            let point = Point {
                x: left + x,
                y: top + y,
            };
            self.check_point(point)?;
            self.check_input()?;
            if unsafe { SetCursorPos(point.x, point.y) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if buttons.is_some() {
                self.check_input()?;
            }
            let actual = cursor()?;
            if actual != point {
                return Err(denied("курсор сместился с выбранной точки"));
            }
            self.check_point(actual)?;
            if let Some(buttons) = buttons {
                win::buttons_at_cursor(buttons)
            } else {
                Ok(String::new())
            }
        }

        pub fn confirm(&self, owner_label: &str) -> io::Result<()> {
            default_input()?;
            self.check(false)?;
            let window = self.description()?;
            let label = consent_text(owner_label);
            let process = consent_text(&window.process);
            let title = consent_text(&window.title);
            let text = format!(
                "Разрешить агенту управлять этим окном с повышенными правами?\n\n\
                 Контроллер и сессия: {label:?}\n\
                 Процесс: {process:?} (PID {})\n\
                 Окно: {title:?} (@{})\n\n\
                 Разрешение действует до 15 минут. Подтверждайте только знакомый запрос.",
                window.pid, window.id,
            );
            self.check(false)?;
            confirm_locally(&text)?;
            self.check(false)
        }

        pub fn execute(&self, action: Action) -> io::Result<String> {
            let _lock = INPUT_LOCK
                .lock()
                .map_err(|_| io::Error::other("ввод прервался"))?;
            default_input()?;
            self.check(!matches!(action, Action::Probe | Action::Focus))?;
            match action {
                Action::Probe => {
                    serde_json::to_string(&self.description()?).map_err(io::Error::other)
                }
                Action::Focus => {
                    let window = self.description()?;
                    self.check(false)?;
                    win::focus(&window)?;
                    self.check(true)?;
                    Ok(String::new())
                }
                Action::Move { x, y } => self.pointer(x, y, None),
                Action::Click { x, y, right } => self.pointer(
                    x,
                    y,
                    Some(if right {
                        [screen::MOUSE_RIGHT_DOWN, screen::MOUSE_RIGHT_UP]
                    } else {
                        [screen::MOUSE_LEFT_DOWN, screen::MOUSE_LEFT_UP]
                    }),
                ),
                Action::Scroll { clicks } => {
                    if clicks.checked_mul(120).is_none() {
                        return Err(denied("слишком большое число шагов прокрутки"));
                    }
                    self.check_input()?;
                    self.check_point(cursor()?)?;
                    win::scroll(clicks)
                }
                Action::Key { combo } => {
                    check_combo(&combo)?;
                    self.check_input()?;
                    win::keys(&[&combo])
                }
                Action::Type { text } => {
                    if text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
                        return Err(denied("нужен текст до 4096 байт без управляющих символов"));
                    }
                    self.check_input()?;
                    win::type_text(&text)
                }
            }
        }
    }

    pub fn confirm_desktop(owner_label: &str) -> io::Result<()> {
        let label = consent_text(owner_label);
        confirm_locally(&format!(
            "Разрешить компьютеру {label:?} управлять всеми окнами приложений, запущенных \
             от администратора, на этом рабочем столе Windows?\n\n\
             Разрешение действует для текущей и последующих сессий агента с этого компьютера \
             при активной брони рабочего стола. Оно распространяется и на окна, которые \
             откроются позднее, и действует до отзыва или закрытия ferry.\n\n\
             При блокировке Windows управление недоступно. \
             Системные окна и защищённый рабочий стол требуют вашего участия. \
             Подтверждайте, только если доверяете этому компьютеру."
        ))
    }

    fn confirm_locally(text: &str) -> io::Result<()> {
        if integrity(unsafe { GetCurrentProcess() })? < SECURITY_MANDATORY_HIGH_RID {
            return Err(denied("подтверждение требует повышенных прав"));
        }
        let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<_>>();
        let text = wide(text);
        let title = wide("ferry — подтверждение управления Windows");
        const IDYES: i32 = 6;
        const MB_YESNO: u32 = 0x0004;
        const MB_ICONWARNING: u32 = 0x0030;
        const MB_DEFBUTTON2: u32 = 0x0100;
        const MB_SETFOREGROUND: u32 = 0x10000;
        const MB_TOPMOST: u32 = 0x40000;
        default_input()?;
        let choice = unsafe {
            MessageBoxW(
                0,
                text.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        if choice != IDYES {
            return Err(denied("управление не подтверждено"));
        }
        default_input()
    }

    fn cursor() -> io::Result<Point> {
        let mut point = Point::default();
        if unsafe { GetCursorPos(&mut point) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(point)
    }

    pub fn candidates() -> io::Result<Vec<Window>> {
        default_input()?;
        let session = session_of(std::process::id())?;
        extern "system" fn collect(window: isize, data: isize) -> i32 {
            unsafe { (*(data as *mut Vec<isize>)).push(window) };
            1
        }
        let mut handles = Vec::new();
        if unsafe { EnumWindows(collect, from_mut(&mut handles) as isize) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(handles
            .into_iter()
            .filter_map(|window| Target::open_on_desktop(window, session).ok())
            .filter_map(|target| target.description().ok())
            .filter(|window| !window.title.is_empty())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_labels_cannot_add_rows_or_reverse_display_text() {
        assert_eq!(consent_text("агент:\n\r\t\u{202e} 42\0"), "агент: 42");
        assert_eq!(consent_text(&"А".repeat(200)).chars().count(), 160);
    }

    #[test]
    fn approved_window_shortcuts_omit_system_modifiers() {
        for combo in ["ctrl+shift+i", "f12", "ctrl++", "escape"] {
            assert!(check_combo(combo).is_ok(), "{combo}");
        }
        for combo in ["alt+tab", "win+r", "ctrl+esc", "ctrl+shift+esc", "ctrl"] {
            assert!(check_combo(combo).is_err(), "{combo}");
        }
    }
}
