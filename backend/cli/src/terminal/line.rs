//! Plain line-by-line prompts for terminals that cannot render arrow-key
//! prompts, such as `TERM=dumb`, used by guided `dotc site init` and reset.
//!
//! Each question is one line ending in `: ` and each answer is one typed line,
//! for example `Write the validated initial site configuration? [no]: `. A
//! choice accepts either the item number or its label, a checklist takes them
//! separated by commas or `none`, invalid answers are explained and asked
//! again, and no styling or cursor movement is ever sent.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, BufRead, BufReader, IsTerminal, Write},
    os::fd::AsRawFd,
};

use super::{
    ConfirmPrompt, InputPrompt, Message, MultiSelectPrompt, PromptError, SelectItem, SelectPrompt,
    Terminal, TerminalCondition,
    render::{LastOutput, Symbols, plain_text},
};

/// Line prompts over a reader and writer, normally both the controlling terminal.
pub(super) struct LineTerminal {
    condition: TerminalCondition,
    io: Option<LineIo>,

    /// The terminal whose echo is switched off for secret answers; absent for injected streams.
    echo_control: Option<File>,

    symbols: Symbols,

    /// What was printed last, which decides the blank line before the next output.
    last: LastOutput,
}

struct LineIo {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
}

impl LineTerminal {
    /// Creates a terminal on the controlling terminal, or a non-interactive one without it.
    pub(super) fn from_tty(tty: Option<File>) -> Self {
        let Some((reader, writer)) =
            tty.and_then(|writer| Some((writer.try_clone().ok()?, writer)))
        else {
            return Self {
                condition: TerminalCondition::NonInteractive,
                io: None,
                echo_control: None,
                symbols: Symbols::ASCII,
                last: LastOutput::Nothing,
            };
        };
        let condition = if writer.is_terminal() {
            TerminalCondition::Interactive
        } else {
            TerminalCondition::NonInteractive
        };
        let echo_control = writer.try_clone().ok();
        let mut terminal = Self::with_io(condition, BufReader::new(reader), writer);
        terminal.echo_control = echo_control;
        terminal.symbols = Symbols::detect();
        terminal
    }

    fn with_io(
        condition: TerminalCondition,
        reader: impl BufRead + 'static,
        writer: impl Write + 'static,
    ) -> Self {
        Self {
            condition,
            io: Some(LineIo {
                reader: Box::new(reader),
                writer: Box::new(writer),
            }),
            echo_control: None,
            symbols: Symbols::UNICODE,
            last: LastOutput::Nothing,
        }
    }

    fn io(&mut self) -> Result<&mut LineIo, PromptError> {
        if self.condition == TerminalCondition::NonInteractive {
            return Err(PromptError::NotInteractive);
        }
        self.io.as_mut().ok_or(PromptError::NotInteractive)
    }

    fn write_line(&mut self, line: &str) -> Result<(), PromptError> {
        let io = self.io.as_mut().ok_or(PromptError::NotInteractive)?;
        writeln!(io.writer, "{line}").map_err(PromptError::Io)?;
        io.writer.flush().map_err(PromptError::Io)?;
        self.last = LastOutput::Message;
        Ok(())
    }

    /// Writes `question` and returns the next answer line without its line ending.
    fn ask(&mut self, question: &str) -> Result<String, PromptError> {
        let io = self.io()?;
        write!(io.writer, "{question}: ").map_err(PromptError::Io)?;
        io.writer.flush().map_err(PromptError::Io)?;
        self.last = LastOutput::Message;

        let io = self.io()?;
        let mut answer = String::new();
        match io.reader.read_line(&mut answer) {
            Ok(0) => Err(PromptError::EndOfInput),
            Ok(_) => Ok(answer.trim_end_matches(['\r', '\n']).to_owned()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                let _ = writeln!(io.writer);
                let _ = io.writer.flush();
                Err(PromptError::Cancelled)
            }
            Err(error) => Err(PromptError::Io(error)),
        }
    }

