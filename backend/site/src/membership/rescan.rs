//! The local target re-scan: derives and validates the whole-target replacement for one existing
//! node, classifies it against the committed targets, refuses it when a changed or removed target
//! has a deployment on it, and reports the no-op identity of the whole projection.
//!
//! The replacement is composed only for the selected node — every other node, target, deployment
//! and route stays untouched. Targets are built by the caller through the same target-construction
//! path initialization uses, so a node offers the same capabilities the same way, for every
//! target kind. Nothing is written here: the caller owns the write, and the whole-projection
//! comparison is what a no-op leaves untouched.
//!
//! The registry, the private handshake host and the node-side install build that same
//! replacement into the running site and into the node's own folder. The site's own manager node
//! applies the same replacement in its own process, through the live-site membership write.

use std::{collections::BTreeSet, fmt};

use dotcore_proto::dotcore::{
    application::v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
    model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
    node::v1::NodeObservationConfiguration,
    site::v1::{ExecutionTarget, Site, SiteNode},
};

use crate::{
    membership::removal::DeploymentKind,
    node::observation_configuration::{
        NodeObservationConfigurationDigest, NodeObservationConfigurationError,
        node_observation_configuration,
    },
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, validate_site},
};

mod client;
mod commit;
mod host;
mod installation;
mod management;
mod manager_node;
#[cfg(test)]
mod manager_node_tests;
#[cfg(test)]
mod manager_tests;
mod registry;
mod scan;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
mod worker_tests;

pub use client::{NodeRescan, NodeRescanError, NodeRescanOutcome};
pub(crate) use commit::{ReplacementRole, record_replacement};
pub use commit::{RescanCommitError, RescanSiteWriter};
pub use manager_node::{ManagerNodeRescan, ManagerNodeRescanError, ManagerRescanOutcome};

pub use installation::{InstalledProjection, RescanInstallError};
pub use management::RescanManagementService;
pub use registry::{
    CommittedReplacement, RescanAttempt, RescanAttemptOutcome, RescanLimits, RescanRegistry,
    RescanRegistryError, RescanReview, RescanStatus, RescanWatch, StartedRescan,
};
pub use scan::{RescanCapability, RescanScan, RescanScanError, scan_replacement, scan_request};
/// The whole-node base approved for a rescan: the node as the site declares it, its committed
/// targets and the observation configuration derived from them.
#[derive(Clone, Debug, PartialEq)]
pub struct RescanBase {
    /// The SiteNode the replacement is approved for.
    pub node: SiteNode,

    /// The node's committed targets at approval time.
    pub targets: Vec<ExecutionTarget>,

    /// The committed observation configuration derived from the node's committed targets.
    pub observation: NodeObservationConfiguration,
}

/// Derives the whole-node base of `node_id` from `site`.
pub fn rescan_base(site: &Site, node_id: &str) -> Result<RescanBase, RescanError> {
    let node = site
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .ok_or_else(|| RescanError::NodeNotDeclared {
            node_id: node_id.to_owned(),
        })?;
    let targets = site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == node_id)
        .cloned()
        .collect();
    let observation = node_observation_configuration(site, node_id)?;
    Ok(RescanBase {
        node: node.clone(),
        targets,
        observation,
    })
}

/// What a replacement changes relative to the committed targets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RescanClassification {
    /// Targets in the replacement whose identity is absent from the committed targets.
    pub added: Vec<ExecutionTarget>,

    /// Targets whose identity is unchanged but whose full declaration differs.
    pub changed: Vec<ExecutionTarget>,

    /// Committed targets absent from the replacement, as their committed declarations.
    pub removed: Vec<ExecutionTarget>,

    /// Targets whose full declaration is identical to the committed one.
    pub unchanged: Vec<ExecutionTarget>,
}

