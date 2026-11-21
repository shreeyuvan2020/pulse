//! Grabs the whole screen: X11, Wayland (through the `pulse-wayland` helper,
//! see `wayland.rs`), macOS (ScreenCaptureKit) and Windows (DXGI Desktop
//! Duplication). Every backend returns the same `Frame`.

#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "macos")]
mod macos;

use super::encode::Frame;

pub struct Capture {
    #[cfg(all(unix, not(target_os = "macos")))]
    linux: Linux,
    #[cfg(target_os = "macos")]
    macos: macos::Screen,
    #[cfg(windows)]
    dxgi: dxgi::Screen,
}

impl Capture {
    /// `Err` says why this system cannot record yet.
    pub fn open() -> Result<Capture, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let linux = if crate::wayland::is_session() {
                Linux::Wayland(crate::wayland::client()?)
            } else {
                Linux::X11(Box::new(x11::Screen::open()?))
            };
            Ok(Capture { linux })
        }
        #[cfg(target_os = "macos")]
        {
            macos::Screen::open().map(|macos| Capture { macos })
        }
        #[cfg(windows)]
        {
            dxgi::Screen::open().map(|dxgi| Capture { dxgi })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err("screen recording is not supported on this OS yet".into())
        }
    }

    pub fn grab(&mut self) -> Option<Frame> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match &mut self.linux {
                Linux::X11(screen) => screen.grab(),
                Linux::Wayland(client) => client.grab(),
            }
        }
        #[cfg(target_os = "macos")]
        {
            self.macos.grab()
        }
        #[cfg(windows)]
        {
            self.dxgi.grab()
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    }
}

impl Capture {
    /// Stops using the screen until the next `grab`. On Wayland that closes
    /// the screen share, so the desktop's recording indicator goes off.
    pub fn release(&mut self) {
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Linux::Wayland(client) = &self.linux {
            client.release();
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
enum Linux {
    X11(Box<x11::Screen>),
    Wayland(std::sync::Arc<crate::wayland::Client>),
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11 {
    use std::ptr;

    use x11_dl::xlib;

    use super::Frame;

    pub struct Screen {
        xlib: xlib::Xlib,
        display: *mut xlib::Display,
    }

    impl Screen {
        pub fn open() -> Result<Screen, String> {
            if std::env::var_os("DISPLAY").is_none() {
                return Err("no display to record".into());
            }
            let xlib = xlib::Xlib::open().map_err(|error| error.to_string())?;
            // SAFETY: a null name opens $DISPLAY; a null result is handled
            let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
            if display.is_null() {
                return Err("cannot open the X display".into());
            }
            Ok(Screen { xlib, display })
        }

        pub fn grab(&mut self) -> Option<Frame> {
            // SAFETY: display is open for the lifetime of self; the image is copied, then destroyed once
            unsafe {
                let screen = (self.xlib.XDefaultScreen)(self.display);
                let width = (self.xlib.XDisplayWidth)(self.display, screen);
                let height = (self.xlib.XDisplayHeight)(self.display, screen);
                let root = (self.xlib.XDefaultRootWindow)(self.display);
                let image = (self.xlib.XGetImage)(
                    self.display,
                    root,
                    0,
                    0,
                    width as u32,
                    height as u32,
                    !0,
                    xlib::ZPixmap,
                );
                if image.is_null() {
                    return None;
                }
                let frame =
                    ((*image).bits_per_pixel == 32 && !(*image).data.is_null()).then(|| {
                        let stride = (*image).bytes_per_line as usize;
                        let len = stride * (*image).height as usize;
                        Frame {
                            width: (*image).width as usize,
                            height: (*image).height as usize,
                            stride,
                            bgra: std::slice::from_raw_parts((*image).data.cast::<u8>(), len)
                                .to_vec(),
                        }
                    });
                (self.xlib.XDestroyImage)(image);
                frame
            }
        }
    }

    impl Drop for Screen {
        fn drop(&mut self) {
            // SAFETY: display was opened by XOpenDisplay and is closed once
            unsafe { (self.xlib.XCloseDisplay)(self.display) };
        }
    }
}
