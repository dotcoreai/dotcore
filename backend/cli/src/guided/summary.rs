//! Plan summary shown before the operator confirms a guided site plan.
//!
//! Presents one aligned block of the values about to be confirmed, such as the site directory, the
//! node host with where it came from, the node name and the node's model and application targets,
//! followed by what the node host is used for. It only reads the validated plan and never changes
//! it.

use anyhow::Context;
use dotcore_proto::dotcore::site::v1::{ExecutionTarget, NativeWorkloadLifetime, execution_target};
use dotcore_site::initial::request::InitialInferenceRuntime;

use super::GuidedInitialSitePlan;
use crate::terminal::{Message, Terminal};

/// What the node host is used for, shown next to it so the operator sees its exposure.
pub(super) const NODE_HOST_CONSEQUENCE: &str = "Workers connect to this host, the inference URL \
     uses it, and apps publish on it without authentication.";

/// Presents the validated plan before the confirmation.
pub(super) fn display_summary(
    terminal: &mut dyn Terminal,
    plan: &GuidedInitialSitePlan,
) -> anyhow::Result<()> {
    let node = plan
        .inputs
        .site()
        .nodes
        .first()
        .context("validated guided site inputs lack a node")?;

    terminal.message(&Message::Heading("Initial site plan".to_owned()))?;
    let mut rows = vec![
        (
            "site directory".to_owned(),
            plan.site_dir.display().to_string(),
        ),
        (
            "host".to_owned(),
            format!("{} ({})", plan.request.site_node_host(), plan.host.source()),
        ),
        ("node name".to_owned(), node.display_name.clone()),
        ("models".to_owned(), target_names(plan, true)),
        ("applications".to_owned(), target_names(plan, false)),
    ];
    if plan
        .request
        .inference()
        .contains(&InitialInferenceRuntime::Apple)
    {
        let lifetime = match plan.request.native_workload_lifetime() {
            NativeWorkloadLifetime::Ephemeral => "ephemeral",
            _ => "persistent",
        };
        rows.push(("native workloads".to_owned(), lifetime.to_owned()));
    }
    terminal.message(&Message::KeyValues(rows))?;
    terminal.message(&Message::Hint(NODE_HOST_CONSEQUENCE.to_owned()))?;
    Ok(())
}

/// Lists the display names of the plan's model targets, or of its application targets, or
/// `none`. An application target holding GPUs names them after the model targets holding the same
/// GPUs, such as `Docker applications with NVIDIA GeForce RTX 5090`.
fn target_names(plan: &GuidedInitialSitePlan, models: bool) -> String {
    let targets = &plan.inputs.site().execution_targets;
    let is_model = |target: &ExecutionTarget| {
        matches!(
            target.runtime_binding,
            Some(execution_target::RuntimeBinding::ModelRuntime(_))
        )
    };
    let gpu_name = |device: &String| {
        targets
            .iter()
            .find(|target| is_model(target) && target.accelerator_device_ids == [device.clone()])
            .map_or_else(|| device.clone(), |target| target.display_name.clone())
    };
    let names = targets
        .iter()
        .filter(|target| is_model(target) == models)
        .map(|target| {
            if models || target.accelerator_device_ids.is_empty() {
                return target.display_name.clone();
            }
            let gpus = target
                .accelerator_device_ids
                .iter()
                .map(gpu_name)
                .collect::<Vec<_>>();
            format!("{} with {}", target.display_name, gpus.join(" + "))
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}
