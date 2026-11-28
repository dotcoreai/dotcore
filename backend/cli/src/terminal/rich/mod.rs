//! Arrow-key prompts and forms with colors for guided commands, drawn inline
//! on the controlling terminal below the existing scrollback.
//!
//! Each prompt takes a few rows while it is open, then collapses to one
//! summary line, while a form is erased and leaves the summary to its caller;
//! messages print as styled lines. There is no full-screen mode or mouse
//! capture, Ctrl-C cancels any prompt and Esc any prompt but a form that can go
//! back, a resized window redraws the prompt where it was, and the terminal is
//! always restored when a prompt ends. Opening fails without drawing anything
//! when the terminal cannot report its cursor position, which the inline
//! prompts need.

mod form;
mod theme;
mod tty;
mod view;

use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, IsTerminal, Write},
    time::Instant,
};

use crossterm::{event, style::PrintStyledContent};
use dotcore_domain::timing::TerminalTiming;
use ratatui::{
    TerminalOptions, Viewport,
    backend::{Backend, IntoCrossterm},
    text::Line,
    widgets::Clear,
};

use super::{
    ConfirmPrompt, CountdownPrompt, FormAnswer, FormPrompt, FormSession, InputPrompt, Message,
    MultiSelectPrompt, PromptError, SelectPrompt, Terminal, TerminalCondition,
    line::LineTerminal,
    output::Output,
    render::{LastOutput, Symbols, colors_enabled},
};
use form::FormView;
use theme::Theme;
use tty::{RawModeGuard, TtyBackend, probe_cursor_reports};
use view::{
    ConfirmView, CountdownView, InputView, MultiSelectView, SelectView, Step, View, answer_line,
    cancelled_line, handle_event,
};

/// Rich prompts on the controlling terminal.
pub(super) struct RichTerminal {
    tty: File,
    theme: Theme,
    symbols: Symbols,

    /// What was printed last, which decides the blank line before the next output.
    last: LastOutput,

    /// Real-time bounds of the terminal's answers.
    timing: TerminalTiming,

    /// Where command output goes.
    output: Output,
}

impl RichTerminal {
    /// Opens prompts on `tty` once it has answered a cursor-position query within the bound of
    /// `timing`, deciding colors from that handle and the environment.
    pub(super) fn open(tty: File, timing: TerminalTiming) -> io::Result<Self> {
        probe_cursor_reports(&tty, timing)?;
        let colors = colors_enabled(
            tty.is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("TERM").as_deref(),
        );
        Ok(Self {
            tty,
            theme: Theme::new(colors),
            symbols: Symbols::detect(),
            last: LastOutput::Nothing,
            timing,
            output: Output::stdout(true),
        })
    }

    fn ensure_interactive(&self) -> Result<(), PromptError> {
        match self.condition() {
            TerminalCondition::Interactive => Ok(()),
            TerminalCondition::NonInteractive => Err(PromptError::NotInteractive),
        }
    }

    /// Runs one view to completion in an inline viewport and leaves its summary line.
    fn run<V: View>(&mut self, mut view: V) -> Result<V::Output, PromptError> {
        self.answered(|terminal| {
            let guard = RawModeGuard::enable().map_err(PromptError::Io)?;
            let outcome = terminal.drive(&guard, &mut view).map_err(PromptError::Io)?;
            guard.finish();
            outcome
        })
    }

    /// Runs one prompt, after a blank line when it follows an answer, and records its answer so
    /// the next output is separated from it.
    fn answered<T>(
        &mut self,
        prompt: impl FnOnce(&mut Self) -> Result<T, PromptError>,
    ) -> Result<T, PromptError> {
        self.ensure_interactive()?;
        if self.last.blank_before(false) {
            // Written before the inline viewport opens, which then starts on the row below.
            write_lines(&mut self.tty, vec![Line::default()]).map_err(PromptError::Io)?;
        }
        // A cancelled prompt still leaves its summary line.
        self.last = LastOutput::Message;
        let answer = prompt(self)?;
        self.last = LastOutput::Answer;
        Ok(answer)
    }

    fn drive<V: View>(
        &mut self,
        guard: &RawModeGuard,
        view: &mut V,
    ) -> io::Result<Result<V::Output, PromptError>> {
        let mut terminal = self.open_viewport(view)?;
        let theme = self.theme;

        let outcome = loop {
            draw(&mut terminal, |frame| view.render(frame, &theme))?;
            // The completed frame reports a viewport-relative area; the terminal knows the row.
            let top = terminal.get_frame().area().y;
            guard.track_viewport(top);
            let event = match view.wake_at() {
                Some(at) => {
                    let wait = at.saturating_duration_since(Instant::now());
                    event::poll(wait)?.then(event::read).transpose()?
                }
                None => Some(event::read()?),
            };
            let step = match event {
                None => view.on_wake(Instant::now()),
                Some(event::Event::Resize(..)) => {
                    // Reopening the viewport where it was keeps the scrollback; letting ratatui
                    // resize it clears the whole screen when the window narrows.
                    terminal.backend_mut().erase_from(top)?;
                    terminal = self.open_viewport(view)?;
                    Step::Continue
                }
                Some(event) => handle_event(view, event),
            };
            match step {
                Step::Continue => {}
                Step::Cancel => break Err(PromptError::Cancelled),
                Step::Done(output) => break Ok(output),
            }
        };

        let top = terminal.get_frame().area().y;
        if outcome.is_ok() && !view.leaves_answer() {
            // The caller reports the outcome itself, starting where the view was.
            let backend = terminal.backend_mut();
            backend.erase_from(top)?;
            backend.show_cursor()?;
            return Ok(outcome);
        }
        let answer = outcome.as_ref().ok().map(|output| view.answer_text(output));
        draw(&mut terminal, |frame| {
            let area = frame.area();
            let summary = match &answer {
                Some(answer) => answer_line(&theme, view.title(), answer, area.width),
                None => cancelled_line(&theme, view.title()),
            };
            frame.render_widget(Clear, area);
            frame.render_widget(summary, area);
        })?;
        let top = terminal.get_frame().area().y;
        let backend = terminal.backend_mut();
        backend.move_below(top)?;
        backend.show_cursor()?;
        Ok(outcome)
    }

