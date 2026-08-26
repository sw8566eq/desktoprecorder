//! A global (works-even-when-the-window-isn't-focused) hotkey to
//! start/stop recording, via a direct `XGrabKey` -- a GTK accelerator
//! only fires while the window has keyboard focus, which isn't good
//! enough for a "start recording something else, then hit the hotkey"
//! workflow.
//!
//! This runs its own long-lived X11 connection in a dedicated background
//! thread for the whole process lifetime, unlike `x11_query.rs`'s
//! connect-per-call, one-shot pattern -- it has to stay open to block on
//! `wait_for_event()`. It only ever writes a plain `Send` flag
//! (`Arc<AtomicBool>`); gui.rs is what turns that into an actual
//! start/stop, since the GTK widgets involved are `!Send` and can't be
//! touched from this thread (see the threading note in gui.rs).
//!
//! Default combo: **Ctrl+Alt+R** (`MODIFIER_MASK`/`KEYSYM`), picked to
//! avoid common browser/WM collisions (e.g. Ctrl+Shift+R). No GUI/config
//! exposure yet -- there's no settings screen to put it in -- so this is
//! just two constants to bump if that's ever needed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Keycode, ModMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// X11 keysym for lowercase 'r' -- ASCII-range keysyms match their ASCII
/// code, so no keysym-database dependency is needed just for this.
const KEYSYM_R: u32 = 0x0072;
const KEYSYM_NUM_LOCK: u32 = 0xff7f;

/// Spawns the listener thread. Failure to install the hotkey (the combo
/// is already grabbed by the WM/another app, no X11 connection, etc.) is
/// logged and otherwise ignored -- this is a nice-to-have, not something
/// that should block the GUI from starting.
pub fn spawn_listener(pressed: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        if let Err(err) = run_listener(&pressed) {
            eprintln!("warning: global hotkey (Ctrl+Alt+R) not available: {err:#}");
        }
    });
}

fn run_listener(pressed: &Arc<AtomicBool>) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("failed to connect to the X11 display for the hotkey listener")?;
    let root = conn.setup().roots[screen_num].root;

    let keycode = keycode_for_keysym(&conn, KEYSYM_R)?
        .with_context(|| "no key on this keyboard layout produces 'r' -- can't set up the hotkey")?;

    // A grab with modifiers=Ctrl+Alt only matches when *no other*
    // modifier is also down, so with CapsLock or NumLock on, the same
    // physical keys wouldn't register at all. The fix is grabbing the
    // combo once per combination of whichever lock-ish modifiers happen
    // to be held -- CapsLock is always ModMask::LOCK, NumLock's actual
    // slot is looked up rather than assumed (conventionally Mod2, but
    // not guaranteed).
    let base = ModMask::CONTROL | ModMask::M1;
    for extra in ignorable_lock_mods(&conn)? {
        conn.grab_key(false, root, base | extra, keycode, GrabMode::ASYNC, GrabMode::ASYNC)
            .context("failed to send GrabKey request")?
            .check()
            .context("GrabKey failed -- Ctrl+Alt+R may already be grabbed by the window manager or another app")?;
    }

    loop {
        if let Event::KeyPress(ev) = conn.wait_for_event().context("the hotkey listener's X11 connection was lost")?
            && ev.detail == keycode
        {
            pressed.store(true, Ordering::SeqCst);
        }
    }
}

/// The four `ModMask` combinations (none / CapsLock / NumLock / both) to
/// additionally grab alongside the base Ctrl+Alt, so the hotkey still
/// fires regardless of lock-key state. `None` for NumLock's slot (no
/// NumLock key on this layout) just means there's nothing extra to
/// account for -- CapsLock alone is still covered.
fn ignorable_lock_mods(conn: &RustConnection) -> Result<Vec<ModMask>> {
    let none = ModMask::from(0u16);
    Ok(match numlock_mod_mask(conn)? {
        Some(numlock) => vec![none, ModMask::LOCK, numlock, ModMask::LOCK | numlock],
        None => vec![none, ModMask::LOCK],
    })
}

/// Finds which of the 8 modifier slots (Shift/Lock/Control/Mod1..Mod5)
/// NumLock is actually bound to, via `GetModifierMapping` -- it's
/// conventionally Mod2, but nothing guarantees that, so this looks it up
/// instead of assuming.
fn numlock_mod_mask(conn: &RustConnection) -> Result<Option<ModMask>> {
    let Some(numlock_keycode) = keycode_for_keysym(conn, KEYSYM_NUM_LOCK)? else {
        return Ok(None);
    };

    let reply = conn
        .get_modifier_mapping()
        .context("failed to request the modifier mapping")?
        .reply()
        .context("failed to read the modifier mapping reply")?;
    let per = reply.keycodes_per_modifier() as usize;
    if per == 0 {
        return Ok(None);
    }

    const SLOTS: [ModMask; 8] =
        [ModMask::SHIFT, ModMask::LOCK, ModMask::CONTROL, ModMask::M1, ModMask::M2, ModMask::M3, ModMask::M4, ModMask::M5];
    for (slot, chunk) in reply.keycodes.chunks(per).enumerate() {
        if chunk.contains(&numlock_keycode) {
            return Ok(Some(SLOTS[slot]));
        }
    }
    Ok(None)
}

/// Finds a keycode that produces `keysym` at some shift level, by
/// scanning this connection's full keyboard mapping -- avoids needing a
/// separate keysym-database crate for the handful of keysyms this module
/// cares about.
fn keycode_for_keysym(conn: &RustConnection, keysym: u32) -> Result<Option<Keycode>> {
    let setup = conn.setup();
    let count = setup.max_keycode - setup.min_keycode + 1;
    let reply = conn
        .get_keyboard_mapping(setup.min_keycode, count)
        .context("failed to request the keyboard mapping")?
        .reply()
        .context("failed to read the keyboard mapping reply")?;

    let per = reply.keysyms_per_keycode as usize;
    if per == 0 {
        return Ok(None);
    }
    for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
        if chunk.contains(&keysym) {
            // Safe: i < count == max_keycode - min_keycode + 1, so
            // min_keycode + i never exceeds max_keycode (a valid u8).
            return Ok(Some(setup.min_keycode + i as u8));
        }
    }
    Ok(None)
}
