//! A minimal GTK4 + libadwaita front end for the recorder.
//!
//! This is purely a new caller of `pipeline.rs`/`record.rs`/`source.rs`/
//! `x11_query.rs` -- none of them change to support this. The one real
//! wrinkle is that `record::run_recording` is synchronous and blocks for
//! the whole recording, so it runs on a background `std::thread`, with
//! the result handed back to the GTK main thread over an `mpsc` channel
//! polled by a periodic `glib` timeout (the same tick that also drives
//! the elapsed-time label). The Stop button needs none of that -- it
//! just flips the same `Arc<AtomicBool>` the Ctrl+C handler in main.rs
//! already uses, so stopping from the GUI is purely additive, not a
//! second code path.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gtk4::gdk;
use gtk4::glib;
use gtk4::glib::object::IsA;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::cli::{AudioMode, Container};
use crate::hotkey;
use crate::pipeline::{self, RecordConfig};
use crate::record;
use crate::source::{CaptureSource, Region, X11ScreenConfig};
use crate::x11_query;

const AUDIO_MODES: [AudioMode; 4] = [AudioMode::None, AudioMode::Mic, AudioMode::System, AudioMode::Both];
const AUDIO_LABELS: [&str; 4] = ["No audio", "Microphone", "System audio", "Mic + System audio"];

/// x264enc's speed-preset values, in the same fastest-to-slowest order
/// x264 itself documents them. Matches the CLI's `--speed-preset`
/// (cli.rs), which takes this as a free-text string rather than a
/// clap-validated enum -- kept as one here too rather than introducing a
/// separate GUI-only enum for it.
const PRESETS: [&str; 10] = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow", "placebo"];
/// Index into `PRESETS` matching the CLI's own default (cli.rs).
const DEFAULT_PRESET_INDEX: u32 = 2;
/// Same order/length as `PRESETS` -- shown as a per-row tooltip in the
/// preset dropdown's popup (see `build_preset_dropdown`), since the
/// bare x264 preset names don't say anything about the speed/CPU-load
/// tradeoff they stand for.
const PRESET_DESCRIPTIONS: [&str; 10] = [
    "Fastest encoding, least CPU -- needs the most bitrate for a given quality.",
    "Very fast; only slightly better compression than ultrafast.",
    "Default. Fast enough to keep up with live screen capture without dropping frames.",
    "A little slower than veryfast for a little better compression.",
    "Noticeably more CPU than faster for fairly modest compression gains.",
    "x264's own default balance of speed and compression -- may drop frames on a busy screen.",
    "Better compression, higher CPU load -- real-time capture may start dropping frames.",
    "High CPU load -- real-time capture is likely to drop frames.",
    "Very high CPU load -- not recommended for live capture.",
    "Extremely slow for negligible gains over veryslow -- effectively unusable live.",
];

/// `record::run_recording` always needs a concrete deadline -- there's
/// no real "unbounded" mode in it, and this milestone doesn't touch it.
/// A blank Duration field means "record until Stop," which this
/// implements as a generous safety-capped deadline rather than true
/// unboundedness: if someone really does forget to click Stop for a
/// full day, that's a reasonable place to give up anyway.
const INDEFINITE_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// One entry in the source dropdown, in lockstep with its label so the
/// dropdown's selected index can index straight into a `Vec` of these --
/// this also gets `--monitor`/`--window` mutual exclusivity for free,
/// since only one row can ever be selected.
#[derive(Debug, Clone, Copy)]
enum SourceChoice {
    FullScreen,
    Monitor(Region),
    Window(u64),
}

/// What the background recording thread reports back, exactly once.
enum RecordingOutcome {
    Finished { stopped_early: bool, output: PathBuf },
    Failed(anyhow::Error),
}

/// One decoded preview frame, plain `Send` data only -- constructed on
/// the GStreamer streaming thread (inside the appsink callback), read on
/// the GTK main thread. Never holds a GTK/GDK type directly (those are
/// `!Send`).
struct LatestFrame {
    width: i32,
    height: i32,
    stride: usize,
    data: Vec<u8>, // BGRA, tightly packed
}

/// All GUI state that outlives a single event -- lives only on the GTK
/// main thread (`Rc<RefCell<_>>`, not `Arc<Mutex<_>>`), since only the
/// stop flag and the channel actually need to cross into the background
/// thread.
struct Gui {
    source_dd: gtk4::DropDown,
    audio_dd: gtk4::DropDown,
    audio_device_entry: gtk4::Entry,
    framerate_spin: gtk4::SpinButton,
    bitrate_spin: gtk4::SpinButton,
    preset_dd: gtk4::DropDown,
    output_entry: gtk4::Entry,
    duration_entry: gtk4::Entry,
    record_btn: gtk4::Button,
    stop_btn: gtk4::Button,
    status_label: gtk4::Label,
    toasts: adw::ToastOverlay,
    preview: gtk4::Picture,
    sources: Vec<SourceChoice>,

