//! GStreamer capture → VA-API H.264 encode → encoded frames on a channel.
//!
//! Two front ends share one encode tail:
//! - [`fake_source`] feeds `videotestsrc` (no compositor needed) for end-to-end testing.
//! - [`GstSink`] implements [`FrameSink`]: compositor dmabufs go into an `appsrc` zero-copy.

mod lease;

use std::{
    collections::HashMap,
    io::{Seek, SeekFrom},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use anyhow::{Context, Result};
use bw_core::{Bytes, DmabufFrame, EncodedFrame, FrameSink, OutputGeometry, SinkError, StreamInfo, StreamMsg};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators as gst_allocators;
use gstreamer_allocators::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tokio::sync::mpsc;

/// A running pipeline. Dropping it stops the stream.
pub struct Stream {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    /// Set by the bus watcher on a pipeline error; the sink then rebuilds on the next frame.
    dead: Arc<AtomicBool>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true); // lets the bus-watch thread exit
        }
    }
}

impl Stream {
    /// Force the next frame to be a keyframe (IDR + SPS/PPS).
    pub fn request_keyframe(&self) {
        let ev = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
        if !self.encoder.send_event(ev) {
            tracing::warn!("encoder refused the keyframe request");
        }
    }
}

struct EncodeOpts {
    width: u32,
    height: u32,
    scale: f64,
    bitrate_kbps: u32,
}

/// Test source: an animated pattern, no compositor. Emits `StreamMsg` on `tx`.
pub fn fake_source(bitrate_kbps: u32, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    gst::init()?;
    let (width, height) = (1920, 1080);
    let head = format!(
        "videotestsrc is-live=true pattern=ball ! video/x-raw,format=BGRA,width={width},height={height},framerate=60/1 \
         ! timeoverlay ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12"
    );
    build(&head, EncodeOpts { width, height, scale: 1.0, bitrate_kbps }, tx)
}

static STREAM_SEQ: AtomicU32 = AtomicU32::new(1);