    /// Runs one prompt, after a blank line when it follows an answer, and records its answer so
    /// the next output is separated from it.
    fn answered<T>(
        &mut self,
        prompt: impl FnOnce(&mut Self) -> Result<T, PromptError>,
    ) -> Result<T, PromptError> {
        self.io()?;
        if self.last.blank_before(false) {
            self.write_line("")?;
        }
        let answer = prompt(self)?;
        self.last = LastOutput::Answer;
        Ok(answer)
    }
}

impl Terminal for LineTerminal {
    fn condition(&self) -> TerminalCondition {
        self.condition
    }

    fn select(&mut self, prompt: &SelectPrompt) -> Result<usize, PromptError> {
        self.answered(|terminal| {
            terminal.write_line(prompt.title())?;
            for (index, item) in prompt.items().iter().enumerate() {
                terminal.write_line(&item_line(index, "", item))?;
            }

            let question = format!("Choice [{}]", prompt.default() + 1);
            loop {
                let answer = terminal.ask(&question)?;
                match parse_select_answer(prompt, &answer) {
                    Some(index) => return Ok(index),
                    None => terminal.write_line(&format!(
                        "Choice must be a number from 1 to {} or a listed label.",
                        prompt.items().len()
                    ))?,
                }
            }
        })
    }

    fn multi_select(&mut self, prompt: &MultiSelectPrompt) -> Result<BTreeSet<usize>, PromptError> {
        self.answered(|terminal| {
            terminal.write_line(prompt.title())?;
            for (index, item) in prompt.items().iter().enumerate() {
                let mark = if prompt.checked().contains(&index) {
                    "[x] "
                } else {
                    "[ ] "
                };
                terminal.write_line(&item_line(index, mark, item))?;
            }

            let question = format!(
                "Choices, separated by commas, or none [{}]",
                numbers(prompt.checked())
            );
            loop {
                let answer = terminal.ask(&question)?;
                match parse_multi_select_answer(prompt, &answer) {
                    Some(checked) => return Ok(checked),
                    None => terminal.write_line(&format!(
                        "Choices must be available numbers from 1 to {} or listed labels, \
                         separated by commas, or none.",
                        prompt.items().len()
                    ))?,
                }
            }
        })
    }

    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Result<bool, PromptError> {
        let default = if prompt.default() { "yes" } else { "no" };
        let question = format!("{} [{default}]", prompt.title());
        self.answered(|terminal| {
            loop {
                let answer = terminal.ask(&question)?;
                match parse_confirm_answer(prompt, &answer) {
                    Some(decision) => return Ok(decision),
                    None => terminal.write_line("Confirmation must be yes or no.")?,
                }
            }
        })
    }

    fn input(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        let question = match prompt.default() {
            Some(default) => format!("{} [{default}]", prompt.title()),
            None => prompt.title().to_owned(),
        };
        self.answered(|terminal| Ok(prompt.resolve(terminal.ask(&question)?)))
    }

    fn secret(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        self.answered(|terminal| {
            let hidden = terminal
                .echo_control
                .as_ref()
                .map(EchoGuard::disable)
                .transpose()
                .map_err(PromptError::Io)?;
            let answer = terminal.ask(prompt.title());
            if hidden.is_some() {
                // The newline that ended the answer was not echoed.
                terminal.write_line("")?;
            }
            answer
        })
    }

    fn message(&mut self, message: &Message) -> Result<(), PromptError> {
        let rendered = message.render(self.symbols);
        if self.last.blank_before(rendered.blank_before) {
            self.write_line("")?;
        }
        for line in &rendered.lines {
            self.write_line(&plain_text(line))?;
        }
        Ok(())
    }
}

/// Switches terminal echo off until dropped.
struct EchoGuard {
    tty: File,
    original: libc::termios,
}

impl EchoGuard {
    fn disable(tty: &File) -> io::Result<Self> {
        let tty = tty.try_clone()?;
        // SAFETY: an all-zero termios is a valid value that tcgetattr fully overwrites on success.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open and `original` is a valid termios destination.
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: the descriptor is open and `hidden` is a valid termios.
        if unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, &hidden) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { tty, original })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor is open and `original` came from tcgetattr on it.
        unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.original) };
    }
}

