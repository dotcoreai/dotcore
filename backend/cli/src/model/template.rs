//! Prints a deploy payload for one catalog model, pre-filled with what a deployment starts from,
//! ready to edit and pass to `deploy`.
//!
//! The payload names the model, profile, node, display name and route, then lists the parameters
//! with one comment each giving its catalog summary, what it accepts and, when the server knows
//! it, what fits: the largest value, or for context per request the value every parallel request
//! reaches at once. Basic parameters are written with their starting value; advanced ones, secrets and a
//! total context that still follows the others are commented out. Overrides change the written
//! values, and the fit comments follow them.

use std::collections::BTreeMap;

use clap::Args;
use dotcore_proto::dotcore::{
    deployment::v1::MemoryFit,
    model::v1::{EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse},
};

use super::{
    payload::ModelPayload,
    selection::{Choices, Selection, select},
    view::{CONTEXT_PER_REQUEST, ParameterView, TOTAL_CONTEXT, ViewParameter},
};
use crate::{
    deploy::{PlainValue, Setting, ValueKind},
    management::{DeploymentGroup, Manager, ManagerArgs, call_failed},
    output::write_text,
};

/// Arguments for printing a deploy payload.
#[derive(Debug, Args)]
pub struct ModelTemplateArgs {
    /// Catalog model: its ID, name, or a unique part of either.
    #[arg(value_name = "MODEL")]
    model: String,

    #[command(flatten)]
    choices: ChoiceArgs,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// The target, profile and overrides a deploy command may name.
#[derive(Debug, Args)]
pub(super) struct ChoiceArgs {
    /// Profile ID or name; the first that fits the target by default.
    #[arg(long)]
    pub(super) profile: Option<String>,

    /// Node to deploy on; the first where a profile fits by default.
    #[arg(long)]
    pub(super) node: Option<String>,

    /// Execution target on the node, when it has several.
    #[arg(long)]
    pub(super) target: Option<String>,

    /// Set a parameter, or an option parameter as `option.<option-id>.<parameter-id>`.
    #[arg(long = "set", value_name = "ID=VALUE", value_parser = Setting::parse)]
    pub(super) settings: Vec<Setting>,
}

/// Prints the payload of one catalog model on the chosen target.
pub(crate) async fn model_template(arguments: ModelTemplateArgs) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let choices = &arguments.choices;
    let selection = select(
        &manager,
        &Choices {
            model: &arguments.model,
            profile: choices.profile.as_deref(),
            node: choices.node.as_deref(),
            target: choices.target.as_deref(),
        },
    )
    .await?;

    let mut payload = starting_payload(&selection);
    payload.apply(&choices.settings)?;
    let estimate = estimate(&manager, &selection, &payload).await?;
    write_text(&render(&selection, &payload, &estimate))
}

/// The payload of an untouched deployment of the selection: its model, profile, target, name and
/// a route named after the model, with no parameter changed.
pub(super) fn starting_payload(selection: &Selection) -> ModelPayload {
    ModelPayload {
        model: selection.model.model_id.clone(),
        profile: Some(selection.profile.profile_id.clone()),
        node: Some(selection.node_name.clone()),
        target: selection
            .shared_node
            .then(|| selection.placement.execution_target_id.clone()),
        name: Some(selection.model.display_name.clone()),
        route: Some(selection.model.model_id.clone()),
        ..ModelPayload::default()
    }
}

/// Has the server estimate the payload's configuration on the selected target.
pub(super) async fn estimate(
    manager: &Manager,
    selection: &Selection,
    payload: &ModelPayload,
) -> anyhow::Result<EstimateLocalModelResourcesResponse> {
    let configuration = payload.configuration(&selection.schema())?;
    Ok(manager
        .models()
        .estimate_local_model_resources(EstimateLocalModelResourcesRequest {
            model_id: selection.model.model_id.clone(),
            profile_id: selection.profile.profile_id.clone(),
            configuration: Some(configuration),
            placement: Some(selection.placement.clone()),
        })
        .await
        .map_err(call_failed(
            DeploymentGroup::Model,
            format!("estimate the memory of {}", selection.model.display_name),
        ))?
        .into_inner())
}

