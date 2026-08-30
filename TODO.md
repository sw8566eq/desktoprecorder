# TODO

Roadmap for the three gaps we're actually closing: Wayland support, CI, and
persistent GUI settings. (Pause/resume, tray/minimize, packaging, and a
LICENSE were considered and explicitly declined — not on this list.)

Suggested order: **CI → persistent settings → Wayland.** CI is
infrastructure that should exist before a change as invasive as Wayland
support lands; persistent settings is small and self-contained; Wayland is
the biggest and riskiest, best done last with a safety net already in place.

---

## 1. CI (GitHub Actions) — done, green

`.github/workflows/ci.yml`, on push/PR to `main`. PR #1 tracks this
branch; badge added to README.

- [x] Workflow file written and actually run (twice failed, once fixed,
      now green — see below), not just authored and assumed to work.
- [x] **Resolved: `ubuntu-latest` doesn't have libadwaita ≥ 1.7.** Checked
      packages.ubuntu.com directly: `ubuntu-latest` is Ubuntu 24.04
      (libadwaita 1.5.0) as of this writing; even 25.10 only has 1.8.0, and
      26.04 (1.9.0) is a public-preview runner label, not something to
      depend on long-term. **Runs in a `debian:trixie` container instead**
      (`jobs.<job>.container:`) — trixie ships 1.7.6, matching this
      project's actual dev environment exactly, rather than chasing Ubuntu
      runner versions.
- [x] Apt package list kept in sync with README's Requirements block (comment
      in the workflow points back at it).
- [x] `cargo build` (not `--release` — this job is about correctness, not a
      distributable artifact).
