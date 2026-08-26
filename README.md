# desktoprecorder

A screen recorder for Linux (X11), written in Rust on top of GStreamer. CLI and GTK4/libadwaita GUI, sharing the same recording engine.

## Features

- Full-screen, per-monitor, or per-window capture (`--monitor`/`--window`, or pick from a dropdown in the GUI)
- Optional audio: microphone, system audio (via the default sink's PulseAudio monitor), or both mixed together
- Matroska (`.mkv`) or MP4 output, clean EOS-based shutdown on Ctrl+C/SIGTERM/window close so files are never left truncated
- GUI: live preview of the selected source (before and during recording), optional/blank-means-unbounded duration, Record/Stop

## Requirements

Debian/Ubuntu (adjust package manager elsewhere):

```bash
sudo apt install build-essential pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-libav gstreamer1.0-tools \
  libgtk-4-dev libadwaita-1-dev
```

X11 only — there's no Wayland/`xdg-desktop-portal` support (the app detects a Wayland session and fails with a clear message rather than silently producing a black recording).

## Build

```bash
cargo build --release
```

## Usage

```bash
# Record the full screen for 10 seconds
desktoprecorder record --output demo.mkv --duration 10s

# Record a specific monitor with mic audio, until Ctrl+C
desktoprecorder record --output demo.mkv --duration 1h --monitor 0 --audio mic

# List available monitors and windows (for --monitor/--window)
desktoprecorder list-sources

# Launch the GUI
desktoprecorder gui
```

Run `desktoprecorder record --help` for the full flag list (framerate, bitrate, speed preset, container, audio device override).

## License

None specified yet.
