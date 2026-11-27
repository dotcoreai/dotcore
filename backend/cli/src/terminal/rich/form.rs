//! The arrow-key form view: one step's fields shown together, the focused one edited by typing,
//! with the caller's footer pinned at the bottom of the live area.
//!
//! The view tells the caller about edits once typing settles or the focus leaves a field, and
//! looks for the caller's results while they are on their way, so a slow answer never holds up
//! a key. Enter finishes the step, Esc or Shift-Tab goes back, and Ctrl-C cancels. On a short
//! terminal the help line goes first, then the key hints, then the footer shrinks to its last
//! line, then the fields scroll around the focused one.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    text::{Line, Span},
};

use super::{
    theme::Theme,
    view::{Step, View, tail},
};
use crate::terminal::{
    Footer, FormAnswer, FormField, FormPrompt, FormSession, Message, render::Symbols,
};

/// Columns given to a field's label, so values line up.
const LABEL_WIDTH: usize = 22;

/// Columns given to a field's value before what it accepts.
const VALUE_WIDTH: usize = 12;

/// An open form and the session that answers its edits.
pub(super) struct FormView<'a> {
    prompt: &'a mut FormPrompt,
    session: &'a mut dyn FormSession,
    symbols: Symbols,

    /// The field with the focus.
    focus: usize,

    /// How long typing pauses before the caller hears about it.
    settle: Duration,

    /// How often the view looks for the caller's results while they are on their way.
    refresh: Duration,

    /// When the latest unreported edit counts as settled.
    settle_at: Option<Instant>,

    /// When to look for the caller's results next.
    refresh_at: Option<Instant>,
}

impl<'a> FormView<'a> {
    /// Opens the form with its first field focused.
    pub(super) fn new(
        prompt: &'a mut FormPrompt,
        session: &'a mut dyn FormSession,
        symbols: Symbols,
        settle: Duration,
        refresh: Duration,
    ) -> Self {
        Self {
            prompt,
            session,
            symbols,
            focus: 0,
            settle,
            refresh,
            settle_at: None,
            refresh_at: None,
        }
    }

