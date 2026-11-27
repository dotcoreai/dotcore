//! Forms: one step of a guided flow whose fields are all shown and edited together, with a
//! footer the caller keeps up to date while the operator types.
//!
//! The caller describes the fields and hears about each edit through a session, which checks the
//! values, may start slower work for them, and later hands back its results as refreshed fields
//! and a new footer. The terminal only lays the form out and reads keys; it never interprets the
//! values. A form ends by going to the next step or back to the previous one, or by a cancel.

use super::{InputPrompt, Message, PromptError, SelectItem, SelectPrompt, Terminal};

/// One step of fields edited together, between the steps done before and the ones still to come.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FormPrompt {
    /// The step's title, which also opens its summary line.
    pub title: String,

    /// Every step of the flow, in order, which the form shows as a progress line.
    pub steps: Vec<String>,

    /// The position of this step in `steps`.
    pub current: usize,

    /// The fields, in the order the operator moves through them.
    pub fields: Vec<FormField>,

    /// Read-only lines under the fields, such as a review's facts and its warnings.
    pub notes: Vec<Message>,

    /// What finishing the form does, such as `Deploy`, shown as its button; none goes to the next
    /// step.
    pub action: Option<String>,

    /// Whether the form can go back to a previous step; without one, going back cancels.
    pub can_go_back: bool,
}

/// One field of a form: what it is, its value as typed and what the caller says about it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FormField {
    /// What the field sets, such as `Context per request`.
    pub label: String,

    /// The value as typed; empty leaves it to `placeholder`.
    pub value: String,

    /// The value the field takes while left empty, shown dimmed, such as a value that follows
    /// others.
    pub placeholder: Option<String>,

    /// What the field accepts, such as a range, shown after the value.
    pub accepts: String,

    /// One sentence saying what the field does, shown while it has the focus.
    pub summary: String,

    /// The values the field cycles through instead of being typed; empty takes any text.
    pub choices: Vec<String>,

    /// A suggested value with the words presenting it, such as the largest value that fits.
    pub hint: Option<FieldHint>,

    /// Why the value is not accepted, shown under the field.
    pub error: Option<String>,

    /// Whether the value is hidden as it is typed, such as a credential.
    pub secret: bool,
}

impl FormField {
    /// The value the field holds: the typed one, else the placeholder.
    pub fn effective(&self) -> &str {
        match (&self.value, &self.placeholder) {
            (value, Some(placeholder)) if value.is_empty() => placeholder,
            (value, _) => value,
        }
    }
}

/// A value the operator can take for a field with one key, and the words presenting it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldHint {
    /// The words shown, such as `Fits all 4 at once: 49152`.
    pub text: String,

    /// The value taking the hint sets.
    pub value: String,
}

/// The footer under a form: a title row, then the caller's lines, such as memory bars and a
/// verdict.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Footer {
    /// What the footer is about, such as the machine a deployment would run on.
    pub title: String,

    /// Whether newer lines are on their way; the current ones stay shown meanwhile.
    pub updating: bool,

    /// The lines, top first; the last is kept when the terminal is too short for all of them.
    pub lines: Vec<Message>,
}

/// How a form ended, other than a cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormAnswer {
    /// The operator finished the step.
    Next,

    /// The operator went back to the previous step.
    Back,
}

/// The caller's side of an open form: it checks the values and keeps the footer and the fields'
/// hints current.
pub trait FormSession {
    /// The values changed: sets each field's error and hint for them, and may start slower work
    /// whose result [`FormSession::refresh`] applies.
    fn edited(&mut self, fields: &mut [FormField]);

    /// Applies the results of finished work to the fields, first waiting for the pending work
    /// when `wait` is set, and returns whether anything changed.
    fn refresh(&mut self, fields: &mut [FormField], wait: bool) -> bool;

    /// Whether work started by an edit is still running.
    fn pending(&self) -> bool;

    /// The footer for the latest results.
    fn footer(&self) -> Option<Footer>;
}

/// Asks a form's fields one prompt at a time, for terminals that cannot draw the whole form:
/// the notes first, then each field with its summary and hint, asked again while its value is not
/// accepted. A form with an action then asks whether to take it, go back or cancel. The footer is
/// not shown; the caller states what it says.
pub(super) fn ask_each_field(
    terminal: &mut dyn Terminal,
    prompt: &mut FormPrompt,
    session: &mut dyn FormSession,
) -> Result<FormAnswer, PromptError> {
    terminal.prompt_context(&Message::Heading(prompt.title.clone()))?;
    for note in &prompt.notes {
        terminal.prompt_context(note)?;
    }
    for index in 0..prompt.fields.len() {
        loop {
            ask_field(terminal, &mut prompt.fields[index])?;
            session.edited(&mut prompt.fields);
            settle(session, &mut prompt.fields);
            match &prompt.fields[index].error {
                Some(error) => terminal.prompt_context(&Message::Error(error.clone()))?,
                None => break,
            }
        }
    }

    let Some(action) = &prompt.action else {
        return Ok(FormAnswer::Next);
    };
    let mut choices = vec![(action.clone(), Some(FormAnswer::Next))];
    if prompt.can_go_back {
        choices.push(("Back".to_owned(), Some(FormAnswer::Back)));
    }
    choices.push(("Cancel".to_owned(), None));
    let index = terminal.select(&SelectPrompt::new(
        format!("{action}?"),
        choices
            .iter()
            .map(|(label, _)| SelectItem::new(label.clone())),
    ))?;
    choices[index].1.ok_or(PromptError::Cancelled)
}

/// Waits for the work an edit started, so the next field's hint reflects it.
pub(super) fn settle(session: &mut dyn FormSession, fields: &mut [FormField]) {
    while session.pending() {
        session.refresh(fields, true);
    }
}

/// Asks one field with the prompt its kind takes, keeping its value when the answer is empty.
fn ask_field(terminal: &mut dyn Terminal, field: &mut FormField) -> Result<(), PromptError> {
    if !field.summary.is_empty() {
        terminal.prompt_context(&Message::Hint(field.summary.clone()))?;
    }
    if let Some(hint) = &field.hint {
        terminal.prompt_context(&Message::Hint(hint.text.clone()))?;
    }
    if !field.choices.is_empty() {
        let default = field
            .choices
            .iter()
            .position(|choice| choice == field.effective())
            .unwrap_or(0);
        let index = terminal.select(
            &SelectPrompt::new(
                field.label.clone(),
                field.choices.iter().map(SelectItem::new),
            )
            .with_default(default),
        )?;
        field.value = field.choices[index].clone();
        return Ok(());
    }

    let title = if field.accepts.is_empty() {
        field.label.clone()
    } else {
        format!("{} ({})", field.label, field.accepts)
    };
    if field.secret {
        let typed = terminal.secret(&InputPrompt::new(title))?;
        if !typed.is_empty() {
            field.value = typed;
        }
        return Ok(());
    }
    let mut question = InputPrompt::new(title);
    let shown = field.effective();
    if !shown.is_empty() {
        question = question.with_default(shown.to_owned());
    }
    let typed = terminal.input(&question)?;
    if typed != field.effective() {
        field.value = typed;
    }
    Ok(())
}
