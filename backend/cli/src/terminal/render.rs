//! The one layout every `dotc` terminal message goes through, for example a green `✔ Added
//! gb10-host-2` line or an aligned table under a heading.
//!
//! A message is turned into lines of role-tagged pieces, so the plain terminal prints the text
//! and the colored terminal maps each role to a style, and both show the same symbols,
//! indentation, alignment and spacing. Symbols fall back to ASCII on terminals that cannot show
//! them.

use std::ffi::OsStr;
use std::io::IsTerminal;

use ratatui::text::Span;

use super::{Message, NextStep, supports_rich_prompts};

/// What a piece of a line means, which a terminal maps to a style.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Role {
    /// Unstyled text.
    Plain,

    /// A section title.
    Heading,

    /// The label of a fact or a table header.
    Label,

    /// A fact value or a next-step marker.
    Accent,

    /// A positive outcome.
    Success,

    /// A degraded but acceptable situation.
    Warning,

    /// A failed step or rejected answer.
    Error,

    /// Secondary guidance.
    Hint,

    /// A command to copy.
    Command,
}

/// A run of text with one role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Piece {
    pub(super) role: Role,
    pub(super) text: String,
}

/// One printed line.
pub(super) type RenderedLine = Vec<Piece>;

/// A message as lines, with whether a blank line separates it from what was printed before.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Rendered {
    pub(super) blank_before: bool,
    pub(super) lines: Vec<RenderedLine>,
}

/// What a terminal printed last, which decides whether its next output starts after a blank
/// line. An answered prompt closes its block, so whatever follows it is separated; a prompt right
/// after messages stays attached to them, since they are its context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum LastOutput {
    /// Nothing yet, so not even a block is separated.
    #[default]
    Nothing,

    /// A message, or the lines of a prompt that was not answered.
    Message,

    /// An answered prompt.
    Answer,
}

impl LastOutput {
    /// Reports whether the next output needs a blank line first; `block` is set for output that
    /// starts a block of its own, such as a heading.
    pub(super) fn blank_before(self, block: bool) -> bool {
        match self {
            Self::Nothing => false,
            Self::Message => block,
            Self::Answer => true,
        }
    }
}

/// The status marks of a terminal: Unicode symbols, or ASCII where they cannot be shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Symbols {
    unicode: bool,
}

impl Symbols {
    /// Unicode marks: `✔ ✘ ! ›`.
    pub(super) const UNICODE: Self = Self { unicode: true };

    /// ASCII marks: `OK x ! >`.
    pub(super) const ASCII: Self = Self { unicode: false };

    /// Picks the marks for the process environment.
    pub(super) fn detect() -> Self {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(std::env::var_os)
            .find(|value| !value.is_empty());
        Self::choose(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("TERM").as_deref(),
            locale.as_deref(),
        )
    }

    /// Uses Unicode only on a known terminal type with `NO_COLOR` unset or empty and a UTF-8
    /// locale.
    pub(super) fn choose(
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
        locale: Option<&OsStr>,
    ) -> Self {
        let no_color = no_color.is_some_and(|value| !value.is_empty());
        let utf8 = locale.is_some_and(|locale| {
            let locale = locale.to_string_lossy().to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        });
        Self {
            unicode: !no_color && supports_rich_prompts(term) && utf8,
        }
    }

    fn success(self) -> &'static str {
        if self.unicode { "✔ " } else { "OK " }
    }

    fn error(self) -> &'static str {
        if self.unicode { "✘ " } else { "x " }
    }

    fn warning(self) -> &'static str {
        "! "
    }

    fn step(self) -> &'static str {
        if self.unicode { "› " } else { "> " }
    }
}

impl Message {
    /// Lays the message out as lines with the given status marks.
    pub(super) fn render(&self, symbols: Symbols) -> Rendered {
        let one = |role, text: String| vec![vec![Piece { role, text }]];
        let spaced = |lines| Rendered {
            blank_before: false,
            lines,
        };
        match self {
            Self::Heading(text) => Rendered {
                blank_before: true,
                lines: one(Role::Heading, text.clone()),
            },
            Self::Info(text) => spaced(one(Role::Plain, text.clone())),
            Self::Success(text) => {
                spaced(one(Role::Success, format!("{}{text}", symbols.success())))
            }
            Self::Warning(text) => {
                spaced(one(Role::Warning, format!("{}{text}", symbols.warning())))
            }
            Self::Error(text) => spaced(one(Role::Error, format!("{}{text}", symbols.error()))),
            Self::Hint(text) => spaced(one(Role::Hint, text.clone())),
            Self::Command(text) => spaced(one(Role::Command, format!("  {text}"))),
            Self::KeyValues(pairs) => spaced(key_value_lines(pairs)),
            Self::Table { header, rows } => spaced(table_lines(header, rows)),
            Self::Next(steps) => Rendered {
                blank_before: true,
                lines: next_lines(steps, symbols),
            },
        }
    }

    /// Renders the message as unstyled lines with Unicode marks, without separating blank lines.
    pub fn plain_lines(&self) -> Vec<String> {
        self.render(Symbols::UNICODE)
            .lines
            .iter()
            .map(plain_text)
            .collect()
    }
}

