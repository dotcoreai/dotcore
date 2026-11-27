//! Recognizes the oMLX package launcher's refusal to start on a macOS older than the pinned
//! release supports, for example `omlx: macOS 26 or newer is required, found 15.5`.
//!
//! The refusal is reported with the macOS version required and the one found. Only the
//! launcher's fixed wording is matched, so other lines are not recognized. When the line repeats,
//! the last occurrence wins.

use crate::model::engine::startup_refusal::StartupRefusal;

/// Start of the launcher's refusal, after which it names the required major version.
const REFUSAL_MARKER: &str = "omlx: macOS ";

/// Text between the required version and the version found.
const REFUSAL_COMPARISON: &str = " or newer is required, found ";

/// Returns the launcher's refusal in startup output, in chronological order with one entry per
/// line.
pub(super) fn startup_refusal(output: &str) -> Option<StartupRefusal> {
    output.lines().rev().find_map(macos_refusal)
}

fn macos_refusal(line: &str) -> Option<StartupRefusal> {
    let refusal = &line[line.find(REFUSAL_MARKER)? + REFUSAL_MARKER.len()..];
    let (required, found) = refusal.split_once(REFUSAL_COMPARISON)?;
    let found = found.trim();
    if !is_version(required) || !is_version(found) {
        return None;
    }
    Some(StartupRefusal::UnsupportedOperatingSystem {
        required: format!("macOS {required}"),
        found: format!("macOS {found}"),
        line: line.trim().to_owned(),
    })
}

/// Whether `value` is a dotted version such as `26` or `15.5.1`.
fn is_version(value: &str) -> bool {
    value
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_refusal_reports_the_required_and_found_macos() {
        let output = "Traceback (most recent call last):\nomlx: macOS 26 or newer is required, found 15.5.1\n";

        assert_eq!(
            startup_refusal(output),
            Some(StartupRefusal::UnsupportedOperatingSystem {
                required: "macOS 26".to_owned(),
                found: "macOS 15.5.1".to_owned(),
                line: "omlx: macOS 26 or newer is required, found 15.5.1".to_owned(),
            })
        );
    }

    #[test]
    fn other_startup_failures_have_no_refusal() {
        for output in [
            "omlx: macOS 26 or newer is required, found ",
            "omlx: macOS twenty-six or newer is required, found 15.5",
            "RuntimeError: Metal device not found",
        ] {
            assert_eq!(startup_refusal(output), None, "{output}");
        }
    }
}