impl RescanClassification {
    /// Classifies a replacement against the committed targets of one node. The same node and
    /// target identity with the full declaration equal is unchanged; the same identity with a
    /// declaration difference is changed; differing identity is added or removed.
    pub fn classify(committed: &[ExecutionTarget], replacement: &[ExecutionTarget]) -> Self {
        let committed_by_id = committed
            .iter()
            .map(|target| (target.execution_target_id.as_str(), target))
            .collect::<std::collections::HashMap<_, _>>();
        let replacement_by_id = replacement
            .iter()
            .map(|target| (target.execution_target_id.as_str(), target))
            .collect::<std::collections::HashMap<_, _>>();
        let mut classification = Self::default();
        for target in replacement {
            match committed_by_id.get(target.execution_target_id.as_str()) {
                Some(committed) if **committed == *target => {
                    classification.unchanged.push((*target).clone());
                }
                Some(_) => classification.changed.push((*target).clone()),
                None => classification.added.push((*target).clone()),
            }
        }
        for target in committed {
            if !replacement_by_id.contains_key(target.execution_target_id.as_str()) {
                classification.removed.push((*target).clone());
            }
        }
        let by_id = |left: &ExecutionTarget, right: &ExecutionTarget| {
            left.execution_target_id.cmp(&right.execution_target_id)
        };
        classification.added.sort_by(by_id);
        classification.changed.sort_by(by_id);
        classification.removed.sort_by(by_id);
        classification.unchanged.sort_by(by_id);
        classification
    }

    /// Returns whether the replacement declares the same targets, identically, as the committed ones.
    pub fn is_unchanged(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// Returns every changed or removed target identity: the targets the deployment guard refuses
    /// when any of them has a deployment.
    fn guarded_target_ids(&self) -> BTreeSet<&str> {
        self.changed
            .iter()
            .chain(self.removed.iter())
            .map(|target| target.execution_target_id.as_str())
            .collect()
    }
}

/// One deployment placed on a changed or removed target, including one pending teardown.
#[derive(Clone, Debug, PartialEq)]
pub struct RescanDeploymentImpact {
    /// Owner that declared the deployment.
    pub kind: DeploymentKind,

    /// Model or application deployment identity.
    pub deployment_id: String,

    /// Display name of the deployment, or the model when a model deployment has none.
    pub name: String,

    /// Execution target the deployment is placed on.
    pub execution_target_id: String,

    /// Whether the deployment is being torn down.
    pub tearing_down: bool,
}

impl fmt::Display for RescanDeploymentImpact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} ({}, {})",
            self.name,
            self.kind,
            self.execution_target_id,
            if self.tearing_down {
                "teardown"
            } else {
                "present"
            }
        )
    }
}

/// Why a replacement was refused.
#[derive(Debug, thiserror::Error)]
pub enum RescanError {
    /// The node is not declared in the site.
    #[error("node {node_id:?} is not declared in this site")]
    NodeNotDeclared { node_id: String },

    /// The replacement declares a target owned by another node.
    #[error("execution target {target_id:?} does not belong to node {node_id:?}")]
    ForeignTarget { target_id: String, node_id: String },

    /// The replacement declares one execution target more than once.
    #[error("the replacement declares execution target {target_id:?} more than once")]
    DuplicateTarget { target_id: String },

    /// The whole-node base this replacement was approved against no longer matches the durable
    /// declaration; the rescan must be started again.
    #[error("the node's base no longer matches the durable declaration; start the rescan again")]
    StaleBase,

    /// The site with the replacement would be invalid.
    #[error("the site with the replacement would be invalid: {0}")]
    InvalidSite(#[source] SiteRepositoryError),

    /// The replacement removes or changes a target with deployments placed on it, including one
    /// pending teardown, so the whole replacement is refused.
    #[error(
        "the replacement would remove or change targets with deployments placed on them:{}\nthe whole replacement is refused",
        deployment_guard_lines(.impacts)
    )]
    DeploymentGuard {
        impacts: Vec<RescanDeploymentImpact>,
    },

    /// The node's replacement targets could not be derived or validated.
    #[error("the replacement could not be derived: {0}")]
    Derivation(#[source] NodeObservationConfigurationError),
}

fn deployment_guard_lines(impacts: &[RescanDeploymentImpact]) -> String {
    impacts
        .iter()
        .map(|impact| format!("\n  - {impact}"))
        .collect()
}

/// A validated replacement for one existing node.
#[derive(Clone, Debug, PartialEq)]
pub struct RescanReplacement {
    /// The node the replacement is approved for, unchanged.
    node: SiteNode,

    /// The whole replacement target set.
    targets: Vec<ExecutionTarget>,

    /// The refreshed observation configuration for the replacement.
    observation: NodeObservationConfiguration,

    /// Canonical lowercase SHA-256 digest of the refreshed observation configuration.
    observation_configuration_sha256: String,

    /// The classification of the replacement against the committed targets.
    classification: RescanClassification,

    /// Whether the whole projection is equal to the committed one.
    is_noop: bool,
}

impl RescanReplacement {
    /// Returns the node the replacement is approved for.
    pub fn node(&self) -> &SiteNode {
        &self.node
    }

