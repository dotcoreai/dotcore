//! The arrow-key prompt views of guided `dotc site init` and reset: a list
//! choice, a checklist, a yes/no question, a text answer, and a countdown, each
//! drawn in a few inline rows and driven one key press, or for the countdown one
//! tick, at a time.
//!
//! Views only draw into a frame and react to keys, so they are testable
//! without a terminal. Ctrl-C and Esc cancel every view, keys held with Ctrl
//! or Alt and stray escape sequences are ignored, and a finished prompt leaves
//! a single line such as `✔ Site host · 127.0.0.1` in the scrollback. Text too
//! long for the line shows its end, where the operator is typing.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Position,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::theme::Theme;
use crate::terminal::{
    ConfirmPrompt, CountdownPrompt, InputPrompt, MultiSelectPrompt, SelectItem, SelectPrompt,
};

/// Most item rows a choice shows at once; longer lists scroll.
const MAX_VISIBLE_ITEMS: u16 = 8;

/// One interactive prompt drawn in an inline viewport.
pub(super) trait View {
    /// The answer the view produces.
    type Output;

    /// Returns the question, reused by the one-line summary.
    fn title(&self) -> &str;

    /// Returns the viewport rows the view wants on a terminal with `rows` rows.
    fn height(&self, rows: u16) -> u16;

    /// Draws the view into the whole frame.
    fn render(&mut self, frame: &mut Frame, theme: &Theme);

    /// Applies one key press.
    fn on_key(&mut self, key: KeyEvent) -> Step<Self::Output>;

    /// Returns when the view next changes without a key press; `None` waits for keys only.
    fn wake_at(&self) -> Option<Instant> {
        None
    }

    /// Applies the passing of time up to `now`, once [`View::wake_at`] is reached.
    fn on_wake(&mut self, _now: Instant) -> Step<Self::Output> {
        Step::Continue
    }

    /// Returns the text of an answer for the summary line.
    fn answer_text(&self, output: &Self::Output) -> String;

    /// Whether an answer leaves its summary line; without one the view is erased and the caller
    /// reports the outcome.
    fn leaves_answer(&self) -> bool {
        true
    }
}

/// The effect of one terminal event on a view.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Step<T> {
    /// The prompt is still open.
    Continue,

    /// The operator cancelled the prompt.
    Cancel,

    /// The operator answered the prompt.
    Done(T),
}

/// Applies one terminal event: Ctrl-C cancels every view, key presses without modifiers or
/// with only Shift go to the view, and everything else, including other Ctrl and Alt
/// combinations and key releases, is ignored.
pub(super) fn handle_event<V: View>(view: &mut V, event: Event) -> Step<V::Output> {
    let Event::Key(key) = event else {
        return Step::Continue;
    };
    if key.kind != KeyEventKind::Press {
        return Step::Continue;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Step::Cancel;
    }
    // Shift only changes the typed character; any other modifier makes a shortcut the views
    // do not define, such as Ctrl-Y, which must not answer the prompt.
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return Step::Continue;
    }
    view.on_key(key)
}

/// The summary line left by an answered prompt, showing the end of an answer too long for
/// `width` columns.
pub(super) fn answer_line(theme: &Theme, title: &str, answer: &str, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::styled("✔ ", theme.success()),
        Span::styled(title.to_owned(), theme.heading()),
        Span::styled(" · ", theme.hint()),
    ];
    let room = usize::from(width).saturating_sub(spans.iter().map(Span::width).sum());
    spans.push(Span::styled(tail(answer, room), theme.accent()));
    Line::from(spans)
}

/// The summary line left by a cancelled prompt.
pub(super) fn cancelled_line(theme: &Theme, title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("✘ ", theme.error()),
        Span::styled(title.to_owned(), theme.heading()),
        Span::styled(" · cancelled", theme.hint()),
    ])
}

/// A list choice narrowed by typing, moved with the arrow keys and chosen with Enter.
///
/// Typed characters filter the items to those whose label or description contains them,
/// ignoring case; Backspace removes the last one. The cursor moves over the matching items only,
/// and the chosen item keeps its index in the whole list.
pub(super) struct SelectView<'a> {
    prompt: &'a SelectPrompt,

    /// The item under the cursor, by its index in the whole list.
    cursor: usize,

    /// The first shown match, by its position among the matches.
    offset: usize,

    filter: String,

    /// The indices of the items the filter matches, in list order.
    matches: Vec<usize>,
}