    recording: bool,
    stop_requested: Option<Arc<AtomicBool>>,
    rx: Option<mpsc::Receiver<RecordingOutcome>>,
    started_at: Option<Instant>,
    /// Whether the in-progress recording has no user-set duration (the
    /// field was left blank) -- read by `finish_recording` to phrase
    /// the status text as "Stopped" rather than "Stopped early", since
    /// "early" implies a deadline the user never actually set.
    indefinite: bool,
    /// Written by whichever appsink callback is currently active (either
    /// the standalone preview pipeline below, or the recording
    /// pipeline's own tee'd-in branch), read and cleared once per
    /// `schedule_tick` firing (see `build_preview_sink`).
    preview_latest: Option<Arc<Mutex<Option<LatestFrame>>>>,
    /// The lightweight, encoder/file-free pipeline that shows a live
    /// preview whenever a recording *isn't* in progress -- started at
    /// window-open and restarted on every source change, so picking a
    /// source shows what it looks like before committing to Record
    /// (the recording pipeline's own tee'd preview branch takes over
    /// only once actually recording; see `start_recording`/
    /// `finish_recording`). `None` exactly when `recording` is `true`.
    preview_pipeline: Option<gst::Pipeline>,
    /// Shared with the process-wide Ctrl+C/SIGTERM handler installed in
    /// `run()` -- kept in sync with `stop_requested` (`Some` with the
    /// same `Arc` exactly while `recording` is `true`) so the signal
    /// handler always has a way to reach whichever recording is
    /// currently active.
    active_stop: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

pub fn run() -> anyhow::Result<()> {
    adw::init().context("failed to initialize libadwaita")?;

    // Unlike the CLI's record_command (main.rs), which installs its own
    // ctrlc handler scoped to one known Arc<AtomicBool>, the GUI has no
    // single recording in scope at startup -- it may or may not be
    // recording at any given moment, and which Arc<AtomicBool> that is
    // changes across the process's lifetime. This indirection is Some
    // exactly while a recording is in progress (kept in sync by
    // start_recording/finish_recording), so the handler -- installed
    // once here, since ctrlc only allows one process-wide handler --
    // always has a way to reach whichever recording is currently active.
    let active_stop: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
    {
        let active_stop = Arc::clone(&active_stop);
        ctrlc::set_handler(move || match active_stop.lock().unwrap().as_ref() {
            // A recording is running: stop it the same way the Stop
            // button does. record::run_recording's own poll loop
            // notices the flag and finalizes normally -- this is what
            // was missing before (Ctrl+C fell through to the default
            // SIGINT disposition and killed the process mid-recording,
            // leaving a truncated file).
            Some(flag) => flag.store(true, Ordering::SeqCst),
            // Idle: nothing to finalize, so just exit like a normal
            // terminal app would. Safe to call directly here --
            // ctrlc's handler runs on its own dedicated thread, not
            // actual signal-handler context.
            None => std::process::exit(0),
        })
        .context("failed to install Ctrl+C/SIGTERM handler")?;
    }

    // Set by hotkey.rs's background X11-listener thread on a matching
    // key press, cleared by schedule_hotkey_tick once handled -- the
    // listener thread can never call start_recording or touch
    // stop_requested directly, since both involve !Send GTK state.
    let hotkey_pressed = Arc::new(AtomicBool::new(false));
    hotkey::spawn_listener(Arc::clone(&hotkey_pressed));

    let app = adw::Application::builder().application_id("com.desktoprecorder.Gui").build();
    app.connect_activate(move |app| build_ui(app, Arc::clone(&active_stop), Arc::clone(&hotkey_pressed)));

    // Bypass std::env::args() here -- clap already consumed argv (it
    // contains e.g. "desktoprecorder gui"), and GApplication's own
    // option parser doesn't know what to do with the "gui" positional.
    let empty: [&str; 0] = [];
    let exit_code = app.run_with_args(&empty);
    if exit_code != glib::ExitCode::SUCCESS {
        anyhow::bail!("GUI exited with a non-success status");
    }
    Ok(())
}

fn build_ui(app: &adw::Application, active_stop: Arc<Mutex<Option<Arc<AtomicBool>>>>, hotkey_pressed: Arc<AtomicBool>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Desktop Recorder")
        .default_width(480)
        .default_height(360)
        .build();

    let header = adw::HeaderBar::new();
    let toasts = adw::ToastOverlay::new();

    let (source_labels, sources) = build_source_choices(&toasts);
    let source_label_refs: Vec<&str> = source_labels.iter().map(String::as_str).collect();
    let source_dd = gtk4::DropDown::from_strings(&source_label_refs);
    source_dd.set_hexpand(true);

    let audio_dd = gtk4::DropDown::from_strings(&AUDIO_LABELS);
    audio_dd.set_hexpand(true);

    let audio_device_entry =
        gtk4::Entry::builder().hexpand(true).placeholder_text("Default device (see `pactl list sources short`) -- ignored for Mic + System").build();
    audio_device_entry.set_tooltip_text(Some(
        "Overrides the default PulseAudio device for Microphone or System audio (see `pactl list sources short` for names). \
         Not used with Mic + System, which always mixes the default mic with the default sink's monitor. Matches --audio-device on the CLI.",
    ));

    let framerate_spin = gtk4::SpinButton::with_range(1.0, 240.0, 1.0);
    framerate_spin.set_digits(0);
    framerate_spin.set_value(30.0);
    framerate_spin.set_hexpand(true);
    framerate_spin.set_tooltip_text(Some("Frames per second to capture and encode. Matches --framerate on the CLI (default 30)."));

    let bitrate_spin = gtk4::SpinButton::with_range(500.0, 50_000.0, 100.0);
    bitrate_spin.set_digits(0);
    bitrate_spin.set_value(8000.0);
    bitrate_spin.set_hexpand(true);
    bitrate_spin.set_tooltip_text(Some(
        "Target video bitrate in kbps -- higher means better quality at a larger file size. Matches --bitrate on the CLI (default 8000).",
    ));

    let preset_dd = build_preset_dropdown();
    preset_dd.set_tooltip_text(Some(
        "x264 encoding speed vs. compression efficiency, fastest to slowest -- open the menu for what each option trades off. \
         Matches --speed-preset on the CLI (default veryfast).",
    ));

    let output_entry = gtk4::Entry::builder().hexpand(true).placeholder_text("/path/to/recording.mkv").build();
    let browse_btn = gtk4::Button::with_label("Browse…");

    let duration_entry = gtk4::Entry::builder().hexpand(true).placeholder_text("e.g. 10s, 2m30s -- blank records until you click Stop").build();

    let record_btn = gtk4::Button::with_label("Record");
    record_btn.add_css_class("suggested-action");
    let stop_btn = gtk4::Button::with_label("Stop");
    stop_btn.set_sensitive(false);

    let status_label = gtk4::Label::new(Some("Idle"));
    status_label.set_xalign(0.0);

    // Blank (no paintable) until a recording is started; reset to blank
    // again once one finishes rather than leaving the last frame up,
    // which would misleadingly look still-live. Permanently in the
    // layout (never hidden) so the window doesn't jump size every
    // Record/Stop.
    let preview = gtk4::Picture::new();
    preview.set_content_fit(gtk4::ContentFit::Contain);
    preview.set_can_shrink(true);
    preview.set_hexpand(true);
    preview.set_size_request(-1, 180);
    preview.add_css_class("card");

    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    form.set_margin_top(18);
    form.set_margin_bottom(18);
    form.set_margin_start(18);
    form.set_margin_end(18);

    form.append(&preview);
    form.append(&labeled_row("Source:", &source_dd));
    form.append(&labeled_row("Audio:", &audio_dd));
    form.append(&labeled_row("Audio device:", &audio_device_entry));
    form.append(&labeled_row("Framerate:", &framerate_spin));
    form.append(&labeled_row("Bitrate (kbps):", &bitrate_spin));
    form.append(&labeled_row("Preset:", &preset_dd));

    let output_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let output_label = gtk4::Label::new(Some("Output:"));
    output_label.set_width_chars(10);
    output_label.set_xalign(0.0);
    output_row.append(&output_label);
    output_row.append(&output_entry);
    output_row.append(&browse_btn);
    form.append(&output_row);

    form.append(&labeled_row("Duration:", &duration_entry));

    let button_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    button_row.append(&record_btn);
    button_row.append(&stop_btn);
    form.append(&button_row);

    form.append(&status_label);

    toasts.set_child(Some(&form));

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&toasts);
    window.set_content(Some(&root));