/// The payload text: a summary comment, the selection, then every shown parameter with its
/// guidance comment.
fn render(
    selection: &Selection,
    payload: &ModelPayload,
    estimate: &EstimateLocalModelResourcesResponse,
) -> String {
    let view = ParameterView::new(
        &selection.model,
        &selection.snapshot,
        selection.compatibility.as_ref(),
    );
    let hints = fit_hints(estimate, view.parallel_requests(&payload.parameters));
    let mut lines = vec![format!(
        "# {} · {} on {} {} · {}",
        selection.model.display_name,
        selection.profile.profile_id,
        selection.node_name,
        selection.target_name,
        fit_words(estimate),
    )];
    lines.extend(
        estimate
            .hardware_class_estimates
            .iter()
            .filter_map(|entry| entry.launch_rejection.as_deref())
            .map(|reason| format!("# A launch would reject these values: {reason}")),
    );

    lines.push(format!("model: {}", payload.model));
    for (key, value) in [
        ("profile", &payload.profile),
        ("node", &payload.node),
        ("target", &payload.target),
    ] {
        if let Some(value) = value {
            lines.push(format!("{key}: {}", quoted(value)));
        }
    }
    if let Some(name) = &payload.name {
        lines.push(format!("name: {}", quoted(name)));
    }
    match &payload.route {
        Some(route) => lines.push(format!("route: {}  # null keeps it hidden", quoted(route))),
        None => lines.push("route: null".to_owned()),
    }

    let (engine, options): (Vec<_>, Vec<_>) = view
        .parameters
        .iter()
        .partition(|parameter| !parameter.option);
    let entries = engine
        .iter()
        .map(|parameter| {
            entry(
                parameter,
                payload.parameters.get(&parameter.key),
                hints.get(parameter.key.as_str()).map(String::as_str),
                "  ",
            )
        })
        .collect::<Vec<_>>();
    section(&mut lines, "parameters", entries);

    let mut groups: Vec<(&str, &str, Vec<Entry>)> = Vec::new();
    for parameter in options {
        let (option, name) = parameter
            .key
            .split_once('.')
            .unwrap_or((&parameter.key, ""));
        let set = payload
            .options
            .get(option)
            .and_then(|values| values.get(name));
        let entry = entry(parameter, set, None, "    ");
        match groups.last_mut() {
            Some((last, _, entries)) if *last == option => entries.push(entry),
            _ => groups.push((option, parameter.summary.as_str(), vec![entry])),
        }
    }
    let option_entries = groups
        .into_iter()
        .map(|(option, summary, entries)| {
            let active = entries.iter().any(|entry| entry.active);
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(format!("  # {summary}"));
            }
            lines.push(format!("  {}{option}:", if active { "" } else { "# " }));
            for entry in entries {
                lines.extend(entry.lines);
            }
            Entry { active, lines }
        })
        .collect::<Vec<_>>();
    section(&mut lines, "options", option_entries);

    if !payload.custom_arguments.is_empty() {
        lines.push("customArguments:".to_owned());
        lines.extend(
            payload
                .custom_arguments
                .iter()
                .map(|argument| format!("  - {}", quoted(argument))),
        );
    }

    lines.push(String::new());
    lines.join("\n")
}

/// The lines of one parameter, and whether its value is written rather than commented out.
struct Entry {
    active: bool,
    lines: Vec<String>,
}

