use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

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
///
/// `Serialize`/`Deserialize` are for `config.rs`'s persisted GUI settings
/// (`rename_all = "lowercase"` so the TOML spelling matches `--audio`'s
/// own CLI values) -- unrelated to `ValueEnum`, which clap derives its
/// own casing for independently.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn parse_window_id_accepts_hex_with_prefix() {
        assert_eq!(parse_window_id("0x2c00003").unwrap(), 0x2c00003);
        assert_eq!(parse_window_id("0X2C00003").unwrap(), 0x2c00003);
    }

    #[test]
    fn parse_window_id_accepts_plain_decimal() {
        // "20" without a 0x/0X prefix must stay decimal (20), not be
        // misread as hex (which would be 32) -- the exact footgun the
        // strip_prefix check exists to avoid.
        assert_eq!(parse_window_id("20").unwrap(), 20);
        assert_eq!(parse_window_id("0").unwrap(), 0);
    }

    #[test]
    fn parse_window_id_rejects_garbage() {
        assert!(parse_window_id("not-a-window-id").is_err());
        assert!(parse_window_id("0xzzzz").is_err());
        assert!(parse_window_id("").is_err());
    }

    #[test]
    fn container_inferred_from_extension() {
        assert_eq!(Container::infer_from_path(Path::new("out.mp4")), Container::Mp4);
        assert_eq!(Container::infer_from_path(Path::new("out.m4v")), Container::Mp4);
        assert_eq!(Container::infer_from_path(Path::new("out.mov")), Container::Mp4);
        // Case-insensitive: the match lowercases the extension first.
        assert_eq!(Container::infer_from_path(Path::new("out.MP4")), Container::Mp4);
        assert_eq!(Container::infer_from_path(Path::new("out.mkv")), Container::Mkv);
    }

    #[test]
    fn container_defaults_to_mkv_for_unknown_or_missing_extension() {
        assert_eq!(Container::infer_from_path(Path::new("out.avi")), Container::Mkv);
        assert_eq!(Container::infer_from_path(Path::new("out")), Container::Mkv);
    }

    #[test]
    fn container_mux_element_names() {
        assert_eq!(Container::Mkv.mux_element_name(), "matroskamux");
        assert_eq!(Container::Mp4.mux_element_name(), "qtmux");
    }

    /// `--monitor`/`--window` mutual exclusion is enforced in
    /// `main.rs::record_command`, not by clap -- confirms clap itself
    /// still accepts both flags at parse time, i.e. that app-level check
    /// is load-bearing and not redundant with anything clap already does.
    #[test]
    fn clap_allows_monitor_and_window_together() {
        let cli = Cli::try_parse_from(["desktoprecorder", "record", "-o", "out.mkv", "-d", "10s", "--monitor", "0", "--window", "0x1"]).unwrap();
        let Command::Record(args) = cli.command else { panic!("expected Record") };
        assert_eq!(args.monitor, Some(0));
        assert_eq!(args.window, Some(1));
    }

    #[test]
    fn clap_defaults_match_documented_values() {
        let cli = Cli::try_parse_from(["desktoprecorder", "record", "-o", "out.mkv", "-d", "10s"]).unwrap();
        let Command::Record(args) = cli.command else { panic!("expected Record") };
        assert_eq!(args.framerate, 30);
        assert_eq!(args.bitrate, 8000);
        assert_eq!(args.speed_preset, "veryfast");
        assert_eq!(args.audio, AudioMode::None);
        assert_eq!(args.container, None);
        assert_eq!(args.audio_device, None);
    }

    #[test]
    fn clap_parses_duration_and_window_flag_end_to_end() {
        let cli = Cli::try_parse_from(["desktoprecorder", "record", "-o", "out.mkv", "-d", "2m30s", "--window", "0x2c00003"]).unwrap();
        let Command::Record(args) = cli.command else { panic!("expected Record") };
        assert_eq!(*args.duration, Duration::from_secs(150));
        assert_eq!(args.window, Some(0x2c00003));
    }

    #[test]
    fn clap_rejects_invalid_duration() {
        assert!(Cli::try_parse_from(["desktoprecorder", "record", "-o", "out.mkv", "-d", "not-a-duration"]).is_err());
    }

    #[test]
    fn clap_rejects_missing_required_args() {
        assert!(Cli::try_parse_from(["desktoprecorder", "record"]).is_err());
    }
}
