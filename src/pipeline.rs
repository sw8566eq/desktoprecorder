use std::path::PathBuf;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::audio;
use crate::cli::{AudioMode, Container};
use crate::source::CaptureSource;

/// Fully resolved settings needed to build a recording pipeline.
pub struct RecordConfig {
    pub output_path: PathBuf,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub speed_preset: String,
    pub container: Container,
    pub audio_mode: AudioMode,
    pub audio_device: Option<String>,
}

/// Builds (but does not start) a GStreamer pipeline that captures
/// `source` (and, per `cfg.audio_mode`, an audio branch) and
/// encodes/muxes it to `cfg.output_path`.
///
/// This is the only place that knows the concrete element chain
/// (capture source -> videoconvert -> x264enc -> h264parse -> mux ->
/// filesink, plus whatever `audio.rs` adds alongside it); `record.rs`
/// just drives whatever `gst::Pipeline` comes back through
/// Playing/EOS/Null without caring how it was assembled.
pub fn build_recording_pipeline(source: &CaptureSource, cfg: &RecordConfig) -> Result<gst::Pipeline> {
    build_recording_pipeline_inner(source, cfg, None)
}

/// GUI-only variant: identical to `build_recording_pipeline`, except the
/// raw (post-`videoconvert`) video is additionally tee'd into a
/// downscaled, GTK-friendly branch feeding `preview_sink`. `preview_sink`
/// must already have its caps/callbacks configured (see gui.rs) -- this
/// only wires it into the graph.
pub fn build_recording_pipeline_with_preview(source: &CaptureSource, cfg: &RecordConfig, preview_sink: &gst_app::AppSink) -> Result<gst::Pipeline> {
    build_recording_pipeline_inner(source, cfg, Some(preview_sink))
}

/// A lightweight, preview-only pipeline: just `source` downscaled into
/// `preview_sink`, nothing else -- no encoder/muxer/file. Used by the
/// GUI so the preview can show live video as soon as a source is picked,
/// not only once a recording is actually in progress (the recording
/// pipeline above has its own tee'd-in preview branch for that case).
/// Unlike a recording pipeline, this has nothing to finalize on
/// shutdown -- callers can just `set_state(Null)`, no EOS needed.
pub fn build_preview_only_pipeline(source: &CaptureSource, preview_sink: &gst_app::AppSink) -> Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::with_name("desktoprecorder-preview-only-pipeline");
    let src = source.build_element()?;

    // Capped well below recording framerate -- this may run continuously
    // in the background (e.g. while the user is just browsing source
    // options), and a live thumbnail has no need to update faster than
    // this. Still required even at a low rate: ximagesrc's use-damage=false
    // (source.rs) needs an explicit framerate cap or negotiation is left
    // to whatever the source defaults to.
    const PREVIEW_FRAMERATE: i32 = 15;
    let caps = gst::Caps::builder("video/x-raw").field("framerate", gst::Fraction::new(PREVIEW_FRAMERATE, 1)).build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("failed to create 'capsfilter' element")?;

    let (preview_scale, preview_capsfilter) = build_preview_scale_chain()?;
    let preview_elem = preview_sink.upcast_ref::<gst::Element>();

    pipeline
        .add_many([&src, &capsfilter, &preview_scale, &preview_capsfilter, preview_elem])
        .context("failed to add elements to preview pipeline")?;
    gst::Element::link_many([&src, &capsfilter, &preview_scale, &preview_capsfilter, preview_elem])
        .context("failed to link preview pipeline")?;

    Ok(pipeline)
}

/// Shared by the recording pipeline's tee'd-in preview branch and the
/// standalone preview-only pipeline: downscales+color-converts into the
/// BGRA format gui.rs's appsink consumes. Returns the two elements
/// already built but not yet added/linked -- callers add them to their
/// own pipeline and link them into whatever precedes/follows.
fn build_preview_scale_chain() -> Result<(gst::Element, gst::Element)> {
    // Does colorspace conversion (to BGRA, chosen to map directly onto
    // gdk::MemoryFormat::B8g8r8a8 with no further pixel shuffling in
    // gui.rs) and downscaling together. pixel-aspect-ratio=1/1 is
    // required alongside width, not optional -- leaving only width
    // pinned lets negotiation "cheat" by keeping height unchanged and
    // lying about it via a stretched pixel-aspect-ratio instead of
    // actually resampling (confirmed with a direct gst-launch-1.0 caps
    // trace before writing this).
    let preview_scale = gst::ElementFactory::make("videoconvertscale")
        .build()
        .context("failed to create 'videoconvertscale' element (is gstreamer1.0-plugins-base installed?)")?;
    const PREVIEW_WIDTH: i32 = 480;
    let preview_caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", PREVIEW_WIDTH)
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .build();
    let preview_capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &preview_caps)
        .build()
        .context("failed to create preview 'capsfilter' element")?;
    Ok((preview_scale, preview_capsfilter))
}

