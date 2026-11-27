//! The deployment the guided deploy is building: the chosen profile and machine, the payload,
//! which values still follow others, and the server's latest estimate of it.
//!
//! It turns a step's parameters into form fields and the operator's typed values back into the
//! payload, checking only each value's type, as a payload file would be. A value equal to its
//! starting one sets nothing, the total context follows context per request and parallel requests
//! until set, and the route follows the name until edited. Every fit it shows comes from the
//! estimate.

use dotcore_proto::dotcore::model::v1::{
    EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
};

use super::{
    super::{
        payload::ModelPayload,
        selection::Selection,
        template::{estimate_request, fit_hints, starting_payload},
        view::{
            CONTEXT_PER_REQUEST, PARALLEL_REQUESTS, ParameterView, TOTAL_CONTEXT, ViewParameter,
        },
    },
    Step, WizardDefaults, profile_label, route_name,
};
use crate::{
    deploy::{PlainValue, ValueKind},
    terminal::{FieldHint, FormField},
};

/// Most estimates kept, so going back to earlier values needs no new call.
const KEPT_ESTIMATES: usize = 16;

/// The deployment being built and what the server last said about it.
pub(super) struct Draft {
    pub(super) selection: Selection,
    pub(super) view: ParameterView,
    pub(super) payload: ModelPayload,

    /// Whether the total context follows context per request and parallel requests.
    pub(super) total_follows: bool,

    /// Whether the name was typed rather than taken from the model.
    pub(super) name_edited: bool,

    /// Whether the route follows the name.
    pub(super) route_follows: bool,

    /// The estimate shown, with the request it answered.
    pub(super) estimate: Option<Estimate>,

    /// Recent estimates, newest last, so values seen before need no new call.
    estimates: Vec<Estimate>,
}

/// One estimate and the request it answered.
pub(super) type Estimate = (
    EstimateLocalModelResourcesRequest,
    EstimateLocalModelResourcesResponse,
);

/// What a form field sets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FieldKey {
    /// An engine parameter, or an option parameter as `<option>.<parameter>`.
    Parameter(String),

    /// The deployment's display name.
    Name,

    /// The route created with the deployment.
    Route,
}

impl Draft {
    /// Starts from the selection's starting values and the command line's overrides.
    pub(super) fn new(selection: Selection, defaults: &WizardDefaults) -> anyhow::Result<Self> {
        let mut draft = Self {
            view: ParameterView {
                parameters: Vec::new(),
            },
            payload: ModelPayload::default(),
            selection,
            total_follows: true,
            name_edited: defaults.name.is_some(),
            route_follows: defaults.route.is_none(),
            estimate: None,
            estimates: Vec::new(),
        };
        draft.payload.name = defaults.name.clone();
        draft.payload.route = defaults.route.clone();
        draft.reset(defaults)?;
        Ok(draft)
    }

    /// Starts the values over for a new selection, keeping the name and route.
    pub(super) fn select(
        &mut self,
        selection: Selection,
        defaults: &WizardDefaults,
    ) -> anyhow::Result<()> {
        self.selection = selection;
        self.estimates.clear();
        self.reset(defaults)
    }

    fn reset(&mut self, defaults: &WizardDefaults) -> anyhow::Result<()> {
        let (name, route) = (self.payload.name.take(), self.payload.route.take());
        self.payload = starting_payload(&self.selection);
        self.payload.name = match name {
            Some(name) if self.name_edited => Some(name),
            _ => Some(self.selection.model.display_name.clone()),
        };
        self.payload.route = if self.route_follows {
            self.payload.name.as_deref().map(route_name)
        } else {
            route
        };
        self.payload.apply(&defaults.settings)?;
        self.payload.configuration(&self.selection.schema())?;

        self.view = ParameterView::new(
            &self.selection.model,
            &self.selection.snapshot,
            self.selection.compatibility.as_ref(),
        );
        let total = self.parameter(TOTAL_CONTEXT);
        let capped = total
            .filter(|total| !total.follows)
            .and_then(|total| total.starting.clone());
        self.total_follows = total.is_none_or(|total| total.follows);
        // A total context the server capped below what the others make stays at the cap.
        if let Some(starting) = capped {
            self.payload
                .parameters
                .entry(TOTAL_CONTEXT.to_owned())
                .or_insert(PlainValue::Text(starting));
            self.payload.normalize(&self.selection.schema());
        }
        if self.payload.parameters.contains_key(TOTAL_CONTEXT) {
            self.total_follows = false;
        }
        self.estimate = None;
        Ok(())
    }

    /// The request estimating the values as they stand.
    pub(super) fn request(&self) -> anyhow::Result<EstimateLocalModelResourcesRequest> {
        Ok(estimate_request(&self.selection, &self.payload)?)
    }

