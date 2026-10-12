use super::ChannelItem;
use crate::{
    capturer::{Area, Options, Point, Resolution, Size},
    frame::{Frame, FrameType},
    targets::{self, Target},
};
use anyhow::{Context as _, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    define_class, msg_send, rc::Retained, runtime::ProtocolObject, AnyThread, DefinedClass,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};
use std::{
    cmp,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

mod pixel_buffer;
mod pixelformat;
pub use pixel_buffer::PixelBuffer;

pub struct Sample {
    pub buffer: CFRetained<CMSampleBuffer>,
    pub frame_status: SCFrameStatus,
}
// ScreenCaptureKit publishes immutable samples. Retaining them preserves the
// pixel buffer until all consumers finish reading it.
unsafe impl Send for Sample {}

pub(crate) struct ShareableContent(pub Retained<SCShareableContent>);
// Shareable content is an immutable snapshot supplied by ScreenCaptureKit.
unsafe impl Send for ShareableContent {}

pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = unsafe {
                if let Some(error) = error.as_ref() {
                    Err(anyhow::anyhow!("{error}"))
                } else {
                    Retained::retain(content)
                        .map(ShareableContent)
                        .context("ScreenCaptureKit returned no shareable content")
                }
            };
            let _ = tx.send(result);
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    Ok(rx.recv_timeout(Duration::from_secs(30))??.0)
}

struct OutputState {
    tx: mpsc::Sender<Result<ChannelItem>>,
    error_flag: Arc<AtomicBool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ScapStreamOutput"]
    #[ivars = OutputState]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            use objc2_core_foundation::Type;
            let Some(frame_status) = pixelformat::frame_status(sample) else {
                return;
            };
            let sample = Sample {
                buffer: sample.retain(),
                frame_status,
            };
            let _ = self.ivars().tx.send(Ok((sample, kind)));
        }
    }
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stopped(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().error_flag.store(true, Ordering::Relaxed);
            let _ = self.ivars().tx.send(Err(anyhow::anyhow!("{error}")));
        }
    }
);

pub struct CaptureStream {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    started: bool,
}

impl CaptureStream {
    fn set_running(&self, running: bool) -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(1);
        let handler = RcBlock::new(move |error: *mut NSError| {
            let result =
                unsafe { error.as_ref() }.map_or(Ok(()), |error| Err(anyhow::anyhow!("{error}")));
            let _ = tx.send(result);
        });
        unsafe {
            if running {
                self.stream
                    .startCaptureWithCompletionHandler(Some(&handler));
            } else {
                self.stream.stopCaptureWithCompletionHandler(Some(&handler));
            }
        }
        rx.recv_timeout(Duration::from_secs(30))?
    }

    pub fn start_capture(&mut self) -> Result<()> {
        self.set_running(true)?;
        self.started = true;
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Result<()> {
        if self.started {
            self.set_running(false)?;
            self.started = false;
        }
        Ok(())
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                self.stream.stopCaptureWithCompletionHandler(None);
            }
        }
    }
}

pub fn create_capturer(
    options: &Options,
    tx: mpsc::Sender<Result<ChannelItem>>,
    error_flag: Arc<AtomicBool>,
) -> Result<(CaptureStream, Target)> {
    let target = options
        .target
        .clone()
        .map(Ok)
        .unwrap_or_else(|| targets::get_main_display().map(Target::Display))?;
    let content = shareable_content()?;
    // The snapshot, filter and configuration are owned while the stream is initialized.
    unsafe {
        let filter = match &target {
            Target::Window(window) => {
                let selected = content
                    .windows()
                    .into_iter()
                    .find(|candidate| candidate.windowID() == window.id)
                    .context("Capture window is no longer available")?;
                SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    &selected,
                )
            }
            Target::Display(display) => {
                let selected = content
                    .displays()
                    .into_iter()
                    .find(|candidate| candidate.displayID() == display.id)
                    .context("Capture display is no longer available")?;
                let excluded: Vec<_> = content.windows().into_iter().filter(|window| {
                    options.excluded_targets.as_ref().is_some_and(|targets| targets.iter().any(|target| {
                        matches!(target, Target::Window(excluded) if excluded.id == window.windowID())
                    }))
                }).collect();
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &selected,
                    &NSArray::from_retained_slice(&excluded),
                )
            }
        };
        let config = SCStreamConfiguration::new();
        let area = get_crop_area(options);
        let [width, height] = get_output_frame_size(options);
        anyhow::ensure!(
            width > 0 && height > 0 && options.fps > 0,
            "Capture dimensions and FPS must be positive"
        );
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setSourceRect(CGRect::new(
            CGPoint::new(area.origin.x, area.origin.y),
            CGSize::new(area.size.width, area.size.height),
        ));
        config.setPixelFormat(match options.output_type {
            FrameType::YUVFrame => u32::from_be_bytes(*b"420v"),
            _ => u32::from_be_bytes(*b"BGRA"),
        });
        config.setShowsCursor(options.show_cursor);
        config.setMinimumFrameInterval(CMTime::new(1, i32::try_from(options.fps)?));
        let output = Output::alloc().set_ivars(OutputState { tx, error_flag });
        let output: Retained<Output> = msg_send![super(output), init];
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        );
        let queue = DispatchQueue::new("org.gpui.scap.capture", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok((
            CaptureStream {
                stream,
                _output: output,
                _queue: queue,
                started: false,
            },
            target,
        ))
    }
}
pub fn get_output_frame_size(options: &Options) -> [u32; 2] {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display().unwrap()));

    let scale_factor = targets::get_scale_factor(&target);
    let source_rect = get_crop_area(options);

    // Calculate the output height & width based on the required resolution
    // Output width and height need to be multiplied by scale (or dpi)
    let mut output_width = (source_rect.size.width as u32) * (scale_factor as u32);
    let mut output_height = (source_rect.size.height as u32) * (scale_factor as u32);
    // 1200x800
    match options.output_resolution {
        Resolution::Captured => {}
        _ => {
            let [resolved_width, resolved_height] = options
                .output_resolution
                .value((source_rect.size.width as f32) / (source_rect.size.height as f32));
            // 1280 x 853
            output_width = cmp::min(output_width, resolved_width);
            output_height = cmp::min(output_height, resolved_height);
        }
    }

    output_width -= output_width % 2;
    output_height -= output_height % 2;

    [output_width, output_height]
}

pub fn get_crop_area(options: &Options) -> Area {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display().unwrap()));

    let (width, height) = targets::get_target_dimensions(&target);

    options
        .crop_area
        .as_ref()
        .map(|val| {
            let input_width = val.size.width + (val.size.width % 2.0);
            let input_height = val.size.height + (val.size.height % 2.0);

            Area {
                origin: Point {
                    x: val.origin.x,
                    y: val.origin.y,
                },
                size: Size {
                    width: input_width,
                    height: input_height,
                },
            }
        })
        .unwrap_or_else(|| Area {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: width as f64,
                height: height as f64,
            },
        })
}

pub fn process_sample_buffer(
    sample: Sample,
    kind: SCStreamOutputType,
    output_type: FrameType,
) -> Option<Frame> {
    let pixel_buffer = PixelBuffer::new((sample, kind))?;
    pixelformat::create_frame(&pixel_buffer, output_type)
}