/// Shared by both entry points above so the muxer/audio-branch wiring
/// (and its carefully worded error context) lives in exactly one place --
/// only the tee-insertion point differs between the CLI's plain path and
/// the GUI's preview-branching path.
fn build_recording_pipeline_inner(source: &CaptureSource, cfg: &RecordConfig, preview_sink: Option<&gst_app::AppSink>) -> Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::with_name("desktoprecorder-pipeline");

    let src = source.build_element()?;

    // Pin an explicit framerate: ximagesrc's default damage-tracking
    // (use-damage=true, disabled in source.rs) only pushes frames when
    // pixels change, giving a variable framerate that desyncs badly on
    // an idle screen. With use-damage=false, this caps filter is what
    // actually forces a steady rate.
    let caps = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(cfg.framerate as i32, 1))
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .context("failed to create 'capsfilter' element")?;

    // ximagesrc outputs the X11 display's native pixel format (commonly
    // BGRx); x264enc needs a colorspace it understands, so this is
    // required, not boilerplate.
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .context("failed to create 'videoconvert' element (is gstreamer1.0-plugins-base installed?)")?;

    let key_int_max = cfg.framerate.saturating_mul(2);
    let encoder = gst::ElementFactory::make("x264enc")
        .property("bitrate", cfg.bitrate_kbps)
        .property("key-int-max", key_int_max)
        .property_from_str("speed-preset", &cfg.speed_preset)
        .property_from_str("tune", "zerolatency")
        .build()
        .context("failed to create 'x264enc' element (is gstreamer1.0-plugins-ugly installed?)")?;

    // Cheap insurance against stream-format/alignment mismatches between
    // the encoder's output and what the muxer expects.
    let parse = gst::ElementFactory::make("h264parse")
        .build()
        .context("failed to create 'h264parse' element")?;

    let mux_name = cfg.container.mux_element_name();
    let mut mux_builder = gst::ElementFactory::make(mux_name);
    if cfg.container == Container::Mp4 {
        // qtmux writes its index (moov atom) at the end by default;
        // faststart does a second-pass rewrite so it ends up at the
        // front, same as a browser/streaming-friendly mp4.
        mux_builder = mux_builder.property("faststart", true);
    }
    let mux = mux_builder
        .build()
        .with_context(|| format!("failed to create '{mux_name}' muxer element"))?;

    let output_str = cfg
        .output_path
        .to_str()
        .with_context(|| format!("output path is not valid UTF-8: {:?}", cfg.output_path))?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", output_str)
        .build()
        .context("failed to create 'filesink' element")?;

    match preview_sink {
        None => {
            pipeline
                .add_many([&src, &capsfilter, &convert, &encoder, &parse, &mux, &sink])
                .context("failed to add elements to pipeline")?;
            gst::Element::link_many([&src, &capsfilter, &convert, &encoder, &parse, &mux, &sink])
                .context("failed to link pipeline elements")?;
        }
        Some(preview_sink) => {
            // tee's branches need their own queue each -- without one,
            // a tee's src pads are fed serially by whatever thread is
            // pushing into it, so a stalled branch (e.g. a slow GTK
            // consumer on the preview side) would block the other
            // branch and everything upstream of the tee, including the
            // capture source itself.
            let tee = gst::ElementFactory::make("tee").build().context("failed to create 'tee' element")?;
            let record_queue = gst::ElementFactory::make("queue")
                .name("record-queue")
                .build()
                .context("failed to create record-branch 'queue' element")?;
            // Leaky (drops old buffers, not events -- EOS still forwards)
            // and capped to one buffer: the preview branch may freely
            // drop frames under load, and must never be able to back up
            // into the tee and stall the record branch or delay EOS.
            let preview_queue = gst::ElementFactory::make("queue")
                .name("preview-queue")
                .property_from_str("leaky", "downstream")
                .property("max-size-buffers", 1u32)
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 0u64)
                .build()
                .context("failed to create preview-branch 'queue' element")?;

            let (preview_scale, preview_capsfilter) = build_preview_scale_chain()?;
            let preview_elem = preview_sink.upcast_ref::<gst::Element>();

            pipeline
                .add_many([
                    &src,
                    &capsfilter,
                    &convert,
                    &tee,
                    &record_queue,
                    &encoder,
                    &parse,
                    &mux,
                    &sink,
                    &preview_queue,
                    &preview_scale,
                    &preview_capsfilter,
                    preview_elem,
                ])
                .context("failed to add elements to pipeline")?;

            gst::Element::link_many([&src, &capsfilter, &convert, &tee]).context("failed to link capture chain to tee")?;
            gst::Element::link_many([&tee, &record_queue, &encoder, &parse, &mux, &sink]).context("failed to link record branch")?;
            gst::Element::link_many([&tee, &preview_queue, &preview_scale, &preview_capsfilter, preview_elem])
                .context("failed to link preview branch")?;
        }
    }

    // audio.rs adds its own elements to `pipeline` directly (elements
    // must belong to the pipeline before they're linked -- see the
    // comment on `build_audio_branch`), then hands back just the tail
    // to link into the muxer's next available (audio) pad, the same way
    // `parse` above claimed the video pad -- muxers like matroskamux
    // and qtmux hand out a compatible request pad per `link()` call.
    if let Some(audio_branch) = audio::build_audio_branch(&pipeline, cfg.audio_mode, cfg.audio_device.as_deref())? {
        audio_branch
            .tail
            .link(&mux)
            .context("failed to link audio branch into muxer")?;
    }

    Ok(pipeline)
}