- [x] `cargo test` — confirmed clean with no X11/display needed, exactly as
      predicted. **What wasn't predicted, and only showed up by actually
      running it:** `audio.rs`'s System/Both-mode tests failed in the bare
      container — no PulseAudio *server* at all (`pulseaudio-utils` is
      client-only). Fixed by installing `pulseaudio` and starting it in
      `--system` mode. That in turn hit a second real failure: the distro
      default ACLs the native socket to the `pulse-access` group, and root
      (there's no non-root user in this container) isn't in it — fixed by
      loading a minimal config with `auth-anonymous=1` instead of the
      distro default. Both fixes are the kind of thing no amount of reading
      the workflow file would have caught — this is exactly why it was
      worth actually pushing and watching it run rather than stopping at
      "looks right."
- [x] `cargo clippy -- -D warnings` — clean, included as a hard gate.
      `cargo fmt --check` deliberately left out: the existing codebase
      isn't currently clean under rustfmt's defaults, and reformatting
      every file is its own follow-up, not something to bundle in here.
- [x] `Swatinem/rust-cache@v2` added.
- [x] README badge added.

## 2. Persistent GUI settings — done

Scope: **GUI only.** The CLI's settings are just its argv per invocation —
nothing to persist there.

- [x] Add `serde` (+ `derive`), `toml`. **Deviation from the original plan:**
      skipped the `dirs`/`directories` crate — hand-rolled the two-env-var
      XDG lookup instead (`$XDG_CONFIG_HOME` else `$HOME/.config`), matching
      this codebase's existing habit of hand-rolling small OS queries
      directly rather than reaching for a crate over it.
- [x] Config location: `~/.config/desktoprecorder/config.toml`. **Real
      finding along the way:** that directory turned out to already exist on
      this box — an unrelated Electron app's profile (`Cache/`, `Local
      State`, `Preferences`, etc.), predating this project. Flagged it and
      confirmed with the user before writing anything there; using the same
      directory was fine.
- [x] Settings struct fields: audio mode, audio-device override, framerate,
      bitrate, preset, and source choice (full screen / monitor index, never
      a specific window — see reasoning below). **Deviation:** dropped
      `container` from the original plan — the GUI has no container widget
      of its own (it's inferred from the output path's extension), so a
      persisted value would never have anything to apply back to. Output
      path stays excluded, as planned, so a restored session can't silently
      overwrite a prior recording on the next Start.
- [x] Load path: `config::load()` called right after `Gui` is constructed in
      `build_ui`, before any signal handler is wired up or the first
      `start_standalone_preview` call. Never fails outright — missing,
      partial, or corrupt all fall back to per-field defaults.
- [x] Save path: on a validated Record click (after every input-validation
      guard has already passed), not on the pipeline actually reaching
      Playing — `start_recording`'s background thread doesn't report that
      back synchronously, and gating on it would need new signaling machinery
      for little real benefit.
- [x] Unit tests: 12 new ones — `config.rs`'s round-trip/lowercase-spelling/
      missing-fields/corrupt-TOML/no-resolvable-base cases, plus `gui.rs`'s
      new pure index-mapping helpers (`source_dropdown_index`,
      `audio_dropdown_index`, `preset_dropdown_index` and their fallback
      paths). `settings_source_monitor` (the save-side inverse) takes a live
      `&Gui` and is left to the same manual, not-automated pass as
      `selected_source_choice` and the rest of `gui.rs`'s signal wiring.
- [ ] README: note the config file path.

## 3. Wayland support (`xdg-desktop-portal` + PipeWire)

The big one. `source.rs` already has the seam for this — a commented-out
`CaptureSource::Portal(PortalConfig)` variant — because everything
downstream of `CaptureSource::build_element()` only ever sees a
`gst::Element` and doesn't care how it was constructed. Both spikes below
are now resolved; implementation hasn't started.

- [x] **Spike resolved: use `zbus::blocking`, not `ashpd`.** Checked both:
      `ashpd` is async-only (no blocking API at all — ties you to `tokio` or
      an `async-io`-compatible executor). `zbus` ships a real
      `zbus::blocking` module built specifically for exactly this case
      ("blocking wrappers are provided for convenience"). Given portal
      negotiation here is a handful of one-shot calls
      (`CreateSession`/`SelectSources`/`Start`) rather than a sustained
      loop, and this project has no async runtime anywhere else (threads +
      `Arc<Mutex<_>>`/`AtomicBool` throughout, see CLAUDE.md), hand-rolling
      the `org.freedesktop.portal.ScreenCast` proxy calls directly against
      `zbus::blocking` is the right trade — a bit more code than `ashpd`'s
      convenience wrappers would need, in exchange for not pulling in
      `tokio` for one startup sequence.
- [x] **Spike resolved: `pipewiresrc` needs an explicit new package.** Not
      installed by default on Debian trixie (confirmed via `gst-inspect-1.0
      pipewiresrc` → "No such element or plugin", despite the base PipeWire
      libraries already being present for unrelated reasons). It's packaged
      as `gstreamer1.0-pipewire` (1.4.2-1 in trixie's repos) — add this to
      README's Requirements block and both CI workflow apt lists once this
      lands; it's genuinely new, not already covered by the existing
      gstreamer plugin packages.
- [ ] **Caveat surfaced, not yet resolvable on this box:** `xdg-desktop-portal`
      + `xdg-desktop-portal-gtk` are already installed and running here
      (confirmed live on the session bus) — but this machine's a plain X11
      session with no Wayland compositor, and the ScreenCast portal
      interface needs compositor-side support (GNOME Mutter,
      KDE KWin, or wlroots' screencopy protocol) that a bare X11 desktop's
      portal backend doesn't provide. That means once this is implemented,
      it genuinely can't be manually end-to-end verified on this specific
      dev box (consistent with the Testing story item below) — the actual
      "does `CreateSession`/`Start` succeed and hand back a working node
      id" check needs a real Wayland session (a different machine, or a
      nested compositor), not just code that compiles here.
- [ ] **Design wrinkle to resolve, not gloss over:** portals don't expose
      monitor/window enumeration ahead of picking, by design (privacy) —
      the compositor draws its own picker dialog during
      `SelectSources`/`Start`. That means `--monitor N` / `--window ID`
      (and `list-sources`) have no direct Wayland equivalent; the user
      picks interactively every time, or we look into a portal
      "restore token" for remembering a prior choice. Decide the actual UX
      before writing code around it.
- [ ] Session lifecycle: `CreateSession` → `SelectSources` → `Start` gives
      back a PipeWire node id + fd, which feeds
      `pipewiresrc fd=<fd> path=<node-id>`. Unlike `X11ScreenConfig`
      (stateless, built fresh per call), this needs the D-Bus session kept
      alive for the pipeline's whole lifetime — a real structural
      difference from the X11 path.
- [ ] Implement `CaptureSource::Portal(PortalConfig)` + its `build_element()`.
- [ ] `main.rs`: replace `check_not_wayland()`'s hard bail with a portal
      branch when `WAYLAND_DISPLAY` is set (keep failing clearly if portal
      negotiation itself isn't available, rather than silently
      black-recording).
- [ ] `gui.rs`: `build_capture_source`/the source dropdown currently assume
      `X11ScreenConfig` only — needs a portal branch, and the UX itself
      changes shape (Start button → compositor picker dialog → recording
      begins), which is worth spelling out in the README once built.
- [ ] Verify audio capture is unaffected. `audio.rs`'s mic/system-audio
      paths go through PulseAudio/`pactl`, which sits below the display
      protocol — the expectation is this needs zero changes under
      Wayland, but confirm rather than assume.
- [ ] Testing story: live portal calls need a running compositor + portal
      backend + interactive consent, so — same pattern as
      `x11_query.rs`/`hotkey.rs` — extract whatever pure logic exists
      (parsing the portal's response, building the `pipewiresrc` element
      string from an already-resolved fd/node-id) into testable functions,
      and leave the actual D-Bus round trip to a documented manual pass.
- [ ] Docs once merged: README's "X11 only" framing, CLAUDE.md's module map
      (`source.rs`'s role) and Known-gaps section.