/// Build `<head> ! vah264enc ! h264parse ! appsink` and start it. `head` must end at NV12/VAMemory.
fn build(head: &str, opts: EncodeOpts, tx: mpsc::Sender<StreamMsg>) -> Result<Stream> {
    let stream_id = STREAM_SEQ.fetch_add(1, Ordering::Relaxed);
    let desc = format!(
        "{head} \
         ! vah264enc name=enc rate-control=cbr bitrate={} target-usage=7 b-frames=0 ref-frames=1 \
         ! video/x-h264,profile=high ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=1 leaky-type=downstream",
        opts.bitrate_kbps
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().expect("parse::launch returns a pipeline");
    let encoder = pipeline.by_name("enc").context("enc element")?;
    let sink = pipeline.by_name("sink").context("sink element")?.downcast::<gst_app::AppSink>().unwrap();

    let mut info = Some(StreamInfo { stream_id, codec: String::new(), width: opts.width, height: opts.height, scale: opts.scale });
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                // First keyframe: derive the WebCodecs codec string from the SPS and announce the stream.
                if keyframe && info.is_some() {
                    let mut i = info.take().unwrap();
                    i.codec = codec_string(&map).unwrap_or_else(|| {
                        tracing::error!("no SPS in the first keyframe; guessing the codec string");
                        "avc1.640028".into()
                    });
                    let _ = tx.blocking_send(StreamMsg::Info(i));
                }
                // ponytail: blocking send so no frame silently vanishes between encoder and server;
                // the consumer only does try_send, so this never blocks in practice.
                let _ = tx.blocking_send(StreamMsg::Frame(EncodedFrame {
                    stream_id,
                    keyframe,
                    pts_us: buffer.pts().map(|t| t.useconds()).unwrap_or(0),
                    data: Bytes::copy_from_slice(&map),
                }));
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let bus = pipeline.bus().unwrap();
    let dead = Arc::new(AtomicBool::new(false));
    let flag = dead.clone();
    std::thread::spawn(move || {
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    tracing::error!(src = ?e.src().map(|s| s.path_string()), "gstreamer: {} ({:?})", e.error(), e.debug());
                    flag.store(true, Ordering::Relaxed);
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
    });

    pipeline.set_state(gst::State::Playing)?;
    Ok(Stream { pipeline, encoder, dead })
}

/// `avc1.PPCCLL` from the first SPS in an Annex B access unit, per the WebCodecs AVC registration.
fn codec_string(au: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 4 < au.len() {
        let sc4 = au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1;
        let sc3 = au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1;
        let nal = if sc4 { i + 4 } else if sc3 { i + 3 } else { i += 1; continue };
        if au.get(nal)? & 0x1f == 7 {
            let p = au.get(nal + 1..nal + 4)?;
            return Some(format!("avc1.{:02X}{:02X}{:02X}", p[0], p[1], p[2]));
        }
        i = nal;
    }
    None
}

/// The real sink: compositor dmabufs → `appsrc`. Clone it for a keyframe handle before moving it into the compositor.
#[derive(Clone)]
pub struct GstSink(Arc<Mutex<Inner>>);

struct Inner {
    tx: mpsc::Sender<StreamMsg>,
    bitrate_kbps: u32,
    accepted: Vec<(u32, u64)>,
    /// Set by `output_changed`; the pipeline is (re)built lazily on the next frame.
    geo: Option<(OutputGeometry, u32, u64)>,
    stream: Option<(Stream, gst_app::AppSrc)>,
    /// One imported memory per compositor swapchain slot, so VA keeps its surface import.
    mems: HashMap<u32, gst::Memory>,
    alloc: gst_allocators::DmaBufAllocator,
}

impl GstSink {
    pub fn new(bitrate_kbps: u32, tx: mpsc::Sender<StreamMsg>) -> Result<GstSink> {
        gst::init()?;
        let accepted = vapostproc_dmabuf_formats();
        tracing::info!(?accepted, "vapostproc dmabuf import formats (fourcc, modifier)");
        Ok(GstSink(Arc::new(Mutex::new(Inner {
            tx,
            bitrate_kbps,
            accepted,
            geo: None,
            stream: None,
            mems: HashMap::new(),
            alloc: gst_allocators::DmaBufAllocator::new(),
        }))))
    }

    pub fn request_keyframe(&self) {
        if let Some((stream, _)) = &self.0.lock().unwrap().stream {
            stream.request_keyframe();
        }
    }
}

impl FrameSink for GstSink {
    fn accepted_formats(&self) -> Vec<(u32, u64)> {
        self.0.lock().unwrap().accepted.clone()
    }

    fn output_changed(&mut self, geo: OutputGeometry, fourcc: u32, modifier: u64) {
        let mut i = self.0.lock().unwrap();
        i.geo = Some((geo, fourcc, modifier));
        let old = i.take_stream();
        drop(i);
        drop(old); // tearing down waits on streaming threads that may need this lock
    }

    fn submit(&mut self, frame: DmabufFrame) -> Result<(), SinkError> {
        let mut i = self.0.lock().unwrap();
        if i.stream.as_ref().is_some_and(|(s, _)| s.dead.load(Ordering::Relaxed)) {
            let old = i.take_stream();
            drop(i);
            drop(old);
            i = self.0.lock().unwrap();
        }
        Ok(i.push(frame)?)
    }
}

impl Inner {
    /// Detach the pipeline and its imported memories; the caller drops them *after* unlocking.
    fn take_stream(&mut self) -> (Option<(Stream, gst_app::AppSrc)>, HashMap<u32, gst::Memory>) {
        (self.stream.take(), std::mem::take(&mut self.mems))
    }

    fn start(&mut self) -> Result<()> {
        let (geo, fourcc, modifier) = self.geo.context("output_changed was never called")?;
        let head = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true block=false max-buffers=2 leaky-type=downstream \
             caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={},width={},height={},framerate=60/1\" \
             ! vapostproc ! video/x-raw(memory:VAMemory),format=NV12",
            drm_format_string(fourcc, modifier),
            geo.width_px,
            geo.height_px
        );
        let opts = EncodeOpts { width: geo.width_px, height: geo.height_px, scale: geo.scale, bitrate_kbps: self.bitrate_kbps };
        let stream = build(&head, opts, self.tx.clone())?;
        let appsrc = stream.pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        self.stream = Some((stream, appsrc));
        Ok(())
    }

    fn push(&mut self, frame: DmabufFrame) -> Result<()> {
        if self.stream.is_none() {
            self.start()?;
        }
        let mem = match self.mems.get(&frame.slot_id) {
            Some(m) => m.clone(), // frame.fd (a dup) is closed on drop; GStreamer already holds one
            None => {
                let size = dmabuf_size(&frame.fd)?;
                // Safety: the fd is a dmabuf and GStreamer takes ownership of it.
                let m = unsafe { self.alloc.alloc_dmabuf(frame.fd, size) }?;
                self.mems.insert(frame.slot_id, m.clone());
                m
            }
        };
        let format = gst_video::dma_drm_fourcc_to_format(frame.fourcc).context("unmappable fourcc")?;
        let mut buffer = gst::Buffer::new();
        {
            let b = buffer.get_mut().unwrap();
            b.append_memory(mem);
            gst_video::VideoMeta::add_full(b, gst_video::VideoFrameFlags::empty(), format, frame.width, frame.height, &[frame.offset as usize], &[frame.stride as i32])?;
            lease::attach(b, frame.lease);
        }
        let (_, appsrc) = self.stream.as_ref().unwrap();
        appsrc.push_buffer(buffer)?; // leaky appsrc drops on its own; the lease then frees the slot
        Ok(())
    }
}

fn dmabuf_size(fd: &OwnedFd) -> Result<usize> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    Ok(file.seek(SeekFrom::End(0))? as usize)
}

fn drm_format_string(fourcc: u32, modifier: u64) -> String {
    let code = String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned();
    if modifier == 0 { code } else { format!("{code}:0x{modifier:016x}") }
}

/// `(fourcc, modifier)` pairs vapostproc advertises for `memory:DMABuf` input.
fn vapostproc_dmabuf_formats() -> Vec<(u32, u64)> {
    let Some(factory) = gst::ElementFactory::find("vapostproc") else { return vec![] };
    let mut out = vec![];
    for tmpl in factory.static_pad_templates() {
        if tmpl.direction() != gst::PadDirection::Sink {
            continue;
        }
        for (s, features) in tmpl.caps().iter_with_features() {
            if !features.contains("memory:DMABuf") {
                continue;
            }
            let Ok(v) = s.value("drm-format") else { continue };
            let names: Vec<String> = match v.get::<gst::List>() {
                Ok(list) => list.iter().filter_map(|x| x.get::<String>().ok()).collect(),
                Err(_) => v.get::<String>().into_iter().collect(),
            };
            out.extend(names.iter().filter_map(|n| gst_video::dma_drm_fourcc_from_str(n).ok()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_high_level_42() {
        let au = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xac];
        assert_eq!(codec_string(&au).as_deref(), Some("avc1.64002A"));
    }

    #[test]
    fn drm_format_strings() {
        assert_eq!(drm_format_string(u32::from_le_bytes(*b"AR24"), 0x0100000000000009), "AR24:0x0100000000000009");
        assert_eq!(drm_format_string(u32::from_le_bytes(*b"XR24"), 0), "XR24");
    }
}