    let state = Rc::new(RefCell::new(Gui {
        source_dd: source_dd.clone(),
        audio_dd: audio_dd.clone(),
        audio_device_entry: audio_device_entry.clone(),
        framerate_spin: framerate_spin.clone(),
        bitrate_spin: bitrate_spin.clone(),
        preset_dd: preset_dd.clone(),
        output_entry: output_entry.clone(),
        duration_entry: duration_entry.clone(),
        record_btn: record_btn.clone(),
        stop_btn: stop_btn.clone(),
        status_label: status_label.clone(),
        toasts: toasts.clone(),
        preview: preview.clone(),
        sources,
        recording: false,
        stop_requested: None,
        rx: None,
        started_at: None,
        indefinite: false,
        preview_latest: None,
        preview_pipeline: None,
        active_stop,
    }));

    {
        let state = Rc::clone(&state);
        record_btn.connect_clicked(move |_| start_recording(&state));
    }
    {
        let state = Rc::clone(&state);
        // Re-point the standalone preview at whatever's newly selected --
        // this is what lets picking a source show it live before ever
        // touching Record. Only fires from a user selection (not from
        // the initial construction above, since this handler isn't
        // attached yet at that point) and never while recording, since
        // the dropdown is insensitive then (see set_controls_for_recording).
        source_dd.connect_selected_notify(move |_| {
            let mut gui = state.borrow_mut();
            if !gui.recording {
                stop_standalone_preview(&mut gui);
                start_standalone_preview(&mut gui);
            }
        });
    }
    {
        let state = Rc::clone(&state);
        stop_btn.connect_clicked(move |_| {
            if let Some(flag) = &state.borrow().stop_requested {
                flag.store(true, Ordering::SeqCst);
            }
        });
    }
    {
        let output_entry = output_entry.clone();
        let window = window.clone();
        browse_btn.connect_clicked(move |_| {
            let dialog = gtk4::FileDialog::builder().title("Choose output file").initial_name("recording.mkv").build();
            let output_entry = output_entry.clone();
            dialog.save(Some(&window), None::<&gtk4::gio::Cancellable>, move |result| {
                // A cancelled dialog is Err too -- not an error condition, just ignored.
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    output_entry.set_text(&path.display().to_string());
                }
            });
        });
    }
    {
        let state = Rc::clone(&state);
        window.connect_close_request(move |_| {
            let recording = state.borrow().recording;
            if recording {
                state.borrow().toasts.add_toast(adw::Toast::new("Stop the recording before closing"));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    start_standalone_preview(&mut state.borrow_mut());
    schedule_preview_tick(Rc::clone(&state));
    schedule_hotkey_tick(Rc::clone(&state), hotkey_pressed);
    window.present();
}

/// Builds the Preset dropdown with the CLI's default pre-selected and a
/// per-row tooltip (see `PRESET_DESCRIPTIONS`) -- `DropDown::from_strings`
/// alone has no way to attach one, so this replaces its default popup
/// factory with a custom one that's identical (a plain, left-aligned
/// label) except for the added `set_tooltip_text`. `list_factory` only
/// affects the open popup's rows; the closed button's own display
/// (built from `from_strings`'s default factory) is left untouched.
fn build_preset_dropdown() -> gtk4::DropDown {
    let preset_dd = gtk4::DropDown::from_strings(&PRESETS);
    preset_dd.set_selected(DEFAULT_PRESET_INDEX);
    preset_dd.set_hexpand(true);

    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk4::ListItem>() else { return };
        let label = gtk4::Label::new(None);
        label.set_xalign(0.0);
        label.set_margin_start(6);
        label.set_margin_end(6);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        list_item.set_child(Some(&label));
    });
    factory.connect_bind(|_, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk4::ListItem>() else { return };
        let Some(label) = list_item.child().and_downcast::<gtk4::Label>() else { return };
        // The popup lists rows in the same order PRESETS/PRESET_DESCRIPTIONS
        // were given to from_strings/built in, so position doubles as the
        // index into both -- no need to read the row's item back out.
        let i = list_item.position() as usize;
        label.set_label(PRESETS.get(i).copied().unwrap_or_default());
        label.set_tooltip_text(PRESET_DESCRIPTIONS.get(i).copied());
    });
    preset_dd.set_list_factory(Some(&factory));

    preset_dd
}

