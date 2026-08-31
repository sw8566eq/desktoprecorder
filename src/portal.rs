//! Wayland screen capture via `xdg-desktop-portal`'s ScreenCast interface
//! and PipeWire -- the counterpart to `x11_query.rs`'s direct-X11 queries
//! and `source.rs`'s `ximagesrc` path, for when there's no X11 root
//! window to read from at all.
//!
//! Uses `zbus::blocking`, not the higher-level `ashpd` crate: `ashpd` is
//! async-only (`tokio`, or an `async-io`-compatible executor), and this
//! project has no async runtime anywhere else (threads +
//! `Arc<Mutex<_>>`/`AtomicBool` throughout -- see CLAUDE.md's threading
//! notes). Portal negotiation here is a handful of one-shot calls at
//! recording start, not a sustained loop, so `zbus::blocking` (built for
//! exactly this) is the better trade -- more code than `ashpd`'s
//! convenience wrappers would need, in exchange for not pulling in an
//! async runtime for one startup sequence.
//!
//! The portal's `CreateSession`/`SelectSources`/`Start` calls all follow
//! the same two-phase dance (see the portal spec): the method itself
//! only returns a `Request` object path, and the actual result arrives
//! later as a `Response` signal on that path -- so the caller has to
//! subscribe to the signal *before* making the call, to avoid a race
//! against a fast-responding portal. `call_and_wait` below is that dance,
//! factored out once since all three calls need it identically.
//!
//! Verified against a real, live portal (see TODO.md for the disposable
//! test rig used): `CreateSession`, `SelectSources`, and `Start` all
//! round-trip correctly, confirmed by the portal's own debug log echoing
//! back this code's exact computed request/session paths and option
//! values. Not verified beyond that -- `Start` never actually succeeded
//! in that test (a GPU/KMS limitation of the test box, not of this code),
//! so `OpenPipeWireRemote`, the `streams` field's real shape, and every
//! line in `source.rs`'s `build_pipewiresrc` that runs after `negotiate`
//! returns `Ok` have never executed even once, in a test or otherwise.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