    /// Opens an inline viewport at the cursor, as tall as `view` wants on this terminal.
    fn open_viewport<V: View>(&self, view: &V) -> io::Result<ratatui::Terminal<TtyBackend>> {
        let backend = TtyBackend::new(self.tty.try_clone()?, self.timing)?;
        let rows = backend.size()?.height.max(1);
        let height = view.height(rows).clamp(1, rows);
        ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
    }

    /// Lays out a message with the theme and the prompts' spacing: on stdout when `on_output` is
    /// set, which then shares the prompts' screen, else on the controlling terminal.
    fn show(&mut self, message: &Message, on_output: bool) -> Result<(), PromptError> {
        let rendered = message.render(self.symbols);
        let mut lines = Vec::new();
        if self.last.blank_before(rendered.blank_before) {
            lines.push(Line::default());
        }
        lines.extend(rendered.lines.iter().map(|line| self.theme.line(line)));
        self.last = LastOutput::Message;
        let writer: &mut dyn Write = if on_output {
            self.output.writer()
        } else {
            &mut self.tty
        };
        write_lines(writer, lines).map_err(PromptError::Io)
    }
}

impl Terminal for RichTerminal {
    fn condition(&self) -> TerminalCondition {
        if self.tty.is_terminal() {
            TerminalCondition::Interactive
        } else {
            TerminalCondition::NonInteractive
        }
    }

    fn select(&mut self, prompt: &SelectPrompt) -> Result<usize, PromptError> {
        self.run(SelectView::new(prompt))
    }

    fn multi_select(&mut self, prompt: &MultiSelectPrompt) -> Result<BTreeSet<usize>, PromptError> {
        self.run(MultiSelectView::new(prompt))
    }

    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Result<bool, PromptError> {
        self.run(ConfirmView::new(prompt))
    }

    fn input(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        self.run(InputView::new(prompt))
    }

    fn secret(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        self.answered(|terminal| {
            // Hidden answers need no arrow-key view, so a plain line prompt on the same terminal
            // reads them.
            let tty = terminal.tty.try_clone().map_err(PromptError::Io)?;
            LineTerminal::from_tty(Some(tty)).secret(prompt)
        })
    }

    fn countdown(&mut self, prompt: &CountdownPrompt) -> Result<bool, PromptError> {
        // Ctrl-C and Esc skip the action rather than stop the command.
        match self.run(CountdownView::new(prompt, Instant::now())) {
            Err(PromptError::Cancelled) => Ok(false),
            outcome => outcome,
        }
    }

    fn form(
        &mut self,
        prompt: &mut FormPrompt,
        session: &mut dyn FormSession,
    ) -> Result<FormAnswer, PromptError> {
        let (symbols, timing) = (self.symbols, self.timing);
        let answer = self.run(FormView::new(
            prompt,
            session,
            symbols,
            timing.form_settle,
            timing.form_refresh,
        ));
        // An erased form leaves no answer line, so the caller's summary follows without a gap.
        self.last = LastOutput::Message;
        answer
    }

    fn live_footer(&mut self) -> bool {
        true
    }

    fn message(&mut self, message: &Message) -> Result<(), PromptError> {
        if matches!(message, Message::Warning(_)) {
            return self.output.write_warning(message).map_err(PromptError::Io);
        }
        self.ensure_interactive()?;
        if self.output.shares_screen() {
            return self.show(message, true);
        }
        let rendered = message.render(self.symbols);
        self.output.write_plain(&rendered).map_err(PromptError::Io)
    }

    fn prompt_context(&mut self, message: &Message) -> Result<(), PromptError> {
        self.ensure_interactive()?;
        self.show(message, false)
    }
}

/// Draws one frame inside a synchronized update, so the terminal shows it whole.
fn draw(
    terminal: &mut ratatui::Terminal<TtyBackend>,
    render: impl FnOnce(&mut ratatui::Frame),
) -> io::Result<()> {
    terminal.backend_mut().synchronize(true)?;
    let drawn = terminal.draw(render).map(|_| ());
    terminal.backend_mut().synchronize(false)?;
    drawn
}

/// Writes styled lines to `writer`, one per row.
fn write_lines(mut writer: &mut dyn Write, lines: Vec<Line<'static>>) -> io::Result<()> {
    for line in lines {
        for span in line.spans {
            let style = span.style.patch(line.style).into_crossterm();
            crossterm::queue!(
                &mut writer,
                PrintStyledContent(crossterm::style::StyledContent::new(style, span.content))
            )?;
        }
        writer.write_all(b"\n")?;
    }
    writer.flush()
}
