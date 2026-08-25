use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

const POLL_SLICE: Duration = Duration::from_millis(200);
const EOS_TIMEOUT_SECS: u64 = 10;

/// Runs `pipeline` until `duration` elapses or `stop_requested` is set
/// (by a Ctrl+C/SIGTERM handler -- see `main.rs`), then cleanly
/// finalizes the recording so the output file isn't left
/// truncated/corrupt either way.
pub fn run_recording(pipeline: &gst::Pipeline, duration: Duration, stop_requested: &Arc<AtomicBool>) -> Result<()> {
    pipeline
        .set_state(gst::State::Playing)
        .context("failed to start pipeline (Playing)")?;

    let bus = pipeline.bus().context("pipeline has no bus")?;
    let deadline = Instant::now() + duration;

    loop {
        if stop_requested.load(Ordering::SeqCst) {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let slice = remaining.min(POLL_SLICE);
        let timeout = gst::ClockTime::try_from(slice).ok();

        if let Some(msg) = bus.timed_pop_filtered(timeout, &[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    pipeline.set_state(gst::State::Null).ok();
                    anyhow::bail!(
                        "pipeline error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
    }

    finalize(pipeline, &bus)
}

/// Sends EOS and blocks (with a safety-net timeout) until it has actually
/// propagated through every element. This is what lets the muxer flush
/// its trailer/index -- and x264enc flush any frames held in its
/// lookahead/B-frame reorder buffer -- instead of leaving a truncated or
/// unplayable file. Required even for the normal "duration elapsed"
/// stop path, not just an interrupted one.
fn finalize(pipeline: &gst::Pipeline, bus: &gst::Bus) -> Result<()> {
    pipeline.send_event(gst::event::Eos::new());

    match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(EOS_TIMEOUT_SECS),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(msg) if matches!(msg.view(), gst::MessageView::Eos(_)) => {}
        Some(msg) => {
            pipeline.set_state(gst::State::Null).ok();
            anyhow::bail!("error while finalizing recording: {:?}", msg);
        }
        None => {
            eprintln!("warning: timed out waiting for EOS; file may not be fully finalized");
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .context("failed to set pipeline to Null after EOS")?;
    Ok(())
}