    /// Applies one key press at `now`.
    fn press(&mut self, key: KeyEvent, now: Instant) -> Step<FormAnswer> {
        match key.code {
            KeyCode::Enter => return self.finish(now),
            KeyCode::Esc if self.prompt.can_go_back => return Step::Done(FormAnswer::Back),
            KeyCode::Esc => return Step::Cancel,
            KeyCode::BackTab if self.prompt.can_go_back => return Step::Done(FormAnswer::Back),
            KeyCode::Up => self.move_focus(-1, now),
            KeyCode::Down => self.move_focus(1, now),
            KeyCode::Tab => {
                let hint = self.focused().and_then(|field| field.hint.clone());
                if let Some(hint) = hint {
                    self.edit(now, |field| field.value = hint.value);
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self
                    .focused()
                    .is_some_and(|field| !field.choices.is_empty()) =>
            {
                let step = if key.code == KeyCode::Left { -1 } else { 1 };
                self.edit(now, |field| cycle(field, step));
            }
            KeyCode::Char(character)
                if self.focused().is_some_and(|field| field.choices.is_empty()) =>
            {
                self.edit(now, |field| field.value.push(character));
            }
            KeyCode::Backspace => self.edit(now, |field| {
                field.value.pop();
            }),
            _ => {}
        }
        Step::Continue
    }

    /// Finishes the step unless a field is not accepted, which then takes the focus.
    fn finish(&mut self, now: Instant) -> Step<FormAnswer> {
        if self.settle_at.is_some() {
            self.report(now);
        }
        match self
            .prompt
            .fields
            .iter()
            .position(|field| field.error.is_some())
        {
            Some(index) => {
                self.focus = index;
                Step::Continue
            }
            None => Step::Done(FormAnswer::Next),
        }
    }

    fn move_focus(&mut self, step: isize, now: Instant) {
        if self.prompt.fields.is_empty() {
            return;
        }
        // Leaving a field reports its edit right away.
        if self.settle_at.is_some() {
            self.report(now);
        }
        let count = self.prompt.fields.len();
        self.focus = (self.focus as isize + step).rem_euclid(count as isize) as usize;
    }

    fn edit(&mut self, now: Instant, change: impl FnOnce(&mut FormField)) {
        let Some(field) = self.prompt.fields.get_mut(self.focus) else {
            return;
        };
        change(field);
        self.settle_at = Some(now + self.settle);
    }

    /// Tells the session about the edits so far and starts looking for its results.
    fn report(&mut self, now: Instant) {
        self.settle_at = None;
        self.session.edited(&mut self.prompt.fields);
        self.look_again(now);
    }

    fn look_again(&mut self, now: Instant) {
        self.refresh_at = self.session.pending().then(|| now + self.refresh);
    }

    fn focused(&self) -> Option<&FormField> {
        self.prompt.fields.get(self.focus)
    }

    fn footer(&self) -> Option<Footer> {
        self.session.footer()
    }

    /// Lays the form out in `rows` rows of `width` columns: the progress line and the fields on
    /// top, the footer and key hints at the bottom, dropping the least needed lines when short.
    fn layout(&self, rows: u16, width: u16, theme: &Theme) -> (Vec<Line<'static>>, usize) {
        let rows = usize::from(rows);
        let mut top = vec![self.progress_line(theme)];
        let fields = self.field_lines(width, theme);
        let notes = self.note_lines(theme);
        let help = self.help_line(theme);
        let keys = self.keys_line(theme);
        let footer = self.footer_lines(theme);

        let fixed = 1 + notes.len();
        let mut bottom = footer.clone();
        let mut extra = [help, Some(keys)];
        let wanted = |bottom: &Vec<Line<'static>>, extra: &[Option<Line<'static>>]| {
            fixed + fields.len() + bottom.len() + extra.iter().flatten().count()
        };
        // Drop the help line, then the key hints, then all but the footer's last line.
        if wanted(&bottom, &extra) > rows {
            extra[0] = None;
        }
        if wanted(&bottom, &extra) > rows {
            extra[1] = None;
        }
        if wanted(&bottom, &extra) > rows && bottom.len() > 1 {
            bottom = bottom.split_off(bottom.len() - 1);
        }

        let room = rows.saturating_sub(fixed + bottom.len() + extra.iter().flatten().count());
        let start = scroll_start(fields.len(), self.focus, room);
        // The focused field's row, below the progress line.
        let cursor_row = 1 + self.focus.saturating_sub(start);
        top.extend(fields.into_iter().skip(start).take(room));
        top.extend(notes);
        let [help, keys] = extra;
        top.extend(help);
        let mut lines = top;
        let used = lines.len() + bottom.len() + usize::from(keys.is_some());
        lines.extend(std::iter::repeat_n(
            Line::default(),
            rows.saturating_sub(used),
        ));
        lines.extend(bottom);
        lines.extend(keys);
        lines.truncate(rows);
        (lines, cursor_row)
    }

    fn progress_line(&self, theme: &Theme) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for (index, step) in self.prompt.steps.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            let (mark, mark_style, style) = match index.cmp(&self.prompt.current) {
                std::cmp::Ordering::Less => ("✔ ", theme.success(), ratatui::style::Style::new()),
                std::cmp::Ordering::Equal => ("● ", theme.accent(), theme.heading()),
                std::cmp::Ordering::Greater => ("○ ", theme.hint(), theme.hint()),
            };
            spans.push(Span::styled(mark, mark_style));
            spans.push(Span::styled(step.clone(), style));
        }
        Line::from(spans)
    }

    /// Each field's row, then under the focused one its error or its hint.
    fn field_lines(&self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (index, field) in self.prompt.fields.iter().enumerate() {
            let focused = index == self.focus;
            lines.push(self.field_line(field, focused, width, theme));
            if !focused {
                continue;
            }
            if let Some(error) = &field.error {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(format!("✘ {error}"), theme.error()),
                ]));
            } else if let Some(hint) = &field.hint {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(format!("⇥ {}", hint.text), theme.accent()),
                ]));
            }
        }
        lines
    }

    fn field_line(
        &self,
        field: &FormField,
        focused: bool,
        width: u16,
        theme: &Theme,
    ) -> Line<'static> {
        let mut spans = if focused {
            vec![
                Span::styled(" ❯ ", theme.accent()),
                Span::styled(pad(&field.label, LABEL_WIDTH), theme.heading()),
            ]
        } else {
            vec![Span::raw("   "), Span::raw(pad(&field.label, LABEL_WIDTH))]
        };
        let (value, style) = if field.value.is_empty() {
            (field.placeholder.clone().unwrap_or_default(), theme.hint())
        } else if field.secret {
            (
                "•".repeat(field.value.chars().count()),
                ratatui::style::Style::new(),
            )
        } else {
            (field.value.clone(), ratatui::style::Style::new())
        };
        let used: usize = spans.iter().map(Span::width).sum();
        let room = usize::from(width).saturating_sub(used + 1);
        let shown = tail(&value, room);
        spans.push(Span::styled(pad(&shown, VALUE_WIDTH), style));
        if !field.accepts.is_empty() {
            spans.push(Span::styled(format!(" {}", field.accepts), theme.hint()));
        }
        Line::from(spans)
    }

    fn note_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = self
            .prompt
            .notes
            .iter()
            .flat_map(|note| note.render(self.symbols).lines)
            .map(|line| {
                let mut line = theme.line(&line);
                line.spans.insert(0, Span::raw(" "));
                line
            })
            .collect::<Vec<_>>();
        if let Some(action) = &self.prompt.action {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(format!("[ {action} ]"), theme.accent()),
            ]));
        }
        lines
    }

    fn help_line(&self, theme: &Theme) -> Option<Line<'static>> {
        self.focused()
            .filter(|field| !field.summary.is_empty())
            .map(|field| {
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled(field.summary.clone(), theme.hint()),
                ])
            })
    }

    fn keys_line(&self, theme: &Theme) -> Line<'static> {
        let mut keys = Vec::new();
        if self.focused().is_some_and(|field| field.hint.is_some()) {
            keys.push("⇥ use hint");
        }
        if self.prompt.fields.len() > 1 {
            keys.push("↑/↓ field");
        }
        if self
            .focused()
            .is_some_and(|field| !field.choices.is_empty())
        {
            keys.push("←/→ change");
        } else if self.focused().is_some() {
            keys.push("type to edit");
        }
        let finish = match &self.prompt.action {
            Some(action) => format!("enter {}", action.to_lowercase()),
            None => "enter next".to_owned(),
        };
        let back = if self.prompt.can_go_back {
            "esc back"
        } else {
            "esc cancel"
        };
        let mut text = keys.join(" · ");
        for key in [finish.as_str(), back, "ctrl-c cancel"] {
            if !text.is_empty() {
                text.push_str(" · ");
            }
            text.push_str(key);
        }
        Line::from(Span::styled(format!(" {text}"), theme.hint()))
    }

    fn footer_lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let Some(footer) = self.footer() else {
            return Vec::new();
        };
        let mut title = vec![
            Span::styled(" ── ", theme.hint()),
            Span::styled(footer.title.clone(), theme.heading()),
            Span::styled(" ──", theme.hint()),
        ];
        if footer.updating {
            title.push(Span::styled(" updating…", theme.accent()));
        }
        let mut lines = vec![Line::from(title)];
        lines.extend(
            footer
                .lines
                .iter()
                .flat_map(|message| message.render(self.symbols).lines)
                .map(|line| {
                    let mut line = theme.line(&line);
                    line.spans.insert(0, Span::raw(" "));
                    line
                }),
        );
        lines
    }
}

