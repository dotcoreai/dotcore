//! Semantic styles for the guided prompts of `dotc site init` and reset, and
//! the decision whether the controlling terminal gets colors at all.
//!
//! Colors depend only on the controlling terminal and the environment, never
//! on where process stdout goes: they are off when `NO_COLOR` is set to a
//! non-empty value, when the terminal type is dumb or unset, or when the
//! handle is not a terminal. With colors off every style is plain text.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::terminal::render::{RenderedLine, Role};

/// Semantic styles for prompts and messages; every style is plain when colors are off.
#[derive(Clone, Copy, Debug)]
pub(super) struct Theme {
    colors: bool,
}

impl Theme {
    /// Creates a theme that styles output only when `colors` is true.
    pub(super) fn new(colors: bool) -> Self {
        Self { colors }
    }

    /// Titles and the question of a prompt.
    pub(super) fn heading(&self) -> Style {
        self.styled(Style::new().add_modifier(Modifier::BOLD))
    }

    /// Selection cursor, chosen values, and emphasized values.
    pub(super) fn accent(&self) -> Style {
        self.styled(Style::new().fg(Color::Cyan))
    }

    /// Completed prompts and positive outcomes.
    pub(super) fn success(&self) -> Style {
        self.styled(Style::new().fg(Color::Green))
    }

    /// Degraded but acceptable outcomes.
    pub(super) fn warning(&self) -> Style {
        self.styled(Style::new().fg(Color::Yellow))
    }

    /// Cancelled prompts and rejected answers.
    pub(super) fn error(&self) -> Style {
        self.styled(Style::new().fg(Color::Red))
    }

    /// Secondary text such as key hints, descriptions, and defaults.
    pub(super) fn hint(&self) -> Style {
        self.styled(Style::new().add_modifier(Modifier::DIM))
    }

    /// Commands the operator copies.
    pub(super) fn command(&self) -> Style {
        self.styled(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    }

    /// Maps a rendered line to its styled form.
    pub(super) fn line(&self, line: &RenderedLine) -> Line<'static> {
        Line::from(
            line.iter()
                .map(|piece| Span::styled(piece.text.clone(), self.role(piece.role)))
                .collect::<Vec<_>>(),
        )
    }

    fn role(&self, role: Role) -> Style {
        match role {
            Role::Plain => Style::new(),
            Role::Heading => self.heading(),
            Role::Label | Role::Hint => self.hint(),
            Role::Accent => self.accent(),
            Role::Success => self.success(),
            Role::Warning => self.warning(),
            Role::Error => self.error(),
            Role::Command => self.command(),
        }
    }

    fn styled(&self, style: Style) -> Style {
        if self.colors { style } else { Style::new() }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };

    use super::Theme;
    use crate::terminal::{Message, render::Symbols};

    #[test]
    fn disabled_colors_leave_every_style_plain() {
        let theme = Theme::new(false);

        for style in [
            theme.heading(),
            theme.accent(),
            theme.success(),
            theme.warning(),
            theme.error(),
            theme.hint(),
            theme.command(),
        ] {
            assert_eq!(style, Style::new());
        }
    }

    #[test]
    fn rendered_lines_map_roles_to_semantic_styles() {
        let theme = Theme::new(true);
        let message = Message::Warning("Docker is unavailable".to_owned());

        assert_eq!(
            theme.line(&message.render(Symbols::UNICODE).lines[0]),
            Line::from(vec![Span::styled(
                "! Docker is unavailable",
                Style::new().fg(Color::Yellow)
            )])
        );
        let heading = Message::Heading("Initial site plan".to_owned());
        assert_eq!(
            theme.line(&heading.render(Symbols::UNICODE).lines[0]),
            Line::from(vec![Span::styled(
                "Initial site plan",
                Style::new().add_modifier(Modifier::BOLD)
            )])
        );
    }

    #[test]
    fn commands_are_indented_and_accented() {
        let message = Message::Command("dotcore".to_owned());
        let lines = message.render(Symbols::UNICODE).lines;

        assert_eq!(
            Theme::new(true).line(&lines[0]),
            Line::from(vec![Span::styled(
                "  dotcore",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            )])
        );
        assert_eq!(
            Theme::new(false).line(&lines[0]),
            Line::from(vec![Span::styled("  dotcore", Style::new())])
        );
    }

    #[test]
    fn facts_dim_their_labels_and_accent_their_values() {
        let theme = Theme::new(true);
        let dim = Style::new().add_modifier(Modifier::DIM);
        let cyan = Style::new().fg(Color::Cyan);
        let message = Message::KeyValues(vec![
            ("host".to_owned(), "127.0.0.1".to_owned()),
            ("site ID".to_owned(), "site-1".to_owned()),
        ]);

        let lines = message
            .render(Symbols::UNICODE)
            .lines
            .iter()
            .map(|line| theme.line(line))
            .collect::<Vec<_>>();

        assert_eq!(
            lines,
            [
                Line::from(vec![
                    Span::styled("  host     ", dim),
                    Span::styled("127.0.0.1", cyan),
                ]),
                Line::from(vec![
                    Span::styled("  site ID  ", dim),
                    Span::styled("site-1", cyan),
                ]),
            ]
        );
    }
}
