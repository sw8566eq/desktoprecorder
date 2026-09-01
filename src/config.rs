//! Persisted GUI settings.
//!
//! GUI only, deliberately -- the CLI's settings are just its argv for a
//! given invocation, so there's nothing there to persist across runs.
//!
//! Stored as TOML at `$XDG_CONFIG_HOME/desktoprecorder/config.toml`
//! (falling back to `~/.config/...` per the XDG Base Directory spec, the
//! same fallback `@DEFAULT_SINK@`-style PulseAudio lookups in audio.rs
//! already assume the box has). No `dirs`/`directories` crate: it's two
//! env var reads, in keeping with this codebase's habit of hand-rolling
//! small OS queries directly (`x11_query.rs`, `audio.rs`'s own
//! `pactl`-shelling) rather than reaching for a crate over it.
//!
//! Every field is `#[serde(default)]`, and `load()` never fails outright
//! -- a missing file (the common first-run case), one from a future
//! version with unknown fields, or one hand-edited down to a single line
//! all fall back to `Settings::default()` on a per-field basis rather
//! than taking down the whole GUI over one bad settings file.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{AudioMode, DEFAULT_BITRATE_KBPS, DEFAULT_FRAMERATE, DEFAULT_SPEED_PRESET};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `None` => full screen; `Some(i)` => monitor index `i` (matching
    /// `list-sources`' ordering). A saved *window* is deliberately never
    /// persisted here -- window IDs are ephemeral across sessions (they
    /// don't survive the window, or the session, closing), so a saved
    /// one would almost always be stale by the next launch.
    pub source_monitor: Option<u32>,
    pub audio_mode: AudioMode,
    pub audio_device: Option<String>,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub speed_preset: String,
    // Deliberately no `container` or `output_path` field: the GUI has no
    // container widget of its own (Container::infer_from_path derives it
    // from the output path's extension every time), and the output path
    // itself is deliberately excluded so a restored session can't
    // silently overwrite a prior recording on the next Start.
}

impl Default for Settings {
    /// Reuses the CLI's own flag defaults (`cli.rs`) rather than
    /// hardcoding a second copy of the same numbers -- this is what a
    /// brand-new install falls back to before any config file has ever
    /// been written, and what `gui.rs`'s widgets end up showing on first
    /// launch (see `apply_settings`, called unconditionally right after
    /// `build_ui` constructs them).
    fn default() -> Self {
        Self {
            source_monitor: None,
            audio_mode: AudioMode::None,
            audio_device: None,
            framerate: DEFAULT_FRAMERATE,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            speed_preset: DEFAULT_SPEED_PRESET.to_string(),
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("desktoprecorder").join("config.toml"))
}

/// Loads saved settings, falling back to `Settings::default()` on any
/// problem at all: no resolvable config dir, missing file, unreadable,
/// or unparseable. A broken settings file should never be the reason the
/// GUI won't start -- unlike `save`'s failure, which the caller is
/// expected to surface, a failed *load* is the routine, silent case.
pub fn load() -> Settings {
    let Some(path) = config_path() else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Saves `settings`, creating the config directory if it doesn't exist
/// yet. Returns an error (for the caller to toast a warning on, not
/// panic over) rather than failing silently -- a failed *save* is worth
/// surfacing, since unlike a missing file on load, it means a setting
/// the user just changed won't stick.
pub fn save(settings: &Settings) -> Result<()> {
    let path = config_path().context("couldn't determine a config directory (no $XDG_CONFIG_HOME or $HOME set)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("failed to create '{}'", parent.display()))?;
    }
    let text = toml::to_string_pretty(settings).context("failed to serialize settings")?;
    std::fs::write(&path, text).with_context(|| format!("failed to write '{}'", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            source_monitor: Some(1),
            audio_mode: AudioMode::Both,
            audio_device: Some("alsa_input.foo".to_string()),
            framerate: 60,
            bitrate_kbps: 12_000,
            speed_preset: "fast".to_string(),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let settings = sample();
        let text = toml::to_string_pretty(&settings).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, settings);
    }

    #[test]
    fn lowercase_enum_value_matches_cli_flag_spelling() {
        // AudioMode::Both should round-trip as the same lowercase string
        // --audio accepts on the CLI, per the
        // #[serde(rename_all = "lowercase")] on the enum.
        let text = toml::to_string_pretty(&sample()).unwrap();
        assert!(text.contains("audio_mode = \"both\""));
    }

    #[test]
    fn missing_fields_fall_back_to_per_field_defaults() {
        // Simulates a config file from an older/partial version, or one
        // hand-edited down to a single line.
        let parsed: Settings = toml::from_str("framerate = 60\n").unwrap();
        assert_eq!(parsed.framerate, 60);
        assert_eq!(parsed.audio_mode, Settings::default().audio_mode);
        assert_eq!(parsed.bitrate_kbps, Settings::default().bitrate_kbps);
        assert_eq!(parsed.source_monitor, Settings::default().source_monitor);
    }

    #[test]
    fn corrupt_toml_fails_to_parse_so_load_can_fall_back() {
        // load() itself touches the filesystem (not exercised here, same
        // reasoning as x11_query.rs/hotkey.rs's live-connection tests) --
        // this pins down the parsing half of its unwrap_or_default fallback.
        let result: std::result::Result<Settings, _> = toml::from_str("not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn config_path_is_none_without_any_resolvable_base() {
        // SAFETY: test-only, single-threaded within this process's test
        // harness for env vars this test itself controls; restored
        // immediately after.
        let xdg = std::env::var_os("XDG_CONFIG_HOME");
        let home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("HOME");
        }
        let path = config_path();
        unsafe {
            if let Some(v) = xdg {
                std::env::set_var("XDG_CONFIG_HOME", v);
            }
            if let Some(v) = home {
                std::env::set_var("HOME", v);
            }
        }
        assert!(path.is_none());
    }
}