impl View for FormView<'_> {
    type Output = FormAnswer;

    fn title(&self) -> &str {
        &self.prompt.title
    }

    fn height(&self, rows: u16) -> u16 {
        let footer = self
            .footer()
            .map_or(0, |footer| 1 + footer_rows(&footer, self.symbols));
        let notes: usize = self
            .prompt
            .notes
            .iter()
            .map(|note| note.render(self.symbols).lines.len())
            .sum::<usize>()
            + usize::from(self.prompt.action.is_some());
        // The progress line, each field, one row under the focused field, the notes, the help
        // line, the footer and the key hints.
        let wanted = 1 + self.prompt.fields.len() + 1 + notes + 1 + footer + 1;
        u16::try_from(wanted).unwrap_or(u16::MAX).min(rows)
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        let area = frame.area();
        let (lines, cursor_row) = self.layout(area.height, area.width, theme);
        for (row, line) in lines.into_iter().enumerate() {
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            frame.render_widget(line, Rect::new(area.x, area.y + row, area.width, 1));
        }
        if let Some(field) = self.focused()
            && field.choices.is_empty()
        {
            let value = if field.secret {
                "•".repeat(field.value.chars().count())
            } else {
                field.value.clone()
            };
            let room = usize::from(area.width).saturating_sub(3 + LABEL_WIDTH + 1);
            let column = 3 + LABEL_WIDTH + Span::raw(tail(&value, room)).width();
            let row = cursor_row.min(usize::from(area.height.saturating_sub(1)));
            let column = u16::try_from(column).unwrap_or(u16::MAX);
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(
                area.x + column.min(area.width.saturating_sub(1)),
                area.y + row,
            ));
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<FormAnswer> {
        self.press(key, Instant::now())
    }

    fn wake_at(&self) -> Option<Instant> {
        match (self.settle_at, self.refresh_at) {
            (Some(settle), Some(refresh)) => Some(settle.min(refresh)),
            (settle, refresh) => settle.or(refresh),
        }
    }

    fn on_wake(&mut self, now: Instant) -> Step<FormAnswer> {
        if self.settle_at.is_some_and(|at| at <= now) {
            self.report(now);
        }
        if self.refresh_at.is_some_and(|at| at <= now) {
            self.session.refresh(&mut self.prompt.fields, false);
            self.look_again(now);
        }
        Step::Continue
    }

    fn answer_text(&self, _output: &FormAnswer) -> String {
        String::new()
    }

    fn leaves_answer(&self) -> bool {
        false
    }
}