/// Joins the pieces of a line without styling.
pub(super) fn plain_text(line: &RenderedLine) -> String {
    line.iter().map(|piece| piece.text.as_str()).collect()
}

/// Decides colors for a terminal: it must be a terminal, `NO_COLOR` must be unset or empty, and
/// the terminal type must be set and not dumb.
pub(super) fn colors_enabled(
    is_terminal: bool,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    let no_color = no_color.is_some_and(|value| !value.is_empty());
    is_terminal && !no_color && supports_rich_prompts(term)
}

/// Lays out messages for process stderr when it is a terminal, with ANSI colors where the
/// environment allows them. Returns `None` when stderr is not a terminal, so redirected
/// diagnostics keep their plain form.
pub(super) fn render_for_stderr(messages: &[Message]) -> Option<String> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let colors = colors_enabled(
        true,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    );
    Some(ansi_text(messages, Symbols::detect(), colors))
}

/// Joins the rendered messages with their blank-line spacing, styling pieces with ANSI codes
/// when `colors` is set.
fn ansi_text(messages: &[Message], symbols: Symbols, colors: bool) -> String {
    let mut text = String::new();
    for message in messages {
        let rendered = message.render(symbols);
        if rendered.blank_before && !text.is_empty() {
            text.push('\n');
        }
        for line in &rendered.lines {
            for piece in line {
                match (colors, ansi_code(piece.role)) {
                    (true, Some(code)) => {
                        text.push_str(&format!("\x1b[{code}m{}\x1b[0m", piece.text));
                    }
                    _ => text.push_str(&piece.text),
                }
            }
            text.push('\n');
        }
    }
    text
}

fn ansi_code(role: Role) -> Option<&'static str> {
    match role {
        Role::Plain => None,
        Role::Heading => Some("1"),
        Role::Label | Role::Hint => Some("2"),
        Role::Accent => Some("36"),
        Role::Success => Some("32"),
        Role::Warning => Some("33"),
        Role::Error => Some("31"),
        Role::Command => Some("1;36"),
    }
}

fn key_value_lines(pairs: &[(String, String)]) -> Vec<RenderedLine> {
    let width = pairs
        .iter()
        .map(|(key, _)| text_width(key))
        .max()
        .unwrap_or(0);
    pairs
        .iter()
        .map(|(key, value)| {
            let padding = " ".repeat(width - text_width(key));
            vec![
                Piece {
                    role: Role::Label,
                    text: format!("  {key}{padding}  "),
                },
                Piece {
                    role: Role::Accent,
                    text: value.clone(),
                },
            ]
        })
        .collect()
}

