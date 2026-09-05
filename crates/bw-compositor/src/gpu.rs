//! The renderer and what it renders into: a render node's GBM device with dmabuf swapchains the encoders
//! import, or, without a GPU, Mesa's surfaceless platform (llvmpipe) with a texture read back into memory.

use std::{fs::OpenOptions, os::fd::OwnedFd, path::Path};

use anyhow::{Context, Result};
use bw_core::OutputGeometry;
use smithay::{
    backend::{
        allocator::{
            Format, Fourcc, Modifier, Slot, Swapchain,
            dmabuf::{Dmabuf, DmabufAllocator},
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDeviceFd, DrmNode},
        egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
        renderer::{Bind, ExportMem, Offscreen, TextureMapping, gles::{GlesRenderer, GlesTarget, GlesTexture}},
    },
    utils::{Buffer, Rectangle, Size},
};

pub type DmabufSwapchain = Swapchain<DmabufAllocator<GbmAllocator<DrmDeviceFd>>>;

/// The GPU's device, when there is one.
pub struct Device {
    pub node: DrmNode,
    pub drm: DrmDeviceFd,
    gbm: GbmDevice<DrmDeviceFd>,
}

pub struct Gpu {
    pub device: Option<Device>,
    pub renderer: GlesRenderer,
    /// The output's; window streams have their own (`targets()`).
    pub targets: Targets,
    pub fourcc: Fourcc,
    pub modifier: Modifier,
    /// The first buffer of each swapchain is checked against `modifier` (see `render_frame`).
    pub modifier_verified: bool,
}

/// What frames are rendered into.
pub enum Targets {
    /// Dmabufs the encoders import zero-copy, in the negotiated format.
    Dmabuf(DmabufSwapchain),
    /// No GPU: one texture, read back into memory after each frame (made on first use, again after a resize).
    Texture { texture: Option<GlesTexture>, size: Size<i32, Buffer> },
}

/// One of the targets, acquired for a frame.
pub enum Target {
    Slot { slot: Slot<Dmabuf>, dmabuf: Dmabuf },
    Texture,
}

impl Gpu {
    /// `accepted` is what the encoder can import; we pick a format both sides support. Without a render
    /// node the encoders are the software ones, which take the pixels as read back (BGRx).
    pub fn new(render_node: Option<&Path>, geo: &OutputGeometry, accepted: &[(u32, u64)]) -> Result<Gpu> {
        let Some(render_node) = render_node else {
            // Safety: the display is only used from this thread and outlives the renderer through the context.
            let renderer = unsafe { EGLDisplay::new(EGLSurfacelessDisplay).and_then(|egl| EGLContext::new(&egl)).map_err(anyhow::Error::from).and_then(|ctx| Ok(GlesRenderer::new(ctx)?)) }
                .context("Mesa's surfaceless EGL platform (no render node; LIBGL_ALWAYS_SOFTWARE=1 forces llvmpipe)")?;
            tracing::info!("no GPU: rendering with Mesa's surfaceless platform, frames read back into memory");
            let targets = Targets::Texture { texture: None, size: (geo.width_px as i32, geo.height_px as i32).into() };
            return Ok(Gpu { device: None, renderer, targets, fourcc: Fourcc::Xrgb8888, modifier: Modifier::Linear, modifier_verified: false });
        };
        let node = DrmNode::from_path(render_node)?;
        let file = OpenOptions::new().read(true).write(true).open(render_node)?;
        let fd = DrmDeviceFd::new(OwnedFd::from(file).into());
        let gbm = GbmDevice::new(fd.clone())?;
        // Safety: the display is only used from this thread and outlives the renderer through the context.
        let egl = unsafe { EGLDisplay::new(gbm.clone())? };
        let renderer = unsafe { GlesRenderer::new(EGLContext::new(&egl)?)? };

        let renderable = Bind::<Dmabuf>::supported_formats(&renderer).unwrap_or_default();
        let (fourcc, modifier) = accepted
            .iter()
            .filter_map(|&(f, m)| {
                let format = Format { code: Fourcc::try_from(f).ok()?, modifier: Modifier::from(m) };
                renderable.contains(&format).then_some((format.code, format.modifier))
            })
            // Prefer the alpha-less/opaque-friendly ARGB layout the encoder likes, and tiled over linear.
            .max_by_key(|&(f, m)| (f == Fourcc::Argb8888, m != Modifier::Linear))
            .with_context(|| format!("no dmabuf format shared by renderer and encoder; encoder accepts {accepted:x?}"))?;
        tracing::info!(?fourcc, ?modifier, "render target format");

        let targets = Targets::Dmabuf(swapchain(&gbm, fourcc, modifier, geo.width_px, geo.height_px));
        Ok(Gpu { device: Some(Device { node, drm: fd, gbm }), renderer, targets, fourcc, modifier, modifier_verified: false })
    }