impl<'a> SelectView<'a> {
    /// Opens the choice with its default item under the cursor and every item shown.
    pub(super) fn new(prompt: &'a SelectPrompt) -> Self {
        Self {
            prompt,
            cursor: prompt.default(),
            offset: 0,
            filter: String::new(),
            matches: (0..prompt.items().len()).collect(),
        }
    }

    fn visible_items(&self, rows: u16) -> usize {
        // The question and the key hint take one row each; keep at least one item row. The
        // rows stay the same while the filter narrows the list, since the viewport is fixed.
        let room = rows.saturating_sub(2).clamp(1, MAX_VISIBLE_ITEMS);
        usize::from(room).min(self.prompt.items().len())
    }

    /// The cursor's position among the matches, none when no item matches.
    fn position(&self) -> Option<usize> {
        self.matches.iter().position(|index| *index == self.cursor)
    }

    fn scroll_to_cursor(&mut self, visible: usize) {
        let position = self.position().unwrap_or(0);
        if position < self.offset {
            self.offset = position;
        } else if position >= self.offset + visible {
            self.offset = position + 1 - visible;
        }
    }

    /// Narrows the items to the filter, keeping the cursor on its item when it still matches,
    /// else on the first match.
    fn refilter(&mut self) {
        self.matches = self
            .prompt
            .items()
            .iter()
            .enumerate()
            .filter(|(_, item)| item.matches(&self.filter))
            .map(|(index, _)| index)
            .collect();
        if self.position().is_none()
            && let Some(first) = self.matches.first()
        {
            self.cursor = *first;
        }
        self.offset = 0;
    }

    fn item_line(
        &self,
        index: usize,
        mark: &'static str,
        label_width: usize,
        theme: &Theme,
    ) -> Line<'static> {
        let item = &self.prompt.items()[index];
        let selected = index == self.cursor;
        let label_style = match (selected, item.unavailable_reason()) {
            (true, _) => theme.accent(),
            (false, Some(_)) => theme.hint(),
            (false, None) => Style::new(),
        };
        let padding = if has_details(item) {
            label_width.saturating_sub(Span::raw(item.label()).width())
        } else {
            0
        };
        let mut spans = vec![
            Span::styled(if selected { "❯ " } else { "  " }, theme.accent()),
            Span::styled(mark, label_style),
            Span::styled(
                format!("{}{}", item.label(), " ".repeat(padding)),
                label_style,
            ),
        ];
        if let Some(description) = item.description() {
            spans.push(Span::styled(format!("  {description}"), theme.hint()));
        }
        if let Some(reason) = item.unavailable_reason() {
            spans.push(Span::styled(
                format!("  unavailable: {reason}"),
                theme.hint(),
            ));
        }
        Line::from(spans)
    }

    /// Draws the question with the filter, the visible matches with their `mark`, and the key
    /// hint naming `action`.
    fn draw(
        &mut self,
        frame: &mut Frame,
        theme: &Theme,
        mark: impl Fn(usize) -> &'static str,
        action: &str,
    ) {
        let area = frame.area();
        let visible = self.visible_items(area.height);
        self.scroll_to_cursor(visible);
        // Only rows with details are aligned, so a long bare row does not push the column out.
        let label_width = self
            .prompt
            .items()
            .iter()
            .filter(|item| has_details(item))
            .map(|item| Span::raw(item.label()).width())
            .max()
            .unwrap_or(0);

        let mut title = vec![
            Span::styled("? ", theme.accent()),
            Span::styled(self.prompt.title().to_owned(), theme.heading()),
        ];
        if !self.filter.is_empty() {
            title.push(Span::styled("  filter: ", theme.hint()));
            title.push(Span::styled(self.filter.clone(), theme.accent()));
        }
        let mut lines = vec![Line::from(title)];
        if self.matches.is_empty() {
            lines.push(Line::styled("  no match", theme.hint()));
        }
        lines.extend(
            self.matches
                .iter()
                .skip(self.offset)
                .take(visible)
                .map(|index| self.item_line(*index, mark(*index), label_width, theme)),
        );
        // Rows the filter emptied stay blank, so the hint keeps its row.
        lines.resize(visible + 1, Line::default());
        let position = if self.matches.len() > visible {
            format!(
                "  ({}/{}) ",
                self.position().map_or(0, |position| position + 1),
                self.matches.len()
            )
        } else {
            "  ".to_owned()
        };
        lines.push(Line::styled(
            format!("{position}↑/↓ move · {action} · esc cancel"),
            theme.hint(),
        ));
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Moves the cursor among the matches for an arrow, Home or End key, wrapping at both ends.
    fn move_cursor(&mut self, code: KeyCode) {
        let (Some(position), Some(last)) = (self.position(), self.matches.len().checked_sub(1))
        else {
            return;
        };
        let position = match code {
            KeyCode::Up => position.checked_sub(1).unwrap_or(last),
            KeyCode::Down if position >= last => 0,
            KeyCode::Down => position + 1,
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => position,
        };
        self.cursor = self.matches[position];
    }
}

