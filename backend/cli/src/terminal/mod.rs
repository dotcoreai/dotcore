//! Terminal interaction for guided `dotc site init` and `dotc site reset`:
//! typed choices, checklists, yes/no confirmations, text answers, and styled messages
//! behind one injectable seam, for example a host choice returning its index.
//!
//! Every prompt and message uses the controlling terminal (`/dev/tty`), never
//! process stdout or stderr, so the command's result records and diagnostics
//! stay exact. Messages share one layout (status marks, spacing, aligned facts,
//! tables, next steps), which diagnostics on a terminal stderr reuse. An answered
//! question closes its block, so whatever follows starts after a blank line,
//! while the messages leading up to a question stay attached to it as its
//! context. Capable terminals get arrow-key prompts with colors; dumb or
//! unknown terminal types, and terminals that never report their cursor
//! position, get plain line prompts. An operator cancel always surfaces as one
//! distinct outcome so the command exits with 130.

mod line;
mod render;
mod rich;
mod scripted;

use std::{collections::BTreeSet, fs::File, io};

use line::LineTerminal;
pub use scripted::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

/// Guided operator interaction without exposing a terminal implementation to site authoring.
pub trait Terminal {
    /// Reports whether a controlling terminal is available for input and presentation.
    fn condition(&self) -> TerminalCondition;

    /// Presents a list of choices and returns the index of the chosen item.
    fn select(&mut self, prompt: &SelectPrompt) -> Result<usize, PromptError>;

    /// Presents a list of items to check and returns the indices of the checked ones, possibly
    /// none; unavailable items cannot be checked.
    fn multi_select(&mut self, prompt: &MultiSelectPrompt) -> Result<BTreeSet<usize>, PromptError>;

    /// Asks a yes/no question and returns the operator's decision.
    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Result<bool, PromptError>;

    /// Asks for free text and returns the answer, or the default for an empty answer.
    fn input(&mut self, prompt: &InputPrompt) -> Result<String, PromptError>;

    /// Asks for free text that is not echoed back, such as a credential, and returns the answer
    /// exactly as typed.
    fn secret(&mut self, prompt: &InputPrompt) -> Result<String, PromptError>;

    /// Displays one styled, non-prompt message to the operator.
    fn message(&mut self, message: &Message) -> Result<(), PromptError>;
}

/// Opens the process terminal: rich prompts on a capable controlling terminal, plain line
/// prompts for a dumb or unset `TERM` or a terminal that does not answer cursor-position
/// queries, and a non-interactive terminal without `/dev/tty`.
///
/// Checking cursor reports sends one invisible query and may wait up to two seconds on a
/// silent terminal; use [`controlling_terminal_condition`] when only availability matters.
pub fn open_terminal() -> Box<dyn Terminal> {
    let tty = open_controlling_terminal();

    if let Some(tty) = &tty
        && supports_rich_prompts(std::env::var_os("TERM").as_deref())
        && let Ok(tty) = tty.try_clone()
        // Any failure to prove cursor reports leaves the line prompts, which need none.
        && let Ok(rich) = rich::RichTerminal::open(tty)
    {
        return Box::new(rich);
    }

    Box::new(LineTerminal::from_tty(tty))
}

/// Lays out messages for process stderr with the shared look when stderr is a terminal, else
/// returns `None` so redirected diagnostics stay plain.
pub fn render_for_stderr(messages: &[Message]) -> Option<String> {
    render::render_for_stderr(messages)
}

/// Prints a diagnostic notice on process stderr: laid out with the shared look when stderr is a
/// terminal, else as the unchanged `plain` text so redirected diagnostics stay stable.
pub fn notify_stderr(messages: &[Message], plain: &str) {
    match render_for_stderr(messages) {
        Some(text) => eprint!("{text}"),
        None => eprintln!("{plain}"),
    }
}

/// Reports whether a controlling terminal is available, without opening prompts on it.
pub fn controlling_terminal_condition() -> TerminalCondition {
    LineTerminal::from_tty(open_controlling_terminal()).condition()
}

/// Availability of the terminal handles required for guided interaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCondition {
    /// A controlling terminal is available for input and presentation.
    Interactive,

    /// No controlling terminal is available.
    NonInteractive,
}

/// A choice among labeled items with one preselected item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectPrompt {
    title: String,
    items: Vec<SelectItem>,
    default: usize,
}

