//! Arrow-key prompts with colors for guided `dotc site init` and reset, drawn
//! inline on the controlling terminal below the existing scrollback.
//!
//! Each prompt takes a few rows while it is open, then collapses to one
//! summary line such as `✔ Site host · 127.0.0.1`; messages print as styled
//! lines. There is no full-screen mode or mouse capture, Ctrl-C and Esc cancel
//! any prompt, and the terminal is always restored when a prompt ends. Opening
//! fails without drawing anything when the terminal cannot report its cursor
//! position, which the inline prompts need.

mod theme;
mod tty;
mod view;

use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, IsTerminal, Write},
};

use crossterm::{event, style::PrintStyledContent};
use ratatui::{
    TerminalOptions, Viewport,
    backend::{Backend, IntoCrossterm},
    text::Line,
    widgets::Clear,
};

use super::{
    ConfirmPrompt, InputPrompt, Message, MultiSelectPrompt, PromptError, SelectPrompt, Terminal,
    TerminalCondition,
    line::LineTerminal,
    render::{LastOutput, Symbols, colors_enabled},
};
use theme::Theme;
use tty::{RawModeGuard, TtyBackend, probe_cursor_reports};
use view::{
    ConfirmView, InputView, MultiSelectView, SelectView, Step, View, answer_line, cancelled_line,
    handle_event,
};

/// Rich prompts on the controlling terminal.
pub(super) struct RichTerminal {
    tty: File,
    theme: Theme,
    symbols: Symbols,

    /// What was printed last, which decides the blank line before the next output.
    last: LastOutput,
}

impl RichTerminal {
    /// Opens prompts on `tty` once it has answered a cursor-position query, deciding colors
    /// from that handle and the environment.
    pub(super) fn open(tty: File) -> io::Result<Self> {
        probe_cursor_reports(&tty)?;
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
            self.write_lines(vec![Line::default()])
                .map_err(PromptError::Io)?;
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
        let backend = TtyBackend::new(self.tty.try_clone()?)?;
        let rows = backend.size()?.height.max(1);
        let height = view.height(rows).clamp(1, rows);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        let theme = self.theme;

        let outcome = loop {
            terminal.draw(|frame| view.render(frame, &theme))?;
            // The completed frame reports a viewport-relative area; the terminal knows the row.
            guard.track_viewport(terminal.get_frame().area().y);
            match event::read()? {
                event::Event::Resize(..) => terminal.autoresize()?,
                event => match handle_event(view, event) {
                    Step::Continue => {}
                    Step::Cancel => break Err(PromptError::Cancelled),
                    Step::Done(output) => break Ok(output),
                },
            }
        };

        let answer = outcome.as_ref().ok().map(|output| view.answer_text(output));
        terminal.draw(|frame| {
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

    fn write_lines(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        for line in lines {
            for span in line.spans {
                let style = span.style.patch(line.style).into_crossterm();
                crossterm::queue!(
                    self.tty,
                    PrintStyledContent(crossterm::style::StyledContent::new(style, span.content))
                )?;
            }
            self.tty.write_all(b"\n")?;
        }
        self.tty.flush()
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

    fn message(&mut self, message: &Message) -> Result<(), PromptError> {
        self.ensure_interactive()?;
        let rendered = message.render(self.symbols);
        let mut lines = Vec::new();
        if self.last.blank_before(rendered.blank_before) {
            lines.push(Line::default());
        }
        lines.extend(rendered.lines.iter().map(|line| self.theme.line(line)));
        self.last = LastOutput::Message;
        self.write_lines(lines).map_err(PromptError::Io)
    }
}
