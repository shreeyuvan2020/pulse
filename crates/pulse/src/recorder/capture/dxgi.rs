//! Windows capture through DXGI Desktop Duplication (Windows 8 and newer).
//! Windows hands over the desktop image only when something on screen
//! changed, so `grab` returns the latest image and costs almost nothing while
//! the screen is still. It captures the primary monitor.
//!
//! The lock screen and UAC prompts are on a separate desktop that duplication
//! cannot see: the capture ends there and `grab` returns `None` until the
//! person is back at their own desktop, so nothing from them is recorded.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use super::Frame;

struct Duplication {
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
}

enum Update {
    /// Nothing on screen changed since the last grab.
    Unchanged,
    Image(Frame),
    /// The desktop went away (lock screen, resolution change, another user).
    Lost,
}

pub struct Screen {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<Duplication>,
    latest: Option<Frame>,
    unavailable: bool,
}

impl Screen {
    pub fn open() -> Result<Screen, String> {
        // SAFETY: plain DXGI and Direct3D calls on objects this function owns
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|error| format!("no graphics system: {error}"))?;
            let (adapter, output) = primary_output(&factory).ok_or("no display to record")?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|error| format!("cannot start Direct3D: {error}"))?;
            Ok(Screen {
                device: device.ok_or("cannot start Direct3D")?,
                context: context.ok_or("cannot start Direct3D")?,
                output,
                duplication: None,
                latest: None,
                unavailable: false,
            })
        }
    }

    pub fn grab(&mut self) -> Option<Frame> {
        if self.duplication.is_none() {
            self.latest = None;
            self.duplication = self.connect();
        }
        let duplication = self.duplication.as_mut()?;
        // SAFETY: the duplication belongs to this device and is used from this thread only
        match unsafe { duplication.next(self.latest.is_none()) } {
            Update::Unchanged => {}
            Update::Image(frame) => self.latest = Some(frame),
            Update::Lost => {
                self.duplication = None;
                self.latest = None;
            }
        }
        self.latest.as_ref().map(|frame| Frame {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            bgra: frame.bgra.clone(),
        })
    }

    /// Starts duplicating the monitor. Says once when that stops working and
    /// once when it works again, so a locked screen does not flood the log.
    fn connect(&mut self) -> Option<Duplication> {
        // SAFETY: the output and device are alive for as long as self
        let result = unsafe { self.output.DuplicateOutput(&self.device) };
        match result {
            Ok(duplication) => {
                if self.unavailable {
                    self.unavailable = false;
                    eprintln!("pulse: the screen can be recorded again");
                }
                Some(Duplication {
                    context: self.context.clone(),
                    duplication,
                    staging: None,
                })
            }
            Err(error) => {
                if !self.unavailable {
                    self.unavailable = true;
                    eprintln!(
                        "pulse: cannot record the screen right now ({error}); it is normal while the screen is locked"
                    );
                }
                None
            }
        }
    }
}

/// The monitor at the desktop's origin, which Windows treats as the primary
/// one, together with the graphics adapter that drives it.
unsafe fn primary_output(factory: &IDXGIFactory1) -> Option<(IDXGIAdapter, IDXGIOutput1)> {
    let mut fallback = None;
    // SAFETY: enumeration stops at the first error, which DXGI returns after the last index
    unsafe {
        for adapter_index in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            let adapter: IDXGIAdapter = adapter.cast().ok()?;
            for output_index in 0.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                let Ok(description) = output.GetDesc() else {
                    continue;
                };
                if !description.AttachedToDesktop.as_bool() {
                    continue;
                }
                let Ok(output) = output.cast::<IDXGIOutput1>() else {
                    continue;
                };
                let rect = description.DesktopCoordinates;
                if rect.left == 0 && rect.top == 0 {
                    return Some((adapter, output));
                }
                fallback.get_or_insert((adapter.clone(), output));
            }
        }
    }
    fallback
}

impl Duplication {
    /// Takes the next desktop image if the screen changed. `wait` allows up
    /// to half a second for the first image after connecting.
    unsafe fn next(&mut self, wait: bool) -> Update {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: info and resource are valid for the call; the frame is released below
        unsafe {
            match self.duplication.AcquireNextFrame(
                if wait { 500 } else { 0 },
                &mut info,
                &mut resource,
            ) {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Update::Unchanged,
                Err(_) => return Update::Lost,
            }
            // a frame with no present time only moved the mouse pointer
            let update = match (info.LastPresentTime != 0, resource) {
                (true, Some(resource)) => self
                    .copy(&resource)
                    .map_or(Update::Unchanged, Update::Image),
                _ => Update::Unchanged,
            };
            if self.duplication.ReleaseFrame().is_err() {
                return Update::Lost;
            }
            update
        }
    }

    /// Copies the desktop image into CPU memory through a staging texture.
    unsafe fn copy(&mut self, resource: &IDXGIResource) -> Option<Frame> {
        // SAFETY: the staging texture is mapped only while its rows are copied out
        unsafe {
            let texture: ID3D11Texture2D = resource.cast().ok()?;
            let mut description = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut description);
            let staging = match &self.staging {
                Some(staging) if same_size(staging, &description) => staging.clone(),
                _ => {
                    let device = texture.GetDevice().ok()?;
                    let staging_description = D3D11_TEXTURE2D_DESC {
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                        ..description
                    };
                    let mut created: Option<ID3D11Texture2D> = None;
                    device
                        .CreateTexture2D(&staging_description, None, Some(&mut created))
                        .ok()?;
                    self.staging = created.clone();
                    created?
                }
            };
            self.context.CopyResource(&staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .ok()?;
            let stride = mapped.RowPitch as usize;
            let height = description.Height as usize;
            let frame = Frame {
                width: description.Width as usize,
                height,
                stride,
                bgra: std::slice::from_raw_parts(mapped.pData.cast::<u8>(), stride * height)
                    .to_vec(),
            };
            self.context.Unmap(&staging, 0);
            Some(frame)
        }
    }
}

unsafe fn same_size(staging: &ID3D11Texture2D, wanted: &D3D11_TEXTURE2D_DESC) -> bool {
    let mut have = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: have is a valid out parameter
    unsafe { staging.GetDesc(&mut have) };
    have.Width == wanted.Width && have.Height == wanted.Height && have.Format == wanted.Format
}
