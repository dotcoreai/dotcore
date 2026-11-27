//! A deterministic terminal for tests of guided `dotc site init` and reset
//! that replays typed answers instead of reading a real terminal.
//!
//! Each answer names the prompt kind it answers, for example a choice index
//! or a yes/no decision, and a test fails immediately when the flow asks a
//! different kind of question than the script expects. Every prompt and the
//! plain text of every message, output or prompt context, are recorded in order for
//! assertions.

use std::collections::{BTreeSet, VecDeque};

use super::{
    ConfirmPrompt, CountdownPrompt, FormAnswer, FormPrompt, FormSession, InputPrompt, Message,
    MultiSelectPrompt, PromptError, SelectPrompt, Terminal, TerminalCondition, form::settle,
};

/// A terminal that replays scripted answers and records what the flow presented.
pub struct ScriptedTerminal {
    condition: TerminalCondition,
    answers: VecDeque<ScriptedAnswer>,
    prompts: Vec<ScriptedPrompt>,
    messages: Vec<Message>,
    lines: Vec<String>,

    /// Whether forms keep a live footer, as on a rich terminal.
    live_footer: bool,

    /// Every footer the open forms showed, as plain lines, each recorded when it changed.
    footers: Vec<Vec<String>>,
}

impl ScriptedTerminal {
    /// Creates a terminal with fixed availability and queued answers.
    pub fn new(
        condition: TerminalCondition,
        answers: impl IntoIterator<Item = ScriptedAnswer>,
    ) -> Self {
        Self {
            condition,
            answers: answers.into_iter().collect(),
            prompts: Vec::new(),
            messages: Vec::new(),
            lines: Vec::new(),
            live_footer: true,
            footers: Vec::new(),
        }
    }

    /// Makes forms ask one field at a time without a footer, as on a line terminal.
    pub fn without_live_footer(mut self) -> Self {
        self.live_footer = false;
        self
    }

    /// Creates an interactive terminal with queued answers.
    pub fn interactive(answers: impl IntoIterator<Item = ScriptedAnswer>) -> Self {
        Self::new(TerminalCondition::Interactive, answers)
    }

    /// Returns every prompt presented, in order, including ones that consumed a stop outcome.
    pub fn prompts(&self) -> &[ScriptedPrompt] {
        &self.prompts
    }

    /// Returns the number of answers not consumed by prompts.
    pub fn remaining_answers(&self) -> usize {
        self.answers.len()
    }

    /// Returns every message displayed, in order.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns the plain text of every displayed message, one entry per line.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Returns every footer the forms showed, as plain lines, each recorded when it changed.
    pub fn footers(&self) -> &[Vec<String>] {
        &self.footers
    }

    /// Records the session's footer when it differs from the last one recorded.
    fn record_footer(&mut self, session: &dyn FormSession) {
        let Some(footer) = session.footer() else {
            return;
        };
        let mut lines = vec![footer.title.clone()];
        lines.extend(footer.lines.iter().flat_map(Message::plain_lines));
        if self.footers.last() != Some(&lines) {
            self.footers.push(lines);
        }
    }

    /// Records `prompt` and returns the next answer, or the stop outcome it scripts.
    fn next_answer(&mut self, prompt: ScriptedPrompt) -> Result<ScriptedAnswer, PromptError> {
        if self.condition == TerminalCondition::NonInteractive {
            return Err(PromptError::NotInteractive);
        }

        self.prompts.push(prompt);
        match self
            .answers
            .pop_front()
            .unwrap_or(ScriptedAnswer::EndOfInput)
        {
            ScriptedAnswer::Cancelled => Err(PromptError::Cancelled),
            ScriptedAnswer::EndOfInput => Err(PromptError::EndOfInput),
            answer => Ok(answer),
        }
    }
}

impl Terminal for ScriptedTerminal {
    fn condition(&self) -> TerminalCondition {
        self.condition
    }

    fn select(&mut self, prompt: &SelectPrompt) -> Result<usize, PromptError> {
        match self.next_answer(ScriptedPrompt::Select(prompt.clone()))? {
            ScriptedAnswer::Select(index) => {
                assert!(
                    index < prompt.items().len(),
                    "scripted choice {index} is outside select prompt {:?}",
                    prompt.title()
                );
                Ok(index)
            }
            ScriptedAnswer::SelectDefault => Ok(prompt.default()),
            answer => panic!(
                "scripted {answer:?} does not answer select prompt {:?}",
                prompt.title()
            ),
        }
    }