fn labeled_row(label: &str, control: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let label_widget = gtk4::Label::new(Some(label));
    label_widget.set_width_chars(10);
    label_widget.set_xalign(0.0);
    row.append(&label_widget);
    row.append(control);
    row
}

/// Builds the source dropdown's labels and their parallel `SourceChoice`
/// values from the same `x11_query` calls `list_sources_command` (in
/// main.rs) already uses -- monitors are a hard requirement (RandR
/// failing would be unusual on a working X server), windows fail soft
/// exactly like the CLI's `list-sources` already treats them.
fn build_source_choices(toasts: &adw::ToastOverlay) -> (Vec<String>, Vec<SourceChoice>) {
    let mut labels = vec!["Full screen (all monitors)".to_string()];
    let mut choices = vec![SourceChoice::FullScreen];

    match x11_query::list_monitors() {
        Ok(monitors) => {
            for m in monitors {
                let primary = if m.primary { " (primary)" } else { "" };
                labels.push(format!("Monitor {}: {}{} — {}x{}", m.index, m.name, primary, m.region.width, m.region.height));
                choices.push(SourceChoice::Monitor(m.region));
            }
        }
        Err(err) => {
            toasts.add_toast(adw::Toast::new(&format!("Could not list monitors: {err:#}")));
        }
    }

    match x11_query::list_windows() {
        Ok(windows) => {
            for w in windows {
                labels.push(format!("Window: [{}] {}", w.class, w.title));
                choices.push(SourceChoice::Window(w.xid));
            }
        }
        Err(err) => {
            eprintln!("warning: could not list windows: {err:#}");
        }
    }

    (labels, choices)
}

