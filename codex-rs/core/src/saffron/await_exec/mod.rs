//! Event-driven coordination for long-running unified-exec sessions.
//!
//! The feature has two boundaries. [`Handler`] owns the model-visible tool
//! contract and response rendering. The private `wait` module selects the wake
//! condition and asks unified exec to acquire, wait on, and finalize one
//! serialized process interaction.
//!
//! Callers enter through [`Handler`]. Process launch, input, termination,
//! sandbox policy, session storage, output ownership, and lifecycle cleanup
//! remain owned by unified exec.

mod handler;
mod wait;

pub(crate) use handler::Handler;

/// Selects which process events may complete an await operation.
///
/// Process exit always completes the operation because an exited session
/// cannot satisfy a later condition.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReturnOn {
    /// Return when output is available or the process exits.
    #[default]
    OutputOrExit,

    /// Retain intermediate output and return on process exit or timeout.
    Exit,
}
