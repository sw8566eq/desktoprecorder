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
                    force_null(pipeline);
                    anyhow::bail!(
                        "pipeline error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                // The pipeline reached EOS on its own (e.g. a source
                // that naturally ends, unlike ximagesrc/pulsesrc which
                // never do in this app today) -- it's already fully
                // flushed at this point, so finalize()'s explicit
                // send-Eos-then-wait would just be sending a second EOS
                // to an already-EOS'd pipeline. Elements don't repost
                // EOS in response to that (confirmed via a direct
                // `gst::parse::launch` reproduction in record.rs's
                // tests), so finalize() would stall for the full
                // EOS_TIMEOUT_SECS before giving up -- skip straight to
                // Null instead.
                gst::MessageView::Eos(_) => return set_null(pipeline),
                _ => {}
            }
        }
    }

    finalize(pipeline, &bus)
}

/// Best-effort teardown for when we're already returning (or about to
/// return) a more important error -- a failure to reach Null here
/// shouldn't shadow that original error, so this swallows its own.
fn force_null(pipeline: &gst::Pipeline) {
    pipeline.set_state(gst::State::Null).ok();
}

/// The non-error-path teardown: reaching Null itself is the thing that
/// can fail here, so unlike `force_null`, that failure is what gets
/// reported.
fn set_null(pipeline: &gst::Pipeline) -> Result<()> {
    pipeline.set_state(gst::State::Null).map(|_| ()).context("failed to set pipeline to Null after EOS")
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
            force_null(pipeline);
            anyhow::bail!("error while finalizing recording: {:?}", msg);
        }
        None => {
            eprintln!("warning: timed out waiting for EOS; file may not be fully finalized");
        }
    }

    set_null(pipeline)
}

/// Unlike the other modules' tests, these actually run pipelines to
/// Playing/EOS/Null -- `run_recording` is this project's core state
/// machine and deserves more than a construction-only check. They use
/// `videotestsrc`/`fakesink` (plain software elements from gst-plugins-base,
/// no capture hardware or X11 display involved) instead of the real
/// `ximagesrc` source, so they run the same everywhere the base
/// GStreamer install does.
#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;

    fn init_gst() {
        gst::init().expect("gst::init should always succeed in a test process");
    }

    /// `videotestsrc` capped to a handful of buffers so the pipeline
    /// EOSes on its own well before any duration/stop-flag in these
    /// tests would kick in.
    fn short_finite_pipeline() -> gst::Pipeline {
        gst::parse::launch("videotestsrc num-buffers=5 ! fakesink sync=false")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap()
    }

    /// A source with no `num-buffers` limit -- keeps producing until
    /// something (duration elapsing, or the stop flag) tells
    /// `run_recording` to stop.
    fn unbounded_pipeline() -> gst::Pipeline {
        gst::parse::launch("videotestsrc ! fakesink sync=false").unwrap().downcast::<gst::Pipeline>().unwrap()
    }

    #[test]
    fn stops_on_eos_before_the_duration_elapses() {
        init_gst();
        let pipeline = short_finite_pipeline();
        let stop_requested = Arc::new(AtomicBool::new(false));

        let start = Instant::now();
        run_recording(&pipeline, Duration::from_secs(30), &stop_requested).unwrap();
        // 5 buffers at whatever rate videotestsrc/fakesink can push
        // unsynced is fast -- nowhere near the 30s duration, so this
        // only passes if the Eos break actually fired instead of the
        // deadline.
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}, expected an early EOS-driven return", start.elapsed());
        assert_eq!(pipeline.state(gst::ClockTime::NONE).1, gst::State::Null);
    }

    #[test]
    fn stops_when_the_duration_elapses() {
        init_gst();
        let pipeline = unbounded_pipeline();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let duration = Duration::from_millis(400);

        let start = Instant::now();
        run_recording(&pipeline, duration, &stop_requested).unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= duration, "returned after {elapsed:?}, before the {duration:?} deadline");
        // Generous upper bound -- just confirms it didn't run anywhere
        // close to indefinitely; POLL_SLICE plus EOS finalization
        // account for the slack above `duration` itself.
        assert!(elapsed < duration + Duration::from_secs(5), "took {elapsed:?}, expected to stop near the {duration:?} deadline");
    }

    #[test]
    fn stops_early_when_stop_requested_is_set() {
        init_gst();
        let pipeline = unbounded_pipeline();
        let stop_requested = Arc::new(AtomicBool::new(false));

        {
            let stop_requested = Arc::clone(&stop_requested);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                stop_requested.store(true, Ordering::SeqCst);
            });
        }

        let start = Instant::now();
        // A long duration that would fail the test if the stop flag
        // weren't actually being honored.
        run_recording(&pipeline, Duration::from_secs(30), &stop_requested).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}, expected the stop flag to cut it short", start.elapsed());
    }

    #[test]
    fn surfaces_an_error_instead_of_hanging_when_the_pipeline_cant_start() {
        init_gst();
        // filesink can't open a location under a directory that doesn't
        // exist -- a deterministic, display-independent way to make a
        // pipeline fail during startup instead of running normally.
        let pipeline = gst::parse::launch("videotestsrc num-buffers=1 ! filesink location=/nonexistent-desktoprecorder-test-dir/out.dat")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let stop_requested = Arc::new(AtomicBool::new(false));

        let result = run_recording(&pipeline, Duration::from_secs(30), &stop_requested);
        assert!(result.is_err(), "expected an error from a pipeline that can't open its output file");
    }
}
