//! Real monitor/window enumeration for `list-sources` and for resolving
//! `--monitor`/`--window` into a `Region`/xid, via a direct X11 protocol
//! connection (`x11rb`) rather than shelling out to `xrandr`/`wmctrl` --
//! this needs no extra runtime tools installed, and gives typed replies
//! instead of parsing CLI text output (which is what bit us with
//! `ffprobe` not being installed during earlier verification -- prefer
//! not depending on optional external tools when a library will do).
//!
//! This is the only module that talks XCB directly; `source.rs` and
//! everything downstream of it only ever sees plain `Region`/`u64` xid
//! values, same as the `CaptureSource` seam keeps GStreamer plumbing
//! ignorant of X11 vs. a future Wayland/portal source.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use crate::source::Region;

pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub primary: bool,
    pub region: Region,
}

pub struct WindowInfo {
    pub xid: u64,
    pub title: String,
    pub class: String,
    /// `None` if geometry couldn't be resolved (e.g. the window closed
    /// between listing and querying it) -- informational only, doesn't
    /// block using the xid with `--window`.
    pub region: Option<Region>,
}

fn connect() -> Result<(RustConnection, usize)> {
    x11rb::connect(None).context("failed to connect to the X11 display (is $DISPLAY set and an X server running?)")
}

/// Converts RandR/geometry's signed protocol coordinates into this
/// crate's unsigned `Region`. `ximagesrc`'s startx/starty/endx/endy
/// properties are themselves unsigned, so a monitor/window positioned
/// left of or above the virtual screen's origin (a real but uncommon
/// multi-monitor layout) can't be captured via region cropping anyway --
/// surfacing that as an error here is more honest than silently wrapping
/// to a huge u32.
fn nonneg(x: i16, y: i16, width: u16, height: u16, what: &str) -> Result<Region> {
    if x < 0 || y < 0 {
        anyhow::bail!("{what} is positioned off the virtual screen's origin ({x}, {y}), which isn't capturable as a region");
    }
    Ok(Region {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
    })
}

/// Lists monitors via the RandR extension's `GetMonitors` request --
/// what `xrandr --listmonitors` itself is built on. Index order matches
/// what `--monitor <index>` expects.
pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let (conn, screen_num) = connect()?;
    let root = conn.setup().roots[screen_num].root;

    let reply = conn
        .randr_get_monitors(root, true)
        .context("failed to request RandR monitor list (does this X server support RandR?)")?
        .reply()
        .context("failed to read RandR monitor list reply")?;

    let mut monitors = Vec::with_capacity(reply.monitors.len());
    for (index, m) in reply.monitors.into_iter().enumerate() {
        let name = atom_name(&conn, m.name).unwrap_or_else(|_| format!("monitor-{index}"));
        let region = nonneg(m.x, m.y, m.width, m.height, &format!("monitor '{name}'"))?;
        monitors.push(MonitorInfo {
            index,
            name,
            primary: m.primary,
            region,
        });
    }
    Ok(monitors)
}

/// Resolves the region for one monitor by index (as listed by
/// `list_monitors`/`list-sources`).
pub fn monitor_region(index: usize) -> Result<Region> {
    let monitors = list_monitors()?;
    monitors
        .into_iter()
        .find(|m| m.index == index)
        .map(|m| m.region)
        .with_context(|| format!("no monitor with index {index} (see `list-sources`)"))
}

/// Confirms `xid` is a window that currently exists, so a typo'd
/// `--window` fails fast with a clear message instead of a confusing
/// GStreamer runtime error once the pipeline starts.
pub fn window_exists(xid: u64) -> Result<bool> {
    let (conn, _) = connect()?;
    let window = u32::try_from(xid).context("window id doesn't fit in 32 bits (X11 window IDs are 32-bit)")?;
    Ok(conn.get_geometry(window).context("failed to query window geometry")?.reply().is_ok())
}

