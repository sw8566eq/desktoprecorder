# TODO

Roadmap for the three gaps we're actually closing: Wayland support, CI, and
persistent GUI settings. (Pause/resume, tray/minimize, packaging, and a
LICENSE were considered and explicitly declined — not on this list.)

Suggested order: **CI → persistent settings → Wayland.** CI is
infrastructure that should exist before a change as invasive as Wayland
support lands; persistent settings is small and self-contained; Wayland is
the biggest and riskiest, best done last with a safety net already in place.

---

## 1. CI (GitHub Actions)

Repo already has a remote (`origin` → `sw8566eq/desktoprecorder` on GitHub),
so this is just adding a workflow file.

- [ ] `.github/workflows/ci.yml`, triggered on push/PR to `main`.
- [ ] **Verify `libadwaita-1-dev` >= 1.7 is actually available** on whatever
      `ubuntu-latest` resolves to right now — `Cargo.toml` pins the `v1_7`
      feature, and older Ubuntu apt repos may only carry an older
      libadwaita. Check before writing the rest of the workflow around an
      assumption; if it's not there, either pin an older runner image with
      a PPA, or relax the feature flag (decide which once we know).
- [ ] Install the same apt package list from README's Requirements block.
      Keep it in sync manually for now (comment in the workflow pointing
      back at the README section) rather than over-engineering a shared
      script for one list.
- [ ] `cargo build` (skip `--release` in CI for speed unless we want a
      downloadable artifact out of this — decide when writing the job).
- [ ] `cargo test` — should run clean with no `Xvfb`/display needed at all,
      per the existing test design (nothing touches a live X11 connection).
      Worth confirming on the first real CI run rather than assuming it
      holds outside this box.
- [ ] `cargo fmt --check` + `cargo clippy -- -D warnings` as a lint gate.
      Recommended, not load-bearing for the other two items — fine to land
      in a follow-up if it turns up a pile of pre-existing warnings.
- [ ] Cache `~/.cargo` + `target/` (e.g. `Swatinem/rust-cache`) — the
      gstreamer/gtk4/libadwaita dependency tree is heavy enough that an
      uncached rebuild every push is a real cost.
- [ ] Add a status badge to README once green.

## 2. Persistent GUI settings

Scope: **GUI only.** The CLI's settings are just its argv per invocation —
nothing to persist there.

- [ ] Add `serde` (+ `derive`), `toml`, and a directories crate (`dirs` or
      `directories`) — none are dependencies today.
- [ ] Config location: XDG config dir, e.g.
      `~/.config/desktoprecorder/config.toml`.
- [ ] Decide the settings struct's fields. Straightforward: audio mode,
      audio-device override text, container, framerate, bitrate, preset.
      Needs an explicit call: **source choice** — remember "full screen" or
      "monitor N", but not a specific window (window IDs are ephemeral
      across sessions, a saved one would almost always be stale). **Output
      path** — deliberately excluded, to avoid silently overwriting
      yesterday's recording on the next Start.
- [ ] Load path: in `gui::run()`, construct widgets with today's hardcoded
      defaults first, then apply the loaded config over them before
      `window.present()`. A missing file is the common case (first run) —
      not an error. A partially-corrupt file should fall back to defaults
      per-field rather than failing the whole GUI over one bad line.
- [ ] Save path: on successful recording start. (Covers the normal case and
      survives a later force-kill of the window; simpler than also hooking
      close-request.)
- [ ] Unit tests: serialize/deserialize round-trip on the settings struct,
      and "missing/corrupt file → falls back to defaults" — both pure
      logic, no display needed, consistent with how the rest of the suite
      is scoped.
- [ ] README: note the config file path once this lands.

## 3. Wayland support (`xdg-desktop-portal` + PipeWire)

The big one. `source.rs` already has the seam for this — a commented-out
`CaptureSource::Portal(PortalConfig)` variant — because everything
downstream of `CaptureSource::build_element()` only ever sees a
`gst::Element` and doesn't care how it was constructed. Start with a spike
to resolve the open questions below before committing to an approach.

- [ ] **Spike: async runtime vs. blocking D-Bus.** Portal negotiation
      (`org.freedesktop.portal.ScreenCast`) is naturally done via the
      `ashpd` crate, but `ashpd` is async and this project has no async
      runtime today (it's threads + `Arc<Mutex<_>>`/`AtomicBool>`
      throughout — see the GTK4/gstreamer-rs threading notes in
      CLAUDE.md). Portal negotiation is a one-shot startup call, not a
      sustained loop, so pulling in `tokio` for it is disproportionate.
      Evaluate `zbus`'s blocking API as a hand-rolled alternative before
      deciding.
- [ ] **Spike: confirm the `pipewiresrc` GStreamer element is actually
      available** on this box/distro (may need `gstreamer1.0-pipewire` or
      be bundled in `gst-plugins-good`/`bad` depending on version — check
      before assuming the README's existing apt list already covers it).
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
