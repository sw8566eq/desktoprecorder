use std::cell::RefCell;

use anyhow::{Context, Result};
use gstreamer as gst;

use crate::portal::{self, PortalSession};

/// Where the recorded pixels come from.
///
/// Everything downstream of `build_element` (pipeline construction,
/// encoding, the CLI) only ever sees a `gst::Element`, which is what let
/// `Portal` get added here without touching `pipeline.rs` or `record.rs`
/// at all -- exactly the seam this enum was written to leave open.
///
/// This is deliberately an enum rather than a trait object: there are
/// only ever two real implementations, and a trait would only buy
/// dynamic dispatch neither needs. Converting it into a trait later, if a
/// genuinely pluggable set of sources ever shows up, is a mechanical
/// refactor -- not a dead end.
pub enum CaptureSource {
    X11Screen(X11ScreenConfig),
    Portal(PortalConfig),
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// There's deliberately no `region`/`xid` here to mirror
/// `X11ScreenConfig`'s: portals don't expose monitor/window enumeration
/// ahead of picking, by design (privacy) -- the compositor draws its own
/// picker during negotiation instead, so there's nothing for a caller to
/// pre-select. See `portal.rs`'s `negotiate`.
pub struct PortalConfig {
    pub show_pointer: bool,
    /// Populated by `build_element` the first time it's called. `RefCell`
    /// because `build_element` takes `&self`, but negotiation only
    /// happens (and only can happen -- it blocks on the user interacting
    /// with the picker dialog) once an element is actually being built.
    /// See `PortalSession`'s doc comment for why keeping it alive exactly
    /// as long as this config -- and therefore the `CaptureSource` it's
    /// wrapped in -- is the right lifetime.
    negotiated: RefCell<Option<PortalSession>>,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self { show_pointer: true, negotiated: RefCell::new(None) }
    }
}

impl CaptureSource {
    /// Builds the actual GStreamer source element for this capture
    /// source. Callers only ever get back a `gst::Element` -- they never
    /// need to know it came from `ximagesrc`/`pipewiresrc` specifically.
    pub fn build_element(&self) -> Result<gst::Element> {
        match self {
            CaptureSource::X11Screen(cfg) => build_ximagesrc(cfg),
            CaptureSource::Portal(cfg) => build_pipewiresrc(cfg),
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

/// Unlike `build_ximagesrc`, this one blocks: `portal::negotiate` waits on
/// the compositor's own picker dialog and the user interacting with it.
fn build_pipewiresrc(cfg: &PortalConfig) -> Result<gst::Element> {
    let session = portal::negotiate(cfg.show_pointer).context(
        "failed to negotiate screen capture via xdg-desktop-portal (is a portal backend with ScreenCast support running? a plain X11 session typically doesn't have one)",
    )?;

    let element = gst::ElementFactory::make("pipewiresrc")
        .property("fd", session.fd_raw())
        .property("target-object", session.node_id().to_string())
        .build()
        .context("failed to create 'pipewiresrc' element (is gstreamer1.0-pipewire installed?)")?;

    // See PortalSession's doc comment: this is what keeps the negotiated
    // fd (and the D-Bus session used to close it politely) alive for
    // exactly as long as this CaptureSource value is.
    *cfg.negotiated.borrow_mut() = Some(session);

    Ok(element)
}

/// `build_element` just constructs and configures a GStreamer element
/// (`ElementFactory::make(...).build()`) -- it never opens an X11
/// connection or reads the screen, so these tests run safely with no
/// display involved, unlike anything that actually starts a pipeline
/// (see record.rs's tests for that, using a synthetic non-X11 source).
#[cfg(test)]
mod tests {
    use gstreamer::prelude::*;

    use super::*;

    fn init_gst() {
        gst::init().expect("gst::init should always succeed in a test process");
    }

    #[test]
    fn default_config_builds_with_expected_defaults() {
        init_gst();
        let elem = CaptureSource::X11Screen(X11ScreenConfig::default()).build_element().unwrap();
        assert!(!elem.property::<bool>("use-damage"));
        assert!(elem.property::<bool>("show-pointer"));
    }

    #[test]
    fn region_sets_start_end_from_position_and_size() {
        init_gst();
        let cfg = X11ScreenConfig {
            region: Some(Region { x: 100, y: 50, width: 800, height: 600 }),
            ..X11ScreenConfig::default()
        };
        let elem = CaptureSource::X11Screen(cfg).build_element().unwrap();
        assert_eq!(elem.property::<u32>("startx"), 100);
        assert_eq!(elem.property::<u32>("starty"), 50);
        // endx/endy are the region's far corner, not its width/height --
        // this is exactly the arithmetic a copy-paste typo could get
        // wrong (e.g. reusing width instead of x + width).
        assert_eq!(elem.property::<u32>("endx"), 900);
        assert_eq!(elem.property::<u32>("endy"), 650);
    }

    #[test]
    fn xid_is_set_when_present() {
        init_gst();
        let cfg = X11ScreenConfig { xid: Some(0x2c00003), ..X11ScreenConfig::default() };
        let elem = CaptureSource::X11Screen(cfg).build_element().unwrap();
        assert_eq!(elem.property::<u64>("xid"), 0x2c00003);
    }

    #[test]
    fn show_pointer_false_is_respected() {
        init_gst();
        let cfg = X11ScreenConfig { show_pointer: false, ..X11ScreenConfig::default() };
        let elem = CaptureSource::X11Screen(cfg).build_element().unwrap();
        assert!(!elem.property::<bool>("show-pointer"));
    }

    #[test]
    fn display_name_is_set_when_present() {
        init_gst();
        let cfg = X11ScreenConfig { display_name: Some(":1".to_string()), ..X11ScreenConfig::default() };
        let elem = CaptureSource::X11Screen(cfg).build_element().unwrap();
        assert_eq!(elem.property::<String>("display-name"), ":1");
    }
}