fn build_capture_source(choice: SourceChoice) -> CaptureSource {
    let mut cfg = X11ScreenConfig::default();
    match choice {
        SourceChoice::FullScreen => {}
        SourceChoice::Monitor(region) => cfg.region = Some(region),
        SourceChoice::Window(xid) => cfg.xid = Some(xid),
    }
    CaptureSource::X11Screen(cfg)
}

/// Starts (or restarts) the standalone, non-recording preview pipeline
/// for whatever source is currently selected. A no-op precondition, not
/// enforced here, is that `gui.recording` is `false` -- callers (the
/// source-dropdown handler, window setup, and `finish_recording`) only
/// ever call this outside of a recording; during one, the recording
/// pipeline's own tee'd branch is what feeds the preview instead.
fn start_standalone_preview(gui: &mut Gui) {
    let choice = gui.sources.get(gui.source_dd.selected() as usize).copied().unwrap_or(SourceChoice::FullScreen);
    let source = build_capture_source(choice);
    let (appsink, latest) = build_preview_sink();

    match pipeline::build_preview_only_pipeline(&source, &appsink) {
        Ok(pipeline) => match pipeline.set_state(gst::State::Playing) {
            Ok(_) => {
                gui.preview_pipeline = Some(pipeline);
                gui.preview_latest = Some(latest);
            }
            Err(err) => {
                gui.toasts.add_toast(adw::Toast::new(&format!("Couldn't start preview: {err}")));
                gui.preview.set_paintable(None::<&gdk::Paintable>);
            }
        },
        Err(err) => {
            gui.toasts.add_toast(adw::Toast::new(&format!("Couldn't start preview: {err:#}")));
            gui.preview.set_paintable(None::<&gdk::Paintable>);
        }
    }
}

/// Tears down the standalone preview pipeline, if one is running. No EOS
/// needed -- unlike a recording pipeline, there's no file/muxer to flush.
fn stop_standalone_preview(gui: &mut Gui) {
    if let Some(pipeline) = gui.preview_pipeline.take() {
        let _ = pipeline.set_state(gst::State::Null);
    }
    gui.preview_latest = None;
}

