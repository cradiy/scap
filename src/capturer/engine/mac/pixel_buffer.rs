use core::slice;
use core_foundation::base::TCFType;
use core_video::pixel_buffer::CVPixelBuffer;
use core_video_sys::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferRef, CVPixelBufferUnlockBaseAddress,
};
use screencapturekit::{
    cm_sample_buffer::CMSampleBuffer, sc_output_handler::SCStreamOutputType,
    sc_types::SCFrameStatus,
};
use screencapturekit_sys::cm_sample_buffer_ref::CMSampleBufferGetImageBuffer;
use std::{
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use crate::capturer::{engine::ChannelItem, RawCapturer};

pub struct PixelBuffer {
    display_time: u64,
    width: usize,
    height: usize,
    bytes_per_row: usize,
    buffer: CMSampleBuffer,
}

impl PixelBuffer {
    pub fn display_time(&self) -> u64 {
        self.display_time
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn buffer(&self) -> &CMSampleBuffer {
        &self.buffer
    }

    /// Returns an independently retained CoreVideo buffer without copying pixels.
    /// The returned buffer remains valid after this capture frame is dropped.
    pub fn as_core_video(&self) -> CVPixelBuffer {
        // SAFETY: `new` only accepts samples with a non-null image buffer, and
        // this frame owns the sample while the image buffer is retained.
        unsafe { retain_pixel_buffer(sample_buffer_to_pixel_buffer(&self.buffer)) }
    }

    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    pub fn data(&self) -> PixelBufferData<'_> {
        unsafe {
            let pixel_buffer = sample_buffer_to_pixel_buffer(&self.buffer);

            CVPixelBufferLockBaseAddress(pixel_buffer, 0);

            let base_address = CVPixelBufferGetBaseAddress(pixel_buffer);

            PixelBufferData {
                buffer: pixel_buffer,
                data: slice::from_raw_parts(
                    base_address as *mut _,
                    self.bytes_per_row * self.height,
                ),
            }
        }
    }

    pub fn planes(&self) -> Vec<Plane> {
        unsafe {
            let pixel_buffer = sample_buffer_to_pixel_buffer(&self.buffer);
            let count = CVPixelBufferGetPlaneCount(pixel_buffer);

            CVPixelBufferLockBaseAddress(pixel_buffer, 0);

            (0..count)
                .map(|i| Plane {
                    buffer: pixel_buffer,
                    width: CVPixelBufferGetWidthOfPlane(pixel_buffer, i),
                    height: CVPixelBufferGetHeightOfPlane(pixel_buffer, i),
                    bytes_per_row: CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, i),
                    index: i,
                })
                .collect()
        }
    }

    pub(crate) fn new(item: ChannelItem) -> Option<Self> {
        if !matches!(item.1, SCStreamOutputType::Screen)
            || !matches!(
                item.0.frame_status,
                SCFrameStatus::Complete | SCFrameStatus::Started
            )
        {
            return None;
        }
        unsafe {
            let pixel_buffer = sample_buffer_to_pixel_buffer(&item.0);
            if pixel_buffer.is_null() {
                return None;
            }
            let (width, height) = pixel_buffer_bounds(pixel_buffer);
            if width == 0 || height == 0 {
                return None;
            }
            Some(Self {
                display_time: pixel_buffer_display_time(&item.0),
                width,
                height,
                bytes_per_row: pixel_buffer_bytes_per_row(pixel_buffer),
                buffer: item.0,
            })
        }
    }
}

impl From<PixelBuffer> for CMSampleBuffer {
    fn from(buffer: PixelBuffer) -> Self {
        buffer.buffer
    }
}

#[derive(Debug)]
pub struct Plane {
    buffer: CVPixelBufferRef,
    index: usize,
    width: usize,
    height: usize,
    bytes_per_row: usize,
}

impl Plane {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    pub fn data(&self) -> PixelBufferData<'_> {
        unsafe {
            CVPixelBufferLockBaseAddress(self.buffer, 0);

            let base_address = CVPixelBufferGetBaseAddressOfPlane(self.buffer, self.index);

            PixelBufferData {
                buffer: self.buffer,
                data: slice::from_raw_parts(
                    base_address as *mut _,
                    self.bytes_per_row * self.height,
                ),
            }
        }
    }
}

pub struct PixelBufferData<'a> {
    buffer: CVPixelBufferRef,
    data: &'a [u8],
}

