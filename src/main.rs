mod cli;
mod error;
mod pipeline;
mod record;
mod source;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use gstreamer as gst;

use cli::{Cli, Command, Container, RecordArgs};
use error::RecorderError;
use pipeline::RecordConfig;
use source::{CaptureSource, X11ScreenConfig};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    gst::init().context("failed to initialize GStreamer")?;

    let cli = Cli::parse();
    match cli.command {
        Command::Record(args) => record_command(args),
        Command::ListSources => list_sources_command(),
    }
}

fn record_command(args: RecordArgs) -> Result<()> {
    if args.monitor.is_some() {
        return Err(RecorderError::NotYetImplemented { flag: "monitor" }.into());
    }
    if args.window.is_some() {
        return Err(RecorderError::NotYetImplemented { flag: "window" }.into());
    }

    let container = args
        .container
        .unwrap_or_else(|| Container::infer_from_path(&args.output));

    let source = CaptureSource::X11Screen(X11ScreenConfig::default());
    let cfg = RecordConfig {
        output_path: args.output.clone(),
        framerate: args.framerate,
        bitrate_kbps: args.bitrate,
        speed_preset: args.speed_preset,
        container,
    };

    let pipeline = pipeline::build_recording_pipeline(&source, &cfg)?;

    let stop_requested = Arc::new(AtomicBool::new(false));
    {
        let stop_requested = Arc::clone(&stop_requested);
        ctrlc::set_handler(move || {
            stop_requested.store(true, Ordering::SeqCst);
        })
        .context("failed to install Ctrl+C/SIGTERM handler")?;
    }

    println!(
        "recording to {} for {} ({}fps, {}kbps, {})... press Ctrl+C to stop early",
        args.output.display(),
        args.duration,
        args.framerate,
        args.bitrate,
        container.mux_element_name(),
    );

    record::run_recording(&pipeline, *args.duration, &stop_requested)?;

    if stop_requested.load(Ordering::SeqCst) {
        println!("stopped early: {}", args.output.display());
    } else {
        println!("done: {}", args.output.display());
    }
    Ok(())
}

fn list_sources_command() -> Result<()> {
    println!("0  Full virtual screen (all monitors combined) — default");
    Ok(())
}
