//! Picks what one local model deployment uses: the catalog model, the execution target and the
//! profile, then has the server preview the result.
//!
//! The operator may name each choice; otherwise the target is the first where a profile fits now
//! (else the first that runs one), and the profile is the first that fits now on that target
//! (else the first that runs). Fit is the site's compatibility read; the CLI judges none itself.

use anyhow::bail;
use dotcore_proto::dotcore::{
    catalog::v1::{Model, ModelConfigurationOption, ModelProfile},
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{
        CreateLocalModelDeploymentRequest, LocalModelDeploymentAdvisory,
        LocalModelDeploymentConfiguration, ModelCompatibilityStatus, ModelProfileCompatibility,
        PreviewLocalModelDeploymentRequest, ResolvedLocalModelDeploymentSnapshot,
    },
};

use super::{catalog::SiteCatalog, payload::ParameterSchema};
use crate::{
    management::{DeploymentGroup, Manager, call_failed},
    reference::{Referable, ReferenceKind, resolve},
};

/// What the operator named for one deployment; anything left out takes the default.
pub(super) struct Choices<'a> {
    /// Catalog model reference.
    pub(super) model: &'a str,

    /// Profile reference.
    pub(super) profile: Option<&'a str>,

    /// Node reference.
    pub(super) node: Option<&'a str>,

    /// Execution target reference on the node.
    pub(super) target: Option<&'a str>,
}

/// The model, target and profile of one deployment, with the server's preview of it.
pub(super) struct Selection {
    /// The catalog model.
    pub(super) model: Model,

    /// The chosen profile.
    pub(super) profile: ModelProfile,

    /// The chosen execution target.
    pub(super) placement: ExactPlacement,

    /// The node's name, or its ID when unnamed.
    pub(super) node_name: String,

    /// The target's name, or its ID when unnamed.
    pub(super) target_name: String,

    /// Whether the node has other model targets, so a payload must name this one.
    pub(super) shared_node: bool,

    /// The profile's compatibility on the target, absent when the site reported none.
    pub(super) compatibility: Option<ModelProfileCompatibility>,

    /// The resolved deployment the server previewed with the profile's defaults.
    pub(super) snapshot: ResolvedLocalModelDeploymentSnapshot,

    /// The preview's non-blocking advisories.
    pub(super) advisories: Vec<LocalModelDeploymentAdvisory>,
}

impl Selection {
    /// The parameters a payload's values are typed against.
    pub(super) fn schema(&self) -> ParameterSchema<'_> {
        ParameterSchema {
            parameters: self
                .snapshot
                .engine_parameter_schema
                .as_ref()
                .map_or(&[][..], |schema| &schema.parameters),
            options: self.options(),
        }
    }

    fn options(&self) -> &[ModelConfigurationOption] {
        self.model
            .configurations
            .iter()
            .find(|configuration| configuration.configuration_id == self.snapshot.configuration_id)
            .map_or(&[][..], |configuration| &configuration.options)
    }
}

/// Resolves the operator's choices on the site and previews the deployment they select.
pub(super) async fn select(manager: &Manager, choices: &Choices<'_>) -> anyhow::Result<Selection> {
    let catalog = SiteCatalog::read(manager, Some(choices.model)).await?;
    let Some(model) = catalog.models.first() else {
        bail!("no catalog model matches {:?}", choices.model);
    };

    let targets = targets(&catalog, choices)?;
    let profile = match choices.profile {
        Some(reference) => resolve(ReferenceKind::Profile, reference, &model.profiles)?,
        None => default_profile(&catalog, model, &targets)?,
    };
    let entry = |target: &Target| entry(&catalog, model, profile, target);
    let target = targets
        .iter()
        .find(|target| entry(target).is_some_and(fits_now))
        .or_else(|| {
            targets
                .iter()
                .find(|target| entry(target).is_some_and(runs))
        })
        .unwrap_or(&targets[0]);

    let preview = manager
        .models()
        .preview_local_model_deployment(PreviewLocalModelDeploymentRequest {
            local: Some(CreateLocalModelDeploymentRequest {
                model_id: model.model_id.clone(),
                profile_id: profile.profile_id.clone(),
                placement: Some(target.placement.clone()),
                configuration: Some(LocalModelDeploymentConfiguration::default()),
            }),
        })
        .await
        .map_err(call_failed(
            DeploymentGroup::Model,
            format!("preview {} on {}", model.display_name, target.node_name),
        ))?
        .into_inner();

    let shared_node = catalog
        .compatibility
        .targets
        .iter()
        .filter_map(|compatibility| compatibility.target.as_ref())
        .filter(|placement| placement.site_node_id == target.placement.site_node_id)
        .count()
        > 1;
    Ok(Selection {
        compatibility: entry(target).cloned(),
        model: model.clone(),
        profile: profile.clone(),
        placement: target.placement.clone(),
        node_name: target.node_name.clone(),
        target_name: target.target_name.clone(),
        shared_node,
        advisories: preview.advisories().collect(),
        snapshot: preview.resolved_snapshot.unwrap_or_default(),
    })
}

