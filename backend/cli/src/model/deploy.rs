//! Deploys one local model from a payload, or from its starting values, after confirmation.
//!
//! The payload comes from a file or stdin; without one, `--yes` deploys the model's starting
//! values. Flags and overrides apply on top. A dry run has the server estimate the result and
//! creates nothing: it reports the fit, what fits (for context per request, the value every
//! parallel request reaches at once), the values sizing derives and any reason a launch would
//! reject them. Only an invalid payload or a launch rejection fails it.

use std::path::PathBuf;

use clap::Args;
use dotcore_proto::dotcore::{
    catalog::v1::ParameterValues,
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        EstimateLocalModelResourcesResponse, LocalModelDeploymentAdvisory,
        create_model_deployment_request,
    },
};
use serde::Serialize;

use super::{
    document,
    payload::ModelPayload,
    selection::{Choices, Selection, select},
    template::{ChoiceArgs, all_at_once, estimate, starting_payload},
    view::ParameterView,
};
use crate::{
    deploy::PayloadSource,
    management::{DeploymentGroup, ManagerArgs, call_failed},
    output::{OutputFormat, present, require_consent, write_yaml},
    terminal::{ConfirmPrompt, Message, PromptError, open_terminal},
};

/// Arguments for deploying a model.
#[derive(Debug, Args)]
pub struct ModelDeployArgs {
    /// Catalog model: its ID, name, or a unique part of either; replaces the payload's.
    #[arg(value_name = "MODEL", required_unless_present = "file")]
    model: Option<String>,

    /// Payload file to deploy, or `-` for stdin.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Option<PathBuf>,

    #[command(flatten)]
    choices: ChoiceArgs,

    /// Display name of the deployment.
    #[arg(long)]
    name: Option<String>,

    /// Name of the route created with the deployment.
    #[arg(long)]
    route: Option<String>,

    /// Estimate the deployment and report its fit without creating it.
    #[arg(long)]
    dry_run: bool,

    /// Deploy without asking for confirmation.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Deploys the payload, or reports its estimate on a dry run.
pub(crate) async fn model_deploy(
    arguments: ModelDeployArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let from_file = arguments.file.is_some();
    if !from_file && !arguments.yes && !arguments.dry_run {
        anyhow::bail!(
            "deploying needs a payload; pass -f <payload>, or --yes to deploy the starting values"
        );
    }
    let mut payload = match &arguments.file {
        Some(path) => ModelPayload::parse(&PayloadSource::from(path.clone()).read()?)?,
        None => ModelPayload::default(),
    };
    if let Some(model) = &arguments.model {
        payload.model = model.clone();
    }
    let choices = &arguments.choices;
    for (field, flag) in [
        (&mut payload.profile, &choices.profile),
        (&mut payload.node, &choices.node),
        (&mut payload.target, &choices.target),
        (&mut payload.name, &arguments.name),
        (&mut payload.route, &arguments.route),
    ] {
        if flag.is_some() {
            field.clone_from(flag);
        }
    }

    let manager = arguments.manager.connect().await?;
    let selection = select(
        &manager,
        &Choices {
            model: &payload.model,
            profile: payload.profile.as_deref(),
            node: payload.node.as_deref(),
            target: payload.target.as_deref(),
        },
    )
    .await?;
    if !from_file {
        let flags = payload;
        payload = starting_payload(&selection);
        payload.name = flags.name.or(payload.name);
        payload.route = flags.route.or(payload.route);
    }
    payload.apply(&choices.settings)?;
    let configuration = payload.configuration(&selection.schema())?;

    if arguments.dry_run {
        let estimate = estimate(&manager, &selection, &payload).await?;
        return report_dry_run(&selection, payload, estimate, format);
    }

    require_consent(
        arguments.yes,
        format,
        "deploying needs a terminal to confirm; run in a terminal or pass --yes",
        "deploying needs confirmation, which --yaml never asks for; pass --yes to deploy",
    )?;
    let name = payload
        .name
        .clone()
        .unwrap_or_else(|| selection.model.display_name.clone());
    if !arguments.yes {
        let question = format!(
            "Deploy {name} ({}) on {} {}?",
            selection.profile.profile_id, selection.node_name, selection.target_name
        );
        if !open_terminal().confirm(&ConfirmPrompt::new(question, true))? {
            return Err(PromptError::Cancelled.into());
        }
    }

    let deployment = manager
        .models()
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: selection.model.model_id.clone(),
                    profile_id: selection.profile.profile_id.clone(),
                    placement: Some(selection.placement.clone()),
                    configuration: Some(configuration),
                },
            )),
            exposure: Some(payload.exposure()),
            model_deployment_name: payload.name.clone(),
        })
        .await
        .map_err(call_failed(
            DeploymentGroup::Model,
            format!("deploy {name}"),
        ))?
        .into_inner()
        .deployment
        .unwrap_or_default();

    if format == OutputFormat::Yaml {
        let nodes = manager.node_names().await;
        return write_yaml(&document(deployment, &nodes, false));
    }
    present(&[Message::Success(format!(
        "Deploying {} ({})",
        if deployment.model_deployment_name.is_empty() {
            &name
        } else {
            &deployment.model_deployment_name
        },
        deployment.model_deployment_id
    ))])
}

