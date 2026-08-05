use std::{
    cell::RefCell,
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU8},
        mpsc::{sync_channel, RecvError, SendError, Sender, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context as _, Result};
use pipewire as pw;
use pw::{
    context::ContextBox,
    loop_::Timeout,
    main_loop::MainLoopBox,
    node::Node,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
        },
        pod::{ChoiceValue, Pod, Property, PropertyFlags, Value},
        sys::{spa_buffer, spa_meta_header, SPA_META_Header},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction},
    },
    stream::{Stream, StreamState},
    types::ObjectType,
};

use crate::{
    capturer::Options,
    frame::{BGRxFrame, Frame, RGBFrame, RGBxFrame, XBGRFrame},
};

use self::{
    dma_buf::{DmaBufFrame, DmaBufReader},
    portal::ScreenCastPortal,
};

use super::LinuxCapturerImpl;

mod dma_buf;
mod portal;

// TODO: Move to wayland capturer with Arc<>
static CAPTURER_STATE: AtomicU8 = AtomicU8::new(0);
static STREAM_STATE_CHANGED_TO_ERROR: AtomicBool = AtomicBool::new(false);
struct ListenerUserData {
    pub tx: Sender<Result<Frame>>,
    pub format: spa::param::video::VideoInfoRaw,
    dma_buf_reader: Option<DmaBufReader>,
}

fn param_changed_callback(
    _stream: &Stream,
    user_data: &mut ListenerUserData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    if let Err(err) = user_data.format.parse(param) {
        user_data
            .tx
            .send(Err(
                anyhow!(err).context("Failed to parse PipeWire video format.")
            ))
            .ok();
        return;
    }
}