/// One model target the deployment may use.
struct Target {
    placement: ExactPlacement,
    node_name: String,
    target_name: String,
}

/// The targets the choices allow, in node then target order: the named target of the named node,
/// every target of the named node, or every model target.
fn targets(catalog: &SiteCatalog, choices: &Choices<'_>) -> anyhow::Result<Vec<Target>> {
    let mut targets: Vec<Target> = catalog
        .compatibility
        .targets
        .iter()
        .filter_map(|compatibility| compatibility.target.clone())
        .map(|placement| Target {
            node_name: catalog.nodes.display(&placement.site_node_id).to_owned(),
            target_name: catalog
                .nodes
                .target_display(&placement.site_node_id, &placement.execution_target_id)
                .to_owned(),
            placement,
        })
        .collect();
    if targets.is_empty() {
        bail!("the site has no model target");
    }

    if let Some(reference) = choices.node {
        let mut nodes: Vec<Node> = Vec::new();
        for target in &targets {
            if !nodes
                .iter()
                .any(|node| node.id == target.placement.site_node_id)
            {
                nodes.push(Node {
                    id: target.placement.site_node_id.clone(),
                    name: target.node_name.clone(),
                });
            }
        }
        let node = resolve(ReferenceKind::Node, reference, &nodes)?.id.clone();
        targets.retain(|target| target.placement.site_node_id == node);
    }
    if let Some(reference) = choices.target {
        let target = resolve(ReferenceKind::ExecutionTarget, reference, &targets)?
            .placement
            .clone();
        targets.retain(|candidate| candidate.placement == target);
    }
    Ok(targets)
}

/// The first profile that fits now on one of the targets, else the first that runs on one.
fn default_profile<'a>(
    catalog: &SiteCatalog,
    model: &'a Model,
    targets: &[Target],
) -> anyhow::Result<&'a ModelProfile> {
    let on_targets = |profile: &ModelProfile, accepts: fn(&ModelProfileCompatibility) -> bool| {
        targets
            .iter()
            .any(|target| entry(catalog, model, profile, target).is_some_and(accepts))
    };
    model
        .profiles
        .iter()
        .find(|profile| on_targets(profile, fits_now))
        .or_else(|| {
            model
                .profiles
                .iter()
                .find(|profile| on_targets(profile, runs))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no profile of {} runs on {}",
                model.display_name,
                targets
                    .iter()
                    .map(|target| target.node_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn entry<'a>(
    catalog: &'a SiteCatalog,
    model: &Model,
    profile: &ModelProfile,
    target: &Target,
) -> Option<&'a ModelProfileCompatibility> {
    catalog
        .entries(&model.model_id, &profile.profile_id)
        .find(|entry| entry.target.as_ref() == Some(&target.placement))
}

fn runs(entry: &ModelProfileCompatibility) -> bool {
    entry.status() == ModelCompatibilityStatus::Runs
}

fn fits_now(entry: &ModelProfileCompatibility) -> bool {
    runs(entry) && entry.free_memory_fit() == MemoryFit::Fits
}

/// A node as the operator names it.
struct Node {
    id: String,
    name: String,
}

impl Referable for Node {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Referable for Target {
    fn id(&self) -> &str {
        &self.placement.execution_target_id
    }

    fn name(&self) -> &str {
        &self.target_name
    }
}

impl Referable for ModelProfile {
    fn id(&self) -> &str {
        &self.profile_id
    }

    fn name(&self) -> &str {
        &self.display_name
    }
}
