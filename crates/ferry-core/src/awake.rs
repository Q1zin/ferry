//! Не давать системе и экрану заснуть по бездействию до освобождения запроса.

pub use platform::Inhibitor;

#[cfg(any(target_os = "macos", windows))]
const REASON: &str = "ferry is using the desktop";

#[cfg(target_os = "macos")]
mod platform {
    use std::io;

    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    use super::REASON;

    const ASSERTION_ON: u32 = 255;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    pub struct Inhibitor {
        system: u32,
        display: u32,
    }

    impl Inhibitor {
        pub fn acquire() -> io::Result<Self> {
            let system = assertion("PreventUserIdleSystemSleep")?;
            match assertion("PreventUserIdleDisplaySleep") {
                Ok(display) => Ok(Self { system, display }),
                Err(error) => {
                    unsafe { IOPMAssertionRelease(system) };
                    Err(error)
                }
            }
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            unsafe {
                IOPMAssertionRelease(self.display);
                IOPMAssertionRelease(self.system);
            }
        }
    }

    fn assertion(kind: &str) -> io::Result<u32> {
        let assertion_type = CFString::new(kind);
        let reason = CFString::new(REASON);
        let mut id = 0;
        let status = unsafe {
            IOPMAssertionCreateWithName(
                assertion_type.as_concrete_TypeRef(),
                ASSERTION_ON,
                reason.as_concrete_TypeRef(),
                &mut id,
            )
        };
        if status == 0 {
            Ok(id)
        } else {
            Err(io::Error::other(format!("{kind}: IOReturn {status:#x}")))
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use super::REASON;

    const CONTEXT_VERSION: u32 = 0;
    const SIMPLE_STRING: u32 = 1;
    const DISPLAY_REQUIRED: i32 = 0;
    const SYSTEM_REQUIRED: i32 = 1;

    #[repr(C)]
    struct ReasonContext {
        version: u32,
        flags: u32,
        reason: Reason,
    }

    #[repr(C)]
    union Reason {
        simple: *const u16,
        detailed: DetailedReason,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DetailedReason {
        module: RawHandle,
        id: u32,
        count: u32,
        strings: *const *const u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PowerCreateRequest(context: *const ReasonContext) -> RawHandle;
        fn PowerSetRequest(handle: RawHandle, request: i32) -> i32;
        fn PowerClearRequest(handle: RawHandle, request: i32) -> i32;
    }

    pub struct Inhibitor {
        handle: OwnedHandle,
    }

    impl Inhibitor {
        pub fn acquire() -> io::Result<Self> {
            let reason: Vec<_> = REASON.encode_utf16().chain([0]).collect();
            let context = ReasonContext {
                version: CONTEXT_VERSION,
                flags: SIMPLE_STRING,
                reason: Reason {
                    simple: reason.as_ptr(),
                },
            };
            let raw = unsafe { PowerCreateRequest(&context) };
            if raw as isize == -1 {
                return Err(io::Error::last_os_error());
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { PowerSetRequest(raw, SYSTEM_REQUIRED) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { PowerSetRequest(raw, DISPLAY_REQUIRED) } == 0 {
                let error = io::Error::last_os_error();
                unsafe { PowerClearRequest(raw, SYSTEM_REQUIRED) };
                return Err(error);
            }
            Ok(Self { handle })
        }
    }

    impl Drop for Inhibitor {
        fn drop(&mut self) {
            let raw = self.handle.as_raw_handle();
            unsafe {
                PowerClearRequest(raw, DISPLAY_REQUIRED);
                PowerClearRequest(raw, SYSTEM_REQUIRED);
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use std::io;

    pub struct Inhibitor;

    impl Inhibitor {
        pub fn acquire() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "удержание системы и экрана от сна недоступно на этой платформе",
            ))
        }
    }
}
