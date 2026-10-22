//! Records only the time of non-injected input, never keys, positions, or application names.
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

static LAST: AtomicU64 = AtomicU64::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn start(root: PathBuf) {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let since = crate::now();
    std::thread::spawn(platform);
    std::thread::spawn(move || {
        loop {
            let value = json!({"updated_at":crate::now(),"observed_since":since,
            "last_input_at":LAST.load(Ordering::Relaxed),"available":READY.load(Ordering::Relaxed)});
            let _ = crate::write_private(&root.join("presence.json"), &value.to_string());
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

pub fn read(root: &Path) -> Value {
    let value = std::fs::read(root.join("presence.json"))
        .ok()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .unwrap_or(Value::Null);
    report(value, crate::now())
}
fn report(value: Value, now: u64) -> Value {
    let updated = value["updated_at"].as_u64();
    let available =
        value["available"] == true && updated.is_some_and(|t| t <= now && now - t <= 10);
    let last = value["last_input_at"]
        .as_u64()
        .filter(|t| *t > 0 && *t <= now);
    json!({"available":available,"idle_seconds":if available{last.map(|t|now-t)}else{None},
        "last_input_at":if available{last}else{None},"observed_since":value["observed_since"],
        "method":"non_injected_input","note":if !available{"Наблюдение недоступно или устарело"}
            else if last.is_none(){"Физический ввод с начала наблюдения ещё не замечен"}
            else{"Время последнего ввода не доказывает отсутствие человека"}})
}
fn seen() {
    LAST.store(crate::now(), Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn platform() {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
    };
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDown,
            CGEventType::RightMouseDown,
            CGEventType::OtherMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::ScrollWheel,
        ],
        |_, kind, event| {
            if matches!(
                kind,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                READY.store(false, Ordering::Relaxed);
            } else if event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID) == 0 {
                seen();
            }
            CallbackResult::Keep
        },
    );
    let Ok(tap) = tap else {
        return;
    };
    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        return;
    };
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    READY.store(true, Ordering::Relaxed);
    CFRunLoop::run_current();
    READY.store(false, Ordering::Relaxed);
}

#[cfg(windows)]
fn platform() {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    #[repr(C)]
    struct Keyboard {
        key: u32,
        scan: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }
    #[repr(C)]
    struct Mouse {
        x: i32,
        y: i32,
        data: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }
    #[repr(C)]
    struct Message {
        window: Handle,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        x: i32,
        y: i32,
        private: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            callback: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: Handle,
            thread: u32,
        ) -> Handle;
        fn CallNextHookEx(hook: Handle, code: i32, w: usize, l: isize) -> isize;
        fn UnhookWindowsHookEx(hook: Handle) -> i32;
        fn GetMessageW(message: *mut Message, window: Handle, min: u32, max: u32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Handle;
    }
    unsafe extern "system" fn keyboard(code: i32, w: usize, l: isize) -> isize {
        unsafe {
            if code >= 0 && (*(l as *const Keyboard)).flags & 0x10 == 0 {
                seen();
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }
    unsafe extern "system" fn mouse(code: i32, w: usize, l: isize) -> isize {
        unsafe {
            if code >= 0 && (*(l as *const Mouse)).flags & 1 == 0 {
                seen();
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let keyboard = SetWindowsHookExW(13, keyboard, module, 0);
        let mouse = SetWindowsHookExW(14, mouse, module, 0);
        if !keyboard.is_null() && !mouse.is_null() {
            READY.store(true, Ordering::Relaxed);
            let mut message: Message = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {}
            READY.store(false, Ordering::Relaxed);
        }
        if !keyboard.is_null() {
            UnhookWindowsHookEx(keyboard);
        }
        if !mouse.is_null() {
            UnhookWindowsHookEx(mouse);
        }
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn platform() {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_stale_and_never_observed_input_are_unknown() {
        assert!(report(Value::Null, 100)["idle_seconds"].is_null());
        let ready =
            json!({"available":true,"updated_at":99,"observed_since":10,"last_input_at":90});
        assert_eq!(report(ready.clone(), 100)["idle_seconds"], 10);
        assert!(report(ready, 120)["idle_seconds"].is_null());
        let empty = json!({"available":true,"updated_at":99,"last_input_at":0});
        assert!(report(empty, 100)["idle_seconds"].is_null());
    }
}
