use std::{
    ffi::{c_int, c_void},
    fs::{File, OpenOptions},
    path::PathBuf,
};

use anyhow::{anyhow, Context as _, Result};
use glow::HasContext as _;
use khronos_egl as egl;

const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

type EglImageTargetTexture2d = unsafe extern "system" fn(u32, *mut c_void);

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: c_int) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
}

pub struct DmaBufFrame {
    pub fd: c_int,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

pub struct DmaBufReader {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    _context: egl::Context,
    _surface: egl::Surface,
    gl: glow::Context,
    image_target_texture_2d: EglImageTargetTexture2d,
    gbm_device: *mut c_void,
    _render_node: File,
}

impl DmaBufReader {
    pub fn open_and_read(frame: &DmaBufFrame) -> Result<(Self, Vec<u8>)> {
        let mut errors = Vec::new();
        let mut render_nodes = render_nodes()?;
        render_nodes.sort();

        for path in render_nodes {
            match Self::open(&path).and_then(|reader| {
                let pixels = reader.read(frame)?;
                Ok((reader, pixels))
            }) {
                Ok(result) => return Ok(result),
                Err(err) => errors.push(format!("{}: {err:#}", path.display())),
            }
        }

        Err(anyhow!(
            "Failed to import Wayland DMA-BUF on any render node: {}",
            errors.join("; ")
        ))
    }

    fn open(path: &PathBuf) -> Result<Self> {
        let render_node = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open render node {}.", path.display()))?;
        use std::os::fd::AsRawFd as _;
        let gbm_device = unsafe { gbm_create_device(render_node.as_raw_fd()) };
        if gbm_device.is_null() {
            return Err(anyhow!("Failed to create GBM device."));
        }

        let result = Self::open_with_device(render_node, gbm_device);
        if result.is_err() {
            unsafe { gbm_device_destroy(gbm_device) };
        }
        result
    }

    fn open_with_device(render_node: File, gbm_device: *mut c_void) -> Result<Self> {
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .context("Failed to load EGL 1.5.")?;
        let display = unsafe {
            egl.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE])
        }
        .context("Failed to create GBM EGL display.")?;
        egl.initialize(display)
            .context("Failed to initialize GBM EGL display.")?;
        egl.bind_api(egl::OPENGL_ES_API)
            .context("Failed to bind OpenGL ES API.")?;

        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES2_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .context("Failed to choose EGL configuration.")?
            .context("No EGL configuration supports DMA-BUF readback.")?;
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
            )
            .context("Failed to create EGL context.")?;
        let surface = egl
            .create_pbuffer_surface(display, config, &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE])
            .context("Failed to create EGL pbuffer.")?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .context("Failed to make EGL context current.")?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name)
                    .map(|address| address as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };
        let image_target_texture_2d = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .context("EGL image texture import is unavailable.")?;
        let image_target_texture_2d = unsafe { std::mem::transmute(image_target_texture_2d) };

        Ok(Self {
            egl,
            display,
            _context: context,
            _surface: surface,
            gl,
            image_target_texture_2d,
            gbm_device,
            _render_node: render_node,
        })
    }

    pub fn read(&self, frame: &DmaBufFrame) -> Result<Vec<u8>> {
        let attributes = [
            egl::WIDTH as egl::Attrib,
            frame.width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            frame.height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            DRM_FORMAT_XRGB8888 as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            frame.fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            frame.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            frame.stride as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (frame.modifier & 0xffff_ffff) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (frame.modifier >> 32) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let client_buffer = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        let no_context = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let image = self
            .egl
            .create_image(
                self.display,
                no_context,
                EGL_LINUX_DMA_BUF_EXT,
                client_buffer,
                &attributes,
            )
            .context("Failed to import PipeWire DMA-BUF as an EGL image.")?;

        let result = unsafe {
            let texture = self.gl.create_texture().map_err(|err| anyhow!(err))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            (self.image_target_texture_2d)(glow::TEXTURE_2D, image.as_ptr());

            let framebuffer = self.gl.create_framebuffer().map_err(|err| anyhow!(err))?;
            self.gl
                .bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );

            let result = if self.gl.check_framebuffer_status(glow::FRAMEBUFFER)
                != glow::FRAMEBUFFER_COMPLETE
            {
                Err(anyhow!("Imported DMA-BUF framebuffer is incomplete."))
            } else {
                let byte_count = (frame.width as usize)
                    .checked_mul(frame.height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .context("DMA-BUF frame size overflowed.")?;
                let mut pixels = vec![0; byte_count];
                self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
                self.gl.read_pixels(
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixels)),
                );
                let error = self.gl.get_error();
                if error == glow::NO_ERROR {
                    Ok(pixels)
                } else {
                    Err(anyhow!(
                        "OpenGL DMA-BUF readback failed with error {error:#x}."
                    ))
                }
            };

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.gl.delete_framebuffer(framebuffer);
            self.gl.delete_texture(texture);
            result
        };
        self.egl
            .destroy_image(self.display, image)
            .context("Failed to destroy imported DMA-BUF EGL image.")?;
        result
    }
}

impl Drop for DmaBufReader {
    fn drop(&mut self) {
        self.egl.make_current(self.display, None, None, None).ok();
        self.egl.terminate(self.display).ok();
        unsafe { gbm_device_destroy(self.gbm_device) };
    }
}

fn render_nodes() -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir("/dev/dri").context("Failed to enumerate DRM render nodes.")?;
    Ok(entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect())
}
