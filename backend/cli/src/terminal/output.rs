//! Where a terminal writes command output: process stdout, kept apart from the prompts on the
//! controlling terminal, and process stderr for outcome warnings.
//!
//! When stdout is that terminal, output shares the prompts' screen, styles and spacing. When
//! stdout is redirected, output is written as plain lines with its own spacing, so a file or a
//! pipe receives the same text without colors or the prompts' blank lines. Warnings always go to
//! stderr, so stdout carries only the result.

use std::io::{self, IsTerminal, Write};

use super::{
    Message,
    render::{LastOutput, Rendered, plain_text, render_for_stderr},
};

/// The command output stream of one terminal.
pub(super) struct Output {
    writer: Box<dyn Write>,
    placement: Placement,
    diagnostics: Diagnostics,
}

/// Where outcome warnings go.
enum Diagnostics {
    /// Process stderr, laid out with the shared look when it is a terminal.
    Stderr,

    /// An injected stream, written as plain lines.
    #[cfg(test)]
    Plain(Box<dyn Write>),
}

/// Whether output lands on the prompts' screen or on a separate stream.
enum Placement {
    /// Output and prompts appear on the same screen, which the terminal spaces as one.
    Screen,

    /// Output goes elsewhere and is spaced on its own.
    Stream { last: LastOutput },
}

impl Output {
    /// Opens process stdout; `screen` tells whether a controlling terminal draws the prompts, so a
    /// terminal stdout shares their screen.
    pub(super) fn stdout(screen: bool) -> Self {
        let shares_screen = screen && io::stdout().is_terminal();
        Self::with_diagnostics(Box::new(io::stdout()), shares_screen, Diagnostics::Stderr)
    }

    /// Creates output on `writer` with warnings on `diagnostics`, sharing the prompts' screen
    /// when `shares_screen` is set.
    #[cfg(test)]
    pub(super) fn new(
        writer: Box<dyn Write>,
        diagnostics: Box<dyn Write>,
        shares_screen: bool,
    ) -> Self {
        Self::with_diagnostics(writer, shares_screen, Diagnostics::Plain(diagnostics))
    }

    fn with_diagnostics(
        writer: Box<dyn Write>,
        shares_screen: bool,
        diagnostics: Diagnostics,
    ) -> Self {
        let placement = if shares_screen {
            Placement::Screen
        } else {
            Placement::Stream {
                last: LastOutput::Nothing,
            }
        };
        Self {
            writer,
            placement,
            diagnostics,
        }
    }

    /// Reports whether output shares the prompts' screen.
    pub(super) fn shares_screen(&self) -> bool {
        matches!(self.placement, Placement::Screen)
    }

    /// Returns the writer, for a terminal laying out output on its own screen.
    pub(super) fn writer(&mut self) -> &mut dyn Write {
        &mut self.writer
    }

    /// Writes a message as plain lines. On a separate stream, a blank line comes first when the
    /// message starts a block after earlier output; on the screen, the terminal spaces it.
    pub(super) fn write_plain(&mut self, rendered: &Rendered) -> io::Result<()> {
        if let Placement::Stream { last } = &mut self.placement {
            if last.blank_before(rendered.blank_before) {
                writeln!(self.writer)?;
            }
            *last = LastOutput::Message;
        }
        for line in &rendered.lines {
            writeln!(self.writer, "{}", plain_text(line))?;
        }
        self.writer.flush()
    }

    /// Writes an outcome warning on stderr, apart from the result on stdout.
    pub(super) fn write_warning(&mut self, message: &Message) -> io::Result<()> {
        let plain = || message.plain_lines().join("\n") + "\n";
        match &mut self.diagnostics {
            Diagnostics::Stderr => {
                let text = render_for_stderr(std::slice::from_ref(message)).unwrap_or_else(plain);
                let mut stderr = io::stderr().lock();
                stderr.write_all(text.as_bytes())?;
                stderr.flush()
            }
            #[cfg(test)]
            Diagnostics::Plain(writer) => {
                writer.write_all(plain().as_bytes())?;
                writer.flush()
            }
        }
    }
}