/// Lays out a header and rows in columns two spaces apart, indented like facts, with no trailing
/// padding.
fn table_lines(header: &[String], rows: &[Vec<String>]) -> Vec<RenderedLine> {
    let widths = (0..header.len())
        .map(|column| {
            std::iter::once(header)
                .chain(rows.iter().map(Vec::as_slice))
                .map(|row| row.get(column).map_or(0, |cell| text_width(cell)))
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let line = |role, row: &[String]| {
        let cells = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell}{}", " ".repeat(width - text_width(cell))))
            .collect::<Vec<_>>()
            .join("  ");
        vec![Piece {
            role,
            text: format!("  {}", cells.trim_end()),
        }]
    };
    std::iter::once(line(Role::Label, header))
        .chain(rows.iter().map(|row| line(Role::Plain, row)))
        .collect()
}

fn next_lines(steps: &[NextStep], symbols: Symbols) -> Vec<RenderedLine> {
    let mut lines = vec![vec![Piece {
        role: Role::Heading,
        text: "Next".to_owned(),
    }]];
    for step in steps {
        lines.push(vec![
            Piece {
                role: Role::Accent,
                text: format!("  {}", symbols.step()),
            },
            Piece {
                role: Role::Plain,
                text: step.text().to_owned(),
            },
        ]);
        if let Some(command) = step.command() {
            lines.push(vec![Piece {
                role: Role::Command,
                text: format!("      {command}"),
            }]);
        }
    }
    lines
}

fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{Piece, Role, Symbols, ansi_text, colors_enabled};
    use crate::terminal::{Message, NextStep};

    fn plain(message: &Message, symbols: Symbols) -> Vec<String> {
        message
            .render(symbols)
            .lines
            .iter()
            .map(super::plain_text)
            .collect()
    }

    #[test]
    fn symbols_need_a_known_terminal_a_utf8_locale_and_no_no_color() {
        let xterm = Some(OsStr::new("xterm-256color"));
        let utf8 = Some(OsStr::new("en_US.UTF-8"));

        assert_eq!(Symbols::choose(None, xterm, utf8), Symbols::UNICODE);
        assert_eq!(
            Symbols::choose(Some(OsStr::new("")), xterm, Some(OsStr::new("C.utf8"))),
            Symbols::UNICODE
        );
        assert_eq!(
            Symbols::choose(Some(OsStr::new("1")), xterm, utf8),
            Symbols::ASCII
        );
        assert_eq!(
            Symbols::choose(None, Some(OsStr::new("dumb")), utf8),
            Symbols::ASCII
        );
        assert_eq!(Symbols::choose(None, None, utf8), Symbols::ASCII);
        assert_eq!(
            Symbols::choose(None, xterm, Some(OsStr::new("C"))),
            Symbols::ASCII
        );
        assert_eq!(Symbols::choose(None, xterm, None), Symbols::ASCII);
    }

    #[test]
    fn colors_follow_the_terminal_no_color_and_term() {
        let xterm = Some(OsStr::new("xterm-256color"));

        assert!(colors_enabled(true, None, xterm));
        assert!(colors_enabled(true, Some(OsStr::new("")), xterm));
        assert!(!colors_enabled(true, Some(OsStr::new("1")), xterm));
        assert!(!colors_enabled(true, None, Some(OsStr::new("dumb"))));
        assert!(!colors_enabled(true, None, None));
        assert!(!colors_enabled(false, None, xterm));
    }

    #[test]
    fn stderr_text_separates_blocks_and_styles_pieces_only_with_colors() {
        let messages = [
            Message::Error("Failed".to_owned()),
            Message::Next(vec![NextStep::new("Retry").with_command("dotc node list")]),
        ];

        assert_eq!(
            ansi_text(&messages, Symbols::ASCII, false),
            "x Failed\n\nNext\n  > Retry\n      dotc node list\n"
        );
        assert_eq!(
            ansi_text(&messages[..1], Symbols::UNICODE, true),
            "\x1b[31m✘ Failed\x1b[0m\n"
        );
    }

    #[test]
    fn status_lines_carry_a_mark_with_an_ascii_fallback() {
        let cases = [
            (Message::Success("Added".to_owned()), "✔ Added", "OK Added"),
            (Message::Error("Failed".to_owned()), "✘ Failed", "x Failed"),
            (
                Message::Warning("Careful".to_owned()),
                "! Careful",
                "! Careful",
            ),
        ];

        for (message, unicode, ascii) in cases {
            assert_eq!(plain(&message, Symbols::UNICODE), [unicode]);
            assert_eq!(plain(&message, Symbols::ASCII), [ascii]);
        }
    }

    #[test]
    fn only_headings_and_next_steps_are_separated_by_a_blank_line() {
        let next = Message::Next(vec![NextStep::new("Go")]);

        assert!(
            Message::Heading("Plan".to_owned())
                .render(Symbols::UNICODE)
                .blank_before
        );
        assert!(next.render(Symbols::UNICODE).blank_before);
        assert!(
            !Message::Info("text".to_owned())
                .render(Symbols::UNICODE)
                .blank_before
        );
        assert!(
            !Message::KeyValues(vec![])
                .render(Symbols::UNICODE)
                .blank_before
        );
    }

    #[test]
    fn facts_align_their_labels() {
        let message = Message::KeyValues(vec![
            ("host".to_owned(), "127.0.0.1".to_owned()),
            ("site ID".to_owned(), "site-1".to_owned()),
        ]);

        assert_eq!(
            plain(&message, Symbols::UNICODE),
            ["  host     127.0.0.1", "  site ID  site-1"]
        );
        assert_eq!(
            message.render(Symbols::UNICODE).lines[0],
            [
                Piece {
                    role: Role::Label,
                    text: "  host     ".to_owned()
                },
                Piece {
                    role: Role::Accent,
                    text: "127.0.0.1".to_owned()
                },
            ]
        );
    }

    #[test]
    fn tables_pad_columns_and_leave_no_trailing_spaces() {
        let message = Message::Table {
            header: vec!["NAME".to_owned(), "MODELS".to_owned()],
            rows: vec![
                vec!["gpu1".to_owned(), "0".to_owned()],
                vec!["gb10-host-2".to_owned(), "12".to_owned()],
            ],
        };

        assert_eq!(
            plain(&message, Symbols::UNICODE),
            [
                "  NAME         MODELS",
                "  gpu1         0",
                "  gb10-host-2  12",
            ]
        );
    }

    #[test]
    fn next_steps_show_their_commands_under_the_step() {
        let message = Message::Next(vec![
            NextStep::new("Remove it in the console first, or"),
            NextStep::new("Drop it without cleaning up the node:")
                .with_command("dotc node remove gb10-host-2 --forget-deployments"),
        ]);

        assert_eq!(
            plain(&message, Symbols::UNICODE),
            [
                "Next",
                "  › Remove it in the console first, or",
                "  › Drop it without cleaning up the node:",
                "      dotc node remove gb10-host-2 --forget-deployments",
            ]
        );
        assert_eq!(
            plain(&message, Symbols::ASCII)[1],
            "  > Remove it in the console first, or"
        );
    }

    #[test]
    fn plain_lines_use_unicode_marks_whatever_the_environment() {
        assert_eq!(
            Message::Success("Added gb10-host-2".to_owned()).plain_lines(),
            ["✔ Added gb10-host-2"]
        );
    }
}