impl SelectPrompt {
    /// Creates a choice whose first item is preselected.
    ///
    /// # Panics
    ///
    /// Panics when `items` is empty, since there would be nothing to choose.
    pub fn new(title: impl Into<String>, items: impl IntoIterator<Item = SelectItem>) -> Self {
        let items: Vec<SelectItem> = items.into_iter().collect();
        assert!(!items.is_empty(), "a select prompt needs at least one item");
        Self {
            title: title.into(),
            items,
            default: 0,
        }
    }

    /// Preselects the item at `index`, which an Enter without moving chooses.
    ///
    /// # Panics
    ///
    /// Panics when `index` does not refer to an item.
    pub fn with_default(mut self, index: usize) -> Self {
        assert!(index < self.items.len(), "select default is out of range");
        self.default = index;
        self
    }

    /// Returns the question shown above the items.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the items in display order.
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Returns the index of the preselected item.
    pub fn default(&self) -> usize {
        self.default
    }
}

/// A list of labeled items the operator checks any number of, starting from a preselection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiSelectPrompt {
    list: SelectPrompt,
    checked: BTreeSet<usize>,
}

impl MultiSelectPrompt {
    /// Creates a list with no item checked.
    ///
    /// # Panics
    ///
    /// Panics when `items` is empty, since there would be nothing to check.
    pub fn new(title: impl Into<String>, items: impl IntoIterator<Item = SelectItem>) -> Self {
        Self {
            list: SelectPrompt::new(title, items),
            checked: BTreeSet::new(),
        }
    }

    /// Preselects the items at `indices`, which an Enter without toggling keeps.
    ///
    /// # Panics
    ///
    /// Panics when an index does not refer to an available item.
    pub fn with_checked(mut self, indices: impl IntoIterator<Item = usize>) -> Self {
        self.checked = indices.into_iter().collect();
        assert!(
            self.checked.iter().all(|index| self.is_checkable(*index)),
            "multi-select preselection must name available items"
        );
        self
    }

    /// Returns the question shown above the items.
    pub fn title(&self) -> &str {
        self.list.title()
    }

    /// Returns the items in display order.
    pub fn items(&self) -> &[SelectItem] {
        self.list.items()
    }

    /// Returns the indices of the preselected items.
    pub fn checked(&self) -> &BTreeSet<usize> {
        &self.checked
    }

    /// Reports whether the item at `index` exists and may be checked.
    pub fn is_checkable(&self, index: usize) -> bool {
        self.items()
            .get(index)
            .is_some_and(|item| item.unavailable_reason().is_none())
    }

    /// Returns the checked items' labels for a one-line answer, or `none`.
    fn answer_text(&self, checked: &BTreeSet<usize>) -> String {
        if checked.is_empty() {
            return "none".to_owned();
        }
        checked
            .iter()
            .map(|index| self.items()[*index].label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One selectable row: a label, an optional description, and an optional unavailability reason.
///
/// An unavailable item is still selectable; it is only presented as unavailable so the caller
/// can explain the reason and ask again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectItem {
    label: String,
    description: Option<String>,
    unavailable_reason: Option<String>,
}

impl SelectItem {
    /// Creates an item identified to the operator by `label`.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
            unavailable_reason: None,
        }
    }

    /// Adds a short description shown next to the label.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Presents the item as unavailable for the given reason.
    pub fn with_unavailable_reason(mut self, reason: impl Into<String>) -> Self {
        self.unavailable_reason = Some(reason.into());
        self
    }

    /// Returns the label that identifies the item.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the description shown next to the label, if any.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns why the item is unavailable, if it is.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }
}

/// A yes/no question with the decision taken by an empty answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmPrompt {
    title: String,
    default: bool,
}

impl ConfirmPrompt {
    /// Creates a question whose empty answer means `default`.
    pub fn new(title: impl Into<String>, default: bool) -> Self {
        Self {
            title: title.into(),
            default,
        }
    }

    /// Returns the question.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the decision taken by an empty answer.
    pub fn default(&self) -> bool {
        self.default
    }
}

/// A free-text question with an optional value accepted by an empty answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputPrompt {
    title: String,
    default: Option<String>,
}

impl InputPrompt {
    /// Creates a question without a default, so an empty answer stays empty.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            default: None,
        }
    }

    /// Configures the value returned when the operator submits an empty answer.
    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    /// Returns the question.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the value accepted by an empty answer, if any.
    pub fn default(&self) -> Option<&str> {
        self.default.as_deref()
    }

    fn resolve(&self, answer: String) -> String {
        match &self.default {
            Some(default) if answer.is_empty() => default.clone(),
            _ => answer,
        }
    }
}

