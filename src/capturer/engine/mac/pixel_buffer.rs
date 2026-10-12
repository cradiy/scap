use super::Sample;
use crate::capturer::{engine::ChannelItem, RawCapturer};
use core::slice;
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::*;
use objc2_screen_capture_kit::{SCFrameStatus, SCStreamOutputType};
use std::{
    ops::Deref,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub struct PixelBuffer {
    display_time: u64,
    buffer: CFRetained<CVPixelBuffer>,
    sample: Sample,
}

impl PixelBuffer {
    pub fn display_time(&self) -> u64 {
        self.display_time
    }
    pub fn width(&self) -> usize {
        CVPixelBufferGetWidth(&self.buffer)
    }
    pub fn height(&self) -> usize {
        CVPixelBufferGetHeight(&self.buffer)
    }
    pub fn bytes_per_row(&self) -> usize {
        CVPixelBufferGetBytesPerRow(&self.buffer)
    }
    pub fn buffer(&self) -> &CMSampleBuffer {
        &self.sample.buffer
    }

    /// Returns an independently retained CoreVideo buffer without copying pixels.
    pub fn as_core_video(&self) -> CFRetained<CVPixelBuffer> {
        self.buffer.clone()
    }

    pub fn data(&self) -> PixelBufferData<'_> {
        PixelBufferData::new(&self.buffer, None, self.bytes_per_row() * self.height())
    }

    pub fn planes(&self) -> Vec<Plane> {
        (0..CVPixelBufferGetPlaneCount(&self.buffer))
            .map(|index| Plane {
                buffer: self.buffer.clone(),
                index,
            })
            .collect()
    }

    pub(crate) fn new(item: ChannelItem) -> Option<Self> {
        if item.1 != SCStreamOutputType::Screen
            || !matches!(
                item.0.frame_status,
                SCFrameStatus::Complete | SCFrameStatus::Started
            )
        {
            return None;
        }
        let buffer = unsafe { item.0.buffer.image_buffer() }?;
        if CVPixelBufferGetWidth(&buffer) == 0 || CVPixelBufferGetHeight(&buffer) == 0 {
            return None;
        }
        let display_time =
            unsafe { item.0.buffer.presentation_time_stamp().seconds() * 1_000_000_000. } as u64;
        Some(Self {
            buffer,
            display_time,
            sample: item.0,
        })
    }
}

impl From<PixelBuffer> for Sample {
    fn from(buffer: PixelBuffer) -> Self {
        buffer.sample
    }
}

#[derive(Debug)]
pub struct Plane {
    buffer: CFRetained<CVPixelBuffer>,
    index: usize,
}
impl Plane {
    pub fn width(&self) -> usize {
        CVPixelBufferGetWidthOfPlane(&self.buffer, self.index)
    }
    pub fn height(&self) -> usize {
        CVPixelBufferGetHeightOfPlane(&self.buffer, self.index)
    }
    pub fn bytes_per_row(&self) -> usize {
        CVPixelBufferGetBytesPerRowOfPlane(&self.buffer, self.index)
    }
    pub fn data(&self) -> PixelBufferData<'_> {
        PixelBufferData::new(
            &self.buffer,
            Some(self.index),
            self.bytes_per_row() * self.height(),
        )
    }
}

pub struct PixelBufferData<'a> {
    buffer: &'a CVPixelBuffer,
    data: &'a [u8],
}
impl<'a> PixelBufferData<'a> {
    fn new(buffer: &'a CVPixelBuffer, plane: Option<usize>, len: usize) -> Self {
        // Samples are immutable and the guard keeps the read lock until the slice is dropped.
        unsafe {
            assert_eq!(
                CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly),
                0,
                "Cannot lock capture buffer"
            );
            let mut guard = Self { buffer, data: &[] };
            if len != 0 {
                let ptr = plane.map_or_else(
                    || CVPixelBufferGetBaseAddress(buffer),
                    |index| CVPixelBufferGetBaseAddressOfPlane(buffer, index),
                );
                assert!(!ptr.is_null(), "Capture buffer has no CPU mapping");
                guard.data = slice::from_raw_parts(ptr.cast(), len);
            }
            guard
        }
    }
}
impl Deref for PixelBufferData<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.data
    }
}
impl Drop for PixelBufferData<'_> {
    fn drop(&mut self) {
        unsafe {
            CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_video_buffer_outlives_its_source_reference() {
        use std::ptr::NonNull;
        let mut raw = std::ptr::null_mut();
        let source = unsafe {
            assert_eq!(
                CVPixelBufferCreate(
                    None,
                    16,
                    8,
                    kCVPixelFormatType_32BGRA,
                    None,
                    NonNull::from(&mut raw)
                ),
                0
            );
            CFRetained::from_raw(NonNull::new(raw).unwrap())
        };
        let retained = source.clone();
        assert!(std::ptr::eq(&*retained, &*source));
        drop(source);
        assert_eq!(CVPixelBufferGetWidth(&retained), 16);
        assert_eq!(CVPixelBufferGetHeight(&retained), 8);
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