/// Lays out one numbered item, such as `  2) [x] nvidia  detected`.
fn item_line(index: usize, mark: &str, item: &SelectItem) -> String {
    let mut line = format!("  {}) {mark}{}", index + 1, item.label());
    if let Some(description) = item.description() {
        line.push_str(&format!("  {description}"));
    }
    if let Some(reason) = item.unavailable_reason() {
        line.push_str(&format!(" (unavailable: {reason})"));
    }
    line
}

/// Lists 1-based item numbers, such as `1, 3`, or `none`.
fn numbers(indices: &BTreeSet<usize>) -> String {
    if indices.is_empty() {
        return "none".to_owned();
    }
    indices
        .iter()
        .map(|index| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolves an item number (1-based) or a case-insensitive label; empty chooses the default.
fn parse_select_answer(prompt: &SelectPrompt, answer: &str) -> Option<usize> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Some(prompt.default());
    }
    item_index(prompt.items(), answer)
}

/// Resolves comma-separated item numbers or labels of available items; empty keeps the
/// preselection and `none` checks nothing.
fn parse_multi_select_answer(prompt: &MultiSelectPrompt, answer: &str) -> Option<BTreeSet<usize>> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Some(prompt.checked().clone());
    }
    if answer.eq_ignore_ascii_case("none") {
        return Some(BTreeSet::new());
    }
    answer
        .split(',')
        .map(|choice| {
            item_index(prompt.items(), choice.trim()).filter(|index| prompt.is_checkable(*index))
        })
        .collect()
}

/// Resolves one item number (1-based) or case-insensitive label.
fn item_index(items: &[SelectItem], answer: &str) -> Option<usize> {
    if let Ok(number) = answer.parse::<usize>() {
        return (1..=items.len()).contains(&number).then(|| number - 1);
    }
    items
        .iter()
        .position(|item| item.label().eq_ignore_ascii_case(answer))
}

