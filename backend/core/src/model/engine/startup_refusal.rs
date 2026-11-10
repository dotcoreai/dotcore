//! Typed reasons an engine gives for refusing to start, read from its startup output.
//!
//! An engine's adapter recognizes its own refusal wording and reports it as one of these
//! reasons with the figures it printed, so the controller can explain the failure in operator
//! terms. Engines without a recognized refusal report none, and the failure stays generic.

/// An engine's own reason for stopping during startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupRefusal {
    /// The device had less free memory than the engine needs to start.
    InsufficientFreeMemory {
        needed_bytes: u64,
        free_bytes: u64,
        line: String,
    },
}

impl StartupRefusal {
    /// The output line the refusal was read from.
    pub(crate) fn line(&self) -> &str {
        match self {
            Self::InsufficientFreeMemory { line, .. } => line,
        }
    }
}