    /// A kept estimate answering `request`, if any.
    pub(super) fn kept(&self, request: &EstimateLocalModelResourcesRequest) -> Option<&Estimate> {
        self.estimates
            .iter()
            .rev()
            .find(|(kept, _)| kept == request)
    }

    /// Keeps an estimate, showing it when it answers the values as they stand.
    pub(super) fn keep(&mut self, estimate: Estimate) {
        if self.request().is_ok_and(|request| request == estimate.0) {
            self.estimate = Some(estimate.clone());
        }
        self.estimates.retain(|(kept, _)| *kept != estimate.0);
        self.estimates.push(estimate);
        if self.estimates.len() > KEPT_ESTIMATES {
            self.estimates.remove(0);
        }
    }

    /// The fields a step asks, in order, each with what it sets. What the command line answered
    /// is not asked.
    pub(super) fn fields(
        &self,
        step: Step,
        defaults: &WizardDefaults,
    ) -> (Vec<FieldKey>, Vec<FormField>) {
        let keys = match step {
            Step::Capabilities => self.capability_keys(defaults),
            Step::Context => self.context_keys(defaults),
            Step::Review => {
                let mut keys = Vec::new();
                if defaults.name.is_none() {
                    keys.push(FieldKey::Name);
                }
                if defaults.route.is_none() {
                    keys.push(FieldKey::Route);
                }
                keys
            }
            Step::ModelAndMachine => Vec::new(),
        };
        let mut fields = keys.iter().map(|key| self.field(key)).collect::<Vec<_>>();
        self.describe(&keys, &mut fields);
        (keys, fields)
    }

    /// Takes the fields' values into the payload, marking each value whose type is wrong, and
    /// returns whether every value was taken.
    pub(super) fn take(&mut self, keys: &[FieldKey], fields: &mut [FormField]) -> bool {
        let mut taken = true;
        for (key, field) in keys.iter().zip(fields.iter_mut()) {
            field.error = None;
            match key {
                FieldKey::Name => {
                    self.name_edited = !field.value.is_empty();
                    self.payload.name = Some(field.effective().to_owned());
                }
                FieldKey::Route => {
                    self.route_follows = field.value.is_empty();
                    self.payload.route = Some(field.effective().to_owned());
                }
                FieldKey::Parameter(key) => {
                    let Some(parameter) = self.parameter(key).cloned() else {
                        continue;
                    };
                    match answer(&parameter, field.value.trim()) {
                        Ok(value) => {
                            if key == TOTAL_CONTEXT {
                                self.total_follows = value.is_none() && parameter.follows;
                            }
                            self.record(&parameter, value);
                        }
                        Err(error) => {
                            field.error = Some(error);
                            taken = false;
                        }
                    }
                }
            }
        }
        if self.route_follows {
            self.payload.route = self.payload.name.as_deref().map(route_name);
        }
        taken
    }

    /// Updates what the fields show besides their typed values: the values they take while
    /// empty, what they accept and the server's hints.
    pub(super) fn describe(&self, keys: &[FieldKey], fields: &mut [FormField]) {
        // The route follows the name as typed, before it is taken into the payload.
        let name = keys
            .iter()
            .position(|key| *key == FieldKey::Name)
            .map_or_else(
                || self.payload.name.clone().unwrap_or_default(),
                |index| match fields[index].value.as_str() {
                    "" => self.selection.model.display_name.clone(),
                    typed => typed.to_owned(),
                },
            );
        for (key, field) in keys.iter().zip(fields.iter_mut()) {
            match key {
                FieldKey::Name => {
                    field.placeholder = Some(self.selection.model.display_name.clone());
                }
                FieldKey::Route => {
                    field.placeholder = Some(route_name(&name));
                    field.accepts = if field.value.is_empty() {
                        "follows the name".to_owned()
                    } else {
                        String::new()
                    };
                }
                FieldKey::Parameter(key) => {
                    let Some(parameter) = self.parameter(key) else {
                        continue;
                    };
                    field.hint = self.hint(key);
                    if key == TOTAL_CONTEXT && parameter.follows {
                        field.placeholder = self.following_total();
                        field.accepts = if field.value.is_empty() {
                            "follows the two above".to_owned()
                        } else {
                            format!("{} · empty follows", parameter.accepts)
                        };
                    }
                }
            }
        }
    }