/// The YAML document of a dry run: the payload with every choice resolved, the values sizing
/// derived, the estimate as returned and the preview's advisories.
#[derive(Serialize)]
struct DryRunDocument<'a> {
    payload: ModelPayload,

    derived: Vec<String>,

    estimate: &'a EstimateLocalModelResourcesResponse,

    advisories: Vec<String>,
}

/// Prints the dry run's report, failing when a launch would reject the values.
fn report_dry_run(
    selection: &Selection,
    mut payload: ModelPayload,
    estimate: EstimateLocalModelResourcesResponse,
    format: OutputFormat,
) -> anyhow::Result<()> {
    payload.model.clone_from(&selection.model.model_id);
    payload.profile = Some(selection.profile.profile_id.clone());
    payload.node = Some(selection.placement.site_node_id.clone());
    payload.target = Some(selection.placement.execution_target_id.clone());
    payload.normalize(&selection.schema());
    let parallel_requests = ParameterView::new(
        &selection.model,
        &selection.snapshot,
        selection.compatibility.as_ref(),
    )
    .parallel_requests(&payload.parameters);
    let derived = estimate
        .hardware_class_estimates
        .iter()
        .flat_map(|entry| entry.derived_engine_parameter_values.iter())
        .flat_map(|values: &ParameterValues| values.values.iter())
        .map(|value| value.parameter_id.clone())
        .collect::<Vec<_>>();
    let advisories = selection
        .advisories
        .iter()
        .map(|advisory| advisory.as_str_name().to_owned())
        .collect::<Vec<_>>();
    let rejection = estimate
        .hardware_class_estimates
        .iter()
        .find_map(|entry| entry.launch_rejection.clone());

    if format == OutputFormat::Yaml {
        write_yaml(&DryRunDocument {
            payload,
            derived,
            estimate: &estimate,
            advisories,
        })?;
    } else {
        present(&dry_run_messages(
            selection,
            &estimate,
            parallel_requests,
            &advisories_words(selection),
        ))?;
    }
    match rejection {
        Some(reason) => anyhow::bail!("a launch would reject these values: {reason}"),
        None => Ok(()),
    }
}

fn dry_run_messages(
    selection: &Selection,
    estimate: &EstimateLocalModelResourcesResponse,
    parallel_requests: Option<u64>,
    advisories: &[&str],
) -> Vec<Message> {
    use dotcore_proto::dotcore::deployment::v1::MemoryFit;

    let place = format!(
        "{} ({}) on {} {}",
        selection.model.display_name,
        selection.profile.profile_id,
        selection.node_name,
        selection.target_name
    );
    let mut messages = vec![match estimate
        .placement_preview
        .as_ref()
        .map(|preview| preview.free_memory_fit())
    {
        Some(MemoryFit::Fits) => Message::Success(format!("{place} fits now")),
        Some(MemoryFit::Exceeds) => Message::Warning(format!("{place} exceeds the free memory")),
        _ => Message::Info(format!("{place}: fit unknown")),
    }];

    let mut facts = Vec::new();
    if let Some(tokens) = estimate.largest_context_tokens_per_request {
        facts.push((
            format!(
                "Context per request fitting {}",
                all_at_once(parallel_requests)
            ),
            tokens.to_string(),
        ));
    }
    if let Some(tokens) = estimate.largest_total_context_tokens {
        facts.push(("Largest total context".to_owned(), tokens.to_string()));
    }
    for fit in &estimate.memory_parameter_fits {
        facts.push((
            format!("Largest {}", fit.parameter_id),
            fit.largest_fit.to_string(),
        ));
    }
    for value in estimate
        .hardware_class_estimates
        .iter()
        .flat_map(|entry| entry.derived_engine_parameter_values.iter())
        .flat_map(|values| values.values.iter())
    {
        if let Some(text) = value
            .value
            .as_ref()
            .map(dotcore_catalog::typed_parameters::canonical_text)
        {
            facts.push((format!("Derived {}", value.parameter_id), text));
        }
    }
    if !facts.is_empty() {
        messages.push(Message::KeyValues(facts));
    }
    messages.extend(
        advisories
            .iter()
            .map(|advisory| Message::Warning((*advisory).to_owned())),
    );
    messages
}

fn advisories_words(selection: &Selection) -> Vec<&'static str> {
    selection
        .advisories
        .iter()
        .filter_map(|advisory| match advisory {
            LocalModelDeploymentAdvisory::HostKvOffloadSharesMemory => {
                Some("Host KV cache offload adds no memory on a target that shares host memory")
            }
            LocalModelDeploymentAdvisory::UnifiedMemoryPoolBelowDeviceMinimum => {
                Some("The target's shared memory is below the model's per-device memory minimum")
            }
            LocalModelDeploymentAdvisory::Unspecified => None,
        })
        .collect()
}
