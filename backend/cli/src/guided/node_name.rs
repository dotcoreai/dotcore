//! The SiteNode name step of guided `dotc site init` and reset: a text prompt
//! pre-filled with the machine's hostname-derived name, for example `studio`.
//!
//! The step reports only a name the operator chose. Accepting the offered name
//! reports nothing, so a repeated initialization still recognizes an existing
//! site whatever name it holds. Without an offered name, an empty answer is
//! accepted and the SiteNode is named after its ID.

use dotcore_site::validation::is_valid_display_name;

use crate::terminal::{InputPrompt, Message, Terminal};

/// Returns the explicit name, or asks for one until it is valid.
///
/// Returns `None` when the operator accepts the offered default or leaves the answer empty
/// without an offered default.
pub(super) fn collect_node_name(
    explicit: Option<String>,
    default: Option<&str>,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<Option<String>> {
    if explicit.is_some() {
        return Ok(explicit);
    }

    let prompt = match default {
        Some(default) => InputPrompt::new("Node name").with_default(default),
        None => InputPrompt::new("Node name"),
    };
    loop {
        let answer = terminal.input(&prompt)?;
        let name = answer.trim();
        if name.is_empty() {
            return Ok(None);
        }
        if !is_valid_display_name(name) {
            terminal.message(&Message::Error(
                "Node name must contain 1 to 128 non-control characters".to_owned(),
            ))?;
            continue;
        }
        return Ok((Some(name) != default).then(|| name.to_owned()));
    }
}