    /// Returns the whole replacement target set.
    pub fn targets(&self) -> &[ExecutionTarget] {
        &self.targets
    }

    /// Returns the refreshed observation configuration.
    pub fn observation(&self) -> &NodeObservationConfiguration {
        &self.observation
    }

    /// Returns the canonical digest of the refreshed observation configuration.
    pub fn observation_configuration_sha256(&self) -> &str {
        &self.observation_configuration_sha256
    }

    /// Returns the classification of the replacement.
    pub fn classification(&self) -> &RescanClassification {
        &self.classification
    }

    /// Returns whether the whole projection is exactly equal to the committed one: the target set,
    /// the refreshed observation configuration and its digest. A no-op writes nothing and needs no
    /// restart.
    pub fn is_noop(&self) -> bool {
        self.is_noop
    }
}

/// Derives and validates the whole-target replacement for one existing node.
///
/// `targets` holds the replacement the caller built through the same target-construction path
/// site initialization uses (every target kind). `base` is the
/// whole-node base the proposal was approved against; when given, the durable node must still
/// match it before the replacement is produced. Nothing is written; the caller owns the write.
pub fn derive_replacement(
    site: &Site,
    rules: &SiteDeclarationRules,
    node_id: &str,
    targets: Vec<ExecutionTarget>,
    base: Option<&RescanBase>,
) -> Result<RescanReplacement, RescanError> {
    let declared = site
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .ok_or_else(|| RescanError::NodeNotDeclared {
            node_id: node_id.to_owned(),
        })?
        .clone();

    let committed = site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == node_id)
        .cloned()
        .collect::<Vec<_>>();

    if let Some(base) = base
        && *base != rescan_base(site, node_id)?
    {
        return Err(RescanError::StaleBase);
    }

    let mut all_targets = Vec::with_capacity(targets.len());
    let mut seen = BTreeSet::new();
    for target in targets {
        if target.site_node_id != node_id {
            return Err(RescanError::ForeignTarget {
                target_id: target.execution_target_id,
                node_id: node_id.to_owned(),
            });
        }
        if !seen.insert(target.execution_target_id.clone()) {
            return Err(RescanError::DuplicateTarget {
                target_id: target.execution_target_id,
            });
        }
        all_targets.push(target);
    }

    let classification = RescanClassification::classify(&committed, &all_targets);

    let impacts = deployment_impacts(
        &site.model_deployments,
        &site.application_deployments,
        &classification.guarded_target_ids(),
    );
    if !impacts.is_empty() {
        return Err(RescanError::DeploymentGuard { impacts });
    }

    let mut candidate = site.clone();
    candidate
        .execution_targets
        .retain(|target| target.site_node_id != node_id);
    candidate
        .execution_targets
        .extend(all_targets.iter().cloned());
    validate_site(&candidate, rules).map_err(RescanError::InvalidSite)?;

    let observation = node_observation_configuration(&candidate, node_id)?;
    let digest = NodeObservationConfigurationDigest::for_configuration(&observation)?;
    let committed_observation = node_observation_configuration(site, node_id)?;
    let committed_digest =
        NodeObservationConfigurationDigest::for_configuration(&committed_observation)?;

    let is_noop = classification.is_unchanged()
        && committed_observation == observation
        && committed_digest == digest;

    Ok(RescanReplacement {
        node: declared,
        targets: all_targets,
        observation,
        observation_configuration_sha256: digest.as_str().to_owned(),
        classification,
        is_noop,
    })
}

/// Returns the deployments placed on one of `guarded` execution targets, including one pending
/// teardown. Upstream model deployments have no placement and are never counted.
fn deployment_impacts(
    models: &[ModelDeploymentDeclaration],
    applications: &[ApplicationDeploymentDeclaration],
    guarded: &BTreeSet<&str>,
) -> Vec<RescanDeploymentImpact> {
    let mut impacts = Vec::new();
    for deployment in models {
        let Some(placement) = deployment.placement.as_ref() else {
            continue;
        };
        if !guarded.contains(placement.execution_target_id.as_str()) {
            continue;
        }
        impacts.push(RescanDeploymentImpact {
            kind: DeploymentKind::Model,
            deployment_id: deployment.model_deployment_id.clone(),
            name: model_deployment_name(deployment),
            execution_target_id: placement.execution_target_id.clone(),
            tearing_down: deployment.desired_state == ModelDeploymentDesiredState::Teardown as i32,
        });
    }
    for deployment in applications {
        let Some(placement) = deployment.placement.as_ref() else {
            continue;
        };
        if !guarded.contains(placement.execution_target_id.as_str()) {
            continue;
        }
        impacts.push(RescanDeploymentImpact {
            kind: DeploymentKind::Application,
            deployment_id: deployment.application_deployment_id.clone(),
            name: deployment.application_deployment_name.clone(),
            execution_target_id: placement.execution_target_id.clone(),
            tearing_down: deployment.desired_state
                == ApplicationDeploymentDesiredState::Teardown as i32,
        });
    }
    impacts.sort_by(|left, right| {
        left.deployment_id
            .cmp(&right.deployment_id)
            .then(left.kind.to_string().cmp(&right.kind.to_string()))
    });
    impacts
}

