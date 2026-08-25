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
    /// screen. Not implemented yet -- reserved for a later milestone.
    #[arg(long)]
    pub monitor: Option<u32>,

    /// Capture a specific window by its X11 window ID (see
    /// `list-sources`). Not implemented yet -- same status as --monitor.
    #[arg(long)]
    pub window: Option<u64>,
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