/// Builds an `appsink` that decodes each frame into a plain `Send`
/// `LatestFrame` and stores it in the returned `Arc<Mutex<...>>` --
/// nothing GTK-specific crosses the thread boundary. The `new_sample`
/// callback fires on a GStreamer streaming thread, and `gtk4::Picture`
/// (like all GTK widgets, and even a `WeakRef` to one) is `!Send`, so it
/// can never be captured there, directly or weakly. Instead, the
/// existing `schedule_tick` GTK-thread timer (already running once
/// every 250ms with no `Send` bound, since `timeout_add_local`'s
/// closure only needs to be `'static`) picks up whatever's latest and
/// updates the `Picture` itself -- reusing the same polling pattern
/// already driving the elapsed-time label and the outcome channel,
/// rather than adding a second, `MainContext::invoke`-based mechanism.
/// A capped, thumbnail-sized preview has no need to update faster than
/// that tick anyway.
fn build_preview_sink() -> (gst_app::AppSink, Arc<Mutex<Option<LatestFrame>>>) {
    let latest: Arc<Mutex<Option<LatestFrame>>> = Arc::new(Mutex::new(None));

    let preview_caps = gst::Caps::builder("video/x-raw").field("format", "BGRA").build();
    let appsink = gst_app::AppSink::builder().name("preview-sink").caps(&preview_caps).max_buffers(1u32).drop(true).sync(false).build();
    // appsink's default wait-on-eos blocks EOS handling until a
    // pull_sample()-style consumer catches up -- irrelevant (and
    // actively harmful, since it'd add a stall) for a push-callback
    // consumer like this one that never pulls. Without this, EOS could
    // be delayed by however long the GTK thread takes to drain frames.
    appsink.set_wait_on_eos(false);

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample({
                let latest = Arc::clone(&latest);
                move |appsink: &gst_app::AppSink| -> Result<gst::FlowSuccess, gst::FlowError> {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                    if let Some(frame) = extract_frame(&sample) {
                        // Simply overwrite -- a preview doesn't need
                        // every frame, only whatever's newest by the
                        // time the tick next checks.
                        *latest.lock().unwrap() = Some(frame);
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .build(),
    );

    (appsink, latest)
}

/// Converts one pulled `gst::Sample` (BGRA, per the appsink's caps) into
/// plain owned bytes safe to hand to the GTK thread. Returns `None` on
/// any malformed/unexpected sample rather than failing the pipeline --
/// dropping an occasional bad preview frame is harmless.
fn extract_frame(sample: &gst::Sample) -> Option<LatestFrame> {
    let buffer = sample.buffer()?;
    let s = sample.caps()?.structure(0)?;
    let width: i32 = s.get("width").ok()?;
    let height: i32 = s.get("height").ok()?;
    let map = buffer.map_readable().ok()?;
    // BGRA is 4 bytes/pixel, so width*4 is always already a multiple of
    // 4 -- GStreamer's default raw-video row alignment never pads this
    // format, so stride == width*4 exactly (double-checked below rather
    // than trusted blindly).
    let stride = (width as usize) * 4;
    let data = map.as_slice();
    if data.len() != stride * height as usize {
        return None;
    }
    Some(LatestFrame { width, height, stride, data: data.to_vec() })
}

fn show_frame(picture: &gtk4::Picture, frame: LatestFrame) {
    let bytes = glib::Bytes::from_owned(frame.data);
    let texture = gdk::MemoryTexture::new(frame.width, frame.height, gdk::MemoryFormat::B8g8r8a8, &bytes, frame.stride);
    picture.set_paintable(Some(&texture));
}

/// Reads the form, validates it, and (if valid) spawns the background
/// recording thread and starts the UI tick that watches it.
fn start_recording(state: &Rc<RefCell<Gui>>) {
    let (source, cfg, duration, indefinite) = {
        let gui = state.borrow();

        let output_text = gui.output_entry.text();
        if output_text.trim().is_empty() {
            gui.toasts.add_toast(adw::Toast::new("Choose an output path first"));
            return;
        }

        let duration_text = gui.duration_entry.text();
        let indefinite = duration_text.trim().is_empty();
        let duration = if indefinite {
            INDEFINITE_DURATION
        } else {
            match duration_text.parse::<humantime::Duration>() {
                Ok(d) => *d,
                Err(_) => {
                    gui.toasts.add_toast(adw::Toast::new(&format!("Invalid duration \"{duration_text}\" (try e.g. \"10s\", \"2m30s\", or leave it blank)")));
                    return;
                }
            }
        };

        let output_path = PathBuf::from(output_text.as_str());
        let choice = gui.sources.get(gui.source_dd.selected() as usize).copied().unwrap_or(SourceChoice::FullScreen);
        let source = build_capture_source(choice);
        let audio_mode = AUDIO_MODES[gui.audio_dd.selected() as usize];
        let container = Container::infer_from_path(&output_path);

        let audio_device_text = gui.audio_device_entry.text();
        let audio_device = if audio_device_text.trim().is_empty() { None } else { Some(audio_device_text.to_string()) };
        // Mirrors audio.rs's own hard error for this combination -- worth
        // catching here too so it's a toast, not a failed-recording
        // round-trip through the background thread.
        if audio_mode == AudioMode::Both && audio_device.is_some() {
            gui.toasts.add_toast(adw::Toast::new(
                "Audio device can't be set for Mic + System audio (it always mixes the default mic with the default sink's monitor)",
            ));
            return;
        }

        let cfg = RecordConfig {
            output_path,
            framerate: gui.framerate_spin.value() as u32,
            bitrate_kbps: gui.bitrate_spin.value() as u32,
            speed_preset: PRESETS[gui.preset_dd.selected() as usize].to_string(),
            container,
            audio_mode,
            audio_device,
        };

        (source, cfg, duration, indefinite)
    };

    // Release the standalone preview's capture of this source before the
    // recording pipeline claims it -- avoids two ximagesrcs briefly
    // capturing the same source at once. The recording pipeline's own
    // tee'd branch (below) takes over feeding the preview from here.
    stop_standalone_preview(&mut state.borrow_mut());

    let stop_requested = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let stop_for_thread = Arc::clone(&stop_requested);
    let output = cfg.output_path.clone();

    // The AppSink -- like the rest of the pipeline -- moves into the
    // background thread. GStreamer elements are already proven Send in
    // this codebase (the whole gst::Pipeline moves into this same
    // thread today).
    let (preview_sink, preview_latest) = build_preview_sink();

    std::thread::spawn(move || {
        let result: anyhow::Result<bool> = (|| {
            let pipeline = pipeline::build_recording_pipeline_with_preview(&source, &cfg, &preview_sink)?;
            record::run_recording(&pipeline, duration, &stop_for_thread)?;
            Ok(stop_for_thread.load(Ordering::SeqCst)) // did Stop get clicked?
        })();
        let outcome = match result {
            Ok(stopped_early) => RecordingOutcome::Finished { stopped_early, output },
            Err(e) => RecordingOutcome::Failed(e),
        };
        let _ = tx.send(outcome); // the GTK thread may be gone; ignore a dead receiver
    });

    {
        let mut gui = state.borrow_mut();
        gui.recording = true;
        *gui.active_stop.lock().unwrap() = Some(Arc::clone(&stop_requested));
        gui.stop_requested = Some(stop_requested);
        gui.rx = Some(rx);
        gui.started_at = Some(Instant::now());
        gui.indefinite = indefinite;
        gui.preview_latest = Some(preview_latest);
        set_controls_for_recording(&gui, true);
    }
    schedule_tick(Rc::clone(state));
}

/// Runs forever (unlike `schedule_tick`, which only lives for the
/// duration of one recording), 4x/sec, showing whatever's the latest
/// preview frame (see `build_preview_sink`). One tick serves both
/// preview producers -- the standalone preview pipeline and, during a
/// recording, the recording pipeline's own tee'd-in branch -- since
/// only one of them is ever writing into `gui.preview_latest` at a time.
fn schedule_preview_tick(state: Rc<RefCell<Gui>>) {
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let gui = state.borrow();
        if let Some(latest) = &gui.preview_latest
            && let Some(frame) = latest.lock().unwrap().take()
        {
            show_frame(&gui.preview, frame);
        }
        glib::ControlFlow::Continue
    });
}

/// Runs forever, independently of `schedule_preview_tick` -- kept on its
/// own faster timer rather than piggybacking on that one, so hotkey
/// responsiveness isn't coupled to the preview's cadence. A press toggles
/// start/stop, mirroring what Record/Stop do respectively. Only the stop
/// half could be done directly from hotkey.rs's listener thread (it's
/// just an `Arc<AtomicBool>::store`, same as the Ctrl+C handler above) --
/// starting needs `start_recording`, which touches `!Send` GTK widgets
/// and so can only ever run on this thread, hence routing both through
/// the same poll for symmetry.
fn schedule_hotkey_tick(state: Rc<RefCell<Gui>>, hotkey_pressed: Arc<AtomicBool>) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if hotkey_pressed.swap(false, Ordering::SeqCst) {
            let recording = state.borrow().recording;
            if recording {
                if let Some(flag) = &state.borrow().stop_requested {
                    flag.store(true, Ordering::SeqCst);
                }
            } else {
                start_recording(&state);
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Ticks 4x/sec on the GTK main thread, only while a recording is in
/// progress: updates the elapsed-time label and polls the channel for
/// the background thread's result. Returning `ControlFlow::Break` once
/// that arrives cancels this same timeout.
fn schedule_tick(state: Rc<RefCell<Gui>>) {
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let mut gui = state.borrow_mut();

        if let Some(started) = gui.started_at {
            let text = format!("Recording — {}", format_elapsed(started.elapsed()));
            gui.status_label.set_text(&text);
        }

        let outcome = match gui.rx.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(outcome)) => Some(outcome),
            // The sender was dropped without sending -- the thread panicked.
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Some(RecordingOutcome::Failed(anyhow::anyhow!("recording thread ended unexpectedly")))
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => None,
        };

        match outcome {
            Some(outcome) => {
                finish_recording(&mut gui, outcome);
                glib::ControlFlow::Break
            }
            None => glib::ControlFlow::Continue,
        }
    });
}

