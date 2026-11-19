//! Asks the OS how long ago the last keyboard or mouse input was. This is a
//! single number from the OS idle timer: Pulse never sees which keys were
//! pressed or where the mouse went.

pub struct IdleTimer {
    #[cfg(all(unix, not(target_os = "macos")))]
    x11: Option<x11::Connection>,
}

impl IdleTimer {
    pub fn new() -> IdleTimer {
        IdleTimer {
            #[cfg(all(unix, not(target_os = "macos")))]
            x11: x11::Connection::open(),
        }
    }

    /// Seconds since the last input, or `None` when this system gives no way
    /// to ask. Pulse then falls back to prompts as the only sign of presence.
    pub fn idle_seconds(&mut self) -> Option<u64> {
        self.platform_idle_seconds()
    }

    #[cfg(windows)]
    fn platform_idle_seconds(&mut self) -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::GetTickCount;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: info is a valid LASTINPUTINFO with cbSize set, as the API requires
        if unsafe { GetLastInputInfo(&mut info) } == 0 {
            return None;
        }
        // both are 32-bit millisecond tick counts, so wrapping subtraction survives the 49-day rollover
        let now = unsafe { GetTickCount() };
        Some(u64::from(now.wrapping_sub(info.dwTime)) / 1000)
    }

    #[cfg(target_os = "macos")]
    fn platform_idle_seconds(&mut self) -> Option<u64> {
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
        }
        // 0 = kCGEventSourceStateCombinedSessionState, u32::MAX = kCGAnyInputEventType
        // SAFETY: plain C function taking two integers; needs no permissions
        let seconds = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
        seconds.is_finite().then(|| seconds.max(0.0) as u64)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn platform_idle_seconds(&mut self) -> Option<u64> {
        self.x11.as_ref()?.idle_seconds()
    }

    #[cfg(not(any(windows, unix)))]
    fn platform_idle_seconds(&mut self) -> Option<u64> {
        None
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11 {
    use std::ptr;

    use x11_dl::{xlib, xss};

    /// X11 screensaver extension, loaded at runtime so Pulse still starts on
    /// machines without libX11/libXss. Wayland-only sessions return `None`
    /// here for now.
    pub struct Connection {
        xlib: xlib::Xlib,
        xss: xss::Xss,
        display: *mut xlib::Display,
    }

    impl Connection {
        pub fn open() -> Option<Connection> {
            let xlib = xlib::Xlib::open().ok()?;
            let xss = xss::Xss::open().ok()?;
            // SAFETY: a null name opens $DISPLAY; a null result is handled
            let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
            (!display.is_null()).then_some(Connection { xlib, xss, display })
        }

        pub fn idle_seconds(&self) -> Option<u64> {
            // SAFETY: display is open for the lifetime of self, info is freed below
            unsafe {
                let info = (self.xss.XScreenSaverAllocInfo)();
                if info.is_null() {
                    return None;
                }
                let root = (self.xlib.XDefaultRootWindow)(self.display);
                let ok = (self.xss.XScreenSaverQueryInfo)(self.display, root, info) != 0;
                let idle = (*info).idle;
                (self.xlib.XFree)(info.cast());
                ok.then_some(idle / 1000)
            }
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            // SAFETY: display was opened by XOpenDisplay and is closed once
            unsafe { (self.xlib.XCloseDisplay)(self.display) };
        }
    }
}