    /// One step's answers on one line, for its summary.
    pub(super) fn summary(&self, step: Step) -> String {
        match step {
            Step::ModelAndMachine => format!(
                "{} · {} on {} {}",
                self.selection.model.display_name,
                profile_label(&self.selection.profile),
                self.selection.node_name,
                self.selection.target_name
            ),
            Step::Capabilities => self
                .capabilities()
                .map(|parameter| format!("{} {}", parameter.name, self.shown(parameter)))
                .collect::<Vec<_>>()
                .join(" · "),
            Step::Context => self.context_summary(),
            Step::Review => format!(
                "{} · route {}",
                self.payload.name.clone().unwrap_or_default(),
                self.payload.route.clone().unwrap_or_default()
            ),
        }
    }

    /// The review's facts: where it runs, the profile, the context, the capabilities and every
    /// other changed parameter.
    pub(super) fn review_facts(&self) -> Vec<(String, String)> {
        let mut facts = vec![
            (
                "Runs on".to_owned(),
                format!(
                    "{} {}",
                    self.selection.node_name, self.selection.target_name
                ),
            ),
            (
                "Profile".to_owned(),
                profile_label(&self.selection.profile).to_owned(),
            ),
            ("Context".to_owned(), self.context_summary()),
        ];
        let changed = self.view.parameters.iter().filter(|parameter| {
            !parameter.option
                && !is_context(&parameter.key)
                && self.payload.parameters.contains_key(&parameter.key)
        });
        for parameter in self.capabilities().chain(changed) {
            facts.push((parameter.name.clone(), self.shown(parameter)));
        }
        facts
    }

    /// Whether the Capabilities step has a basic option to ask.
    pub(super) fn asks_capabilities(&self, defaults: &WizardDefaults) -> bool {
        !self.capability_keys(defaults).is_empty()
    }

    /// The requests the estimate's per-request fit counts.
    pub(super) fn parallel_requests(&self) -> Option<u64> {
        self.view.parallel_requests(&self.payload.parameters)
    }