fn finish_recording(gui: &mut Gui, outcome: RecordingOutcome) {
    gui.recording = false;
    *gui.active_stop.lock().unwrap() = None;
    gui.stop_requested = None;
    gui.rx = None;
    gui.started_at = None;
    set_controls_for_recording(gui, false);
    // Resume the live idle preview now that the recording pipeline (and
    // its tee'd branch) is gone -- overwrites preview_latest with a
    // fresh one of its own; the still-running schedule_preview_tick
    // picks up its frames the same way it did the recording's.
    start_standalone_preview(gui);

    match outcome {
        RecordingOutcome::Finished { stopped_early, output } => {
            // "early" implies a deadline the user actually set -- don't
            // say it for a blank-duration (indefinite) recording, where
            // clicking Stop was the only way it was ever going to end.
            let verb = match (stopped_early, gui.indefinite) {
                (true, true) => "Stopped",
                (true, false) => "Stopped early",
                (false, _) => "Done",
            };
            gui.status_label.set_text(&format!("{verb}: {}", output.display()));
        }
        RecordingOutcome::Failed(err) => {
            gui.status_label.set_text("Idle");
            gui.toasts.add_toast(adw::Toast::new(&format!("Recording failed: {err:#}")));
        }
    }
}

fn set_controls_for_recording(gui: &Gui, recording: bool) {
    gui.record_btn.set_sensitive(!recording);
    gui.stop_btn.set_sensitive(recording);
    gui.source_dd.set_sensitive(!recording);
    gui.audio_dd.set_sensitive(!recording);
    gui.audio_device_entry.set_sensitive(!recording);
    gui.framerate_spin.set_sensitive(!recording);
    gui.bitrate_spin.set_sensitive(!recording);
    gui.preset_dd.set_sensitive(!recording);
    gui.output_entry.set_sensitive(!recording);
    gui.duration_entry.set_sensitive(!recording);
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Everything below is plain data/logic with no GTK widget or live
/// display involved, so it's safe to run in the normal `cargo test`
/// pass. The widgets themselves (signal wiring, layout, tooltip hover)
/// aren't covered here -- this project has no headless-GTK test harness,
/// and building one is a separate undertaking from filling in coverage
/// for what's already testable; that side stays the manual pass
/// CLAUDE.md already describes.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_arrays_stay_in_lockstep() {
        // PRESETS/PRESET_DESCRIPTIONS/DEFAULT_PRESET_INDEX are three
        // independently hand-maintained consts with no compiler-enforced
        // link between them -- this is exactly the kind of thing that
        // silently desyncs when a future edit adds/removes/reorders one
        // preset without touching the other array.
        assert_eq!(PRESETS.len(), PRESET_DESCRIPTIONS.len());
        assert!((DEFAULT_PRESET_INDEX as usize) < PRESETS.len());
        // Matches the CLI's own default (cli.rs's --speed-preset).
        assert_eq!(PRESETS[DEFAULT_PRESET_INDEX as usize], "veryfast");
    }

    #[test]
    fn audio_mode_arrays_stay_in_lockstep() {
        assert_eq!(AUDIO_MODES.len(), AUDIO_LABELS.len());
        assert_eq!(AUDIO_MODES[0], AudioMode::None);
    }

    #[test]
    fn build_capture_source_full_screen_has_no_region_or_xid() {
        let CaptureSource::X11Screen(cfg) = build_capture_source(SourceChoice::FullScreen);
        assert!(cfg.region.is_none());
        assert!(cfg.xid.is_none());
    }

    #[test]
    fn build_capture_source_monitor_sets_region_only() {
        let region = Region { x: 0, y: 0, width: 1920, height: 1080 };
        let CaptureSource::X11Screen(cfg) = build_capture_source(SourceChoice::Monitor(region));
        assert_eq!(cfg.region, Some(region));
        assert!(cfg.xid.is_none());
    }

    #[test]
    fn build_capture_source_window_sets_xid_only() {
        let CaptureSource::X11Screen(cfg) = build_capture_source(SourceChoice::Window(0x2c00003));
        assert_eq!(cfg.xid, Some(0x2c00003));
        assert!(cfg.region.is_none());
    }

    #[test]
    fn format_elapsed_pads_to_two_digits() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "01:05");
        assert_eq!(format_elapsed(Duration::from_secs(599)), "09:59");
    }

    #[test]
    fn format_elapsed_does_not_wrap_past_59_minutes() {
        // Minutes just keep growing rather than rolling into an hours
        // column -- documenting the actual behavior (relevant since
        // INDEFINITE_DURATION is 24h, i.e. "1440:00" at the cap) rather
        // than assuming an hh:mm:ss format that isn't what's implemented.
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "61:01");
    }
}
