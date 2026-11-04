//! The native workload lifetime step of guided `dotc site init` and reset: a choice between
//! workloads that keep running when the server stops and workloads that stop with it.
//!
//! Only an Apple inference runs native workloads, so it is asked only when the node offers one.
//! An explicit lifetime from the command line is never asked again.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime;
use dotcore_site::initial::request::InitialInferenceRuntime;

use crate::terminal::{SelectItem, SelectPrompt, Terminal};

/// Returns the explicit lifetime, or asks for one when the chosen inference runs native
/// workloads. Returns `None` when no lifetime applies, leaving the request default.
pub(super) fn collect_native_workload_lifetime(
    explicit: Option<NativeWorkloadLifetime>,
    inference: &BTreeSet<InitialInferenceRuntime>,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<Option<NativeWorkloadLifetime>> {
    if explicit.is_some() || !inference.contains(&InitialInferenceRuntime::Apple) {
        return Ok(explicit);
    }

    let prompt = SelectPrompt::new(
        "Native workloads",
        [
            SelectItem::new("persistent")
                .with_description("keep running when the server stops and are adopted again"),
            SelectItem::new("ephemeral").with_description("stop when the server exits"),
        ],
    )
    .with_default(0);

    Ok(Some(match terminal.select(&prompt)? {
        0 => NativeWorkloadLifetime::Persistent,
        _ => NativeWorkloadLifetime::Ephemeral,
    }))
}