/// A non-prompt message whose kind selects its presentation style.
///
/// Status kinds (`Success`, `Warning`, `Error`) carry their own mark, so callers write only the
/// sentence. A `Heading` or `Next` block is separated from earlier output by a blank line, which
/// the terminal adds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Message {
    /// A section title, for example before a list or a summary.
    Heading(String),

    /// A neutral statement.
    Info(String),

    /// A positive outcome, for example an available capability.
    Success(String),

    /// A degraded but acceptable situation.
    Warning(String),

    /// A rejected answer or failed step.
    Error(String),

    /// Secondary guidance of low importance.
    Hint(String),

    /// A command the operator copies, on its own indented line.
    Command(String),

    /// An ordered block of labeled values, presented aligned where the terminal allows.
    KeyValues(Vec<(String, String)>),

    /// Rows under a header, laid out in aligned columns.
    Table {
        /// The column titles.
        header: Vec<String>,

        /// The rows, each with one cell per column.
        rows: Vec<Vec<String>>,
    },

    /// What the operator can do next, as steps that may each carry a command to copy.
    Next(Vec<NextStep>),
}

/// One suggestion in a [`Message::Next`] block: a sentence and an optional command to copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NextStep {
    text: String,
    command: Option<String>,
}

impl NextStep {
    /// Creates a step that only states what to do.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            command: None,
        }
    }

    /// Adds the command that carries the step out, shown on its own line.
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    /// Returns what to do.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the command that carries it out, if any.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }
}

/// A terminal interaction outcome that prevents guided initialization from continuing.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// Interaction was requested without terminal input and diagnostic output.
    #[error("guided interaction requires a terminal")]
    NotInteractive,

    /// The operator interrupted the current prompt.
    #[error("guided interaction was cancelled")]
    Cancelled,

    /// The input stream ended before an answer was supplied.
    #[error("guided interaction reached end of input")]
    EndOfInput,

    /// Terminal input or diagnostic output failed.
    #[error("guided terminal I/O failed")]
    Io(#[source] io::Error),
}

/// Reports whether a terminal type can render arrow-key prompts; dumb and unset types cannot.
fn supports_rich_prompts(term: Option<&std::ffi::OsStr>) -> bool {
    term.is_some_and(|term| !term.is_empty() && term != "dumb")
}

fn open_controlling_terminal() -> Option<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{Message, PromptError, SelectItem, SelectPrompt};

    #[test]
    fn prompt_io_error_retains_its_concrete_source_and_reports_it_once() {
        let error = PromptError::Io(std::io::Error::other("injected terminal failure"));

        assert_eq!(error.to_string(), "guided terminal I/O failed");
        assert_eq!(
            crate::format_failure(&anyhow::Error::new(PromptError::Io(std::io::Error::other(
                "injected terminal failure"
            )))),
            "guided terminal I/O failed: injected terminal failure"
        );
        let source = std::error::Error::source(&error).expect("I/O source");
        assert_eq!(source.to_string(), "injected terminal failure");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn rich_prompts_need_a_known_terminal_type() {
        use std::ffi::OsStr;

        use super::supports_rich_prompts;

        assert!(supports_rich_prompts(Some(OsStr::new("xterm-256color"))));
        assert!(supports_rich_prompts(Some(OsStr::new("screen"))));
        assert!(!supports_rich_prompts(Some(OsStr::new("dumb"))));
        assert!(!supports_rich_prompts(Some(OsStr::new(""))));
        assert!(!supports_rich_prompts(None));
    }

    #[test]
    fn messages_render_plain_lines() {
        assert_eq!(
            Message::Warning("Docker is unavailable".to_owned()).plain_lines(),
            ["! Docker is unavailable"]
        );
        assert_eq!(
            Message::KeyValues(vec![
                ("directory".to_owned(), "/srv/site".to_owned()),
                ("host".to_owned(), "127.0.0.1".to_owned()),
            ])
            .plain_lines(),
            ["  directory  /srv/site", "  host       127.0.0.1"]
        );
    }

    #[test]
    #[should_panic(expected = "select default is out of range")]
    fn select_default_must_refer_to_an_item() {
        let _ = SelectPrompt::new("Host", [SelectItem::new("127.0.0.1")]).with_default(1);
    }
}
