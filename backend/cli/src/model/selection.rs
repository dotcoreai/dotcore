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

use super::{catalog::SiteCatalog, payload::ParameterSchema, site::DeploySite};
use crate::{
    management::{DeploymentGroup, call_failed},
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
#[derive(Clone, Debug)]
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
pub(super) async fn select(
    site: &dyn DeploySite,
    choices: &Choices<'_>,
) -> anyhow::Result<Selection> {
    let catalog = SiteCatalog::read(site, Some(choices.model)).await?;
    let Some(model) = catalog.models.first() else {
        bail!("no catalog model matches {:?}", choices.model);
    };

    let targets = targets(&catalog, choices.node, choices.target)?;
    let named = choices
        .profile
        .map(|reference| resolve(ReferenceKind::Profile, reference, &model.profiles))
        .transpose()?;
    let target = default_target(&catalog, model, named, &targets);
    let profile = match named {
        Some(profile) => profile,
        None => default_profile(&catalog, model, std::slice::from_ref(target))?,
    };
    preview(site, &catalog, model, profile, target).await
}

/// Has the server preview an untouched deployment of the profile on the target, and returns the
/// selection it makes.
pub(super) async fn preview(
    site: &dyn DeploySite,
    catalog: &SiteCatalog,
    model: &Model,
    profile: &ModelProfile,
    target: &Target,
) -> anyhow::Result<Selection> {
    let preview = site
        .preview(PreviewLocalModelDeploymentRequest {
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
        ))?;

    let shared_node = catalog
        .compatibility
        .targets
        .iter()
        .filter_map(|compatibility| compatibility.target.as_ref())
        .filter(|placement| placement.site_node_id == target.placement.site_node_id)
        .count()
        > 1;
    Ok(Selection {
        compatibility: entry(catalog, model, profile, target).cloned(),
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

/// The target a deployment defaults to: for a named profile, the first target where it fits now,
/// else the first where it runs; without one, the first where any profile does. Else the first.
pub(super) fn default_target<'a>(
    catalog: &SiteCatalog,
    model: &Model,
    profile: Option<&ModelProfile>,
    targets: &'a [Target],
) -> &'a Target {
    let accepted = |target: &Target, accepts: fn(&ModelProfileCompatibility) -> bool| {
        model
            .profiles
            .iter()
            .filter(|candidate| profile.is_none_or(|profile| profile == *candidate))
            .any(|candidate| entry(catalog, model, candidate, target).is_some_and(accepts))
    };
    targets
        .iter()
        .find(|target| accepted(target, fits_now))
        .or_else(|| targets.iter().find(|target| accepted(target, runs)))
        .unwrap_or(&targets[0])
}

/// One model target a deployment may use.
#[derive(Clone, Debug)]
pub(super) struct Target {
    /// Where the target is.
    pub(super) placement: ExactPlacement,

    /// The node's name, or its ID when unnamed.
    pub(super) node_name: String,

    /// The target's name, or its ID when unnamed.
    pub(super) target_name: String,
}

/// The targets the references allow, in node then target order: the named target of the named
/// node, every target of the named node, or every model target.
pub(super) fn targets(
    catalog: &SiteCatalog,
    node: Option<&str>,
    target: Option<&str>,
) -> anyhow::Result<Vec<Target>> {
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

    if let Some(reference) = node {
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
    if let Some(reference) = target {
        let target = resolve(ReferenceKind::ExecutionTarget, reference, &targets)?
            .placement
            .clone();
        targets.retain(|candidate| candidate.placement == target);
    }
    Ok(targets)
}

/// The first profile that fits now on one of the targets, else the first that runs on one.
pub(super) fn default_profile<'a>(
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

/// The profile's compatibility on the target, when the site reported one.
pub(super) fn entry<'a>(
    catalog: &'a SiteCatalog,
    model: &Model,
    profile: &ModelProfile,
    target: &Target,
) -> Option<&'a ModelProfileCompatibility> {
    catalog
        .entries(&model.model_id, &profile.profile_id)
        .find(|entry| entry.target.as_ref() == Some(&target.placement))
}

/// Whether the profile runs on the entry's target.
pub(super) fn runs(entry: &ModelProfileCompatibility) -> bool {
    entry.status() == ModelCompatibilityStatus::Runs
}

/// Whether the profile runs and fits the free memory of the entry's target now.
pub(super) fn fits_now(entry: &ModelProfileCompatibility) -> bool {
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