impl View for SelectView<'_> {
    type Output = usize;

    fn title(&self) -> &str {
        self.prompt.title()
    }

    fn height(&self, rows: u16) -> u16 {
        // Bounded by the maximum visible rows, so the conversion cannot truncate.
        self.visible_items(rows) as u16 + 2
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        self.draw(frame, theme, |_| "", "type to filter · enter select");
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<usize> {
        match key.code {
            KeyCode::Enter if self.matches.is_empty() => Step::Continue,
            KeyCode::Enter => Step::Done(self.cursor),
            KeyCode::Esc => Step::Cancel,
            KeyCode::Char(character) => {
                self.filter.push(character);
                self.refilter();
                Step::Continue
            }
            KeyCode::Backspace => {
                if self.filter.pop().is_some() {
                    self.refilter();
                }
                Step::Continue
            }
            code => {
                self.move_cursor(code);
                Step::Continue
            }
        }
    }

    fn answer_text(&self, output: &usize) -> String {
        self.prompt.items()[*output].label().to_owned()
    }
}

/// A checklist moved with the arrow keys (or j/k), toggled with Space and confirmed with Enter;
/// it has no filter.
pub(super) struct MultiSelectView<'a> {
    prompt: &'a MultiSelectPrompt,
    list: SelectView<'a>,
    checked: BTreeSet<usize>,
}

impl<'a> MultiSelectView<'a> {
    /// Opens the checklist with its preselection checked and the cursor on the first item.
    pub(super) fn new(prompt: &'a MultiSelectPrompt) -> Self {
        Self {
            prompt,
            list: SelectView::new(&prompt.list),
            checked: prompt.checked().clone(),
        }
    }
}

impl View for MultiSelectView<'_> {
    type Output = BTreeSet<usize>;

    fn title(&self) -> &str {
        self.prompt.title()
    }

    fn height(&self, rows: u16) -> u16 {
        self.list.height(rows)
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        let checked = &self.checked;
        self.list.draw(
            frame,
            theme,
            |index| {
                if checked.contains(&index) {
                    "[x] "
                } else {
                    "[ ] "
                }
            },
            "space toggle · enter confirm",
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<BTreeSet<usize>> {
        match key.code {
            KeyCode::Char(' ') => {
                let cursor = self.list.cursor;
                // Unavailable items stay unchecked; their reason is already on the row.
                if self.prompt.is_checkable(cursor) && !self.checked.remove(&cursor) {
                    self.checked.insert(cursor);
                }
                Step::Continue
            }
            KeyCode::Enter => Step::Done(self.checked.clone()),
            KeyCode::Esc => Step::Cancel,
            KeyCode::Char('k') => {
                self.list.move_cursor(KeyCode::Up);
                Step::Continue
            }
            KeyCode::Char('j') => {
                self.list.move_cursor(KeyCode::Down);
                Step::Continue
            }
            code => {
                self.list.move_cursor(code);
                Step::Continue
            }
        }
    }

    fn answer_text(&self, output: &BTreeSet<usize>) -> String {
        self.prompt.answer_text(output)
    }
}

/// Reports whether an item shows text after its label.
fn has_details(item: &SelectItem) -> bool {
    item.description().is_some() || item.unavailable_reason().is_some()
}

/// A yes/no question answered with y, n, or Enter for the default.
pub(super) struct ConfirmView<'a> {
    prompt: &'a ConfirmPrompt,
}

impl<'a> ConfirmView<'a> {
    /// Opens the question.
    pub(super) fn new(prompt: &'a ConfirmPrompt) -> Self {
        Self { prompt }
    }
}

impl View for ConfirmView<'_> {
    type Output = bool;

    fn title(&self) -> &str {
        self.prompt.title()
    }

    fn height(&self, _rows: u16) -> u16 {
        1
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        let choices = if self.prompt.default() {
            " (Y/n)"
        } else {
            " (y/N)"
        };
        let line = Line::from(vec![
            Span::styled("? ", theme.accent()),
            Span::styled(self.prompt.title().to_owned(), theme.heading()),
            Span::styled(choices, theme.hint()),
        ]);
        frame.render_widget(line, frame.area());
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<bool> {
        match key.code {
            KeyCode::Char('y' | 'Y') => Step::Done(true),
            KeyCode::Char('n' | 'N') => Step::Done(false),
            KeyCode::Enter => Step::Done(self.prompt.default()),
            KeyCode::Esc => Step::Cancel,
            _ => Step::Continue,
        }
    }

    fn answer_text(&self, output: &bool) -> String {
        if *output { "yes" } else { "no" }.to_owned()
    }
}