/// Lists top-level windows via `_NET_CLIENT_LIST` on the root window --
/// requires a window manager that implements the EWMH spec (true of
/// every mainstream Linux WM/DE, including XFCE).
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let (conn, screen_num) = connect()?;
    let root = conn.setup().roots[screen_num].root;

    let net_client_list = intern(&conn, b"_NET_CLIENT_LIST")?;
    let net_wm_name = intern(&conn, b"_NET_WM_NAME")?;
    let utf8_string = intern(&conn, b"UTF8_STRING")?;

    let reply = conn
        .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, u32::MAX)
        .context("failed to request _NET_CLIENT_LIST")?
        .reply()
        .context("failed to read _NET_CLIENT_LIST (is a window manager running with EWMH support?)")?;

    let xids: Vec<u32> = reply.value32().map(|it| it.collect()).unwrap_or_default();

    let mut windows = Vec::with_capacity(xids.len());
    for xid in xids {
        let title = window_title(&conn, xid, net_wm_name, utf8_string).unwrap_or_else(|_| "(untitled)".to_string());
        let class = window_class(&conn, xid).unwrap_or_else(|_| "(unknown)".to_string());
        let region = window_region(&conn, root, xid).ok();
        windows.push(WindowInfo {
            xid: xid as u64,
            title,
            class,
            region,
        });
    }
    Ok(windows)
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<u32> {
    Ok(conn
        .intern_atom(false, name)
        .with_context(|| format!("failed to intern atom {:?}", String::from_utf8_lossy(name)))?
        .reply()
        .with_context(|| format!("failed to read InternAtom reply for {:?}", String::from_utf8_lossy(name)))?
        .atom)
}

fn atom_name(conn: &RustConnection, atom: u32) -> Result<String> {
    let reply = conn
        .get_atom_name(atom)
        .context("failed to request atom name")?
        .reply()
        .context("failed to read GetAtomName reply")?;
    Ok(String::from_utf8_lossy(&reply.name).into_owned())
}

/// Prefers `_NET_WM_NAME` (UTF-8, modern EWMH) and falls back to the
/// legacy ICCCM `WM_NAME` (Latin-1/ASCII in practice) if a window hasn't
/// set the former.
fn window_title(conn: &RustConnection, xid: Window, net_wm_name: u32, utf8_string: u32) -> Result<String> {
    let reply = conn
        .get_property(false, xid, net_wm_name, utf8_string, 0, u32::MAX)
        .context("failed to request _NET_WM_NAME")?
        .reply()
        .context("failed to read _NET_WM_NAME reply")?;
    if !reply.value.is_empty() {
        return Ok(String::from_utf8_lossy(&reply.value).into_owned());
    }

    let reply = conn
        .get_property(false, xid, AtomEnum::WM_NAME, AtomEnum::STRING, 0, u32::MAX)
        .context("failed to request WM_NAME")?
        .reply()
        .context("failed to read WM_NAME reply")?;
    if !reply.value.is_empty() {
        return Ok(String::from_utf8_lossy(&reply.value).into_owned());
    }

    anyhow::bail!("window {xid:#x} has no _NET_WM_NAME or WM_NAME set")
}

/// `WM_CLASS` is two nul-separated strings, "instance\0class\0" -- the
/// class (second part) is the more stable/useful one for identifying
/// which application a window belongs to (e.g. "Firefox", "Gimp").
fn window_class(conn: &RustConnection, xid: Window) -> Result<String> {
    let reply = conn
        .get_property(false, xid, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, u32::MAX)
        .context("failed to request WM_CLASS")?
        .reply()
        .context("failed to read WM_CLASS reply")?;

    let parts: Vec<&[u8]> = reply.value.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
    let class = parts.get(1).or(parts.first()).context("window has no WM_CLASS set")?;
    Ok(String::from_utf8_lossy(class).into_owned())
}

fn window_region(conn: &RustConnection, root: Window, xid: Window) -> Result<Region> {
    let geom = conn.get_geometry(xid).context("failed to request window geometry")?.reply().context("failed to read window geometry")?;
    // GetGeometry's x/y are relative to the window's parent (usually a
    // window-manager-added frame, not the root), so the position has to
    // be translated into root/screen coordinates separately.
    let pos = conn
        .translate_coordinates(xid, root, 0, 0)
        .context("failed to translate window coordinates to the root window")?
        .reply()
        .context("failed to read TranslateCoordinates reply")?;
    nonneg(pos.dst_x, pos.dst_y, geom.width, geom.height, &format!("window {xid:#x}"))
}