    fn multi_select(&mut self, prompt: &MultiSelectPrompt) -> Result<BTreeSet<usize>, PromptError> {
        match self.next_answer(ScriptedPrompt::MultiSelect(prompt.clone()))? {
            ScriptedAnswer::MultiSelect(indices) => {
                let checked = indices.into_iter().collect::<BTreeSet<_>>();
                assert!(
                    checked.iter().all(|index| prompt.is_checkable(*index)),
                    "scripted choices {checked:?} are not available items of {:?}",
                    prompt.title()
                );
                Ok(checked)
            }
            ScriptedAnswer::MultiSelectDefault => Ok(prompt.checked().clone()),
            answer => panic!(
                "scripted {answer:?} does not answer multi-select prompt {:?}",
                prompt.title()
            ),
        }
    }

    fn confirm(&mut self, prompt: &ConfirmPrompt) -> Result<bool, PromptError> {
        match self.next_answer(ScriptedPrompt::Confirm(prompt.clone()))? {
            ScriptedAnswer::Confirm(decision) => Ok(decision),
            answer => panic!(
                "scripted {answer:?} does not answer confirm prompt {:?}",
                prompt.title()
            ),
        }
    }

    fn input(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        match self.next_answer(ScriptedPrompt::Input(prompt.clone()))? {
            ScriptedAnswer::Input(answer) => Ok(prompt.resolve(answer)),
            answer => panic!(
                "scripted {answer:?} does not answer input prompt {:?}",
                prompt.title()
            ),
        }
    }

    fn secret(&mut self, prompt: &InputPrompt) -> Result<String, PromptError> {
        match self.next_answer(ScriptedPrompt::Secret(prompt.clone()))? {
            ScriptedAnswer::Secret(answer) => Ok(answer),
            answer => panic!(
                "scripted {answer:?} does not answer secret prompt {:?}",
                prompt.title()
            ),
        }
    }

    fn countdown(&mut self, prompt: &CountdownPrompt) -> Result<bool, PromptError> {
        match self.next_answer(ScriptedPrompt::Countdown(prompt.clone()))? {
            ScriptedAnswer::Countdown(proceed) => Ok(proceed),
            answer => panic!(
                "scripted {answer:?} does not answer countdown {:?}",
                prompt.title()
            ),
        }
    }

    fn form(
        &mut self,
        prompt: &mut FormPrompt,
        session: &mut dyn FormSession,
    ) -> Result<FormAnswer, PromptError> {
        if !self.live_footer {
            return super::form::ask_each_field(self, prompt, session);
        }
        loop {
            let answer = self.next_answer(ScriptedPrompt::Form(prompt.clone()))?;
            let ScriptedAnswer::Form(edits, then) = answer else {
                panic!(
                    "scripted {answer:?} does not answer form {:?}",
                    prompt.title
                );
            };
            self.record_footer(session);
            for (label, value) in edits {
                let field = prompt
                    .fields
                    .iter_mut()
                    .find(|field| field.label == label)
                    .unwrap_or_else(|| panic!("form {:?} has no field {label:?}", prompt.title));
                field.value = value;
                session.edited(&mut prompt.fields);
                settle(session, &mut prompt.fields);
                self.record_footer(session);
            }
            if then == FormAnswer::Back && !prompt.can_go_back {
                return Err(PromptError::Cancelled);
            }
            let errors = prompt
                .fields
                .iter()
                .filter_map(|field| field.error.as_ref())
                .map(|error| format!("✘ {error}"))
                .collect::<Vec<_>>();
            if then == FormAnswer::Back || errors.is_empty() {
                return Ok(then);
            }
            // The form stays open on a value it does not accept, as Enter does on a terminal.
            self.lines.extend(errors);
        }
    }

    fn live_footer(&mut self) -> bool {
        self.live_footer
    }

    fn message(&mut self, message: &Message) -> Result<(), PromptError> {
        self.lines.extend(message.plain_lines());
        self.messages.push(message.clone());
        Ok(())
    }

    fn prompt_context(&mut self, message: &Message) -> Result<(), PromptError> {
        self.message(message)
    }
}

/// A prompt presented to a scripted terminal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptedPrompt {
    /// A choice among items.
    Select(SelectPrompt),

    /// A checklist of items.
    MultiSelect(MultiSelectPrompt),

    /// A yes/no question.
    Confirm(ConfirmPrompt),

    /// A free-text question.
    Input(InputPrompt),

    /// A free-text question whose answer is not echoed.
    Secret(InputPrompt),

    /// A countdown before an action.
    Countdown(CountdownPrompt),

    /// A form, with its fields as they were when it opened.
    Form(FormPrompt),
}

/// A replayable answer or interruption for the next prompt.
#[derive(Debug)]
pub enum ScriptedAnswer {
    /// The index of the chosen item of a select prompt.
    Select(usize),

    /// Accepts the preselected item of a select prompt, like Enter without moving.
    SelectDefault,

    /// The indices of the checked items of a multi-select prompt, possibly none.
    MultiSelect(Vec<usize>),

