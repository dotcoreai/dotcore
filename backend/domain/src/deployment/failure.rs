//! Shared failure diagnostics for deployments of every owner.
//!
//! A deployment failure carries a few labeled lines, such as its stage and cause, then the last
//! output of what failed. This module bounds them to what one failure may carry: the labeled lines
//! keep their start, and the output tail keeps its newest output. Terminal styling and control
//! characters are removed from the tail, so it reads as plain text wherever it is shown.

use super::{MAX_FAILURE_DIAGNOSTIC_BYTES, MAX_FAILURE_DIAGNOSTICS};

/// Diagnostics of one failure, within the count and byte limits of a failure: the labeled `lines`
/// in order, then the cleaned `output_tail` when it has any text. Lines beyond the count limit are
/// dropped, keeping a line for the tail; beyond the byte limit, the tail loses its oldest output
/// first.
pub fn failure_diagnostics(mut lines: Vec<String>, output_tail: Option<&str>) -> Vec<String> {
    let output_tail = output_tail
        .map(plain_text)
        .filter(|tail| !tail.trim().is_empty());
    lines.truncate(MAX_FAILURE_DIAGNOSTICS - usize::from(output_tail.is_some()));

    let mut remaining = MAX_FAILURE_DIAGNOSTIC_BYTES;
    let mut diagnostics = Vec::with_capacity(lines.len() + 1);
    for line in lines {
        if remaining == 0 {
            break;
        }
        let kept = leading(&line, remaining);
        remaining -= kept.len();
        diagnostics.push(kept.to_owned());
    }

    if let Some(tail) = output_tail
        && remaining > OUTPUT_TAIL_PREFIX.len()
    {
        let tail = trailing(&tail, remaining - OUTPUT_TAIL_PREFIX.len());
        diagnostics.push(format!("{OUTPUT_TAIL_PREFIX}{tail}"));
    }
    diagnostics
}

const OUTPUT_TAIL_PREFIX: &str = "Output tail: ";

/// `output` without terminal escape sequences and control characters other than line breaks and
/// tabs.
fn plain_text(output: &str) -> String {
    let mut plain = String::with_capacity(output.len());
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            if !character.is_control() || matches!(character, '\n' | '\t') {
                plain.push(character);
            }
            continue;
        }
        match characters.next() {
            // A control sequence, such as a color, ends at its first final byte.
            Some('[') => {
                for parameter in characters.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&parameter) {
                        break;
                    }
                }
            }
            // An operating system command, such as a window title, ends at a bell or a string
            // terminator.
            Some(']') => {
                while let Some(parameter) = characters.next() {
                    if parameter == '\u{7}' {
                        break;
                    }
                    if parameter == '\u{1b}' && characters.peek() == Some(&'\\') {
                        characters.next();
                        break;
                    }
                }
            }
            // Any other escape is two characters long.
            _ => {}
        }
    }
    plain
}

/// The longest start of `text` within `limit` bytes, cut on a character boundary.
fn leading(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The longest end of `text` within `limit` bytes, cut on a character boundary.
fn trailing(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_keep_the_lines_then_the_output_tail() {
        assert_eq!(
            failure_diagnostics(
                vec![
                    "Stage: artifact preparation".to_owned(),
                    "Cause: broken".to_owned()
                ],
                Some("building\nerror: failed"),
            ),
            [
                "Stage: artifact preparation",
                "Cause: broken",
                "Output tail: building\nerror: failed",
            ]
        );
        assert_eq!(
            failure_diagnostics(vec!["Stage: artifact preparation".to_owned()], None),
            ["Stage: artifact preparation"]
        );
    }

    #[test]
    fn the_output_tail_loses_terminal_styling_and_control_characters() {
        assert_eq!(
            failure_diagnostics(
                Vec::new(),
                Some(
                    "\u{1b}[1;31merror:\u{1b}[0m failed\r\n\u{1b}]0;title\u{7}step\tdone\u{1b}7\u{8}"
                ),
            ),
            ["Output tail: error: failed\nstep\tdone"]
        );
        assert_eq!(
            failure_diagnostics(Vec::new(), Some("\u{1b}[0m\r\n")),
            Vec::<String>::new(),
            "a tail with no text is left out"
        );
    }

    #[test]
    fn diagnostics_beyond_the_count_limit_drop_lines_and_keep_the_output_tail() {
        let lines = (1..=10).map(|n| format!("Detail: {n}")).collect::<Vec<_>>();

        let diagnostics = failure_diagnostics(lines, Some("last line"));

        assert_eq!(
            diagnostics,
            [
                "Detail: 1",
                "Detail: 2",
                "Detail: 3",
                "Detail: 4",
                "Detail: 5",
                "Detail: 6",
                "Detail: 7",
                "Output tail: last line",
            ]
        );
        assert_eq!(diagnostics.len(), MAX_FAILURE_DIAGNOSTICS);
    }

    #[test]
    fn diagnostics_beyond_the_byte_limit_keep_the_end_of_the_output_tail() {
        let tail = format!(
            "{}{}",
            "a".repeat(10),
            "z".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES)
        );

        let diagnostics = failure_diagnostics(vec!["Stage: build".to_owned()], Some(&tail));

        let room = MAX_FAILURE_DIAGNOSTIC_BYTES - "Stage: build".len() - OUTPUT_TAIL_PREFIX.len();
        assert_eq!(
            diagnostics,
            [
                "Stage: build".to_owned(),
                format!("Output tail: {}", "z".repeat(room)),
            ]
        );
        assert_eq!(
            diagnostics.iter().map(String::len).sum::<usize>(),
            MAX_FAILURE_DIAGNOSTIC_BYTES
        );

        // A multibyte character straddling the cut moves it to the next character boundary.
        let straddling = format!("é{}", "z".repeat(room - 1));
        let diagnostics = failure_diagnostics(vec!["Stage: build".to_owned()], Some(&straddling));
        assert_eq!(
            diagnostics[1],
            format!("Output tail: {}", "z".repeat(room - 1))
        );
    }

    #[test]
    fn lines_beyond_the_byte_limit_are_cut_and_leave_no_room_for_the_tail() {
        let lines = vec![
            "Stage: build".to_owned(),
            "d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES),
        ];

        let diagnostics = failure_diagnostics(lines, Some("last line"));

        assert_eq!(
            diagnostics,
            [
                "Stage: build".to_owned(),
                "d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES - "Stage: build".len()),
            ]
        );
    }
}
