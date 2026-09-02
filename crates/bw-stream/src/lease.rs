//! A `GstMeta` that owns an arbitrary Rust value and drops it when the buffer is freed.
//! Used to hand the compositor's swapchain slot back exactly when GStreamer is done reading it.

use std::{any::Any, ptr, sync::OnceLock};

use gstreamer as gst;
use gstreamer::glib;

#[repr(C)]
struct LeaseMeta {
    parent: gst::ffi::GstMeta,
    lease: Option<Box<dyn Any + Send + Sync>>,
}

unsafe extern "C" fn init(meta: *mut gst::ffi::GstMeta, _: glib::ffi::gpointer, _: *mut gst::ffi::GstBuffer) -> glib::ffi::gboolean {
    unsafe { ptr::write(&mut (*meta.cast::<LeaseMeta>()).lease, None) };
    glib::ffi::GTRUE
}

unsafe extern "C" fn free(meta: *mut gst::ffi::GstMeta, _: *mut gst::ffi::GstBuffer) {
    unsafe { ptr::drop_in_place(&mut (*meta.cast::<LeaseMeta>()).lease) };
}

unsafe extern "C" fn transform(
    _: *mut gst::ffi::GstBuffer,
    _: *mut gst::ffi::GstMeta,
    _: *mut gst::ffi::GstBuffer,
    _: glib::ffi::GQuark,
    _: glib::ffi::gpointer,
) -> glib::ffi::gboolean {
    glib::ffi::GTRUE // copies of the buffer don't extend the lease
}

struct Info(*const gst::ffi::GstMetaInfo);
unsafe impl Send for Info {}
unsafe impl Sync for Info {}

fn info() -> *const gst::ffi::GstMetaInfo {
    static INFO: OnceLock<Info> = OnceLock::new();
    INFO.get_or_init(|| unsafe {
        let mut tags = [ptr::null::<std::ffi::c_char>()];
        let api = gst::ffi::gst_meta_api_type_register(c"BwLeaseMetaAPI".as_ptr(), tags.as_mut_ptr());
        Info(gst::ffi::gst_meta_register(
            api,
            c"BwLeaseMeta".as_ptr(),
            std::mem::size_of::<LeaseMeta>(),
            Some(init),
            Some(free),
            Some(transform),
        ))
    })
    .0
}

/// Keep `lease` alive exactly as long as `buffer`.
pub fn attach(buffer: &mut gst::BufferRef, lease: Box<dyn Any + Send + Sync>) {
    unsafe {
        let meta = gst::ffi::gst_buffer_add_meta(buffer.as_mut_ptr(), info(), ptr::null_mut()).cast::<LeaseMeta>();
        (*meta).lease = Some(lease);
    }
}
