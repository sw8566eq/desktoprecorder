use std::path::PathBuf;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::cli::Container;
use crate::source::CaptureSource;

/// Fully resolved settings needed to build a recording pipeline.
pub struct RecordConfig {
    pub output_path: PathBuf,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub speed_preset: String,
    pub container: Container,
}

/// Builds (but does not start) a GStreamer pipeline that captures
/// `source` and encodes/muxes it to `cfg.output_path`.
///
/// This is the only place that knows the concrete element chain
/// (capture source -> videoconvert -> x264enc -> h264parse -> mux ->
/// filesink); `record.rs` just drives whatever `gst::Pipeline` comes
/// back through Playing/EOS/Null without caring how it was assembled.
pub fn build_recording_pipeline(source: &CaptureSource, cfg: &RecordConfig) -> Result<gst::Pipeline> {
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

    pipeline
        .add_many([&src, &capsfilter, &convert, &encoder, &parse, &mux, &sink])
        .context("failed to add elements to pipeline")?;
    gst::Element::link_many([&src, &capsfilter, &convert, &encoder, &parse, &mux, &sink])
        .context("failed to link pipeline elements")?;

    Ok(pipeline)
}
