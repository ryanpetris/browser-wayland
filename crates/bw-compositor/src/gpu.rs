//! Render node → GBM → EGL → GLES renderer, plus the dmabuf swapchain we composite into.

use std::{fs::OpenOptions, os::fd::OwnedFd, path::Path};

use anyhow::{Context, Result};
use bw_core::OutputGeometry;
use smithay::backend::{
    allocator::{
        Format, Fourcc, Modifier, Swapchain,
        dmabuf::{Dmabuf, DmabufAllocator},
        gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
    },
    drm::{DrmDeviceFd, DrmNode},
    egl::{EGLContext, EGLDisplay},
    renderer::{Bind, gles::GlesRenderer},
};

pub type DmabufSwapchain = Swapchain<DmabufAllocator<GbmAllocator<DrmDeviceFd>>>;

pub struct Gpu {
    pub node: DrmNode,
    pub renderer: GlesRenderer,
    pub swapchain: DmabufSwapchain,
    pub fourcc: Fourcc,
    pub modifier: Modifier,
}

impl Gpu {
    /// `accepted` is what the encoder can import; we pick a format both sides support.
    pub fn new(render_node: &Path, geo: &OutputGeometry, accepted: &[(u32, u64)]) -> Result<Gpu> {
        let node = DrmNode::from_path(render_node)?;
        let file = OpenOptions::new().read(true).write(true).open(render_node)?;
        let fd = DrmDeviceFd::new(OwnedFd::from(file).into());
        let gbm = GbmDevice::new(fd)?;
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

        let allocator = DmabufAllocator(GbmAllocator::new(gbm, GbmBufferFlags::RENDERING));
        let swapchain = Swapchain::new(allocator, geo.width_px, geo.height_px, fourcc, vec![modifier]);
        Ok(Gpu { node, renderer, swapchain, fourcc, modifier })
    }
}
