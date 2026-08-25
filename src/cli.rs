use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "desktoprecorder", about = "A Linux screen recorder built on GStreamer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Record the screen to a file.
    Record(RecordArgs),
    /// List available capture sources (monitors/windows).
    ListSources,
    /// Launch the graphical interface.
    Gui,
}

#[derive(Args, Debug)]
pub struct RecordArgs {
    /// Output file path. Container is inferred from the extension
    /// (.mkv -> matroska, .mp4 -> mp4) unless --container is given.
    #[arg(short, long)]
    pub output: PathBuf,

    /// How long to record, e.g. "10s", "2m30s".
    #[arg(short, long)]
    pub duration: humantime::Duration,

    #[arg(long, default_value_t = 30)]
    pub framerate: u32,

    /// Target video bitrate in kbps.
    #[arg(long, default_value_t = 8000)]
    pub bitrate: u32,

    #[arg(long, default_value = "veryfast")]
    pub speed_preset: String,

    /// Container format. Inferred from --output's extension if omitted.
    #[arg(long, value_enum)]
    pub container: Option<Container>,

    /// Capture a specific monitor by index instead of the full virtual
    /// screen (see `list-sources`). Can't be combined with --window.
    #[arg(long)]
    pub monitor: Option<u32>,

    /// Capture a specific window by its X11 window ID instead of the
    /// full virtual screen, e.g. "0x2c00003" as printed by
    /// `list-sources` (plain decimal also accepted). Can't be combined
    /// with --monitor.
    #[arg(long, value_parser = parse_window_id)]
    pub window: Option<u64>,

    /// What audio, if any, to capture alongside the video.
    #[arg(long, value_enum, default_value = "none")]
    pub audio: AudioMode,

    /// Override the PulseAudio device used for --audio=mic or
    /// --audio=system (see `pactl list sources short` for names).
    /// Not supported with --audio=both, which always uses the default
    /// mic and the default sink's monitor.
    #[arg(long)]
    pub audio_device: Option<String>,
}

/// Parses a `--window` value as hex (with an optional "0x"/"0X" prefix,
/// matching how `list-sources` prints window IDs -- and how `xwininfo`/
/// `wmctrl` conventionally print them) or, failing that, as plain
/// decimal.
fn parse_window_id(s: &str) -> Result<u64, String> {
    let err = || format!("invalid window id '{s}' (expected e.g. \"0x2c00003\" or a decimal number)");
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        // Only treat it as hex if "0x"/"0X" was actually present --
        // otherwise a plain decimal id like "20" would silently be
        // misread as hex (0x20 = 32).
        Some(hex) => u64::from_str_radix(hex, 16).map_err(|_| err()),
        None => s.parse::<u64>().map_err(|_| err()),
    }
}

/// What audio, if any, to capture alongside the video.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    /// Video only (default) -- matches milestones 1/2's behavior.
    None,
    /// The default microphone / input source.
    Mic,
    /// Desktop/system audio, via the default sink's PulseAudio monitor.
    System,
    /// Mic and system audio mixed together.
    Both,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mkv,
    Mp4,
}

impl Container {
    pub fn mux_element_name(self) -> &'static str {
        match self {
            Container::Mkv => "matroskamux",
            Container::Mp4 => "qtmux",
        }
    }

    /// Infers a container from an output path's extension, defaulting to
    /// Matroska for unrecognized/missing extensions -- mkv degrades more
    /// gracefully than mp4 if shutdown ever goes wrong (see plan).
    pub fn infer_from_path(path: &Path) -> Container {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("mp4") | Some("m4v") | Some("mov") => Container::Mp4,
            _ => Container::Mkv,
        }
    }
}