/// A single-line text answer; Enter on an empty answer accepts the default.
pub(super) struct InputView<'a> {
    prompt: &'a InputPrompt,
    value: String,
}

impl<'a> InputView<'a> {
    /// Opens the question with an empty answer.
    pub(super) fn new(prompt: &'a InputPrompt) -> Self {
        Self {
            prompt,
            value: String::new(),
        }
    }
}

impl View for InputView<'_> {
    type Output = String;

    fn title(&self) -> &str {
        self.prompt.title()
    }

    fn height(&self, _rows: u16) -> u16 {
        1
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        let mut spans = vec![
            Span::styled("? ", theme.accent()),
            Span::styled(self.prompt.title().to_owned(), theme.heading()),
            Span::styled(" › ", theme.hint()),
        ];
        let prefix_width: usize = spans.iter().map(Span::width).sum();
        let area = frame.area();
        // The last column stays free for the cursor after the text.
        let room = usize::from(area.width).saturating_sub(prefix_width + 1);
        let shown = tail(&self.value, room);
        let column = prefix_width + Span::raw(shown.as_str()).width();
        if self.value.is_empty() {
            spans.push(Span::styled(
                self.prompt.default().unwrap_or_default().to_owned(),
                theme.hint(),
            ));
        } else {
            spans.push(Span::raw(shown));
        }

        frame.render_widget(Line::from(spans), area);
        let column = u16::try_from(column).unwrap_or(u16::MAX);
        frame.set_cursor_position(Position::new(
            area.x + column.min(area.width.saturating_sub(1)),
            area.y,
        ));
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<String> {
        match key.code {
            KeyCode::Char(character) => self.value.push(character),
            KeyCode::Backspace => {
                self.value.pop();
            }
            KeyCode::Enter => return Step::Done(self.prompt.resolve(self.value.clone())),
            KeyCode::Esc => return Step::Cancel,
            _ => {}
        }
        Step::Continue
    }

    fn answer_text(&self, output: &String) -> String {
        output.clone()
    }
}

/// A countdown before an action: Enter goes ahead right away, Esc cancels, and the action goes
/// ahead by itself when the time runs out. The remaining whole seconds update once a second.
pub(super) struct CountdownView<'a> {
    prompt: &'a CountdownPrompt,
    deadline: Instant,
    now: Instant,
}

impl<'a> CountdownView<'a> {
    /// Starts the countdown at `now`.
    pub(super) fn new(prompt: &'a CountdownPrompt, now: Instant) -> Self {
        Self {
            prompt,
            deadline: now + prompt.duration(),
            now,
        }
    }

    /// Returns the time left, which the view shows rounded up to whole seconds.
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(self.now)
    }
}

impl View for CountdownView<'_> {
    type Output = bool;

    fn title(&self) -> &str {
        self.prompt.title()
    }

    fn height(&self, _rows: u16) -> u16 {
        1
    }

    fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        let remaining = self.remaining();
        let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
        let line = Line::from(vec![
            Span::styled("? ", theme.accent()),
            Span::styled(self.prompt.title().to_owned(), theme.heading()),
            Span::styled(format!(" in {seconds}s"), theme.accent()),
            Span::styled(" · enter start now · esc skip", theme.hint()),
        ]);
        frame.render_widget(line, frame.area());
    }

    fn on_key(&mut self, key: KeyEvent) -> Step<bool> {
        match key.code {
            KeyCode::Enter => Step::Done(true),
            KeyCode::Esc => Step::Cancel,
            _ => Step::Continue,
        }
    }

    fn wake_at(&self) -> Option<Instant> {
        // The shown seconds change when the time left crosses a whole second.
        let fraction = Duration::from_nanos(u64::from(self.remaining().subsec_nanos()));
        let step = if fraction.is_zero() {
            Duration::from_secs(1)
        } else {
            fraction
        };
        Some((self.now + step).min(self.deadline))
    }

    fn on_wake(&mut self, now: Instant) -> Step<bool> {
        self.now = now;
        if self.remaining().is_zero() {
            Step::Done(true)
        } else {
            Step::Continue
        }
    }

    fn answer_text(&self, _output: &bool) -> String {
        "starting".to_owned()
    }
}

