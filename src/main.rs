mod audio;
mod cli;
mod config;
mod gui;
mod hotkey;
mod pipeline;
mod portal;
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
use source::{CaptureSource, PortalConfig, Region, X11ScreenConfig};

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
        Command::Gui => gui_command(),
    }
}

/// Wayland sessions typically still have an X11 connection available via
/// XWayland (most compositors start one automatically, for X11 app
/// compatibility) -- checking for `$WAYLAND_DISPLAY`, not for a live X11
/// connection, is what actually distinguishes "Wayland, with XWayland as
/// a side effect" from "actually X11".
fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// The GUI's monitor/window dropdown and live preview are built entirely
/// around `X11Screen` (`x11_query` enumeration, an `ximagesrc`-based
/// preview pipeline) -- extending that flow to the portal's own
/// compositor-drawn picker is real, separate UX work (see TODO.md), not
/// done yet. `record_command` below is the one Wayland-capable path for
/// now.
fn gui_command() -> Result<()> {
    if is_wayland() {
        anyhow::bail!(
            "this looks like a Wayland session (WAYLAND_DISPLAY is set) -- the GUI only supports X11 for now.\n\
             `desktoprecorder record` supports Wayland via xdg-desktop-portal; the GUI doesn't yet.\n\
             If you're actually running under X11 and WAYLAND_DISPLAY is just left over from something else, unset it and try again."
        );
    }
    gui::run()
}

fn record_command(args: RecordArgs) -> Result<()> {
    if args.monitor.is_some() && args.window.is_some() {
        anyhow::bail!("--monitor and --window can't be combined; pick one capture source");
    }

    let mut source = if is_wayland() {
        // Portals don't expose monitor/window enumeration ahead of
        // picking, by design (privacy) -- the compositor draws its own
        // picker during Start instead, so there's nothing here to apply
        // --monitor/--window to. Warn rather than either silently
        // ignoring them or hard-failing a command that can otherwise
        // still succeed.
        if args.monitor.is_some() || args.window.is_some() {
            eprintln!("note: --monitor/--window are ignored under Wayland -- the system picker will ask you to choose instead");
        }
        println!("waiting for you to choose what to share (look for a system dialog)...");
        CaptureSource::Portal(PortalConfig::default())
    } else {
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
        CaptureSource::X11Screen(screen_cfg)
    };

    let container = args
        .container
        .unwrap_or_else(|| Container::infer_from_path(&args.output));

    let cfg = RecordConfig {
        output_path: args.output.clone(),
        framerate: args.framerate,
        bitrate_kbps: args.bitrate,
        speed_preset: args.speed_preset,
        container,
        audio_mode: args.audio,
        audio_device: args.audio_device.clone(),
    };

    let pipeline = pipeline::build_recording_pipeline(&mut source, &cfg)?;

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
    // x11_query's RandR/EWMH queries would either fail outright or -- via
    // XWayland -- return stale/misleading data under Wayland (same
    // concern `record_command` avoids the same way). Portals don't
    // expose enumeration ahead of picking anyway (see PortalConfig's doc
    // comment), so there's no --monitor/--window list to print here.
    if is_wayland() {
        println!(
            "This is a Wayland session -- xdg-desktop-portal doesn't expose monitor/window \
             enumeration ahead of time (by design, for privacy), so there's no --monitor/--window \
             list to show. `desktoprecorder record` will show the compositor's own picker dialog \
             instead."
        );
        return Ok(());
    }

    println!("(no --monitor/--window)  Full virtual screen (all monitors combined) — default\n");

    println!("Monitors (--monitor <index>):");
    let monitors = x11_query::list_monitors().context("failed to list monitors")?;
    for m in &monitors {
        let primary = if m.primary { " (primary)" } else { "" };
        println!("  {:<3} {:<12}{}  {}", m.index, m.name, primary, format_region(&m.region));
    }

    println!("\nWindows (--window <id>):");
    match x11_query::list_windows() {
        Ok(windows) if windows.is_empty() => println!("  (none found)"),
        Ok(windows) => {
            for w in &windows {
                let geom = w.region.as_ref().map_or_else(|| "position/size unknown".to_string(), format_region);
                println!("  {:#010x}  [{}] {}  {}", w.xid, w.class, w.title, geom);
            }
        }
        Err(err) => eprintln!("  warning: could not list windows: {err:#}"),
    }

    Ok(())
}

/// `list-sources`' shared "WxH+X+Y" geometry format, for both monitors
/// and windows.
fn format_region(r: &Region) -> String {
    format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y)
}
