use anyhow::{Context, Result};
use gstreamer as gst;

/// Where the recorded pixels come from.
///
/// Only `X11Screen` is implemented today. A Wayland/`xdg-desktop-portal`
/// + `pipewiresrc`-based variant is the natural extension for later, once
/// this needs to run under Wayland instead of X11 -- everything
/// downstream of `build_element` (pipeline construction, encoding, the
/// CLI) only ever sees a `gst::Element`, so adding that variant won't
/// require touching `pipeline.rs` or `record.rs`.
///
/// This is deliberately an enum rather than a trait object: there is
/// exactly one real implementation today, and a trait would only buy
/// dynamic dispatch this doesn't need yet. Converting it into a trait
/// later, if a genuinely pluggable set of sources ever shows up, is a
/// mechanical refactor -- not a dead end.
pub enum CaptureSource {
    X11Screen(X11ScreenConfig),
    // Portal(PortalConfig), // future: Wayland via xdg-desktop-portal + pipewiresrc
}

#[derive(Debug, Clone)]
pub struct X11ScreenConfig {
    /// X11 display name (e.g. ":0"); `None` uses ximagesrc's default ($DISPLAY).
    pub display_name: Option<String>,
    /// Capture only this region instead of the whole screen. Wired up in
    /// a later milestone alongside real monitor/window enumeration.
    pub region: Option<Region>,
    /// Capture a specific window instead of the root window. Same status
    /// as `region`.
    pub xid: Option<u64>,
    pub show_pointer: bool,
}

impl Default for X11ScreenConfig {
    fn default() -> Self {
        Self {
            display_name: None,
            region: None,
            xid: None,
            show_pointer: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CaptureSource {
    /// Builds the actual GStreamer source element for this capture
    /// source. Callers only ever get back a `gst::Element` -- they never
    /// need to know it came from `ximagesrc` specifically.
    pub fn build_element(&self) -> Result<gst::Element> {
        match self {
            CaptureSource::X11Screen(cfg) => build_ximagesrc(cfg),
        }
    }
}

fn build_ximagesrc(cfg: &X11ScreenConfig) -> Result<gst::Element> {
    let mut builder = gst::ElementFactory::make("ximagesrc")
        .property("use-damage", false)
        .property("show-pointer", cfg.show_pointer);

    if let Some(name) = &cfg.display_name {
        builder = builder.property("display-name", name);
    }
    if let Some(xid) = cfg.xid {
        builder = builder.property("xid", xid);
    }
    if let Some(region) = &cfg.region {
        builder = builder
            .property("startx", region.x)
            .property("starty", region.y)
            .property("endx", region.x + region.width)
            .property("endy", region.y + region.height);
    }

    builder
        .build()
        .context("failed to create 'ximagesrc' element (is gstreamer1.0-plugins-good installed?)")
}
