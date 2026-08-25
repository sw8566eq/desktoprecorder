/// Domain-specific errors worth matching on directly. Everything else in
/// this crate (element creation failures, state-change failures, linking
/// failures) flows through as plain `anyhow` context chains around the
/// underlying `glib`/GStreamer errors -- there's no real benefit to
/// wrapping those in bespoke variants too.
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("--{flag} is not implemented yet; it's reserved for a later milestone")]
    NotYetImplemented { flag: &'static str },
}
