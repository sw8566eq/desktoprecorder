use std::process::Command;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::cli::AudioMode;

/// The audio branch already added to a pipeline and internally linked.
/// `tail` (always `aacparse`) is what `pipeline.rs` links into the
/// muxer's next available audio pad, the same way it links the video
/// branch -- callers never need to know whether it came from a single
/// `pulsesrc` or two mixed together.
pub struct AudioBranch {
    pub tail: gst::Element,
}

/// Builds the audio branch for `mode` directly into `pipeline` (adding
/// each element as it's created, then linking -- elements must belong
/// to the pipeline before they're linked, or streaming later fails with
/// a "not-linked" flow error even though the link call itself
/// "succeeds"), or returns `Ok(None)` for `AudioMode::None`.
///
/// `device_override` corresponds to `--audio-device` and only applies
/// to `Mic`/`System` (a single source); it's an error to pass one for
/// `Both`, which always mixes the default mic with the default sink's
/// monitor.
pub fn build_audio_branch(pipeline: &gst::Pipeline, mode: AudioMode, device_override: Option<&str>) -> Result<Option<AudioBranch>> {
    // A shared rate/channel layout so audioconvert/audioresample on each
    // branch normalize to something audiomixer (for `Both`) can combine
    // without a separate negotiation step.
    let caps = gst::Caps::builder("audio/x-raw")
        .field("rate", 48_000)
        .field("channels", 2)
        .build();

    match mode {
        AudioMode::None => Ok(None),

        AudioMode::Mic => {
            let capture_tail = build_capture_chain(pipeline, device_override, &caps)?;
            let (enc_head, enc_tail) = build_encoder_tail(pipeline)?;
            capture_tail.link(&enc_head).context("failed to link mic capture chain to AAC encoder")?;
            Ok(Some(AudioBranch { tail: enc_tail }))
        }

        AudioMode::System => {
            let device = match device_override {
                Some(d) => d.to_string(),
                None => default_monitor_device()?,
            };
            let capture_tail = build_capture_chain(pipeline, Some(&device), &caps)?;
            let (enc_head, enc_tail) = build_encoder_tail(pipeline)?;
            capture_tail
                .link(&enc_head)
                .context("failed to link system-audio capture chain to AAC encoder")?;
            Ok(Some(AudioBranch { tail: enc_tail }))
        }

        AudioMode::Both => {
            if device_override.is_some() {
                anyhow::bail!(
                    "--audio-device isn't supported with --audio=both (it always mixes the default mic with the default sink's monitor)"
                );
            }

            let mic_tail = build_capture_chain(pipeline, None, &caps)?;
            let system_device = default_monitor_device()?;
            let sys_tail = build_capture_chain(pipeline, Some(&system_device), &caps)?;

            let mixer = gst::ElementFactory::make("audiomixer")
                .build()
                .context("failed to create 'audiomixer' element")?;
            pipeline.add(&mixer).context("failed to add 'audiomixer' to pipeline")?;
            mic_tail.link(&mixer).context("failed to link mic branch into audiomixer")?;
            sys_tail.link(&mixer).context("failed to link system-audio branch into audiomixer")?;

            // Without this, audiomixer's own output caps negotiation
            // with voaacenc isn't pinned to anything and can settle on
            // a different channel count than the two inputs actually
            // have (observed: silently downmixing to mono) -- pin it
            // back to the same layout every branch was already forced
            // to before the mixer.
            let post_mix_caps = gst::ElementFactory::make("capsfilter")
                .property("caps", &caps)
                .build()
                .context("failed to create post-mixer 'capsfilter' element")?;
            pipeline
                .add(&post_mix_caps)
                .context("failed to add post-mixer capsfilter to pipeline")?;
            mixer
                .link(&post_mix_caps)
                .context("failed to link audiomixer to post-mixer capsfilter")?;

            let (enc_head, enc_tail) = build_encoder_tail(pipeline)?;
            post_mix_caps
                .link(&enc_head)
                .context("failed to link post-mixer capsfilter to AAC encoder")?;

            Ok(Some(AudioBranch { tail: enc_tail }))
        }
    }
}

/// Builds `pulsesrc[device] ! audioconvert ! audioresample ! capsfilter`,
/// added to `pipeline` and linked internally, returning the tail
/// (`capsfilter`). `device: None` uses PulseAudio's default source (the
/// default mic).
fn build_capture_chain(pipeline: &gst::Pipeline, device: Option<&str>, caps: &gst::Caps) -> Result<gst::Element> {
    let mut src_builder = gst::ElementFactory::make("pulsesrc");
    if let Some(d) = device {
        src_builder = src_builder.property("device", d);
    }
    let src = src_builder
        .build()
        .context("failed to create 'pulsesrc' element (is gstreamer1.0-plugins-good installed, and is PulseAudio running?)")?;

    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .context("failed to create 'audioconvert' element")?;
    let resample = gst::ElementFactory::make("audioresample")
        .build()
        .context("failed to create 'audioresample' element")?;
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .context("failed to create audio 'capsfilter' element")?;

    pipeline
        .add_many([&src, &convert, &resample, &capsfilter])
        .context("failed to add audio capture elements to pipeline")?;
    gst::Element::link_many([&src, &convert, &resample, &capsfilter])
        .context("failed to link audio capture chain")?;

    Ok(capsfilter)
}

/// Builds `voaacenc ! aacparse`, added to `pipeline` and linked
/// internally. Returns the head (link a capture chain/mixer into this)
/// and the tail (link this into the muxer).
fn build_encoder_tail(pipeline: &gst::Pipeline) -> Result<(gst::Element, gst::Element)> {
    let encoder = gst::ElementFactory::make("voaacenc")
        .build()
        .context("failed to create 'voaacenc' element (is gstreamer1.0-plugins-bad installed?)")?;
    let parse = gst::ElementFactory::make("aacparse")
        .build()
        .context("failed to create 'aacparse' element")?;

    pipeline
        .add_many([&encoder, &parse])
        .context("failed to add AAC encoder elements to pipeline")?;
    encoder.link(&parse).context("failed to link voaacenc to aacparse")?;

    Ok((encoder, parse))
}

/// Resolves the PulseAudio monitor device for the default sink (i.e.
/// "whatever's currently playing"), by shelling out to `pactl` -- this
/// only needs the CLI tool at runtime, not libpulse at build time.
/// PulseAudio has no `@DEFAULT_MONITOR@`-style alias the way it does for
/// `@DEFAULT_SOURCE@`/`@DEFAULT_SINK@`, so the sink name has to be
/// looked up and `.monitor` appended by convention.
fn default_monitor_device() -> Result<String> {
    let output = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .context("failed to run 'pactl get-default-sink' (is PulseAudio's pactl installed?)")?;

    if !output.status.success() {
        anyhow::bail!(
            "'pactl get-default-sink' failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let sink = String::from_utf8(output.stdout)
        .context("'pactl get-default-sink' output was not valid UTF-8")?
        .trim()
        .to_string();
    if sink.is_empty() {
        anyhow::bail!("could not determine the default PulseAudio sink ('pactl get-default-sink' returned nothing)");
    }

    Ok(format!("{sink}.monitor"))
}