fn entry(
    parameter: &ViewParameter,
    set: Option<&PlainValue>,
    hint: Option<&str>,
    indent: &str,
) -> Entry {
    let mut comment = format!("{indent}# {}.", parameter.name);
    if !parameter.option && !parameter.summary.is_empty() {
        comment.push(' ');
        comment.push_str(parameter.summary.trim_end_matches('.'));
        comment.push('.');
    }
    comment.push(' ');
    comment.push_str(&parameter.accepts);
    if parameter.follows && set.is_none() {
        comment.push_str(". Follows context per request and parallel requests unless set");
    }
    if let Some(hint) = hint {
        comment.push_str(". ");
        comment.push_str(hint);
    }
    if parameter.advanced && set.is_none() {
        comment.push_str(". Advanced");
    }

    let value = set
        .map(|value| plain_text(value, parameter.kind))
        .or_else(|| {
            parameter
                .starting
                .as_deref()
                .map(|text| typed_text(text, parameter.kind))
        });
    let active = set.is_some() || (!parameter.advanced && !parameter.follows && value.is_some());
    let marker = if active { "" } else { "# " };
    let key = parameter
        .key
        .split_once('.')
        .filter(|_| parameter.option)
        .map_or(parameter.key.as_str(), |(_, name)| name);
    let line = match value {
        Some(value) => format!("{indent}{marker}{key}: {value}"),
        None => format!("{indent}# {key}:"),
    };
    Entry {
        active,
        lines: vec![comment, line],
    }
}

/// Adds a section of entries under its key, commenting the key out when no entry is written.
fn section(lines: &mut Vec<String>, key: &str, entries: Vec<Entry>) {
    if entries.is_empty() {
        return;
    }
    let active = entries.iter().any(|entry| entry.active);
    lines.push(format!("{}{key}:", if active { "" } else { "# " }));
    for entry in entries {
        lines.extend(entry.lines);
    }
}

/// What the estimate says fits for each parameter it bounds, by parameter ID, in the words the
/// template and the dry run show.
fn fit_hints(
    estimate: &EstimateLocalModelResourcesResponse,
    parallel_requests: Option<u64>,
) -> BTreeMap<&str, String> {
    let mut hints: BTreeMap<&str, String> = estimate
        .memory_parameter_fits
        .iter()
        .map(|fit| {
            (
                fit.parameter_id.as_str(),
                format!("Largest fit: {}", fit.largest_fit),
            )
        })
        .collect();
    if let Some(tokens) = estimate.largest_context_tokens_per_request {
        // The estimate's per-request value is the one every parallel request reaches at once,
        // not the most one request may use, so the hint says so.
        hints.insert(
            CONTEXT_PER_REQUEST,
            format!("Fits {}: {tokens}", all_at_once(parallel_requests)),
        );
    }
    if let Some(tokens) = estimate.largest_total_context_tokens {
        hints.insert(TOTAL_CONTEXT, format!("Largest fit: {tokens}"));
    }
    hints
}

/// The words for every parallel request running at once, naming their count when it is known.
pub(super) fn all_at_once(parallel_requests: Option<u64>) -> String {
    match parallel_requests {
        Some(count) => format!("all {count} at once"),
        None => "all requests at once".to_owned(),
    }
}

fn fit_words(estimate: &EstimateLocalModelResourcesResponse) -> &'static str {
    match estimate
        .placement_preview
        .as_ref()
        .map(|preview| preview.free_memory_fit())
    {
        Some(MemoryFit::Fits) => "fits now",
        Some(MemoryFit::Exceeds) => "exceeds free memory",
        _ => "fit unknown",
    }
}

/// A written value as YAML: text quoted, booleans and numbers as they are.
fn typed_text(text: &str, kind: ValueKind) -> String {
    match kind {
        ValueKind::Text => quoted(text),
        ValueKind::Boolean | ValueKind::UnsignedInteger => text.to_owned(),
    }
}

fn plain_text(value: &PlainValue, kind: ValueKind) -> String {
    match value {
        PlainValue::Boolean(value) => value.to_string(),
        PlainValue::Number(value) => value.to_string(),
        PlainValue::Text(text) => typed_text(text, kind),
    }
}

/// Text as a YAML scalar: plain when it reads back as the same text, else a JSON string.
fn quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
        && text.parse::<f64>().is_err()
        && !matches!(
            text,
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
        );
    if plain {
        text.to_owned()
    } else {
        serde_json::to_string(text).expect("a string serializes as JSON")
    }
}
