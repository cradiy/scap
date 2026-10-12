use super::PixelBuffer;
use crate::frame::{
    convert_bgra_to_rgb, remove_alpha_channel, BGRAFrame, BGRFrame, Frame, FrameType, RGBFrame,
    YUVFrame,
};
use core_foundation::{
    array::CFArray, base::TCFType, dictionary::CFDictionary, number::CFNumber, string::CFString,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::{SCFrameStatus, SCStreamFrameInfoStatus};

pub(super) fn frame_status(sample: &CMSampleBuffer) -> Option<SCFrameStatus> {
    unsafe {
        let attachments = sample.sample_attachments_array(false)?;
        let array: CFArray<CFDictionary<CFString, core_foundation::base::CFType>> =
            CFArray::wrap_under_get_rule(CFRetained::as_ptr(&attachments).as_ptr().cast());
        let first = array.get(0)?;
        let key = CFString::wrap_under_get_rule(
            (SCStreamFrameInfoStatus as *const objc2_foundation::NSString).cast(),
        );
        let status = first.find(key)?.downcast::<CFNumber>()?.to_i64()?;
        Some(SCFrameStatus(status as isize))
    }
}

pub(super) fn create_frame(buffer: &PixelBuffer, output_type: FrameType) -> Option<Frame> {
    let display_time = buffer.display_time();
    let width = i32::try_from(buffer.width()).ok()?;
    let height = i32::try_from(buffer.height()).ok()?;
    if matches!(output_type, FrameType::YUVFrame) {
        let planes = buffer.planes();
        let y = planes.first()?;
        let uv = planes.get(1)?;
        return Some(Frame::YUVFrame(YUVFrame {
            display_time,
            width,
            height,
            luminance_bytes: y.data().to_vec(),
            luminance_stride: y.bytes_per_row() as i32,
            chrominance_bytes: uv.data().to_vec(),
            chrominance_stride: uv.bytes_per_row() as i32,
        }));
    }
    let mapped = buffer.data();
    let mut pixels = Vec::with_capacity(buffer.width() * buffer.height() * 4);
    for row in mapped
        .chunks_exact(buffer.bytes_per_row())
        .take(buffer.height())
    {
        pixels.extend_from_slice(&row[..buffer.width() * 4]);
    }
    Some(match output_type {
        FrameType::BGRAFrame => Frame::BGRA(BGRAFrame {
            display_time,
            width,
            height,
            data: pixels,
        }),
        FrameType::BGR0 => Frame::BGR0(BGRFrame {
            display_time,
            width,
            height,
            data: remove_alpha_channel(pixels),
        }),
        FrameType::RGB => Frame::RGB(RGBFrame {
            display_time,
            width,
            height,
            data: convert_bgra_to_rgb(pixels),
        }),
        FrameType::YUVFrame => unreachable!(),
    })
}
