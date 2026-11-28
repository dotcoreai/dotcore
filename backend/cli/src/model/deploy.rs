//! Deploys one local model step by step on a terminal, or from a payload, or from its starting
//! values after confirmation.
//!
//! On a terminal with nothing to deploy from, a guided flow asks for each choice. Otherwise the
//! payload comes from a file or stdin; without one, the model's starting values are deployed when
//! the operator agreed beforehand. Choices and overrides named on the command line apply on top,
//! and every way builds the same payload and sends the same request. A dry run has the server
//! estimate the result and creates nothing: it reports the fit, how full each memory would be,
//! what fits (for context per request, the value every parallel request reaches at once), the
//! values sizing derives and any reason a launch would reject them. Only an invalid payload or a
//! launch rejection fails it.

use std::path::PathBuf;

use clap::Args;
use dotcore_proto::dotcore::{
    catalog::v1::ParameterValues,
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        EstimateLocalModelResourcesResponse, LocalModelDeploymentAdvisory, ModelDeployment,
        create_model_deployment_request,
    },
};
use serde::Serialize;

use super::{
    document,
    memory::memory_block,
    payload::{ModelPayload, PayloadError},
    selection::{Choices, Selection, select},
    site::DeploySite,
    template::{ChoiceArgs, estimate, starting_payload},
    view::ParameterView,
    wizard::{self, WizardDefaults},
};
use crate::{
    deploy::{PayloadSource, Setting},
    management::{DeploymentGroup, Manager, ManagerArgs, call_failed},
    output::{OutputFormat, present, require_consent, write_yaml},
    terminal::{
        ConfirmPrompt, Message, PromptError, TerminalCondition, controlling_terminal_condition,
        open_terminal,
    },
};

/// Arguments for deploying a model.
#[derive(Debug, Args)]
pub struct ModelDeployArgs {
    /// Catalog model: its ID, name, or a unique part of either; replaces the payload's. Chosen on
    /// a terminal when left out.
    #[arg(value_name = "MODEL")]
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

/// Deploys step by step on a terminal, else the payload, or reports its estimate on a dry run.
pub(crate) async fn model_deploy(
    arguments: ModelDeployArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if guided(&arguments, format, controlling_terminal_condition())? {
        let manager = arguments.manager.connect().await?;
        let choices = arguments.choices;
        let deployed = wizard::run(
            &manager,
            open_terminal().as_mut(),
            WizardDefaults {
                model: arguments.model,
                profile: choices.profile,
                node: choices.node,
                target: choices.target,
                name: arguments.name,
                route: arguments.route,
                settings: choices.settings,
            },
        )
        .await?;
        let name = deployed.payload.name.clone().unwrap_or_default();
        return report_deployed(&manager, deployed.deployment, &name, format).await;
    }

    let from_file = arguments.file.is_some();
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
    let (selection, payload) =
        resolve_payload(&manager, payload, !from_file, &choices.settings).await?;

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
    let name = deployment_name(&selection, &payload);
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
        .create(create_request(&selection, &payload)?)
        .await
        .map_err(call_failed(
            DeploymentGroup::Model,
            format!("deploy {name}"),
        ))?;
    report_deployed(&manager, deployment, &name, format).await
}

/// Resolves a payload's choices on the site and applies the overrides. Without values of its
/// own (`starting`), the payload is replaced by the selection's starting payload, keeping its
/// name and route.
pub(super) async fn resolve_payload(
    site: &dyn DeploySite,
    mut payload: ModelPayload,
    starting: bool,
    settings: &[Setting],
) -> anyhow::Result<(Selection, ModelPayload)> {
    let selection = select(
        site,
        &Choices {
            model: &payload.model,
            profile: payload.profile.as_deref(),
            node: payload.node.as_deref(),
            target: payload.target.as_deref(),
        },
    )
    .await?;
    if starting {
        let flags = payload;
        payload = starting_payload(&selection);
        payload.name = flags.name.or(payload.name);
        payload.route = flags.route.or(payload.route);
    }
    payload.apply(settings)?;
    payload.configuration(&selection.schema())?;
    Ok((selection, payload))
}

/// The request creating the payload's deployment of the selection; every deploy sends this one.
pub(super) fn create_request(
    selection: &Selection,
    payload: &ModelPayload,
) -> Result<CreateModelDeploymentRequest, PayloadError> {
    Ok(CreateModelDeploymentRequest {
        source: Some(create_model_deployment_request::Source::Local(
            CreateLocalModelDeploymentRequest {
                model_id: selection.model.model_id.clone(),
                profile_id: selection.profile.profile_id.clone(),
                placement: Some(selection.placement.clone()),
                configuration: Some(payload.configuration(&selection.schema())?),
            },
        )),
        exposure: Some(payload.exposure()),
        model_deployment_name: payload.name.clone(),
    })
}

/// The name a deployment is shown by: the payload's, else the model's.
pub(super) fn deployment_name(selection: &Selection, payload: &ModelPayload) -> String {
    payload
        .name
        .clone()
        .unwrap_or_else(|| selection.model.display_name.clone())
}

/// The operator words of the preview's advisories that need them.
pub(super) fn advisories_words(selection: &Selection) -> Vec<&'static str> {
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

/// Whether the deploy is asked for step by step: on a terminal, with no payload, no starting
/// values to deploy and no dry run. Fails, never prompting, when nothing else says what to
/// deploy.
fn guided(
    arguments: &ModelDeployArgs,
    format: OutputFormat,
    condition: TerminalCondition,
) -> anyhow::Result<bool> {
    let named = arguments.model.is_some();
    if arguments.file.is_some() {
        return Ok(false);
    }
    if arguments.yes || arguments.dry_run {
        if !named {
            anyhow::bail!("deploying needs a model; pass <model>, or -f <payload>");
        }
        return Ok(false);
    }
    match (format, condition, named) {
        (OutputFormat::Yaml, _, true) => anyhow::bail!(
            "deploying needs a payload, which --yaml never asks for; pass -f <payload>, or --yes \
             to deploy the starting values"
        ),
        (OutputFormat::Yaml, _, false) => anyhow::bail!(
            "deploying needs a model, which --yaml never asks for; pass <model> with --yes, or \
             -f <payload>"
        ),
        (_, TerminalCondition::NonInteractive, true) => anyhow::bail!(
            "deploying needs a payload; pass -f <payload>, or --yes to deploy the starting values"
        ),
        (_, TerminalCondition::NonInteractive, false) => anyhow::bail!(
            "deploying needs a model; pass <model> with --yes, or -f <payload>, or run in a \
             terminal to choose one"
        ),
        (_, TerminalCondition::Interactive, _) => Ok(true),
    }
}

/// Prints the created deployment: its YAML document, or one line with its name, `name` when the
/// manager returned none, and its ID.
async fn report_deployed(
    manager: &Manager,
    deployment: ModelDeployment,
    name: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if format == OutputFormat::Yaml {
        let nodes = manager.node_names().await;
        return write_yaml(&document(deployment, &nodes, false));
    }
    present(&[Message::Success(format!(
        "Deploying {} ({})",
        if deployment.model_deployment_name.is_empty() {
            name
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
    messages.extend(memory_block(estimate, parallel_requests));
    messages.extend(
        advisories
            .iter()
            .map(|advisory| Message::Warning((*advisory).to_owned())),
    );
    messages
}
