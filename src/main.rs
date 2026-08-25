mod audio;
mod cli;
mod pipeline;
mod record;
mod source;
mod x11_query;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use gstreamer as gst;

use cli::{AudioMode, Cli, Command, Container, RecordArgs};
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
    if args.monitor.is_some() && args.window.is_some() {
        anyhow::bail!("--monitor and --window can't be combined; pick one capture source");
    }

    let mut screen_cfg = X11ScreenConfig::default();
    if let Some(index) = args.monitor {
        screen_cfg.region = Some(x11_query::monitor_region(index as usize)?);
    }
    if let Some(xid) = args.window {
        if !x11_query::window_exists(xid)? {
            anyhow::bail!("no window with id {xid:#x} (see `list-sources`)");
        }
        screen_cfg.xid = Some(xid);
    }

    let container = args
        .container
        .unwrap_or_else(|| Container::infer_from_path(&args.output));

    let source = CaptureSource::X11Screen(screen_cfg);
    let cfg = RecordConfig {
        output_path: args.output.clone(),
        framerate: args.framerate,
        bitrate_kbps: args.bitrate,
        speed_preset: args.speed_preset,
        container,
        audio_mode: args.audio,
        audio_device: args.audio_device.clone(),
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

    let audio_desc = match args.audio {
        AudioMode::None => "no audio",
        AudioMode::Mic => "mic audio",
        AudioMode::System => "system audio",
        AudioMode::Both => "mic+system audio",
    };
    println!(
        "recording to {} for {} ({}fps, {}kbps, {}, {})... press Ctrl+C to stop early",
        args.output.display(),
        args.duration,
        args.framerate,
        args.bitrate,
        container.mux_element_name(),
        audio_desc,
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
    println!("(no --monitor/--window)  Full virtual screen (all monitors combined) — default\n");

    println!("Monitors (--monitor <index>):");
    let monitors = x11_query::list_monitors().context("failed to list monitors")?;
    for m in &monitors {
        let primary = if m.primary { " (primary)" } else { "" };
        println!(
            "  {:<3} {:<12}{}  {}x{}+{}+{}",
            m.index, m.name, primary, m.region.width, m.region.height, m.region.x, m.region.y
        );
    }

    println!("\nWindows (--window <id>):");
    match x11_query::list_windows() {
        Ok(windows) if windows.is_empty() => println!("  (none found)"),
        Ok(windows) => {
            for w in &windows {
                let geom = match w.region {
                    Some(r) => format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y),
                    None => "position/size unknown".to_string(),
                };
                println!("  {:#010x}  [{}] {}  {}", w.xid, w.class, w.title, geom);
            }
        }
        Err(err) => eprintln!("  warning: could not list windows: {err:#}"),
    }

    Ok(())
}
