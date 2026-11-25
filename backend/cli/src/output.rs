//! Machine-readable command results: one YAML document per result on stdout.
//!
//! A command asked for YAML writes its result as the management response's protobuf JSON mapping
//! rendered as YAML, so scripts and agents read the same shape the API returns. A streaming
//! command writes one document per entry, separated by `---`. Failures never go through here:
//! they stay plain text on stderr.

use std::{
    collections::BTreeSet,
    io::{self, Write},
};

use anyhow::Context;
use serde::Serialize;

use crate::terminal::{
    ConfirmPrompt, CountdownPrompt, InputPrompt, Message, MultiSelectPrompt, PromptError,
    SelectPrompt, Terminal, TerminalCondition, controlling_terminal_condition, open_terminal,
};

/// How a command presents its result on stdout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputFormat {
    /// Tables and messages for a person, as plain text without a terminal.
    Human,

    /// One YAML document per result.
    Yaml,
}

/// The process terminal, opened on first use. A command that never asks or shows anything on the
/// terminal, such as one given `--yes` or YAML output, never touches the controlling terminal.
#[derive(Default)]
pub(crate) struct LazyTerminal {
    opened: Option<Box<dyn Terminal>>,
}

impl LazyTerminal {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn terminal(&mut self) -> &mut dyn Terminal {
        self.opened.get_or_insert_with(open_terminal).as_mut()
    }
}

impl Terminal for LazyTerminal {
    fn condition(&self) -> TerminalCondition {
        match &self.opened {
            Some(terminal) => terminal.condition(),
            None => controlling_terminal_condition(),
        }
    }

    fn select(&mut self, prompt: &SelectPrompt) -> Result<usize, PromptError> {
        self.terminal().select(prompt)
    }

    fn multi_select(&mut self, prompt: &MultiSelectPrompt) -> Result<BTreeSet<usize>, PromptError> {
        self.terminal().multi_select(prompt)
    }

    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Result<bool, PromptError> {
        self.terminal().confirm(prompt)
    }

    fn input(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        self.terminal().input(prompt)
    }

    fn secret(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        self.terminal().secret(prompt)
    }

    fn countdown(&mut self, prompt: &CountdownPrompt) -> Result<bool, PromptError> {
        self.terminal().countdown(prompt)
    }

    fn message(&mut self, message: &Message) -> Result<(), PromptError> {
        self.terminal().message(message)
    }

    fn prompt_context(&mut self, message: &Message) -> Result<(), PromptError> {
        self.terminal().prompt_context(message)
    }
}

/// Fails before anything changes when the operator passed no `--yes` and no question can be asked:
/// YAML output never asks, and otherwise a controlling terminal must be there to ask on.
/// `human` and `yaml` are the failures for each case, and both name how to answer.
pub(crate) fn require_consent(
    yes: bool,
    format: OutputFormat,
    human: &str,
    yaml: &str,
) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if format == OutputFormat::Yaml {
        anyhow::bail!("{yaml}");
    }
    if controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!("{human}");
    }
    Ok(())
}

/// Lays messages out as plain lines for stdout, with a blank line before each heading but the
/// first, for a result a person reads that needs no terminal.
pub(crate) fn plain_text(messages: &[Message]) -> String {
    let mut text = String::new();
    for (index, message) in messages.iter().enumerate() {
        if index > 0 && matches!(message, Message::Heading(_)) {
            text.push('\n');
        }
        for line in message.plain_lines() {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

/// Presents messages to a person: on the controlling terminal when there is one, else as plain
/// text on stdout.
pub(crate) fn present(messages: &[Message]) -> anyhow::Result<()> {
    if controlling_terminal_condition() == TerminalCondition::Interactive {
        return present_on(open_terminal().as_mut(), messages);
    }
    write_plain(messages)
}

/// Presents messages on `terminal` when it is interactive, else as plain text on stdout.
pub(crate) fn present_on(terminal: &mut dyn Terminal, messages: &[Message]) -> anyhow::Result<()> {
    if terminal.condition() != TerminalCondition::Interactive {
        return write_plain(messages);
    }
    for message in messages {
        terminal.message(message)?;
    }
    Ok(())
}

/// Writes messages as plain text on stdout, whether or not a terminal is there.
pub(crate) fn write_plain(messages: &[Message]) -> anyhow::Result<()> {
    write_text(&plain_text(messages))
}

/// Writes one result as a single YAML document on stdout.
pub(crate) fn write_yaml<T: Serialize>(document: &T) -> anyhow::Result<()> {
    let text =
        dotcore_proto::yaml::to_string(document).context("cannot render the result as YAML")?;
    write_text(&text)
}

/// Writes already laid out text on stdout. A closed stdout, such as a pipe into `head`, means the
/// reader is done and is not a failure.
pub(crate) fn write_text(text: &str) -> anyhow::Result<()> {
    write_tolerating_closed_reader(&mut io::stdout().lock(), text)
        .context("cannot write the result to standard output")
}

/// Writes `text` to `writer` and flushes it, treating a reader that went away as success.
fn write_tolerating_closed_reader(writer: &mut dyn Write, text: &str) -> io::Result<()> {
    match writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush())
    {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        written => written,
    }
}

/// A stream of YAML documents on stdout, one per entry, separated by `---` lines.
#[derive(Debug, Default)]
pub(crate) struct YamlDocuments {
    started: bool,
}

impl YamlDocuments {
    /// Writes one more document, after a separator when it is not the first.
    pub(crate) fn write<T: Serialize>(
        &mut self,
        writer: &mut dyn Write,
        document: &T,
    ) -> io::Result<()> {
        let text = dotcore_proto::yaml::to_string(document).map_err(io::Error::other)?;
        if self.started {
            writer.write_all(b"---\n")?;
        }
        writer.write_all(text.as_bytes())?;
        writer.flush()?;
        self.started = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_reader_is_not_a_write_failure_but_other_errors_are() {
        struct FailingWriter(io::ErrorKind);

        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(self.0.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        write_tolerating_closed_reader(&mut FailingWriter(io::ErrorKind::BrokenPipe), "text\n")
            .expect("closed reader");
        let error = write_tolerating_closed_reader(
            &mut FailingWriter(io::ErrorKind::PermissionDenied),
            "text\n",
        )
        .expect_err("other failure");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let mut written = Vec::new();
        write_tolerating_closed_reader(&mut written, "text\n").expect("open reader");
        assert_eq!(written, b"text\n");
    }

    #[test]
    fn documents_after_the_first_start_with_a_separator() {
        let mut documents = YamlDocuments::default();
        let mut written = Vec::new();

        documents
            .write(&mut written, &serde_json::json!({"line": "one"}))
            .expect("first document");
        documents
            .write(&mut written, &serde_json::json!({"line": "two"}))
            .expect("second document");

        assert_eq!(
            String::from_utf8(written).expect("UTF-8"),
            "line: one\n---\nline: two\n"
        );
    }
}