    /// The context per request, parallel requests and total context on one line.
    fn context_summary(&self) -> String {
        let value = |key: &str| {
            self.parameter(key).and_then(|parameter| {
                if key == TOTAL_CONTEXT && self.total_follows {
                    self.following_total()
                } else {
                    Some(self.shown(parameter))
                }
            })
        };
        [
            (CONTEXT_PER_REQUEST, "per request"),
            (PARALLEL_REQUESTS, "parallel"),
            (TOTAL_CONTEXT, "total"),
        ]
        .into_iter()
        .filter_map(|(key, words)| {
            value(key)
                .filter(|value| !value.is_empty())
                .map(|value| format!("{value} {words}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
    }

    /// A parameter's field as it opens: the value set so far, else empty with the starting value
    /// in its place.
    fn field(&self, key: &FieldKey) -> FormField {
        match key {
            FieldKey::Name => FormField {
                label: "Name".to_owned(),
                value: if self.name_edited {
                    self.payload.name.clone().unwrap_or_default()
                } else {
                    String::new()
                },
                summary: "The name the console and dotc list the deployment by.".to_owned(),
                ..FormField::default()
            },
            FieldKey::Route => FormField {
                label: "Route".to_owned(),
                value: if self.route_follows {
                    String::new()
                } else {
                    self.payload.route.clone().unwrap_or_default()
                },
                summary: "The model name callers use.".to_owned(),
                ..FormField::default()
            },
            FieldKey::Parameter(key) => {
                let Some(parameter) = self.parameter(key) else {
                    return FormField::default();
                };
                let set = if key == TOTAL_CONTEXT && self.total_follows {
                    None
                } else {
                    self.set_value(key)
                };
                FormField {
                    label: parameter.name.clone(),
                    value: set.map(plain_text).unwrap_or_default(),
                    placeholder: if parameter.secret {
                        Some("generated".to_owned())
                    } else {
                        parameter.starting.clone()
                    },
                    accepts: parameter.accepts.clone(),
                    summary: parameter.summary.clone(),
                    choices: if parameter.kind == ValueKind::Boolean {
                        vec!["true".to_owned(), "false".to_owned()]
                    } else {
                        parameter.choices.clone()
                    },
                    secret: parameter.secret,
                    ..FormField::default()
                }
            }
        }
    }

    fn record(&mut self, parameter: &ViewParameter, value: Option<PlainValue>) {
        match parameter.key.split_once('.').filter(|_| parameter.option) {
            Some((option, name)) => {
                let values = self.payload.options.entry(option.to_owned()).or_default();
                match value {
                    Some(value) => values.insert(name.to_owned(), value),
                    None => values.remove(name),
                };
                if values.is_empty() {
                    self.payload.options.remove(option);
                }
            }
            None => {
                match value {
                    Some(value) => self.payload.parameters.insert(parameter.key.clone(), value),
                    None => self.payload.parameters.remove(&parameter.key),
                };
            }
        }
    }

    /// A parameter's value as the operator reads it: the value set, else the starting one. A
    /// secret only says whether it was set.
    fn shown(&self, parameter: &ViewParameter) -> String {
        let set = self.set_value(&parameter.key);
        if parameter.secret {
            return if set.is_some() { "set" } else { "generated" }.to_owned();
        }
        set.map(plain_text)
            .or_else(|| parameter.starting.clone())
            .unwrap_or_default()
    }

    /// The value the payload sets for a parameter key, if any.
    fn set_value(&self, key: &str) -> Option<&PlainValue> {
        match key.split_once('.') {
            Some((option, name))
                if self
                    .parameter(key)
                    .is_some_and(|parameter| parameter.option) =>
            {
                self.payload
                    .options
                    .get(option)
                    .and_then(|values| values.get(name))
            }
            _ => self.payload.parameters.get(key),
        }
    }

    /// The total context while it follows: the value the server derived in the latest estimate,
    /// else its starting value.
    fn following_total(&self) -> Option<String> {
        self.estimate
            .iter()
            .flat_map(|(_, estimate)| &estimate.hardware_class_estimates)
            .flat_map(|entry| entry.derived_engine_parameter_values.iter())
            .flat_map(|values| &values.values)
            .find(|value| value.parameter_id == TOTAL_CONTEXT)
            .and_then(|value| value.value.as_ref())
            .map(dotcore_catalog::typed_parameters::canonical_text)
            .or_else(|| {
                self.parameter(TOTAL_CONTEXT)
                    .and_then(|parameter| parameter.starting.clone())
            })
    }

    /// What the latest estimate says fits a parameter.
    fn hint(&self, key: &str) -> Option<FieldHint> {
        let (_, estimate) = self.estimate.as_ref()?;
        fit_hints(estimate, self.parallel_requests()).remove(key)
    }

    fn capabilities(&self) -> impl Iterator<Item = &ViewParameter> {
        self.view
            .parameters
            .iter()
            .filter(|parameter| parameter.option && !parameter.advanced)
    }

    /// The basic options, in catalog order.
    fn capability_keys(&self, defaults: &WizardDefaults) -> Vec<FieldKey> {
        self.capabilities()
            .filter(|parameter| !preanswered(defaults, parameter))
            .map(|parameter| FieldKey::Parameter(parameter.key.clone()))
            .collect()
    }

    /// Parallel requests, context per request and total context, then the other basic engine
    /// parameters in schema order.
    fn context_keys(&self, defaults: &WizardDefaults) -> Vec<FieldKey> {
        let context = [PARALLEL_REQUESTS, CONTEXT_PER_REQUEST, TOTAL_CONTEXT]
            .into_iter()
            .filter_map(|key| self.parameter(key));
        let others = self.view.parameters.iter().filter(|parameter| {
            !parameter.option && !parameter.advanced && !is_context(&parameter.key)
        });
        context
            .chain(others)
            .filter(|parameter| !preanswered(defaults, parameter))
            .map(|parameter| FieldKey::Parameter(parameter.key.clone()))
            .collect()
    }

    fn parameter(&self, key: &str) -> Option<&ViewParameter> {
        self.view
            .parameters
            .iter()
            .find(|parameter| parameter.key == key)
    }
}

/// A value as the operator reads it.
pub(super) fn plain_text(value: &PlainValue) -> String {
    match value {
        PlainValue::Boolean(value) => value.to_string(),
        PlainValue::Number(value) => value.to_string(),
        PlainValue::Text(text) => text.clone(),
    }
}

/// The value a typed answer sets: none for an empty answer or the starting value, else the
/// value checked against the parameter's type, or why it does not fit that type.
fn answer(parameter: &ViewParameter, typed: &str) -> Result<Option<PlainValue>, String> {
    if typed.is_empty() {
        return Ok(None);
    }
    let value = PlainValue::Text(typed.to_owned())
        .typed(parameter.kind)
        .map(PlainValue::from)
        .map_err(|error| format!("{} {error}", parameter.name))?;
    // A total context that follows keeps a typed value even when it equals the starting one.
    if parameter.key != TOTAL_CONTEXT && Some(plain_text(&value)) == parameter.starting {
        return Ok(None);
    }
    Ok(Some(value))
}

/// Whether an override on the command line answers the parameter.
fn preanswered(defaults: &WizardDefaults, parameter: &ViewParameter) -> bool {
    defaults.settings.iter().any(|setting| {
        setting
            .key
            .strip_prefix("option.")
            .filter(|_| parameter.option)
            .unwrap_or(&setting.key)
            == parameter.key
    })
}

/// The parameters of the context fields the Context step asks first.
fn is_context(key: &str) -> bool {
    [PARALLEL_REQUESTS, CONTEXT_PER_REQUEST, TOTAL_CONTEXT].contains(&key)
}