fn state_changed_callback(
    _stream: &Stream,
    user_data: &mut ListenerUserData,
    _old: StreamState,
    new: StreamState,
) {
    if let StreamState::Error(error) = new {
        log::debug!("pipewire: State changed to error({error})");
        user_data
            .tx
            .send(Err(anyhow!("PipeWire stream error: {error}")))
            .ok();
        STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

unsafe fn get_timestamp(buffer: *mut spa_buffer) -> i64 {
    let n_metas = (*buffer).n_metas;
    if n_metas > 0 {
        let mut meta_ptr = (*buffer).metas;
        let metas_end = (*buffer).metas.wrapping_add(n_metas as usize);
        while meta_ptr != metas_end {
            if (*meta_ptr).type_ == SPA_META_Header {
                let meta_header: &mut spa_meta_header =
                    &mut *((*meta_ptr).data as *mut spa_meta_header);
                return meta_header.pts;
            }
            meta_ptr = meta_ptr.wrapping_add(1);
        }
        0
    } else {
        0
    }
}

fn process_callback(stream: &Stream, user_data: &mut ListenerUserData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    let frame_result = match process_callback_impl(buffer, user_data) {
        Ok(None) => None,
        Ok(Some(frame)) => Some(Ok(frame)),
        Err(err) => Some(Err(err)),
    };
    if let Some(frame_result) = frame_result {
        match user_data.tx.send(frame_result) {
            Ok(()) => {}
            Err(SendError(_)) => {
                log::debug!("Frame receiver was dropped.")
            }
        }
    }
    if !buffer.is_null() {
        unsafe { stream.queue_raw_buffer(buffer) };
    }
}

fn copy_tightly_packed_rows(
    data: &[u8],
    stride: i32,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<Vec<u8>> {
    let packed_stride = (width as usize)
        .checked_mul(bytes_per_pixel)
        .context("Wayland screen capture row size overflowed.")?;
    let source_stride = stride.unsigned_abs() as usize;
    if source_stride < packed_stride {
        return Err(anyhow!(
            "Wayland screen capture row stride {source_stride} is smaller than {packed_stride}."
        ));
    }

    let packed_len = packed_stride
        .checked_mul(height as usize)
        .context("Wayland screen capture frame size overflowed.")?;
    if source_stride == packed_stride && stride >= 0 {
        return Ok(data[..packed_len].to_vec());
    }

    let mut packed = Vec::with_capacity(packed_len);
    for output_row in 0..height as usize {
        let source_row = if stride < 0 {
            height as usize - output_row - 1
        } else {
            output_row
        };
        let start = source_row * source_stride;
        packed.extend_from_slice(&data[start..start + packed_stride]);
    }
    Ok(packed)
}

fn process_callback_impl(
    buffer: *mut pipewire::sys::pw_buffer,
    user_data: &mut ListenerUserData,
) -> Result<Option<Frame>> {
    if buffer.is_null() {
        return Err(anyhow!("Wayland screen capture out of buffers."));
    }
    let buffer = unsafe { (*buffer).buffer };
    if buffer.is_null() {
        // TODO: This matches the behavior of the original code by not having an error here.
        log::error!("Buffer pointer unexpectedly null in Wayland screen capture.");
        return Ok(None);
    }

    let timestamp = unsafe { get_timestamp(buffer) };

    let n_datas = unsafe { (*buffer).n_datas };
    if n_datas < 1 {
        return Ok(None);
    }
    let data = unsafe { &*(*buffer).datas };

    let frame_size = user_data.format.size();
    let chunk = unsafe { &*data.chunk };
    if chunk.size == 0 && data.fd < 0 {
        return Ok(None);
    }
    let bytes_per_pixel = match user_data.format.format() {
        VideoFormat::RGB => 3,
        VideoFormat::RGBx | VideoFormat::xBGR | VideoFormat::BGRx => 4,
        format => return Err(anyhow!("Unsupported frame format received: {format:?}")),
    };
    let packed_row_bytes = (frame_size.width as usize)
        .checked_mul(bytes_per_pixel)
        .context("Wayland screen capture row size overflowed.")?;
    let row_bytes = if chunk.stride == 0 {
        packed_row_bytes
    } else {
        chunk.stride.unsigned_abs() as usize
    };
    let trailing_rows = (frame_size.height as usize).saturating_sub(1);
    let frame_bytes = row_bytes
        .checked_mul(trailing_rows)
        .and_then(|bytes| bytes.checked_add(packed_row_bytes))
        .context("Wayland screen capture frame size overflowed.")?;
    let offset = chunk.offset as usize;
    let (frame_data, dma_buf_readback) = if !data.data.is_null() {
        let memory_offset = if chunk.stride < 0 {
            offset
                .checked_sub(
                    row_bytes
                        .checked_mul(trailing_rows)
                        .context("Wayland screen capture frame offset overflowed.")?,
                )
                .context("PipeWire screen capture buffer has an invalid negative stride.")?
        } else {
            offset
        };
        if memory_offset
            .checked_add(frame_bytes)
            .is_none_or(|end| end > data.maxsize as usize)
        {
            return Err(anyhow!("PipeWire screen capture buffer is too small."));
        }
        unsafe {
            let start = data.data.cast::<u8>().add(memory_offset);
            let rows = std::slice::from_raw_parts(start, frame_bytes);
            (
                copy_tightly_packed_rows(
                    rows,
                    if chunk.stride == 0 {
                        packed_row_bytes as i32
                    } else {
                        chunk.stride
                    },
                    frame_size.width,
                    frame_size.height,
                    bytes_per_pixel,
                )?,
                false,
            )
        }
    } else if data.fd >= 0 {
        let dma_buf = DmaBufFrame {
            fd: data.fd as i32,
            width: frame_size.width,
            height: frame_size.height,
            offset: data.mapoffset + chunk.offset,
            stride: row_bytes as u32,
            modifier: user_data.format.modifier(),
        };
        let pixels = if let Some(reader) = &user_data.dma_buf_reader {
            reader.read(&dma_buf)?
        } else {
            let (reader, pixels) = DmaBufReader::open_and_read(&dma_buf)?;
            user_data.dma_buf_reader = Some(reader);
            pixels
        };
        (pixels, true)
    } else {
        return Ok(None);
    };

    if dma_buf_readback {
        return Ok(Some(Frame::RGBx(RGBxFrame {
            display_time: timestamp as u64,
            width: frame_size.width as i32,
            height: frame_size.height as i32,
            data: frame_data,
        })));
    }

    match user_data.format.format() {
        VideoFormat::RGBx => Ok(Some(Frame::RGBx(RGBxFrame {
            display_time: timestamp as u64,
            width: frame_size.width as i32,
            height: frame_size.height as i32,
            data: frame_data,
        }))),
        VideoFormat::RGB => Ok(Some(Frame::RGB(RGBFrame {
            display_time: timestamp as u64,
            width: frame_size.width as i32,
            height: frame_size.height as i32,
            data: frame_data,
        }))),
        VideoFormat::xBGR => Ok(Some(Frame::XBGR(XBGRFrame {
            display_time: timestamp as u64,
            width: frame_size.width as i32,
            height: frame_size.height as i32,
            data: frame_data,
        }))),
        VideoFormat::BGRx => Ok(Some(Frame::BGRx(BGRxFrame {
            display_time: timestamp as u64,
            width: frame_size.width as i32,
            height: frame_size.height as i32,
            data: frame_data,
        }))),
        _ => Err(anyhow!("Unsupported frame format received")),
    }
}

fn video_modifiers(param: &Pod) -> Option<Vec<i64>> {
    let (_, Value::Object(object)) =
        pw::spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes()).ok()?
    else {
        return None;
    };
    let format = object
        .properties
        .iter()
        .find(|property| property.key == FormatProperties::VideoFormat.as_raw())?;
    if !matches!(
        format.value,
        Value::Id(format) if format.0 == VideoFormat::BGRx.as_raw()
    ) {
        return None;
    }
    let modifier = object
        .properties
        .iter()
        .find(|property| property.key == FormatProperties::VideoModifier.as_raw())?;

    let (default, alternatives) = match &modifier.value {
        Value::Long(modifier) => (*modifier, Vec::new()),
        Value::Choice(ChoiceValue::Long(Choice(
            _,
            ChoiceEnum::Enum {
                default,
                alternatives,
            },
        ))) => (*default, alternatives.clone()),
        _ => return None,
    };
    let mut modifiers = vec![default];
    for modifier in alternatives {
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    Some(modifiers)
}

fn start_pipewire_capturer(
    options: Options,
    tx: Sender<Result<Frame>>,
    stream_id: u32,
    remote: OwnedFd,
    ready_sender: &SyncSender<Result<()>>,
) -> Result<()> {
    pw::init();

    let mainloop = MainLoopBox::new(None)?;
    let context = ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect_fd(remote, None)?;

    let modifiers = Rc::new(RefCell::new(None));
    let registry = Rc::new(core.get_registry()?);
    let registry_ptr = registry.as_raw_ptr() as usize;
    let capture_node = Rc::new(RefCell::new(None));
    let capture_node_listener = Rc::new(RefCell::new(None));
    let callback_node = capture_node.clone();
    let callback_node_listener = capture_node_listener.clone();
    let registry_modifiers = modifiers.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |object| {
            if object.id != stream_id || object.type_ != ObjectType::Node {
                return;
            }

            if callback_node.borrow().is_some() {
                return;
            }

            // The registry outlives its listener and this callback.
            let registry = unsafe { &*(registry_ptr as *const pw::registry::Registry) };
            let node: Node = registry
                .bind(object)
                .expect("Failed to bind PipeWire capture node.");
            let listener_modifiers = registry_modifiers.clone();
            let listener = node
                .add_listener_local()
                .param(move |_sequence, id, _index, _next, param| {
                    if id != pw::spa::param::ParamType::EnumFormat {
                        return;
                    }
                    if let Some(modifiers) = param.and_then(video_modifiers) {
                        *listener_modifiers.borrow_mut() = Some(modifiers);
                    }
                })
                .register();
            node.enum_params(1, Some(pw::spa::param::ParamType::EnumFormat), 0, u32::MAX);
            *callback_node_listener.borrow_mut() = Some(listener);
            *callback_node.borrow_mut() = Some(node);
        })
        .register();

    let modifier_deadline = Instant::now() + Duration::from_secs(2);
    while modifiers.borrow().is_none() && Instant::now() < modifier_deadline {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(Duration::from_millis(50)));
    }
    let modifiers = modifiers
        .borrow_mut()
        .take()
        .context("PipeWire capture node did not expose a supported BGRx DMA-BUF modifier.")?;

    let user_data = ListenerUserData {
        tx,
        format: Default::default(),
        dma_buf_reader: None,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "scap",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(state_changed_callback)
        .param_changed(param_changed_callback)
        .process(process_callback)
        .register()?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Id,
            pw::spa::param::video::VideoFormat::BGRx
        ),
        {
            let default = modifiers[0];
            let mut property = Property::new(
                FormatProperties::VideoModifier.as_raw(),
                Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default,
                        alternatives: modifiers,
                    },
                ))),
            );
            property.flags = PropertyFlags::MANDATORY;
            property
        },
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction {
                num: options.fps,
                denom: 1
            },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();

    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .context("Not enough space in screen capture format param.")?];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    if ready_sender.send(Ok(())).is_err() {
        return Ok(());
    }

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 1
        && !STREAM_STATE_CHANGED_TO_ERROR.load(std::sync::atomic::Ordering::Relaxed)
    {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(Duration::from_millis(100)));
    }

    Ok(())
}