impl<'a> Deref for PixelBufferData<'a> {
    type Target = [u8];

    fn deref(&self) -> &'a Self::Target {
        self.data
    }
}

impl<'a> Drop for PixelBufferData<'a> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, 0) };
    }
}

impl RawCapturer<'_> {
    /// Waits for the next nonempty screen frame without copying its pixels.
    pub fn get_next_pixel_buffer(&self) -> Result<PixelBuffer, mpsc::RecvError> {
        loop {
            if let Some(frame) = self
                .get_next_pixel_buffer_timeout(Duration::from_millis(100))
                .map_err(|_| mpsc::RecvError)?
            {
                return Ok(frame);
            }
        }
    }

    /// Waits up to `timeout` for a nonempty screen frame without copying pixels.
    /// Returns `None` on timeout, allowing callers to check cancellation even
    /// when the captured screen is idle. Capture errors are returned as errors.
    pub fn get_next_pixel_buffer_timeout(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<Option<PixelBuffer>> {
        receive_pixel_buffer(&self.capturer.rx, &self.capturer.engine.error_flag, timeout)
    }
}

fn receive_pixel_buffer(
    rx: &mpsc::Receiver<anyhow::Result<ChannelItem>>,
    error_flag: &AtomicBool,
    timeout: Duration,
) -> anyhow::Result<Option<PixelBuffer>> {
    let start = Instant::now();
    loop {
        if error_flag.load(Ordering::Relaxed) {
            anyhow::bail!("Screen capture stream failed");
        }
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return Ok(None);
        };
        match rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(item) => {
                if let Some(frame) = PixelBuffer::new(item?) {
                    return Ok(Some(frame));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Screen capture stream disconnected");
            }
        }
    }
}

unsafe fn retain_pixel_buffer(buffer: CVPixelBufferRef) -> CVPixelBuffer {
    CVPixelBuffer::wrap_under_get_rule(buffer.cast())
}

pub unsafe fn sample_buffer_to_pixel_buffer(sample_buffer: &CMSampleBuffer) -> CVPixelBufferRef {
    let buffer_ref = &(*sample_buffer.sys_ref);

    CMSampleBufferGetImageBuffer(buffer_ref) as CVPixelBufferRef
}

pub unsafe fn pixel_buffer_bounds(pixel_buffer: CVPixelBufferRef) -> (usize, usize) {
    let width = CVPixelBufferGetWidth(pixel_buffer);
    let height = CVPixelBufferGetHeight(pixel_buffer);

    (width, height)
}

pub unsafe fn pixel_buffer_bytes_per_row(pixel_buffer: CVPixelBufferRef) -> usize {
    CVPixelBufferGetBytesPerRow(pixel_buffer)
}

pub unsafe fn pixel_buffer_display_time(sample_buffer: &CMSampleBuffer) -> u64 {
    sample_buffer.sys_ref.get_presentation_timestamp().value as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_video::pixel_buffer::kCVPixelFormatType_32BGRA;

    #[test]
    fn core_video_buffer_outlives_its_source_reference() {
        let source = CVPixelBuffer::new(kCVPixelFormatType_32BGRA, 16, 8, None).unwrap();
        let count = source.retain_count();
        let retained = unsafe { retain_pixel_buffer(source.as_concrete_TypeRef().cast()) };
        assert_eq!(retained.as_concrete_TypeRef(), source.as_concrete_TypeRef());
        assert_eq!(source.retain_count(), count + 1);
        drop(source);
        assert_eq!(retained.get_width(), 16);
        assert_eq!(retained.get_height(), 8);
    }

    #[test]
    fn idle_capture_times_out_and_disconnect_is_reported() {
        let (tx, rx) = mpsc::channel();
        let error = AtomicBool::new(false);
        assert!(receive_pixel_buffer(&rx, &error, Duration::from_millis(1))
            .unwrap()
            .is_none());
        drop(tx);
        assert!(receive_pixel_buffer(&rx, &error, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn capture_errors_are_propagated() {
        let (tx, rx) = mpsc::channel();
        let error = AtomicBool::new(false);
        tx.send(Err(anyhow::anyhow!("capture failed"))).unwrap();
        let result = receive_pixel_buffer(&rx, &error, Duration::from_secs(1));
        assert_eq!(result.err().unwrap().to_string(), "capture failed");
        error.store(true, Ordering::Relaxed);
        assert!(receive_pixel_buffer(&rx, &error, Duration::from_secs(1)).is_err());
    }
}
