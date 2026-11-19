//! macOS capture through ScreenCaptureKit (macOS 12.3 and newer). The system
//! scales the screen to the recording size on the GPU and delivers at most
//! one frame per second, and only when something on screen changed, so the
//! CPU does almost nothing while the screen is still. `grab` returns the
//! latest frame.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

use super::Frame;
use crate::recorder::encode::target_size;

type Latest = Arc<Mutex<Option<Frame>>>;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PulseFrameSink"]
    #[ivars = Latest]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // frames that only say "nothing changed" carry no image
            // SAFETY: the sample buffer is valid for the duration of this call
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            if let Some(frame) = copy_frame(&image) {
                *self.ivars().lock().expect("capture lock") = Some(frame);
            }
        }
    }
);

fn copy_frame(buffer: &objc2_core_video::CVPixelBuffer) -> Option<Frame> {
    // SAFETY: the buffer is locked read-only while its memory is copied, then unlocked
    unsafe {
        if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let width = CVPixelBufferGetWidth(buffer);
        let height = CVPixelBufferGetHeight(buffer);
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        let frame = (!base.is_null()).then(|| Frame {
            width,
            height,
            stride,
            bgra: std::slice::from_raw_parts(base, stride * height).to_vec(),
        });
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        frame
    }
}

/// Moves an Objective-C object across threads. ScreenCaptureKit hands back
/// content on its own queue; the objects are only used again on ours.
struct Sendable<T>(T);
// SAFETY: see above; the object is never used from two threads at once
unsafe impl<T> Send for Sendable<T> {}

pub struct Screen {
    stream: Retained<SCStream>,
    latest: Latest,
    // kept alive for as long as the stream may call it
    _sink: Retained<FrameSink>,
}

impl Screen {
    pub fn open() -> Result<Screen, String> {
        let content = shareable_content()?;
        // SAFETY: plain ScreenCaptureKit calls on objects this function owns
        unsafe {
            let display = content
                .displays()
                .firstObject()
                .ok_or("no display to record")?;
            let (width, height) = target_size(display.width() as usize, display.height() as usize);
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width);
            config.setHeight(height);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            config.setShowsCursor(true);
            config.setQueueDepth(3);
            config.setCapturesAudio(false);

            let latest: Latest = Arc::default();
            let sink = FrameSink::alloc().set_ivars(Arc::clone(&latest));
            let sink: Retained<FrameSink> = msg_send![super(sink), init];
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            );
            let queue = DispatchQueue::new("club.hack.pulse.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*sink),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|error| error.localizedDescription().to_string())?;

            let (sender, receiver) = mpsc::channel();
            let started = RcBlock::new(move |error: *mut NSError| {
                let message = NonNull::new(error)
                    .map(|error| error.as_ref().localizedDescription().to_string());
                let _ = sender.send(message);
            });
            stream.startCaptureWithCompletionHandler(Some(&started));
            match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(None) => {}
                Ok(Some(message)) => return Err(format!("cannot start screen capture: {message}")),
                Err(_) => return Err("screen capture did not start".into()),
            }
            Ok(Screen {
                stream,
                latest,
                _sink: sink,
            })
        }
    }

    pub fn grab(&mut self) -> Option<Frame> {
        let latest = self.latest.lock().expect("capture lock");
        latest.as_ref().map(|frame| Frame {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            bgra: frame.bgra.clone(),
        })
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either a valid content object or a valid error
            let result = unsafe {
                match Retained::retain(content) {
                    Some(content) => Ok(Sendable(content)),
                    None => Err(NonNull::new(error)
                        .map(|error| error.as_ref().localizedDescription().to_string())
                        .unwrap_or_default()),
                }
            };
            let _ = sender.send(result);
        },
    );
    // SAFETY: the handler is retained by ScreenCaptureKit until it is called
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(content)) => Ok(content.0),
        Ok(Err(message)) => Err(format!(
            "macOS did not allow screen recording ({message}). Allow Pulse in System Settings > Privacy & Security > Screen Recording"
        )),
        Err(_) => Err("macOS did not answer the screen recording request".into()),
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // SAFETY: stopping a started stream; no completion handler needed
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}
