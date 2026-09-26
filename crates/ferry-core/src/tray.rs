//! Иконка ferry в трее узла. Человек за узлом переключает режим правым кликом: работать,
//! не трогать рабочий стол, отключиться от хаба. Живёт внутри демона (он и так в сессии
//! пользователя) на отдельном цикле сообщений; хаб видит режим через `ferry info`/`ferry ls`.
//!
//! Только Windows и только прямой Win32 (`Shell_NotifyIcon`) — ни одной зависимости, чтобы не
//! рисковать кросс-сборкой под mingw.

/// Показать иконку и крутить цикл сообщений. Не возвращается, пока демон жив; если трей не
/// завёлся, просто засыпает — демон продолжает принимать команды.
pub fn run() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    std::thread::park();
}

#[cfg(windows)]
mod win {
    use std::mem::{size_of, zeroed};

    use crate::mode::{self, Mode};

    const WM_APP_TRAY: u32 = 0x8000 + 1;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_DESTROY: u32 = 0x0002;
    const WM_COMMAND: u32 = 0x0111;
    const WM_TIMER: u32 = 0x0113;

    const NIM_ADD: u32 = 0;
    const NIM_MODIFY: u32 = 1;
    const NIM_DELETE: u32 = 2;
    const NIF_MESSAGE: u32 = 1;
    const NIF_ICON: u32 = 2;
    const NIF_TIP: u32 = 4;

    const MF_STRING: u32 = 0;
    const MF_CHECKED: u32 = 8;
    const MF_SEPARATOR: u32 = 0x800;
    const MF_GRAYED: u32 = 1;
    const TPM_RIGHTBUTTON: u32 = 2;
    const TPM_RETURNCMD: u32 = 0x100;

    const IDI_APPLICATION: usize = 32512;
    const WS_OVERLAPPED: u32 = 0;

    type Wndproc = extern "system" fn(isize, u32, usize, isize) -> isize;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: isize,
        message: u32,
        w: usize,
        l: isize,
        time: u32,
        pt: Point,
    }

    #[repr(C)]
    struct WndClass {
        style: u32,
        wndproc: Wndproc,
        cls_extra: i32,
        wnd_extra: i32,
        instance: isize,
        icon: isize,
        cursor: isize,
        background: isize,
        menu_name: *const u16,
        class_name: *const u16,
    }

    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }

    #[repr(C)]
    struct NotifyIconData {
        cb_size: u32,
        hwnd: isize,
        id: u32,
        flags: u32,
        callback: u32,
        icon: isize,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        version: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid: Guid,
        balloon_icon: isize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> isize;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn LoadIconW(instance: isize, name: usize) -> isize;
        fn RegisterClassW(class: *const WndClass) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            window: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: *const core::ffi::c_void,
        ) -> isize;
        fn DefWindowProcW(hwnd: isize, msg: u32, w: usize, l: isize) -> isize;
        fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> isize;
        fn PostQuitMessage(code: i32);
        fn GetCursorPos(point: *mut Point) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn CreatePopupMenu() -> isize;
        fn AppendMenuW(menu: isize, flags: u32, id: usize, text: *const u16) -> i32;
        fn TrackPopupMenu(
            menu: isize,
            flags: u32,
            x: i32,
            y: i32,
            reserved: i32,
            hwnd: isize,
            rect: *const core::ffi::c_void,
        ) -> i32;
        fn DestroyMenu(menu: isize) -> i32;
        fn SetTimer(hwnd: isize, id: usize, ms: u32, callback: *const core::ffi::c_void) -> usize;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Иконка ferry, свой uID = 1; остальное заполняет вызывающий под действие (ADD/MODIFY).
    // ponytail: пока системная иконка приложения; своя .ico — когда в сборку добавим windres.
    fn icon_data(hwnd: isize) -> NotifyIconData {
        let mut data: NotifyIconData = unsafe { zeroed() };
        data.cb_size = size_of::<NotifyIconData>() as u32;
        data.hwnd = hwnd;
        data.id = 1;
        data.flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.callback = WM_APP_TRAY;
        data.icon = unsafe { LoadIconW(0, IDI_APPLICATION) };
        let tip = wide(&format!("ferry — {}", mode::current().label()));
        data.tip[..tip.len()].copy_from_slice(&tip);
        data
    }

    fn set_tooltip(hwnd: isize) {
        let data = icon_data(hwnd);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    /// Меню режимов у курсора: текущий отмечен; выбранный применяется.
    fn show_menu(hwnd: isize) {
        let menu = unsafe { CreatePopupMenu() };
        if menu == 0 {
            return;
        }
        let title = wide("ferry");
        unsafe { AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, title.as_ptr()) };
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null()) };
        let current = mode::current();
        for (i, m) in Mode::ALL.into_iter().enumerate() {
            let checked = if m == current { MF_CHECKED } else { 0 };
            let text = wide(m.label());
            unsafe { AppendMenuW(menu, MF_STRING | checked, i + 1, text.as_ptr()) };
        }

        let mut pt = Point { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt);
            // Без переднего плана меню не закроется по клику мимо — известный баг Win32.
            SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            DestroyMenu(menu);
            if let Some(mode) = Mode::ALL.get(chosen.wrapping_sub(1) as usize) {
                let _ = mode::set(*mode);
                set_tooltip(hwnd);
            }
        }
    }

    extern "system" fn wndproc(hwnd: isize, msg: u32, w: usize, l: isize) -> isize {
        match msg {
            WM_APP_TRAY => {
                let event = (l as u32) & 0xffff;
                if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                    show_menu(hwnd);
                }
                0
            }
            // Меню отдаёт выбор через TPM_RETURNCMD, этот путь не используется, но пусть будет.
            WM_COMMAND => {
                if let Some(mode) = Mode::ALL.get((w & 0xffff).wrapping_sub(1)) {
                    let _ = mode::set(*mode);
                    set_tooltip(hwnd);
                }
                0
            }
            // Режим могут сменить и командой `ferry mode` — подсказка догоняет его по таймеру.
            WM_TIMER => {
                set_tooltip(hwnd);
                0
            }
            WM_DESTROY => {
                let data = icon_data(hwnd);
                unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
        }
    }

    pub fn run() {
        if setup().is_none() {
            // Трей не завёлся — демон всё равно должен работать.
            std::thread::park();
        }
    }

    fn setup() -> Option<()> {
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class_name = wide("ferry-tray");
        let class = WndClass {
            style: 0,
            wndproc,
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return None;
        }
        let title = wide("ferry");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                0,
                0,
                instance,
                std::ptr::null(),
            )
        };
        if hwnd == 0 {
            return None;
        }
        let data = icon_data(hwnd);
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return None;
        }
        unsafe { SetTimer(hwnd, 1, 2000, std::ptr::null()) };

        let mut msg: Msg = unsafe { zeroed() };
        while unsafe { GetMessageW(&mut msg, 0, 0, 0) } > 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Some(())
    }
}