fn parse_confirm_answer(prompt: &ConfirmPrompt, answer: &str) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Some(prompt.default()),
        "yes" | "y" => Some(true),
        "no" | "n" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        io::{self, Cursor, Write},
        rc::Rc,
    };

    use std::collections::BTreeSet;

    use super::LineTerminal;
    use crate::terminal::{
        ConfirmPrompt, InputPrompt, Message, MultiSelectPrompt, NextStep, PromptError, SelectItem,
        SelectPrompt, Terminal, TerminalCondition, render::Symbols,
    };

    #[test]
    fn select_lists_items_and_accepts_numbers_labels_and_the_default() {
        let prompt = SelectPrompt::new(
            "Inference",
            [
                SelectItem::new("simulated").with_description("no hardware"),
                SelectItem::new("nvidia").with_unavailable_reason("no GPU found"),
            ],
        )
        .with_default(1);

        for (answer, expected) in [("2\n", 1), ("Simulated\n", 0), ("\n", 1)] {
            let (mut terminal, output) = line_terminal(answer);
            assert_eq!(terminal.select(&prompt).expect("choice"), expected);
            assert_eq!(
                output.text(),
                "Inference\n  1) simulated  no hardware\n  2) nvidia (unavailable: no GPU found)\nChoice [2]: "
            );
        }
    }

    #[test]
    fn multi_select_lists_marks_and_accepts_defaults_numbers_labels_and_none() {
        let prompt = MultiSelectPrompt::new(
            "Inference",
            [
                SelectItem::new("GPU 0").with_description("nvidia"),
                SelectItem::new("apple").with_unavailable_reason("detected but unusable"),
                SelectItem::new("simulated").with_description("development only"),
            ],
        )
        .with_checked([0]);

        for (answer, expected) in [
            ("\n", BTreeSet::from([0])),
            ("3\n", BTreeSet::from([2])),
            ("Simulated, 1\n", BTreeSet::from([0, 2])),
            ("none\n", BTreeSet::new()),
        ] {
            let (mut terminal, output) = line_terminal(answer);
            assert_eq!(terminal.multi_select(&prompt).expect("choices"), expected);
            assert_eq!(
                output.text(),
                "Inference\n  1) [x] GPU 0  nvidia\n  2) [ ] apple (unavailable: detected but \
                 unusable)\n  3) [ ] simulated  development only\nChoices, separated by commas, or \
                 none [1]: "
            );
        }
    }

    #[test]
    fn multi_select_refuses_unavailable_or_unknown_items_and_asks_again() {
        let prompt = MultiSelectPrompt::new(
            "Applications",
            [
                SelectItem::new("docker").with_unavailable_reason("not detected"),
                SelectItem::new("simulated"),
            ],
        );
        let (mut terminal, output) = line_terminal("1\n2, 4\n2\n");

        assert_eq!(
            terminal.multi_select(&prompt).expect("choices"),
            BTreeSet::from([1])
        );
        assert_eq!(
            output.text(),
            "Applications\n  1) [ ] docker (unavailable: not detected)\n  2) [ ] simulated\n\
             Choices, separated by commas, or none [none]: \
             Choices must be available numbers from 1 to 2 or listed labels, separated by commas, \
             or none.\nChoices, separated by commas, or none [none]: \
             Choices must be available numbers from 1 to 2 or listed labels, separated by commas, \
             or none.\nChoices, separated by commas, or none [none]: "
        );
    }

    #[test]
    fn select_explains_and_reasks_after_an_invalid_answer() {
        let prompt = SelectPrompt::new("Host", [SelectItem::new("127.0.0.1")]);
        let (mut terminal, output) = line_terminal("3\nother\n1\n");

        assert_eq!(terminal.select(&prompt).expect("choice"), 0);
        assert_eq!(
            output.text(),
            "Host\n  1) 127.0.0.1\nChoice [1]: \
             Choice must be a number from 1 to 1 or a listed label.\nChoice [1]: \
             Choice must be a number from 1 to 1 or a listed label.\nChoice [1]: "
        );
    }

    #[test]
    fn confirm_keeps_the_yes_no_line_and_reasks_until_valid() {
        let prompt = ConfirmPrompt::new("Write the validated initial site configuration?", false);
        let (mut terminal, output) = line_terminal("maybe\nY\n");

        assert!(terminal.confirm(&prompt).expect("confirmation"));
        assert_eq!(
            output.text(),
            "Write the validated initial site configuration? [no]: \
             Confirmation must be yes or no.\n\
             Write the validated initial site configuration? [no]: "
        );

        let (mut terminal, _) = line_terminal("\n");
        assert!(!terminal.confirm(&prompt).expect("default confirmation"));
    }

    #[test]
    fn secret_asks_without_a_default_and_returns_the_answer_untouched() {
        let prompt = InputPrompt::new("Join token (hidden)").with_default("ignored");
        let (mut terminal, output) = line_terminal("  dcjoin1.abc \n\n");

        assert_eq!(terminal.secret(&prompt).expect("typed"), "  dcjoin1.abc ");
        assert_eq!(terminal.secret(&prompt).expect("empty"), "");
        assert_eq!(
            output.text(),
            "Join token (hidden): \nJoin token (hidden): "
        );
    }

    #[test]
    fn input_uses_the_default_only_for_an_empty_answer() {
        let prompt = InputPrompt::new("Node host").with_default("127.0.0.1");
        let (mut terminal, output) = line_terminal("\r\n192.0.2.10\n");

        assert_eq!(terminal.input(&prompt).expect("default"), "127.0.0.1");
        assert_eq!(terminal.input(&prompt).expect("typed"), "192.0.2.10");
        assert_eq!(
            output.text(),
            "Node host [127.0.0.1]: \nNode host [127.0.0.1]: "
        );

        let (mut terminal, output) = line_terminal("\n");
        let prompt = InputPrompt::new("Initial site directory");
        assert_eq!(terminal.input(&prompt).expect("empty"), "");
        assert_eq!(output.text(), "Initial site directory: ");
    }

    #[test]
    fn end_of_input_stops_every_prompt_kind() {
        let (mut terminal, _) = line_terminal("");
        assert!(matches!(
            terminal.input(&InputPrompt::new("Site")),
            Err(PromptError::EndOfInput)
        ));
        assert!(matches!(
            terminal.confirm(&ConfirmPrompt::new("Write?", false)),
            Err(PromptError::EndOfInput)
        ));
        assert!(matches!(
            terminal.select(&SelectPrompt::new("Host", [SelectItem::new("a")])),
            Err(PromptError::EndOfInput)
        ));
    }

    #[test]
    fn messages_write_aligned_lines_with_blank_lines_before_blocks() {
        let (mut terminal, output) = line_terminal("");

        for message in [
            Message::Heading("Initial site plan".to_owned()),
            Message::KeyValues(vec![("host".to_owned(), "127.0.0.1".to_owned())]),
            Message::Success("Written".to_owned()),
            Message::Next(vec![NextStep::new("Start it").with_command("dotcore")]),
        ] {
            terminal.message(&message).expect("message");
        }

        assert_eq!(
            output.text(),
            "Initial site plan\n  host  127.0.0.1\n✔ Written\n\nNext\n  › Start it\n      dotcore\n"
        );
    }

    #[test]
    fn an_answer_is_separated_from_what_follows_but_context_stays_with_its_question() {
        let (mut terminal, output) = line_terminal("node-1\n1\ny\n");

        terminal
            .input(&InputPrompt::new("Node name"))
            .expect("name");
        terminal
            .message(&Message::Info(
                "Accelerator detection: nvidia detected".to_owned(),
            ))
            .expect("context");
        terminal
            .select(&SelectPrompt::new("Inference", [SelectItem::new("nvidia")]))
            .expect("inference");
        terminal
            .message(&Message::Heading("Initial site plan".to_owned()))
            .expect("heading");
        terminal
            .message(&Message::Hint("Workers connect to this host.".to_owned()))
            .expect("hint");
        terminal
            .confirm(&ConfirmPrompt::new("Write this site configuration?", true))
            .expect("confirmation");
        terminal
            .message(&Message::Success("Written".to_owned()))
            .expect("outcome");

        // The injected input is not echoed, so each answer line ends at its question.
        assert_eq!(
            output.text(),
            "Node name: \n\
             Accelerator detection: nvidia detected\n\
             Inference\n  1) nvidia\nChoice [1]: \n\
             Initial site plan\n\
             Workers connect to this host.\n\
             Write this site configuration? [yes]: \n\
             ✔ Written\n"
        );
    }

    #[test]
    fn consecutive_prompts_are_separated_but_a_reask_is_not() {
        let (mut terminal, output) = line_terminal("1\nmaybe\nn\n");

        terminal
            .select(&SelectPrompt::new("Host", [SelectItem::new("127.0.0.1")]))
            .expect("host");
        terminal
            .confirm(&ConfirmPrompt::new("Write?", true))
            .expect("confirmation");
        terminal
            .multi_select(&MultiSelectPrompt::new(
                "Applications",
                [SelectItem::new("docker")],
            ))
            .expect_err("end of input");

        assert_eq!(
            output.text(),
            "Host\n  1) 127.0.0.1\nChoice [1]: \n\
             Write? [yes]: Confirmation must be yes or no.\nWrite? [yes]: \n\
             Applications\n  1) [ ] docker\nChoices, separated by commas, or none [none]: "
        );
    }

    #[test]
    fn the_first_heading_has_no_blank_line_before_it_and_ascii_marks_apply() {
        let (mut terminal, output) = line_terminal("");
        terminal.symbols = Symbols::ASCII;

        terminal
            .message(&Message::Heading("Plan".to_owned()))
            .expect("heading");
        terminal
            .message(&Message::Error("Failed".to_owned()))
            .expect("error");

        assert_eq!(output.text(), "Plan\nx Failed\n");
    }

    #[test]
    fn missing_controlling_terminal_is_non_interactive() {
        let mut terminal = LineTerminal::from_tty(None);

        assert_eq!(terminal.condition(), TerminalCondition::NonInteractive);
        assert!(matches!(
            terminal.input(&InputPrompt::new("Site")),
            Err(PromptError::NotInteractive)
        ));
    }

    fn line_terminal(input: &str) -> (LineTerminal, SharedOutput) {
        let output = SharedOutput::default();
        let terminal = LineTerminal::with_io(
            TerminalCondition::Interactive,
            Cursor::new(input.as_bytes().to_vec()),
            output.clone(),
        );
        (terminal, output)
    }

    #[derive(Clone, Default)]
    struct SharedOutput(Rc<RefCell<Vec<u8>>>);

    impl SharedOutput {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("UTF-8 output")
        }
    }

    impl Write for SharedOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