    /// Keeps the preselected items of a multi-select prompt, like Enter without toggling.
    MultiSelectDefault,

    /// The decision for a confirm prompt.
    Confirm(bool),

    /// The submitted text for an input prompt; empty accepts the prompt default.
    Input(String),

    /// The submitted text for a secret prompt, returned exactly as given.
    Secret(String),

    /// The outcome of a countdown: true when it runs out or the operator goes ahead early, false
    /// when the operator skips the action.
    Countdown(bool),

    /// Values typed into a form's fields, by label and in order, then how the form ends; a value
    /// the form does not accept keeps it open for the next answer.
    Form(Vec<(String, String)>, FormAnswer),

    /// An operator cancellation at any prompt, such as Ctrl-C or Esc.
    Cancelled,

    /// Input closure before an answer is submitted, at any prompt.
    EndOfInput,
}

#[cfg(test)]
mod tests {
    use super::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};
    use crate::terminal::{
        ConfirmPrompt, InputPrompt, Message, PromptError, SelectItem, SelectPrompt, Terminal,
        TerminalCondition,
    };

    #[test]
    fn replays_typed_answers_and_uses_input_defaults() {
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Select(1),
            ScriptedAnswer::SelectDefault,
            ScriptedAnswer::Confirm(true),
            ScriptedAnswer::Input(String::new()),
        ]);
        let select = host_prompt();
        let preselected = host_prompt().with_default(1);
        let confirm = ConfirmPrompt::new("Write?", false);
        let input = InputPrompt::new("Site host").with_default("127.0.0.1");

        assert_eq!(terminal.condition(), TerminalCondition::Interactive);
        assert_eq!(terminal.select(&select).expect("choice"), 1);
        assert_eq!(terminal.select(&preselected).expect("default choice"), 1);
        assert!(terminal.confirm(&confirm).expect("decision"));
        assert_eq!(terminal.input(&input).expect("default"), "127.0.0.1");
        assert_eq!(
            terminal.prompts(),
            [
                ScriptedPrompt::Select(select),
                ScriptedPrompt::Select(preselected),
                ScriptedPrompt::Confirm(confirm),
                ScriptedPrompt::Input(input),
            ]
        );
        assert_eq!(terminal.remaining_answers(), 0);
    }

    #[test]
    #[should_panic(expected = "scripted Confirm(true) does not answer input prompt \"Site host\"")]
    fn answer_of_another_kind_fails_the_test() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        let _ = terminal.input(&InputPrompt::new("Site host"));
    }

    #[test]
    #[should_panic(expected = "scripted choice 2 is outside select prompt \"Host\"")]
    fn choice_outside_the_items_fails_the_test() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Select(2)]);

        let _ = terminal.select(&host_prompt());
    }

    #[test]
    fn non_tty_refuses_prompts_without_consuming_answers() {
        let mut terminal = ScriptedTerminal::new(
            TerminalCondition::NonInteractive,
            [ScriptedAnswer::Input("unused".to_owned())],
        );

        assert!(matches!(
            terminal.input(&InputPrompt::new("Site host")),
            Err(PromptError::NotInteractive)
        ));
        assert_eq!(terminal.remaining_answers(), 1);
        assert!(terminal.prompts().is_empty());
    }

    #[test]
    fn cancellation_and_end_of_input_stop_any_prompt_kind() {
        let mut terminal =
            ScriptedTerminal::interactive([ScriptedAnswer::Cancelled, ScriptedAnswer::EndOfInput]);

        assert!(matches!(
            terminal.select(&host_prompt()),
            Err(PromptError::Cancelled)
        ));
        assert!(matches!(
            terminal.confirm(&ConfirmPrompt::new("Write?", false)),
            Err(PromptError::EndOfInput)
        ));
        assert!(matches!(
            terminal.input(&InputPrompt::new("Site host")),
            Err(PromptError::EndOfInput)
        ));
        assert_eq!(terminal.prompts().len(), 3);
    }

    #[test]
    fn records_messages_and_their_plain_lines() {
        let mut terminal = ScriptedTerminal::interactive([]);
        let heading = Message::Heading("Initial site plan:".to_owned());
        let pairs = Message::KeyValues(vec![("host".to_owned(), "127.0.0.1".to_owned())]);

        terminal.message(&heading).expect("heading");
        terminal.message(&pairs).expect("pairs");

        assert_eq!(terminal.messages(), [heading, pairs]);
        assert_eq!(
            terminal.lines(),
            ["Initial site plan:", "  host  127.0.0.1"]
        );
    }

    fn host_prompt() -> SelectPrompt {
        SelectPrompt::new(
            "Host",
            [SelectItem::new("127.0.0.1"), SelectItem::new("192.0.2.10")],
        )
    }
}
