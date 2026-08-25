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
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use gtk4::glib;
use gtk4::glib::object::IsA;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::cli::{AudioMode, Container};
use crate::pipeline::{self, RecordConfig};
use crate::record;
use crate::source::{CaptureSource, Region, X11ScreenConfig};
use crate::x11_query;

const AUDIO_MODES: [AudioMode; 4] = [AudioMode::None, AudioMode::Mic, AudioMode::System, AudioMode::Both];
const AUDIO_LABELS: [&str; 4] = ["No audio", "Microphone", "System audio", "Mic + System audio"];

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

/// All GUI state that outlives a single event -- lives only on the GTK
/// main thread (`Rc<RefCell<_>>`, not `Arc<Mutex<_>>`), since only the
/// stop flag and the channel actually need to cross into the background
/// thread.
struct Gui {
    source_dd: gtk4::DropDown,
    audio_dd: gtk4::DropDown,
    output_entry: gtk4::Entry,
    duration_entry: gtk4::Entry,
    record_btn: gtk4::Button,
    stop_btn: gtk4::Button,
    status_label: gtk4::Label,
    toasts: adw::ToastOverlay,
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
}

pub fn run() -> anyhow::Result<()> {
    adw::init().context("failed to initialize libadwaita")?;

    let app = adw::Application::builder().application_id("com.desktoprecorder.Gui").build();
    app.connect_activate(build_ui);

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

fn build_ui(app: &adw::Application) {
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

    let output_entry = gtk4::Entry::builder().hexpand(true).placeholder_text("/path/to/recording.mkv").build();
    let browse_btn = gtk4::Button::with_label("Browse…");

    let duration_entry = gtk4::Entry::builder().hexpand(true).placeholder_text("e.g. 10s, 2m30s -- blank records until you click Stop").build();

    let record_btn = gtk4::Button::with_label("Record");
    record_btn.add_css_class("suggested-action");
    let stop_btn = gtk4::Button::with_label("Stop");
    stop_btn.set_sensitive(false);

    let status_label = gtk4::Label::new(Some("Idle"));
    status_label.set_xalign(0.0);

    let form = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    form.set_margin_top(18);
    form.set_margin_bottom(18);
    form.set_margin_start(18);
    form.set_margin_end(18);

    form.append(&labeled_row("Source:", &source_dd));
    form.append(&labeled_row("Audio:", &audio_dd));

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
        output_entry: output_entry.clone(),
        duration_entry: duration_entry.clone(),
        record_btn: record_btn.clone(),
        stop_btn: stop_btn.clone(),
        status_label: status_label.clone(),
        toasts: toasts.clone(),
        sources,
        recording: false,
        stop_requested: None,
        rx: None,
        started_at: None,
        indefinite: false,
    }));

    {
        let state = Rc::clone(&state);
        record_btn.connect_clicked(move |_| start_recording(&state));
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

    window.present();
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

        let cfg = RecordConfig {
            output_path,
            framerate: 30,
            bitrate_kbps: 8000,
            speed_preset: "veryfast".to_string(),
            container,
            audio_mode,
            audio_device: None,
        };

        (source, cfg, duration, indefinite)
    };

    let stop_requested = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let stop_for_thread = Arc::clone(&stop_requested);
    let output = cfg.output_path.clone();

    std::thread::spawn(move || {
        let result: anyhow::Result<bool> = (|| {
            let pipeline = pipeline::build_recording_pipeline(&source, &cfg)?;
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
        gui.stop_requested = Some(stop_requested);
        gui.rx = Some(rx);
        gui.started_at = Some(Instant::now());
        gui.indefinite = indefinite;
        set_controls_for_recording(&gui, true);
    }
    schedule_tick(Rc::clone(state));
}

/// Ticks 4x/sec on the GTK main thread: updates the elapsed-time label
/// and polls the channel for the background thread's result. Returning
/// `ControlFlow::Break` once that arrives cancels this same timeout.
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
    gui.stop_requested = None;
    gui.rx = None;
    gui.started_at = None;
    set_controls_for_recording(gui, false);

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
    gui.output_entry.set_sensitive(!recording);
    gui.duration_entry.set_sensitive(!recording);
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}