/// Returns the end of `text` that fits in `width` columns, led by `…` when the start is cut.
pub(super) fn tail(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
        return text.to_owned();
    }
    let Some(mut room) = width.checked_sub(1) else {
        return String::new();
    };

    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_width = Span::raw(character.encode_utf8(&mut [0; 4]) as &str).width();
        if character_width > room {
            break;
        }
        room -= character_width;
        start = index;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Position, style::Color};

    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    use super::{
        ConfirmView, CountdownView, InputView, MultiSelectView, SelectView, Step, View,
        answer_line, cancelled_line, handle_event,
    };
    use crate::terminal::{
        ConfirmPrompt, CountdownPrompt, InputPrompt, MultiSelectPrompt, SelectItem, SelectPrompt,
        rich::theme::Theme,
    };

    #[test]
    fn select_draws_items_descriptions_reasons_and_key_hints() {
        let prompt = host_prompt();
        let mut view = SelectView::new(&prompt);

        let terminal = draw(&mut view, Theme::new(false), 60, 5);

        terminal.backend().assert_buffer_lines([
            "? Site host                                                 ",
            "❯ 127.0.0.1   lo  loopback                                  ",
            "  192.0.2.10  en0                                           ",
            "  10.0.0.1    wg0  unavailable: not reachable               ",
            "  ↑/↓ move · type to filter · enter select · esc cancel    ",
        ]);
    }

    #[test]
    fn multi_select_draws_check_marks_and_its_key_hints() {
        let prompt = accelerator_prompt();
        let mut view = MultiSelectView::new(&prompt);

        let terminal = draw(&mut view, Theme::new(false), 60, 5);

        terminal.backend().assert_buffer_lines([
            "? Inference                                                 ",
            "❯ [x] GPU 0      nvidia                                     ",
            "  [ ] apple      unavailable: detected but unusable         ",
            "  [ ] simulated  development only                           ",
            "  ↑/↓ move · space toggle · enter confirm · esc cancel      ",
        ]);
    }

    #[test]
    fn multi_select_toggles_available_items_and_confirms_with_enter() {
        let prompt = accelerator_prompt();
        let mut view = MultiSelectView::new(&prompt);

        assert_eq!(
            handle_event(&mut view, key(KeyCode::Char(' '))),
            Step::Continue
        );
        assert_eq!(handle_event(&mut view, key(KeyCode::Down)), Step::Continue);
        // The unavailable item stays unchecked.
        assert_eq!(
            handle_event(&mut view, key(KeyCode::Char(' '))),
            Step::Continue
        );
        assert_eq!(handle_event(&mut view, key(KeyCode::End)), Step::Continue);
        assert_eq!(
            handle_event(&mut view, key(KeyCode::Char(' '))),
            Step::Continue
        );
        let Step::Done(checked) = handle_event(&mut view, key(KeyCode::Enter)) else {
            panic!("enter confirms the checklist")
        };

        assert_eq!(checked, BTreeSet::from([2]));
        assert_eq!(view.answer_text(&checked), "simulated");
        assert_eq!(view.answer_text(&BTreeSet::new()), "none");
        assert_eq!(
            handle_event(&mut MultiSelectView::new(&prompt), key(KeyCode::Enter)),
            Step::Done(BTreeSet::from([0]))
        );
    }

    #[test]
    fn select_aligns_descriptions_without_padding_for_bare_items() {
        let prompt = SelectPrompt::new(
            "Site host",
            [
                SelectItem::new("127.0.0.1").with_description("lo  loopback"),
                SelectItem::new("192.0.2.10").with_description("en0"),
                SelectItem::new("Enter another IPv4 address…"),
            ],
        );
        let mut view = SelectView::new(&prompt);

        let terminal = draw(&mut view, Theme::new(false), 40, 5);

        terminal.backend().assert_buffer_lines([
            "? Site host                             ",
            "❯ 127.0.0.1   lo  loopback              ",
            "  192.0.2.10  en0                       ",
            "  Enter another IPv4 address…           ",
            "  ↑/↓ move · type to filter · enter sele",
        ]);
    }

    #[test]
    fn select_styles_the_cursor_row_with_the_accent_color() {
        let prompt = host_prompt();
        let mut view = SelectView::new(&prompt);

        let terminal = draw(&mut view, Theme::new(true), 60, 5);

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(2, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(2, 2)].fg, Color::Reset);
    }

    #[test]
    fn select_moves_wraps_and_chooses_with_enter() {
        let prompt = host_prompt();
        let mut view = SelectView::new(&prompt);

        assert_eq!(handle_event(&mut view, key(KeyCode::Up)), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::Enter)), Step::Done(2));
        assert_eq!(handle_event(&mut view, key(KeyCode::Down)), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::Down)), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::Enter)), Step::Done(1));
        assert_eq!(view.answer_text(&1), "192.0.2.10");
    }

    #[test]
    fn select_filters_by_label_or_description_and_keeps_the_original_index() {
        let prompt = host_prompt();
        let mut view = SelectView::new(&prompt);

        for character in ['W', 'g'] {
            assert_eq!(
                handle_event(&mut view, key(KeyCode::Char(character))),
                Step::Continue
            );
        }
        let terminal = draw(&mut view, Theme::new(false), 60, 5);

        terminal.backend().assert_buffer_lines([
            "? Site host  filter: Wg                                     ",
            "❯ 10.0.0.1    wg0  unavailable: not reachable               ",
            "                                                            ",
            "                                                            ",
            "  ↑/↓ move · type to filter · enter select · esc cancel    ",
        ]);
        assert_eq!(handle_event(&mut view, key(KeyCode::Down)), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::Enter)), Step::Done(2));
    }

    #[test]
    fn select_without_a_match_says_so_until_backspace_widens_the_filter() {
        let prompt = host_prompt();
        let mut view = SelectView::new(&prompt);

        for character in ['1', 'j'] {
            handle_event(&mut view, key(KeyCode::Char(character)));
        }
        let terminal = draw(&mut view, Theme::new(false), 60, 5);

        terminal.backend().assert_buffer_lines([
            "? Site host  filter: 1j                                     ",
            "  no match                                                  ",
            "                                                            ",
            "                                                            ",
            "  ↑/↓ move · type to filter · enter select · esc cancel    ",
        ]);
        assert_eq!(handle_event(&mut view, key(KeyCode::Enter)), Step::Continue);

        assert_eq!(
            handle_event(&mut view, key(KeyCode::Backspace)),
            Step::Continue
        );
        let terminal = draw(&mut view, Theme::new(false), 60, 5);
        terminal.backend().assert_buffer_lines([
            "? Site host  filter: 1                                      ",
            "❯ 127.0.0.1   lo  loopback                                  ",
            "  192.0.2.10  en0                                           ",
            "  10.0.0.1    wg0  unavailable: not reachable               ",
            "  ↑/↓ move · type to filter · enter select · esc cancel    ",
        ]);
        assert_eq!(handle_event(&mut view, key(KeyCode::End)), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::Enter)), Step::Done(2));
    }

    #[test]
    fn multi_select_still_moves_with_j_and_k() {
        let prompt = accelerator_prompt();
        let mut view = MultiSelectView::new(&prompt);

        handle_event(&mut view, key(KeyCode::Char('j')));
        handle_event(&mut view, key(KeyCode::Char('j')));
        handle_event(&mut view, key(KeyCode::Char('k')));
        handle_event(&mut view, key(KeyCode::Char('j')));
        handle_event(&mut view, key(KeyCode::Char(' ')));

        assert_eq!(
            handle_event(&mut view, key(KeyCode::Enter)),
            Step::Done(BTreeSet::from([0, 2]))
        );
    }

    #[test]
    fn select_scrolls_a_long_list_with_a_position_indicator() {
        let items = (0..20).map(|index| SelectItem::new(format!("10.0.0.{index}")));
        let prompt = SelectPrompt::new("Site host", items).with_default(19);
        let mut view = SelectView::new(&prompt);

        assert_eq!(view.height(6), 6);
        let terminal = draw(&mut view, Theme::new(false), 50, 6);

        terminal.backend().assert_buffer_lines([
            "? Site host                                       ",
            "  10.0.0.16                                       ",
            "  10.0.0.17                                       ",
            "  10.0.0.18                                       ",
            "❯ 10.0.0.19                                       ",
            "  (20/20) ↑/↓ move · type to filter · enter select",
        ]);
    }

    #[test]
    fn ctrl_c_and_esc_cancel_every_view() {
        let select = host_prompt();
        let confirm = ConfirmPrompt::new("Write?", false);
        let input = InputPrompt::new("Site directory");
        let ctrl_c = || Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(
            handle_event(&mut SelectView::new(&select), ctrl_c()),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut ConfirmView::new(&confirm), ctrl_c()),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut InputView::new(&input), ctrl_c()),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut SelectView::new(&select), key(KeyCode::Esc)),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut ConfirmView::new(&confirm), key(KeyCode::Esc)),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut InputView::new(&input), key(KeyCode::Esc)),
            Step::Cancel
        );
    }

    #[test]
    fn non_key_events_and_key_releases_are_ignored() {
        let prompt = ConfirmPrompt::new("Write?", true);
        let mut view = ConfirmView::new(&prompt);
        let release = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };

        assert_eq!(handle_event(&mut view, Event::Key(release)), Step::Continue);
        assert_eq!(handle_event(&mut view, Event::FocusGained), Step::Continue);
        assert_eq!(handle_event(&mut view, key(KeyCode::F(1))), Step::Continue);
        assert_eq!(
            handle_event(&mut view, key(KeyCode::Enter)),
            Step::Done(true)
        );
    }

    #[test]
    fn confirm_shows_its_default_and_answers_y_n_or_enter() {
        let prompt = ConfirmPrompt::new("Write the site?", false);
        let mut view = ConfirmView::new(&prompt);

        let terminal = draw(&mut view, Theme::new(false), 30, 1);

        terminal
            .backend()
            .assert_buffer_lines(["? Write the site? (y/N)       "]);
        assert_eq!(view.on_key(key_event(KeyCode::Char('Y'))), Step::Done(true));
        assert_eq!(
            view.on_key(key_event(KeyCode::Char('n'))),
            Step::Done(false)
        );
        assert_eq!(view.on_key(key_event(KeyCode::Enter)), Step::Done(false));
        assert_eq!(view.on_key(key_event(KeyCode::Char('x'))), Step::Continue);
        assert_eq!(view.answer_text(&false), "no");
    }

    #[test]
    fn input_edits_text_places_the_cursor_and_falls_back_to_the_default() {
        let prompt = InputPrompt::new("Host").with_default("127.0.0.1");
        let mut view = InputView::new(&prompt);

        let mut terminal = draw(&mut view, Theme::new(false), 30, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Host › 127.0.0.1            "]);
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(9, 0));

        for code in [
            KeyCode::Char('1'),
            KeyCode::Char('0'),
            KeyCode::Char('x'),
            KeyCode::Backspace,
        ] {
            assert_eq!(view.on_key(key_event(code)), Step::Continue);
        }
        let mut terminal = draw(&mut view, Theme::new(false), 30, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Host › 10                   "]);
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(11, 0));
        assert_eq!(
            view.on_key(key_event(KeyCode::Enter)),
            Step::Done("10".to_owned())
        );

        let mut empty = InputView::new(&prompt);
        assert_eq!(
            empty.on_key(key_event(KeyCode::Enter)),
            Step::Done("127.0.0.1".to_owned())
        );
    }

    #[test]
    fn modified_keys_neither_answer_nor_move_nor_type() {
        let confirm = ConfirmPrompt::new("Write?", false);
        let mut confirm_view = ConfirmView::new(&confirm);
        for event in [
            modified(KeyCode::Char('y'), KeyModifiers::CONTROL),
            modified(KeyCode::Char('n'), KeyModifiers::CONTROL),
            modified(KeyCode::Char('y'), KeyModifiers::ALT),
            modified(KeyCode::Enter, KeyModifiers::ALT),
        ] {
            assert_eq!(handle_event(&mut confirm_view, event), Step::Continue);
        }
        assert_eq!(
            handle_event(
                &mut confirm_view,
                modified(KeyCode::Char('Y'), KeyModifiers::SHIFT)
            ),
            Step::Done(true)
        );

        let select = host_prompt();
        let mut select_view = SelectView::new(&select);
        for event in [
            modified(KeyCode::Char('j'), KeyModifiers::CONTROL),
            modified(KeyCode::Char('k'), KeyModifiers::CONTROL),
            modified(KeyCode::Char('j'), KeyModifiers::ALT),
            modified(KeyCode::Down, KeyModifiers::CONTROL),
        ] {
            assert_eq!(handle_event(&mut select_view, event), Step::Continue);
        }
        assert_eq!(
            handle_event(&mut select_view, key(KeyCode::Enter)),
            Step::Done(0)
        );

        let input = InputPrompt::new("Site directory");
        let mut input_view = InputView::new(&input);
        for event in [
            modified(KeyCode::Char('x'), KeyModifiers::ALT),
            modified(KeyCode::Char('u'), KeyModifiers::CONTROL),
            modified(KeyCode::Char('S'), KeyModifiers::SHIFT),
            key(KeyCode::Char('r')),
            modified(KeyCode::Backspace, KeyModifiers::ALT),
            modified(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            assert_eq!(handle_event(&mut input_view, event), Step::Continue);
        }
        assert_eq!(
            handle_event(&mut input_view, key(KeyCode::Enter)),
            Step::Done("Sr".to_owned())
        );
    }

    #[test]
    fn long_input_shows_its_tail_with_the_cursor_at_the_end() {
        let prompt = InputPrompt::new("Site directory");
        let mut view = InputView::new(&prompt);
        for character in "/srv/dotcore/sites/production".chars() {
            assert_eq!(
                handle_event(&mut view, key(KeyCode::Char(character))),
                Step::Continue
            );
        }

        let mut terminal = draw(&mut view, Theme::new(false), 30, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Site directory › …roduction "]);
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(29, 0));

        for _ in 0..3 {
            handle_event(&mut view, key(KeyCode::Backspace));
        }
        let mut terminal = draw(&mut view, Theme::new(false), 30, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Site directory › …s/product "]);
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(29, 0));

        for _ in 0..16 {
            handle_event(&mut view, key(KeyCode::Backspace));
        }
        let mut terminal = draw(&mut view, Theme::new(false), 30, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Site directory › /srv/dotco "]);
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(29, 0));
    }

    #[test]
    fn a_countdown_shows_whole_seconds_left_and_goes_ahead_when_time_runs_out() {
        let prompt = CountdownPrompt::new("Start the node", Duration::from_secs(5));
        let start = Instant::now();
        let mut view = CountdownView::new(&prompt, start);

        let terminal = draw(&mut view, Theme::new(false), 60, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Start the node in 5s · enter start now · esc skip         "]);
        assert_eq!(view.wake_at(), Some(start + Duration::from_secs(1)));

        assert_eq!(
            view.on_wake(start + Duration::from_millis(1_500)),
            Step::Continue
        );
        let terminal = draw(&mut view, Theme::new(false), 60, 1);
        terminal
            .backend()
            .assert_buffer_lines(["? Start the node in 4s · enter start now · esc skip         "]);
        // The next change is when 3.5 s left becomes 3 s.
        assert_eq!(view.wake_at(), Some(start + Duration::from_secs(2)));

        assert_eq!(
            view.on_wake(start + Duration::from_millis(4_800)),
            Step::Continue
        );
        assert_eq!(view.wake_at(), Some(start + Duration::from_secs(5)));
        assert_eq!(
            view.on_wake(start + Duration::from_secs(5)),
            Step::Done(true)
        );
        assert_eq!(view.answer_text(&true), "starting");
    }

    #[test]
    fn a_countdown_goes_ahead_on_enter_and_cancels_on_esc_or_ctrl_c() {
        let prompt = CountdownPrompt::new("Start the node", Duration::from_secs(5));
        let start = Instant::now();
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(
            handle_event(&mut CountdownView::new(&prompt, start), key(KeyCode::Enter)),
            Step::Done(true)
        );
        assert_eq!(
            handle_event(&mut CountdownView::new(&prompt, start), key(KeyCode::Esc)),
            Step::Cancel
        );
        assert_eq!(
            handle_event(&mut CountdownView::new(&prompt, start), ctrl_c),
            Step::Cancel
        );
        assert_eq!(
            handle_event(
                &mut CountdownView::new(&prompt, start),
                key(KeyCode::Char('y'))
            ),
            Step::Continue
        );
    }

    #[test]
    fn summaries_leave_one_answer_or_cancel_line() {
        let theme = Theme::new(false);

        assert_eq!(
            answer_line(&theme, "Site host", "127.0.0.1", 80).to_string(),
            "✔ Site host · 127.0.0.1"
        );
        assert_eq!(
            answer_line(
                &theme,
                "Site directory",
                "/srv/dotcore/sites/production",
                30
            )
            .to_string(),
            "✔ Site directory · …production"
        );
        assert_eq!(
            cancelled_line(&theme, "Site host").to_string(),
            "✘ Site host · cancelled"
        );
    }

    fn draw(view: &mut impl View, theme: Theme, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| view.render(frame, &theme))
            .expect("draw view");
        terminal
    }

    fn accelerator_prompt() -> MultiSelectPrompt {
        MultiSelectPrompt::new(
            "Inference",
            [
                SelectItem::new("GPU 0").with_description("nvidia"),
                SelectItem::new("apple").with_unavailable_reason("detected but unusable"),
                SelectItem::new("simulated").with_description("development only"),
            ],
        )
        .with_checked([0])
    }

    fn host_prompt() -> SelectPrompt {
        SelectPrompt::new(
            "Site host",
            [
                SelectItem::new("127.0.0.1").with_description("lo  loopback"),
                SelectItem::new("192.0.2.10").with_description("en0"),
                SelectItem::new("10.0.0.1")
                    .with_description("wg0")
                    .with_unavailable_reason("not reachable"),
            ],
        )
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(key_event(code))
    }

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
}