/// The rows a footer's lines take.
fn footer_rows(footer: &Footer, symbols: Symbols) -> usize {
    footer
        .lines
        .iter()
        .map(|message: &Message| message.render(symbols).lines.len())
        .sum()
}

/// The first of `count` field lines shown in `room` rows, keeping the focused field's row and the
/// one under it in view.
fn scroll_start(count: usize, focused: usize, room: usize) -> usize {
    if count <= room {
        return 0;
    }
    let last = (focused + 1).min(count - 1);
    (last + 1).saturating_sub(room)
}

/// Moves a choice field `step` choices along, wrapping around.
fn cycle(field: &mut FormField, step: isize) {
    let count = field.choices.len() as isize;
    let current = field
        .choices
        .iter()
        .position(|choice| choice == field.effective())
        .map_or(-1, |index| index as isize);
    let next = if current < 0 && step < 0 {
        count - 1
    } else {
        (current + step).rem_euclid(count)
    };
    field.value = field.choices[next as usize].clone();
}

fn pad(text: &str, width: usize) -> String {
    let used = Span::raw(text).width();
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    use super::FormView;
    use crate::terminal::{
        FieldHint, Footer, FormAnswer, FormField, FormPrompt, FormSession, Message,
        render::Symbols,
        rich::{
            theme::Theme,
            view::{Step, View},
        },
    };

    const SETTLE: Duration = Duration::from_millis(500);
    const REFRESH: Duration = Duration::from_millis(50);

    #[test]
    fn a_form_draws_its_steps_fields_hint_help_footer_and_keys() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        let mut view = form_view(&mut prompt, &mut session);
        view.focus = 1;

        let terminal = draw(&mut view, 72, 11);

        terminal.backend().assert_buffer_lines([
            " ✔ Model & machine  ● Context  ○ Review & deploy                        ",
            "   Parallel requests     4            1-64                              ",
            " ❯ Context per request   32768        4096-262144 tokens                ",
            "   ⇥ Fits all 4 at once: 49152                                          ",
            "   Total context         131072       follows the two above             ",
            "   What context per request does.                                       ",
            "                                                                        ",
            "                                                                        ",
            " ── gpu1 CUDA ──                                                        ",
            " ✔ Memory: fits now                                                     ",
            " ⇥ use hint · ↑/↓ field · type to edit · enter next · esc back · ctrl-c ",
        ]);
        assert_eq!(view.height(40), 9, "the view asks for the rows it draws");
    }

    #[test]
    fn the_footer_title_marks_newer_lines_on_their_way() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        session.footer.updating = true;
        let view = form_view(&mut prompt, &mut session);

        let (lines, _) = view.layout(9, 72, &Theme::new(false));

        assert_eq!(lines[6].to_string(), " ── gpu1 CUDA ── updating…");
        assert_eq!(lines[7].to_string(), " ✔ Memory: fits now");
    }

    #[test]
    fn a_short_terminal_drops_the_help_then_the_keys_then_the_footer_title() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        session.footer.lines.insert(
            0,
            Message::Info("GPU 0  16.0 GiB of 24.0 GiB · fits".to_owned()),
        );
        let mut view = form_view(&mut prompt, &mut session);
        let lines = |view: &mut FormView<'_>, rows| {
            let (lines, _) = view.layout(rows, 72, &Theme::new(false));
            lines.iter().map(ToString::to_string).collect::<Vec<_>>()
        };

        assert_eq!(view.height(40), 10, "every line fits on a tall terminal");
        assert_eq!(
            lines(&mut view, 8),
            [
                " ✔ Model & machine  ● Context  ○ Review & deploy",
                " ❯ Parallel requests     4            1-64",
                "   Context per request   32768        4096-262144 tokens",
                "   Total context         131072       follows the two above",
                " ── gpu1 CUDA ──",
                " GPU 0  16.0 GiB of 24.0 GiB · fits",
                " ✔ Memory: fits now",
                " ↑/↓ field · type to edit · enter next · esc back · ctrl-c cancel",
            ],
            "the help line goes first"
        );
        assert_eq!(
            lines(&mut view, 7),
            [
                " ✔ Model & machine  ● Context  ○ Review & deploy",
                " ❯ Parallel requests     4            1-64",
                "   Context per request   32768        4096-262144 tokens",
                "   Total context         131072       follows the two above",
                " ── gpu1 CUDA ──",
                " GPU 0  16.0 GiB of 24.0 GiB · fits",
                " ✔ Memory: fits now",
            ],
            "then the key hints"
        );
        assert_eq!(
            lines(&mut view, 5),
            [
                " ✔ Model & machine  ● Context  ○ Review & deploy",
                " ❯ Parallel requests     4            1-64",
                "   Context per request   32768        4096-262144 tokens",
                "   Total context         131072       follows the two above",
                " ✔ Memory: fits now",
            ],
            "then all but the footer's last line"
        );
        view.focus = 2;
        assert_eq!(
            lines(&mut view, 3),
            [
                " ✔ Model & machine  ● Context  ○ Review & deploy",
                " ❯ Total context         131072       follows the two above",
                " ✔ Memory: fits now",
            ],
            "then the fields scroll to the focused one"
        );
    }

    #[test]
    fn typing_reports_once_it_settles_and_leaving_a_field_reports_at_once() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        let reports = Rc::clone(&session.reports);
        let start = Instant::now();
        let mut view = form_view(&mut prompt, &mut session);

        for (offset, key) in [(0, KeyCode::Backspace), (100, KeyCode::Char('8'))] {
            assert_eq!(
                view.press(press(key), start + Duration::from_millis(offset)),
                Step::Continue
            );
        }
        assert_eq!(
            view.wake_at(),
            Some(start + Duration::from_millis(100) + SETTLE)
        );
        assert_eq!(
            view.on_wake(start + Duration::from_millis(400)),
            Step::Continue
        );
        assert!(reports.borrow().is_empty(), "typing has not settled yet");

        let settled = start + Duration::from_millis(100) + SETTLE;
        assert_eq!(view.on_wake(settled), Step::Continue);
        assert_eq!(*reports.borrow(), [["8", "32768", ""]]);
        assert_eq!(
            view.wake_at(),
            Some(settled + REFRESH),
            "the view looks for the answer on its way"
        );
        assert_eq!(view.on_wake(settled + REFRESH), Step::Continue);
        assert_eq!(view.wake_at(), None, "the answer arrived");

        view.press(press(KeyCode::Char('0')), settled + REFRESH);
        view.press(press(KeyCode::Down), settled + REFRESH);
        assert_eq!(*reports.borrow(), [["8", "32768", ""], ["80", "32768", ""]]);
        assert_eq!(view.focus, 1);
    }

    #[test]
    fn tab_takes_the_hint_and_enter_stays_on_a_value_not_accepted() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        let now = Instant::now();
        let mut view = form_view(&mut prompt, &mut session);

        view.press(press(KeyCode::Down), now);
        view.press(press(KeyCode::Tab), now);
        assert_eq!(view.prompt.fields[1].value, "49152");

        view.press(press(KeyCode::Up), now);
        view.press(press(KeyCode::Char('x')), now);
        view.press(press(KeyCode::Down), now);
        assert_eq!(view.press(press(KeyCode::Enter), now), Step::Continue);
        assert_eq!(view.focus, 0, "the value not accepted takes the focus");
        let terminal = draw(&mut view, 72, 11);
        assert_eq!(
            terminal.backend().buffer().content()[72 * 2..72 * 3]
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end(),
            "   ✘ Parallel requests expects a whole number"
        );

        view.press(press(KeyCode::Backspace), now);
        assert_eq!(
            view.press(press(KeyCode::Enter), now),
            Step::Done(FormAnswer::Next)
        );
    }

    #[test]
    fn esc_and_shift_tab_go_back_and_esc_cancels_without_a_previous_step() {
        let mut prompt = context_prompt();
        let mut session = FakeSession::default();
        let now = Instant::now();
        let mut view = form_view(&mut prompt, &mut session);

        assert_eq!(
            view.press(press(KeyCode::Esc), now),
            Step::Done(FormAnswer::Back)
        );
        assert_eq!(
            view.press(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), now),
            Step::Done(FormAnswer::Back)
        );
        view.prompt.can_go_back = false;
        assert_eq!(view.press(press(KeyCode::Esc), now), Step::Cancel);
        assert!(!view.leaves_answer(), "the caller reports the step");
    }

    #[test]
    fn a_choice_field_cycles_its_choices() {
        let mut prompt = FormPrompt {
            fields: vec![FormField {
                label: "Vision enabled".to_owned(),
                placeholder: Some("false".to_owned()),
                choices: vec!["true".to_owned(), "false".to_owned()],
                ..FormField::default()
            }],
            ..FormPrompt::default()
        };
        let mut session = FakeSession::default();
        let now = Instant::now();
        let mut view = form_view(&mut prompt, &mut session);

        view.press(press(KeyCode::Char('x')), now);
        assert_eq!(view.prompt.fields[0].value, "", "a choice is not typed");
        view.press(press(KeyCode::Right), now);
        assert_eq!(view.prompt.fields[0].value, "true");
        view.press(press(KeyCode::Char(' ')), now);
        assert_eq!(view.prompt.fields[0].value, "false");
        view.press(press(KeyCode::Left), now);
        assert_eq!(view.prompt.fields[0].value, "true");
    }

    /// The context step of a deploy: parallel requests, context per request with a hint, and a
    /// total context that follows them.
    fn context_prompt() -> FormPrompt {
        FormPrompt {
            title: "Context".to_owned(),
            steps: ["Model & machine", "Context", "Review & deploy"]
                .map(str::to_owned)
                .to_vec(),
            current: 1,
            fields: vec![
                FormField {
                    label: "Parallel requests".to_owned(),
                    placeholder: Some("4".to_owned()),
                    accepts: "1-64".to_owned(),
                    summary: "What parallel requests does.".to_owned(),
                    ..FormField::default()
                },
                FormField {
                    label: "Context per request".to_owned(),
                    value: "32768".to_owned(),
                    accepts: "4096-262144 tokens".to_owned(),
                    summary: "What context per request does.".to_owned(),
                    hint: Some(FieldHint {
                        text: "Fits all 4 at once: 49152".to_owned(),
                        value: "49152".to_owned(),
                    }),
                    ..FormField::default()
                },
                FormField {
                    label: "Total context".to_owned(),
                    placeholder: Some("131072".to_owned()),
                    accepts: "follows the two above".to_owned(),
                    ..FormField::default()
                },
            ],
            can_go_back: true,
            ..FormPrompt::default()
        }
    }

    fn form_view<'a>(prompt: &'a mut FormPrompt, session: &'a mut FakeSession) -> FormView<'a> {
        FormView::new(prompt, session, Symbols::UNICODE, SETTLE, REFRESH)
    }

    fn draw(view: &mut impl View, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| view.render(frame, &Theme::new(false)))
            .expect("draw form");
        terminal
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A session that records each report, refuses a parallel requests value that is not a
    /// number, answers on the first look after a report, and shows a fitting footer.
    struct FakeSession {
        reports: Rc<RefCell<Vec<Vec<String>>>>,
        pending: bool,
        footer: Footer,
    }

    impl Default for FakeSession {
        fn default() -> Self {
            Self {
                reports: Rc::default(),
                pending: false,
                footer: Footer {
                    title: "gpu1 CUDA".to_owned(),
                    updating: false,
                    lines: vec![Message::Success("Memory: fits now".to_owned())],
                },
            }
        }
    }

    impl FormSession for FakeSession {
        fn edited(&mut self, fields: &mut [FormField]) {
            self.reports
                .borrow_mut()
                .push(fields.iter().map(|field| field.value.clone()).collect());
            fields[0].error = fields[0]
                .value
                .parse::<u64>()
                .is_err()
                .then(|| "Parallel requests expects a whole number".to_owned())
                .filter(|_| !fields[0].value.is_empty());
            self.pending = true;
        }

        fn refresh(&mut self, _fields: &mut [FormField], _wait: bool) -> bool {
            std::mem::take(&mut self.pending)
        }

        fn pending(&self) -> bool {
            self.pending
        }

        fn footer(&self) -> Option<Footer> {
            Some(self.footer.clone())
        }
    }
}