/// Returns the deployment's display name, falling back to the model id when a model deployment
/// has none.
fn model_deployment_name(deployment: &ModelDeploymentDeclaration) -> String {
    if !deployment.model_deployment_name.is_empty() {
        return deployment.model_deployment_name.clone();
    }
    deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
        .and_then(|source| match source {
            dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(local) => {
                Some(local.model_id.clone())
            }
            _ => None,
        })
        .unwrap_or_default()
}

impl From<NodeObservationConfigurationError> for RescanError {
    fn from(error: NodeObservationConfigurationError) -> Self {
        RescanError::Derivation(error)
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentDesiredState, ResolvedApplicationDeploymentSnapshot,
        },
        catalog::v1::OciPlatform,
        deployment::v1::ExactPlacement,
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentLifecycleTransition,
        },
        site::v1::execution_target,
    };

    use super::{
        RescanClassification, RescanError, RescanReplacement, derive_replacement, rescan_base,
    };
    use crate::{
        membership::DeploymentKind,
        membership::join::test_fixtures::{
            InitializedSite, initialized_site, worker_node, worker_targets,
        },
        validation::SiteDeclarationRules,
    };

    const WORKER: &str = "node-worker";

    fn rules() -> SiteDeclarationRules {
        crate::test_support::site_declaration_rules()
    }

    /// Returns the initialized site plus a worker node with its two simulated targets. The kept
    /// `InitializedSite` holds the fixture's temporary directory.
    fn worker_site() -> (InitializedSite, dotcore_proto::dotcore::site::v1::Site) {
        let fixture = initialized_site();
        let mut site = fixture.site.clone();
        let node = worker_node(WORKER, "Worker", "10.0.0.7");
        let targets = worker_targets(WORKER);
        site.nodes.push(node);
        site.execution_targets.extend(targets.iter().cloned());
        (fixture, site)
    }

    fn committed_targets(
        site: &dotcore_proto::dotcore::site::v1::Site,
    ) -> Vec<dotcore_proto::dotcore::site::v1::ExecutionTarget> {
        site.execution_targets
            .iter()
            .filter(|target| target.site_node_id == WORKER)
            .cloned()
            .collect()
    }

    fn target_by_runtime(
        targets: &[dotcore_proto::dotcore::site::v1::ExecutionTarget],
        model: bool,
    ) -> dotcore_proto::dotcore::site::v1::ExecutionTarget {
        targets
            .iter()
            .find(|target| {
                matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ModelRuntime(_)) if model
                ) || matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(_)) if !model
                )
            })
            .cloned()
            .expect("target")
    }

    fn model_target(
        site: &dotcore_proto::dotcore::site::v1::Site,
    ) -> dotcore_proto::dotcore::site::v1::ExecutionTarget {
        target_by_runtime(&committed_targets(site), true)
    }

    fn application_target(
        site: &dotcore_proto::dotcore::site::v1::Site,
    ) -> dotcore_proto::dotcore::site::v1::ExecutionTarget {
        target_by_runtime(&committed_targets(site), false)
    }

    fn derive(
        site: &dotcore_proto::dotcore::site::v1::Site,
        targets: Vec<dotcore_proto::dotcore::site::v1::ExecutionTarget>,
        base: Option<&super::RescanBase>,
    ) -> Result<RescanReplacement, RescanError> {
        derive_replacement(site, &rules(), WORKER, targets, base)
    }

    /// A model deployment declared on `target_id`.
    fn model_deployment(
        id: &str,
        node_id: &str,
        target_id: &str,
        teardown: bool,
    ) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: "Worker model".to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: node_id.to_owned(),
                execution_target_id: target_id.to_owned(),
            }),
            requested_parallel_shape: dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One
                as i32,
            snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
                "fixture-model",
            )),
            desired_state: if teardown {
                ModelDeploymentDesiredState::Teardown as i32
            } else {
                ModelDeploymentDesiredState::Present as i32
            },
            lifecycle_generation: 1,
            lifecycle_transition: if teardown {
                ModelDeploymentLifecycleTransition::Teardown as i32
            } else {
                ModelDeploymentLifecycleTransition::Create as i32
            },
            ..Default::default()
        }
    }

    /// An application deployment declared on `target`.
    fn application_deployment(
        id: &str,
        target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
        teardown: bool,
    ) -> dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration {
        let placement = ExactPlacement {
            site_node_id: target.site_node_id.clone(),
            execution_target_id: target.execution_target_id.clone(),
        };
        dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration {
            application_deployment_id: id.to_owned(),
            application_deployment_name: "Worker app".to_owned(),
            placement: Some(placement.clone()),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                placement: Some(placement),
                platform: target.platform.clone(),
                ..Default::default()
            }),
            desired_state: if teardown {
                ApplicationDeploymentDesiredState::Teardown as i32
            } else {
                ApplicationDeploymentDesiredState::Present as i32
            },
            lifecycle_generation: 1,
            ..Default::default()
        }
    }

    #[test]
    fn classification_reports_added_changed_removed_and_unchanged_targets() {
        let (_fixture, site) = worker_site();
        let committed = committed_targets(&site);
        let model = model_target(&site);
        let application = application_target(&site);

        let mut changed_model = model.clone();
        changed_model.display_name = "Changed model target".to_owned();
        let mut added = application.clone();
        added.execution_target_id = "node-worker-extra-target".to_owned();
        added.display_name = "Extra target".to_owned();
        // The replacement keeps the model target (changed), adds a new target, and drops the
        // application target (removed).
        let replacement = vec![changed_model, added];

        let classification = RescanClassification::classify(&committed, &replacement);

        assert_eq!(
            classification
                .added
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<Vec<_>>(),
            ["node-worker-extra-target"]
        );
        assert_eq!(
            classification
                .changed
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<Vec<_>>(),
            [model.execution_target_id.as_str()]
        );
        assert_eq!(
            classification
                .removed
                .iter()
                .map(|target| target.execution_target_id.as_str())
                .collect::<Vec<_>>(),
            [application.execution_target_id.as_str()]
        );
        assert!(classification.unchanged.is_empty());
    }

    #[test]
    fn an_identical_replacement_is_a_noop() {
        let (_fixture, site) = worker_site();
        let committed = committed_targets(&site);
        let first = derive(&site, committed.clone(), None).expect("derived");
        let second = derive(&site, committed, None).expect("derived again");

        assert!(first.is_noop());
        assert!(second.is_noop());
        assert!(first.classification().is_unchanged());
        assert!(first.classification().added.is_empty());
        assert!(first.classification().changed.is_empty());
        assert!(first.classification().removed.is_empty());
        assert_eq!(first.targets(), second.targets());
        assert_eq!(first.observation(), second.observation());
        assert_eq!(
            first.observation_configuration_sha256(),
            second.observation_configuration_sha256()
        );
    }

    #[test]
    fn a_changed_target_is_not_a_noop() {
        let (_fixture, site) = worker_site();
        let mut changed = committed_targets(&site);
        changed[0].display_name = "Changed".to_owned();
        let derived = derive(&site, changed, None).expect("derived");

        assert!(!derived.is_noop());
        assert_eq!(derived.classification().changed.len(), 1);
        assert_ne!(derived.observation_configuration_sha256(), "");
    }

    #[test]
    fn a_stale_base_is_refused() {
        let (_fixture, site) = worker_site();
        let base = rescan_base(&site, WORKER).expect("base");

        // The base matches the durable node: the replacement is produced.
        let derived = derive(&site, committed_targets(&site), Some(&base));
        assert!(derived.is_ok(), "{derived:?}");

        // The node's targets changed since the approval: the base is stale.
        let mut stale = site.clone();
        stale
            .execution_targets
            .retain(|target| target.site_node_id != WORKER);
        let mut targets = committed_targets(&site);
        targets[0].display_name = "Stale model target".to_owned();
        stale.execution_targets.extend(targets.iter().cloned());
        let error = derive(&stale, committed_targets(&stale), Some(&base));
        assert!(matches!(error, Err(RescanError::StaleBase)), "{error:?}");
    }

    #[test]
    fn a_deployment_on_a_changed_target_blocks_the_whole_replacement() {
        let (_fixture, site) = worker_site();
        let model = model_target(&site);
        let mut guard_site = site.clone();
        guard_site.model_deployments.push(model_deployment(
            "deployment-present",
            WORKER,
            &model.execution_target_id,
            false,
        ));
        guard_site.model_deployments.push(model_deployment(
            "deployment-teardown",
            WORKER,
            &model.execution_target_id,
            true,
        ));

        let mut changed = committed_targets(&guard_site);
        changed[0].display_name = "Changed".to_owned();
        let error = derive(&guard_site, changed, None).expect_err("blocked");

        let RescanError::DeploymentGuard { impacts } = error else {
            panic!("expected a deployment guard refusal: {error:?}");
        };
        assert_eq!(
            impacts
                .iter()
                .map(|impact| (
                    impact.deployment_id.as_str(),
                    impact.tearing_down,
                    impact.kind
                ))
                .collect::<Vec<_>>(),
            [
                ("deployment-present", false, DeploymentKind::Model),
                ("deployment-teardown", true, DeploymentKind::Model),
            ]
        );
        assert!(
            impacts
                .iter()
                .all(|impact| impact.execution_target_id == model.execution_target_id)
        );
    }

    #[test]
    fn a_deployment_on_a_removed_target_blocks_the_replacement() {
        let (_fixture, site) = worker_site();
        let application = application_target(&site);
        let mut guard_site = site.clone();
        guard_site
            .application_deployments
            .push(application_deployment("app-present", &application, false));
        // A model deployment on the model target, which stays unchanged, does not block.
        guard_site.model_deployments.push(model_deployment(
            "model-on-unchanged",
            WORKER,
            &model_target(&guard_site).execution_target_id,
            false,
        ));

        let error =
            derive(&guard_site, vec![model_target(&guard_site)], None).expect_err("blocked");
        let RescanError::DeploymentGuard { impacts } = error else {
            panic!("expected a deployment guard refusal: {error:?}");
        };
        assert_eq!(
            impacts
                .iter()
                .map(|impact| (impact.deployment_id.as_str(), impact.kind))
                .collect::<Vec<_>>(),
            [("app-present", DeploymentKind::Application)]
        );

        // The model deployment alone does not block anything.
        let result = derive(&guard_site, committed_targets(&guard_site), None);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn validation_rejection_leaves_the_site_unchanged() {
        let (_fixture, site) = worker_site();
        let before = site.clone();

        // A target that names another node is refused.
        let mut foreign = committed_targets(&site);
        foreign[0].site_node_id = "node-other".to_owned();
        let error = derive(&site, foreign, None);
        assert!(
            matches!(error, Err(RescanError::ForeignTarget { .. })),
            "{error:?}"
        );

        // A duplicate target is refused.
        let mut duplicate = committed_targets(&site);
        let clone = duplicate[0].clone();
        duplicate.push(clone);
        let error = derive(&site, duplicate, None);
        assert!(
            matches!(error, Err(RescanError::DuplicateTarget { .. })),
            "{error:?}"
        );

        // A node that is not declared is refused.
        let error = derive_replacement(&site, &rules(), "node-missing", vec![], None);
        assert!(
            matches!(error, Err(RescanError::NodeNotDeclared { .. })),
            "{error:?}"
        );

        // A replacement with an invalid target is refused as a whole site.
        let mut invalid = committed_targets(&site);
        invalid[0].display_name = String::new();
        let error = derive(&site, invalid, None);
        assert!(
            matches!(error, Err(RescanError::InvalidSite(_))),
            "{error:?}"
        );

        assert_eq!(
            site, before,
            "the site must be unchanged by every rejection"
        );
    }

    #[test]
    fn the_observation_and_digest_change_with_the_targets() {
        let (_fixture, site) = worker_site();
        let base_observation = rescan_base(&site, WORKER).expect("base").observation;

        let mut changed = committed_targets(&site);
        changed[0].platform = Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        });
        let derived = derive(&site, changed, None).expect("derived");

        assert_ne!(
            derived.observation().execution_targets,
            base_observation.execution_targets,
            "the refreshed observation follows the replacement targets"
        );
        assert_ne!(
            derived.observation_configuration_sha256(),
            crate::node::observation_configuration::NodeObservationConfigurationDigest::for_configuration(
                &base_observation,
            )
            .expect("digest")
            .as_str()
        );
    }
}
