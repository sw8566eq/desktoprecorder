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
    Ok(lock_mod_combinations(numlock_mod_mask(conn)?))
}

/// The actual combination logic behind `ignorable_lock_mods`, split out
/// so it's testable without a live X11 connection -- `numlock` is
/// whatever `numlock_mod_mask` resolved (or `None` on a layout with no
/// NumLock key at all).
fn lock_mod_combinations(numlock: Option<ModMask>) -> Vec<ModMask> {
    let none = ModMask::from(0u16);
    match numlock {
        Some(numlock) => vec![none, ModMask::LOCK, numlock, ModMask::LOCK | numlock],
        None => vec![none, ModMask::LOCK],
    }
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
    Ok(find_modifier_slot(per, &reply.keycodes, numlock_keycode))
}

const MODIFIER_SLOTS: [ModMask; 8] =
    [ModMask::SHIFT, ModMask::LOCK, ModMask::CONTROL, ModMask::M1, ModMask::M2, ModMask::M3, ModMask::M4, ModMask::M5];

/// The actual `GetModifierMapping`-reply scan behind `numlock_mod_mask`,
/// split out so it's testable with fabricated reply data instead of a
/// live X11 connection. `keycodes` is the reply's flat array (8 chunks of
/// `keycodes_per_modifier` keycodes each, one chunk per `MODIFIER_SLOTS`
/// entry); `per == 0` (a malformed/empty reply) yields `None` rather than
/// panicking on `chunks(0)`.
fn find_modifier_slot(keycodes_per_modifier: usize, keycodes: &[Keycode], target: Keycode) -> Option<ModMask> {
    if keycodes_per_modifier == 0 {
        return None;
    }
    keycodes
        .chunks(keycodes_per_modifier)
        .zip(MODIFIER_SLOTS)
        .find_map(|(chunk, slot)| chunk.contains(&target).then_some(slot))
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
    Ok(find_keycode(setup.min_keycode, per, &reply.keysyms, keysym))
}

/// The actual `GetKeyboardMapping`-reply scan behind `keycode_for_keysym`,
/// split out so it's testable with fabricated reply data instead of a
/// live X11 connection. `keysyms` is the reply's flat array (one chunk of
/// `keysyms_per_keycode` keysyms per keycode, starting at `min_keycode`);
/// `keysyms_per_keycode == 0` (a malformed/empty reply) yields `None`
/// rather than panicking on `chunks(0)`.
fn find_keycode(min_keycode: Keycode, keysyms_per_keycode: usize, keysyms: &[u32], target: u32) -> Option<Keycode> {
    if keysyms_per_keycode == 0 {
        return None;
    }
    let offset = keysyms.chunks(keysyms_per_keycode).position(|chunk| chunk.contains(&target))?;
    // Safe: `offset` is a valid chunk index into a reply describing
    // min_keycode..=max_keycode, so it never pushes the result past a
    // valid u8 keycode.
    Some(min_keycode + offset as u8)
}

/// Tests below cover the pure decode/combination logic factored out
/// above (`find_keycode`, `find_modifier_slot`, `lock_mod_combinations`)
/// with fabricated reply data. `run_listener` itself -- the live
/// connection, the actual `GrabKey`, `wait_for_event` -- is deliberately
/// not exercised here: an automated test that grabs a real global hotkey
/// against whatever desktop happens to be at $DISPLAY on every
/// `cargo test` run would affect that live session, not a disposable
/// test fixture, so it stays a manual verification step (see CLAUDE.md).
#[cfg(test)]
mod tests {
    use super::*;

    // A keyboard mapping reply with keysyms_per_keycode = 2 (unshifted,
    // shifted), starting at min_keycode = 8 -- roughly what a real
    // GetKeyboardMapping reply looks like, just small enough to write by
    // hand. Keycode 8 -> [1, '!'], keycode 9 -> ['r', 'R'] (KEYSYM_R at
    // the unshifted level), keycode 10 -> [0xff7f, 0] (KEYSYM_NUM_LOCK,
    // unshifted only).
    const KEYSYMS_PER_KEYCODE: usize = 2;
    const MIN_KEYCODE: Keycode = 8;
    const KEYSYMS: [u32; 6] = [0x0031, 0x0021, KEYSYM_R, 0x0052, KEYSYM_NUM_LOCK, 0];

    #[test]
    fn find_keycode_locates_matching_chunk() {
        assert_eq!(find_keycode(MIN_KEYCODE, KEYSYMS_PER_KEYCODE, &KEYSYMS, KEYSYM_R), Some(9));
        assert_eq!(find_keycode(MIN_KEYCODE, KEYSYMS_PER_KEYCODE, &KEYSYMS, KEYSYM_NUM_LOCK), Some(10));
    }

    #[test]
    fn find_keycode_matches_shifted_level_too() {
        // 'R' capital-shifted, still keycode 9 -- confirms the scan
        // checks every keysym in a chunk, not just index 0.
        assert_eq!(find_keycode(MIN_KEYCODE, KEYSYMS_PER_KEYCODE, &KEYSYMS, 0x0052), Some(9));
    }

    #[test]
    fn find_keycode_none_when_not_present() {
        assert_eq!(find_keycode(MIN_KEYCODE, KEYSYMS_PER_KEYCODE, &KEYSYMS, 0xdead), None);
    }

    #[test]
    fn find_keycode_none_on_degenerate_reply() {
        assert_eq!(find_keycode(MIN_KEYCODE, 0, &KEYSYMS, KEYSYM_R), None);
        assert_eq!(find_keycode(MIN_KEYCODE, KEYSYMS_PER_KEYCODE, &[], KEYSYM_R), None);
    }

    #[test]
    fn find_modifier_slot_locates_numlock_in_a_non_default_slot() {
        // 4 keycodes per modifier, 8 slots (Shift..Mod5); put NumLock's
        // keycode in the Mod3 slot (index 5) specifically, since Mod2 is
        // the common-but-not-guaranteed convention this exists to not
        // assume.
        let mut keycodes = [0u8; 32];
        keycodes[5 * 4] = 77; // Mod3 slot, first of its 4 keycodes
        assert_eq!(find_modifier_slot(4, &keycodes, 77), Some(ModMask::M3));
    }

    #[test]
    fn find_modifier_slot_none_when_absent() {
        let keycodes = [0u8; 32];
        assert_eq!(find_modifier_slot(4, &keycodes, 77), None);
    }

    #[test]
    fn find_modifier_slot_none_on_degenerate_reply() {
        assert_eq!(find_modifier_slot(0, &[], 77), None);
    }

    #[test]
    fn lock_mod_combinations_without_numlock_covers_capslock_only() {
        let combos = lock_mod_combinations(None);
        assert_eq!(combos, vec![ModMask::from(0u16), ModMask::LOCK]);
    }

    #[test]
    fn lock_mod_combinations_with_numlock_covers_all_four() {
        let combos = lock_mod_combinations(Some(ModMask::M2));
        assert_eq!(combos, vec![ModMask::from(0u16), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2]);
    }
}