// TODO: Format negotiation
fn pipewire_capturer(
    options: Options,
    tx: Sender<Result<Frame>>,
    ready_sender: &SyncSender<Result<()>>,
    stream_id: u32,
    remote: OwnedFd,
) {
    if let Err(err) = start_pipewire_capturer(options, tx, stream_id, remote, ready_sender) {
        ready_sender.send(Err(err)).ok();
    }
}

pub struct WaylandCapturer {
    capturer_join_handle: Option<JoinHandle<()>>,
    // The pipewire stream is deleted when the connection is dropped.
    // That's why we keep it alive
    _connection: dbus::blocking::Connection,
}

impl WaylandCapturer {
    // TODO: Error handling
    pub fn new(options: &Options, tx: Sender<Result<Frame>>) -> Result<Self> {
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
        let connection = dbus::blocking::Connection::new_session()
            .context("Failed to create dbus connection")?;
        let (stream_id, remote) = ScreenCastPortal::new(&connection)
            .show_cursor(options.show_cursor)
            .context("Unsupported screen capture cursor display mode")?
            .create_stream()
            .context("Failed to get screen capture stream")?
            .into_parts();
        let remote = unsafe { OwnedFd::from_raw_fd(remote.into_raw_fd()) };

        // TODO: Fix this hack
        let options = options.clone();
        let (ready_sender, ready_recv) = sync_channel(1);
        let capturer_join_handle = std::thread::spawn(move || {
            pipewire_capturer(options, tx, &ready_sender, stream_id, remote)
        });

        match ready_recv.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                return Err(anyhow!(err));
            }
            Err(RecvError) => {
                return Err(anyhow!(
                    "Wayland screen capture bug: stream unexpectedly dropped."
                ));
            }
        }

        Ok(Self {
            capturer_join_handle: Some(capturer_join_handle),
            _connection: connection,
        })
    }
}

impl LinuxCapturerImpl for WaylandCapturer {
    fn start_capture(&mut self) {
        CAPTURER_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn stop_capture(&mut self) {
        CAPTURER_STATE.store(2, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.capturer_join_handle.take() {
            match handle.join() {
                Ok(()) => {}
                Err(err) => log::error!("Failed to join Wayland screen capture thread: {:?}", err),
            }
        }
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::copy_tightly_packed_rows;

    #[test]
    fn removes_row_padding() {
        let data = [1, 2, 3, 4, 99, 99, 5, 6, 7, 8];

        let packed = copy_tightly_packed_rows(&data, 6, 1, 2, 4).unwrap();

        assert_eq!(packed, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn normalizes_negative_stride() {
        let data = [5, 6, 7, 8, 1, 2, 3, 4];

        let packed = copy_tightly_packed_rows(&data, -4, 1, 2, 4).unwrap();

        assert_eq!(packed, [1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