#[zbus::proxy(
    interface = "org.freedesktop.portal.ScreenCast",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop",
    gen_blocking = true,
    gen_async = false
)]
trait ScreenCast {
    fn create_session(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn select_sources(&self, session_handle: &ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn start(&self, session_handle: &ObjectPath<'_>, parent_window: &str, options: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn open_pipe_wire_remote(&self, session_handle: &ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<zbus::zvariant::OwnedFd>;
}

/// A `Request` object's path varies per-call (it's derived from the
/// caller's bus name + a caller-chosen token -- see `request_path`), so
/// unlike `ScreenCastProxy` this one has no `default_path` and is
/// built fresh, at the right path, for each call in `call_and_wait`.
#[zbus::proxy(interface = "org.freedesktop.portal.Request", default_service = "org.freedesktop.portal.Desktop", gen_blocking = true, gen_async = false)]
trait Request {
    #[zbus(signal)]
    fn response(&self, response: u32, results: HashMap<String, OwnedValue>);
}

#[zbus::proxy(interface = "org.freedesktop.portal.Session", default_service = "org.freedesktop.portal.Desktop", gen_blocking = true, gen_async = false)]
trait Session {
    fn close(&self) -> zbus::Result<()>;
}

/// A live ScreenCast portal session: the negotiated PipeWire fd/node id
/// `pipewiresrc` needs, plus what's needed to close the session politely
/// once recording stops.
///
/// Kept alive by `source.rs` for exactly as long as the `CaptureSource`
/// value it came from is -- both call sites (`main.rs`, `gui.rs`) already
/// keep that alive for the whole pipeline's lifetime, since
/// `CaptureSource::build_element`'s existing contract with `pipeline.rs`
/// is that it returns a bare `gst::Element` and nothing else, so there's
/// nowhere else to hand this back through. That lifetime is also exactly
/// the one `pipewiresrc`'s `fd` property needs: confirmed against
/// `gstpipewiresrc.c` that the element treats it as a *borrowed* fd --
/// it stores the raw number and uses it, but never closes it itself, in
/// finalize or anywhere else. This struct's `Drop` is what actually does
/// that, once recording stops.
pub struct PortalSession {
    fd: zbus::zvariant::OwnedFd,
    node_id: u32,
    connection: zbus::blocking::Connection,
    session_path: OwnedObjectPath,
}

impl PortalSession {
    pub fn fd_raw(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Best-effort: lets the compositor stop showing a "this app is
        // sharing your screen" indicator promptly instead of leaving it
        // up until the whole process exits. There's nothing useful to do
        // with an error here -- Drop can't return one, and a session
        // that's already gone (compositor restarted, etc.) isn't a bug in
        // this code.
        let build = || -> zbus::Result<SessionProxy<'_>> { SessionProxy::builder(&self.connection).path(self.session_path.clone())?.build() };
        if let Ok(proxy) = build() {
            let _ = proxy.close();
        }
    }
}

fn next_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("desktoprecorder_{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Computes the `Request` object path a portal call tagged with
/// `handle_token` will use, per the portal spec: the caller's own unique
/// bus name with its leading `:` stripped and every `.` replaced by `_`.
/// Needed so `call_and_wait` can subscribe to the `Response` signal
/// before making the call that triggers it.
fn request_path(unique_name: &str, token: &str) -> Result<OwnedObjectPath> {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")).context("portal produced an invalid request object path")
}

/// Runs one Request/Response-shaped portal call and blocks for its
/// result. `make_call` receives the token this call was tagged with (the
/// caller mixes it into its own options map as `handle_token`) and
/// should invoke the actual D-Bus method.
fn call_and_wait(
    connection: &zbus::blocking::Connection,
    unique_name: &str,
    make_call: impl FnOnce(&str) -> zbus::Result<OwnedObjectPath>,
) -> Result<HashMap<String, OwnedValue>> {
    let token = next_token();
    let path = request_path(unique_name, &token)?;
    let request = RequestProxy::builder(connection).path(path).context("failed to set the portal request proxy's path")?.build().context("failed to build the portal request proxy")?;

    // Subscribed before the call below -- a fast-responding portal could
    // otherwise reply before this exists, and the response would be lost.
    let mut responses = request.receive_response().context("failed to subscribe to the portal request's Response signal")?;

    make_call(&token).context("the portal method call itself failed")?;

    let signal = responses.next().context("the portal closed the connection before responding")?;
    let args = signal.args().context("malformed Response signal")?;
    if *args.response() != 0 {
        bail!("the portal request was cancelled or failed (response code {})", args.response());
    }
    Ok(args.results().clone())
}

fn get_value<T>(results: &HashMap<String, OwnedValue>, key: &str) -> Result<T>
where
    T: TryFrom<OwnedValue>,
    T::Error: std::fmt::Display,
{
    let value = results.get(key).with_context(|| format!("portal response is missing '{key}'"))?;
    T::try_from(value.clone()).map_err(|e| anyhow::anyhow!("portal response's '{key}' had an unexpected type: {e}"))
}

/// Runs the full ScreenCast negotiation: `CreateSession`, `SelectSources`
/// (letting the compositor draw its own monitor/window picker -- portals
/// don't expose enumeration ahead of that, by design, so there's no
/// portal equivalent of `X11ScreenConfig`'s `region`/`xid` to set here),
/// `Start` (this is what actually shows the picker dialog to the user),
/// then `OpenPipeWireRemote`. Blocks for as long as the user takes to
/// interact with that dialog.
pub fn negotiate(show_pointer: bool) -> Result<PortalSession> {
    let connection = zbus::blocking::Connection::session().context("failed to connect to the D-Bus session bus")?;
    let unique_name = connection.unique_name().context("D-Bus connection has no unique name yet")?.to_string();
    let screencast =
        ScreenCastProxy::new(&connection).context("failed to connect to org.freedesktop.portal.ScreenCast (is xdg-desktop-portal running, with a backend that implements ScreenCast?)")?;

    // 1. CreateSession
    let results = call_and_wait(&connection, &unique_name, |token| {
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", token.into());
        options.insert("session_handle_token", token.into());
        screencast.create_session(options)
    })
    .context("CreateSession failed")?;
    let session_handle: String = get_value(&results, "session_handle")?;
    // Despite being typed `s` (a plain string) in the Response signal's
    // results dict, this is the same value used as the `o` (object path)
    // argument to SelectSources/Start below -- a documented portal quirk,
    // not a mistake here.
    let session_path = OwnedObjectPath::try_from(session_handle).context("portal returned an invalid session handle")?;

    // 2. SelectSources -- MONITOR (1) | WINDOW (2), so the compositor's
    // picker offers both, matching X11Screen's --monitor/--window
    // coverage as closely as a portal-driven picker can.
    call_and_wait(&connection, &unique_name, |token| {
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", token.into());
        options.insert("types", (1u32 | 2u32).into());
        options.insert("multiple", false.into());
        options.insert("cursor_mode", (if show_pointer { 2u32 } else { 1u32 }).into());
        screencast.select_sources(&session_path.as_ref(), options)
    })
    .context("SelectSources failed")?;

    // 3. Start
    let results = call_and_wait(&connection, &unique_name, |token| {
        let mut options: HashMap<&str, Value> = HashMap::new();
        options.insert("handle_token", token.into());
        screencast.start(&session_path.as_ref(), "", options)
    })
    .context("Start failed (did the picker dialog get cancelled?)")?;

    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = get_value(&results, "streams")?;
    let (node_id, _stream_properties) = streams.into_iter().next().context("the portal reported success but selected no streams")?;

    // 4. OpenPipeWireRemote -- a plain method call, not Request/Response.
    let fd = screencast.open_pipe_wire_remote(&session_path.as_ref(), HashMap::new()).context("OpenPipeWireRemote failed")?;

    Ok(PortalSession { fd, node_id, connection, session_path })
}

/// `request_path`/`get_value` are pure logic with no D-Bus connection
/// involved, so they're tested directly here -- `negotiate` itself needs
/// a live portal + compositor to actually exercise (see this module's
/// doc comment), same reasoning as `x11_query.rs`'s real X11 calls and
/// `hotkey.rs`'s real `XGrabKey` not being covered by automated tests.
#[cfg(test)]
mod tests {
    use zbus::zvariant::Value;

    use super::*;

    #[test]
    fn request_path_strips_colon_and_replaces_dots() {
        let path = request_path(":1.42", "mytoken").unwrap();
        assert_eq!(path.as_str(), "/org/freedesktop/portal/desktop/request/1_42/mytoken");
    }

    #[test]
    fn request_path_rejects_a_token_with_invalid_object_path_characters() {
        // A valid object path segment is [A-Za-z0-9_]+ -- a space isn't
        // one of those. (A '/' isn't invalid here, surprisingly: it just
        // produces a deeper but still well-formed path, so it doesn't
        // exercise this error path -- confirmed by actually running this
        // test against that input first, not assumed.)
        assert!(request_path(":1.42", "not a valid token").is_err());
    }

    #[test]
    fn get_value_extracts_a_present_key_of_the_expected_type() {
        let mut results = HashMap::new();
        results.insert("session_handle".to_string(), OwnedValue::try_from(Value::from("/org/freedesktop/portal/desktop/session/1_42/foo")).unwrap());
        let handle: String = get_value(&results, "session_handle").unwrap();
        assert_eq!(handle, "/org/freedesktop/portal/desktop/session/1_42/foo");
    }

    #[test]
    fn get_value_errors_on_a_missing_key() {
        let results: HashMap<String, OwnedValue> = HashMap::new();
        let err = get_value::<String>(&results, "session_handle").unwrap_err();
        assert!(err.to_string().contains("session_handle"));
    }

    #[test]
    fn get_value_errors_on_a_type_mismatch() {
        // Asking for a String but the portal actually sent a u32 --
        // exactly the kind of protocol-shape mistake worth a clear error
        // rather than a panic.
        let mut results = HashMap::new();
        results.insert("streams".to_string(), OwnedValue::try_from(Value::from(42u32)).unwrap());
        assert!(get_value::<String>(&results, "streams").is_err());
    }
}