    /// Targets for a window stream of `width`×`height`.
    pub fn targets(&self, width: u32, height: u32) -> Targets {
        match &self.device {
            Some(d) => Targets::Dmabuf(swapchain(&d.gbm, self.fourcc, self.modifier, width, height)),
            None => Targets::Texture { texture: None, size: (width as i32, height as i32).into() },
        }
    }
}

impl Targets {
    pub fn resize(&mut self, width: u32, height: u32) {
        match self {
            Targets::Dmabuf(s) => s.resize(width, height),
            Targets::Texture { texture, size } => {
                *texture = None;
                *size = (width as i32, height as i32).into();
            }
        }
    }

    /// A target to render into and its buffer age, or `None` while the encoders hold every dmabuf.
    pub fn acquire(&mut self, renderer: &mut GlesRenderer, fourcc: Fourcc) -> Result<Option<(Target, usize)>> {
        Ok(match self {
            Targets::Dmabuf(s) => s.acquire()?.map(|slot| {
                let (age, dmabuf) = (slot.age() as usize, (*slot).clone());
                (Target::Slot { slot, dmabuf }, age)
            }),
            Targets::Texture { texture, size } => {
                let age = if texture.is_some() { 1 } else { 0 };
                if texture.is_none() {
                    *texture = Some(renderer.create_buffer(fourcc, *size).context("create texture")?);
                }
                Some((Target::Texture, age))
            }
        })
    }

    /// The framebuffer of an acquired target (it borrows the target, not the renderer).
    pub fn bind<'a>(&'a mut self, renderer: &mut GlesRenderer, target: &'a mut Target) -> Result<GlesTarget<'a>> {
        Ok(match (self, target) {
            (_, Target::Slot { dmabuf, .. }) => renderer.bind(dmabuf)?,
            (Targets::Texture { texture: Some(t), .. }, Target::Texture) => renderer.bind(t)?,
            _ => unreachable!("a texture target comes from texture targets"),
        })
    }
}

/// The framebuffer's pixels, 4 bytes each in `fourcc`'s order, rows top first (GL may give them bottom first).
pub fn read_pixels(renderer: &mut GlesRenderer, fb: &GlesTarget<'_>, size: Size<i32, Buffer>, fourcc: Fourcc) -> Result<Vec<u8>> {
    let mapping = renderer.copy_framebuffer(fb, Rectangle::from_size(size), fourcc).context("copy framebuffer")?;
    let data = renderer.map_texture(&mapping).context("map texture")?;
    let (w, h) = (size.w as usize, size.h as usize);
    let stride = w * 4;
    let mut out = vec![0u8; stride * h];
    for y in 0..h {
        let src = if mapping.flipped() { y } else { h - 1 - y };
        out[y * stride..(y + 1) * stride].copy_from_slice(&data[src * stride..(src + 1) * stride]);
    }
    Ok(out)
}

fn swapchain(gbm: &GbmDevice<DrmDeviceFd>, fourcc: Fourcc, modifier: Modifier, width: u32, height: u32) -> DmabufSwapchain {
    // asked for as linear too, so GBM can't fall back to a tiled layout the CPU would misread
    let flags = if modifier == Modifier::Linear { GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR } else { GbmBufferFlags::RENDERING };
    let allocator = DmabufAllocator(GbmAllocator::new(gbm.clone(), flags));
    Swapchain::new(allocator, width, height, fourcc, vec![modifier])
}
