//! Takes one worker node out of a stopped site: a plan the operator reviews, then
//! an apply step that removes the node's pin and its membership from `site.yaml`.
//!
//! The plan is computed from the site alone, so the command can show what changes before it asks
//! for confirmation while the exclusive site lock stays held. A node that still has deployments
//! is refused, unless the operator chose to forget them, in which case they and the routes only
//! they serve are withdrawn without cleanup evidence from the node.

use std::{
    collections::BTreeSet,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use dotcore_domain::inference_route::classify_unload_set;
use dotcore_proto::{
    dotcore::{
        application::v1::{ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState},
        deployment::v1::ExactPlacement,
        model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
        site::v1::{ExecutionTarget, Site, SiteNode},
    },
    yaml,
};

use crate::{
    SITE_FILE,
    atomic::{AtomicError, StdFileSystem, persist_bytes},
    initial::input_store::SiteInputLock,
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    membership::OfflineSite,
    repository::{ApplicationRouteReference, SiteRepositoryError, application_route_references},
    validation::{SiteDeclarationRules, validate_site},
};

/// What removing one node does to the site, computed without touching the disk.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeRemovalPlan {
    /// The node leaving the site.
    pub node: SiteNode,

    /// The execution targets that leave with the node.
    pub execution_targets: Vec<ExecutionTarget>,

    /// The deployments placed on the node whose declarations are dropped without cleanup
    /// evidence. Empty unless the operator chose to forget them.
    pub forgotten_deployments: Vec<PlacedDeployment>,

    /// The routes deleted because only forgotten deployments served them.
    pub deleted_routes: Vec<DeletedRoute>,

    updated_site: Site,
}

impl NodeRemovalPlan {
    /// Returns the site as it is written once the plan is applied.
    pub fn updated_site(&self) -> &Site {
        &self.updated_site
    }

    /// Returns the file that pins the node's certificate under `site_root`. It may be absent.
    pub fn pin_path(&self, site_root: &Path) -> PathBuf {
        site_root
            .join(SITE_TRUST_DIRECTORY)
            .join(WORKER_PIN_DIRECTORY)
            .join(format!("{}.sha256", self.node.node_id))
    }
}

/// One deployment declaration placed on the node being removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedDeployment {
    pub kind: DeploymentKind,
    pub id: String,

    /// The deployment's display name, or its model when a model deployment has none.
    pub name: String,

    /// The declared lifecycle state: `present`, `paused`, or `teardown`.
    pub state: &'static str,
}

impl fmt::Display for PlacedDeployment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} deployment {} ({}, {})",
            self.kind, self.id, self.name, self.state
        )
    }
}

/// Which owner declared a placed deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentKind {
    Model,

    Application,
}

impl fmt::Display for DeploymentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Model => "model",
            Self::Application => "application",
        })
    }
}

/// An inference route deleted together with the deployments that alone served it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedRoute {
    pub id: String,
    pub name: String,
}

/// The result of planning a removal against a stopped site.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeRemoval {
    /// The node is declared and the plan says what removing it changes.
    Planned(Box<NodeRemovalPlan>),

    /// The node id is neither declared nor pinned: an earlier removal already finished.
    AlreadyRemoved { node_id: String },
}

/// Plans the removal of the node named by `selector`, a display name or a node id.
///
/// With `forget_deployments`, the declarations placed on the node are withdrawn, together with
/// every route whose members are all withdrawn. A route that also has a surviving member, or an
/// application that stays and is bound to a deleted route, refuses the plan; all such refusals
/// are reported together. Upstream model deployments have no placement and are never touched.
pub(super) fn plan_node_removal(
    site: &Site,
    rules: &SiteDeclarationRules,
    selector: &str,
    forget_deployments: bool,
) -> Result<NodeRemovalPlan, NodeRemovalError> {
    let node = select_node(site, selector)?;
    if node.site_manager.is_some() {
        return Err(NodeRemovalError::ManagerNode {
            node_name: node.display_name.clone(),
        });
    }

    let models = site
        .model_deployments
        .iter()
        .filter(|deployment| is_placed_on(deployment.placement.as_ref(), &node.node_id))
        .collect::<Vec<_>>();
    let applications = site
        .application_deployments
        .iter()
        .filter(|deployment| is_placed_on(deployment.placement.as_ref(), &node.node_id))
        .collect::<Vec<_>>();
    let placed = models
        .iter()
        .map(|deployment| placed_model(deployment))
        .chain(
            applications
                .iter()
                .map(|deployment| placed_application(deployment)),
        )
        .collect::<Vec<_>>();
    if !placed.is_empty() && !forget_deployments {
        return Err(NodeRemovalError::PlacedDeployments {
            node_name: node.display_name.clone(),
            deployments: placed,
        });
    }

    let forgotten_models = models
        .iter()
        .map(|deployment| deployment.model_deployment_id.as_str())
        .collect::<BTreeSet<_>>();
    let forgotten_applications = applications
        .iter()
        .map(|deployment| deployment.application_deployment_id.as_str())
        .collect::<BTreeSet<_>>();
    let route_plan = classify_unload_set(&site.inference_routes, &forgotten_models);
    let linked_applications = application_route_references(
        &site.application_deployments,
        &site.inference_routes,
        &route_plan,
        &forgotten_applications,
    );
    if !route_plan.shared_route_names().is_empty() || !linked_applications.is_empty() {
        return Err(NodeRemovalError::RouteConflicts {
            node_name: node.display_name.clone(),
            shared_routes: route_plan.shared_route_names().to_vec(),
            linked_applications,
        });
    }

    let deleted_routes = site
        .inference_routes
        .iter()
        .filter(|route| route_plan.removes(route))
        .map(|route| DeletedRoute {
            id: route.inference_route_id.clone(),
            name: route.name.clone(),
        })
        .collect();
    let execution_targets = site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == node.node_id)
        .cloned()
        .collect();

    let mut updated_site = site.clone();
    updated_site
        .nodes
        .retain(|candidate| candidate.node_id != node.node_id);
    updated_site
        .execution_targets
        .retain(|target| target.site_node_id != node.node_id);
    updated_site
        .model_deployments
        .retain(|deployment| !is_placed_on(deployment.placement.as_ref(), &node.node_id));
    updated_site
        .application_deployments
        .retain(|deployment| !is_placed_on(deployment.placement.as_ref(), &node.node_id));
    updated_site
        .inference_routes
        .retain(|route| !route_plan.removes(route));
    validate_site(&updated_site, rules).map_err(NodeRemovalError::InvalidSite)?;

    let forgotten_deployments = models
        .iter()
        .map(|deployment| placed_model(deployment))
        .chain(
            applications
                .iter()
                .map(|deployment| placed_application(deployment)),
        )
        .collect();
    Ok(NodeRemovalPlan {
        node: node.clone(),
        execution_targets,
        forgotten_deployments,
        deleted_routes,
        updated_site,
    })
}

/// Counts the model and application deployments placed on `node_id`. Upstream model deployments
/// have no placement and are never counted.
pub(super) fn placed_deployment_count(site: &Site, node_id: &str) -> usize {
    let models = site
        .model_deployments
        .iter()
        .filter(|deployment| is_placed_on(deployment.placement.as_ref(), node_id))
        .count();
    let applications = site
        .application_deployments
        .iter()
        .filter(|deployment| is_placed_on(deployment.placement.as_ref(), node_id))
        .count();
    models + applications
}

impl OfflineSite {
    /// Plans a removal against the site on disk, under the lock this value holds.
    ///
    /// A node id that is neither declared nor pinned reports [`NodeRemoval::AlreadyRemoved`], so
    /// a retry after a removal that landed with an uncertain outcome succeeds. A display name
    /// that is not declared is an error, because it cannot be told from a typo.
    pub fn plan_node_removal(
        &self,
        selector: &str,
        forget_deployments: bool,
    ) -> Result<NodeRemoval, NodeRemovalError> {
        let site = self.load_site().map_err(NodeRemovalError::SiteUnreadable)?;
        match plan_node_removal(&site, &self.rules, selector, forget_deployments) {
            Ok(plan) => Ok(NodeRemoval::Planned(Box::new(plan))),
            Err(NodeRemovalError::UnknownNode { .. })
                if looks_like_node_id(selector) && !self.pin_exists(selector)? =>
            {
                Ok(NodeRemoval::AlreadyRemoved {
                    node_id: selector.to_owned(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Applies a plan made under this lock: removes the node's pin, then replaces `site.yaml`.
    pub fn remove_node(&self, plan: &NodeRemovalPlan) -> Result<(), NodeRemovalError> {
        unpublish_node(&self.site_root, &self.lock, plan)
    }

    fn pin_exists(&self, node_id: &str) -> Result<bool, NodeRemovalError> {
        let pin = self.pin_path(node_id);
        pin.try_exists().map_err(|source| {
            NodeRemovalError::Storage(
                anyhow::Error::new(source)
                    .context(format!("inspect the node pin {}", pin.display())),
            )
        })
    }

    fn pin_path(&self, node_id: &str) -> PathBuf {
        self.site_root
            .join(SITE_TRUST_DIRECTORY)
            .join(WORKER_PIN_DIRECTORY)
            .join(format!("{node_id}.sha256"))
    }
}

/// Removes the node's pin and syncs the pin directory, then replaces `site.yaml` with the
/// planned site, under the exclusive lock that `_lock` proves is held.
///
/// The pin goes first: a node that is still declared without a pin cannot authenticate and does
/// not block the manager from starting, and a retry finishes the removal, whereas a pin without
/// its node would fail the manager's startup. A missing pin is fine. A failure before the
/// replacement leaves the site declaring the node; a failure after it is reported as
/// [`NodeRemovalError::OutcomeUncertain`].
pub(super) fn unpublish_node(
    site_root: &Path,
    _lock: &SiteInputLock,
    plan: &NodeRemovalPlan,
) -> Result<(), NodeRemovalError> {
    let site_yaml = yaml::to_string(&plan.updated_site).map_err(NodeRemovalError::SerializeSite)?;
    let pin = plan.pin_path(site_root);
    match fs::remove_file(&pin) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(NodeRemovalError::Storage(
                anyhow::Error::new(error).context(format!("remove node pin {}", pin.display())),
            ));
        }
    }
    if let Some(pins) = pin.parent() {
        dotcore_domain::fs_sync::sync_directory(pins).map_err(|error| {
            NodeRemovalError::Storage(
                anyhow::Error::new(error).context(format!("sync pin directory {}", pins.display())),
            )
        })?;
    }

    match persist_bytes(
        &StdFileSystem,
        &site_root.join(SITE_FILE),
        site_yaml.as_bytes(),
    ) {
        Ok(()) => Ok(()),
        Err(error @ AtomicError::Storage(_)) => Err(NodeRemovalError::Storage(
            anyhow::Error::new(error).context("replace site.yaml"),
        )),
        Err(error @ AtomicError::OutcomeUncertain(_)) => Err(NodeRemovalError::OutcomeUncertain(
            anyhow::Error::new(error).context("replace site.yaml"),
        )),
    }
}

/// A node could not be planned for removal or removed from the site.
#[derive(Debug, thiserror::Error)]
pub enum NodeRemovalError {
    #[error(
        "node {node_name:?} is the site manager and cannot be removed; only worker nodes can leave the site"
    )]
    ManagerNode { node_name: String },

    #[error("no node {selector:?} in this site; declared nodes: {declared}")]
    UnknownNode { selector: String, declared: String },

    #[error(
        "{selector:?} is the id of node {id_node} and also the display name of node {name_node}; select the node to remove by the other one's id"
    )]
    AmbiguousNode {
        selector: String,
        id_node: String,
        name_node: String,
    },

    #[error(
        "node {node_name:?} still has {} deployment(s) placed on it:{}\nremove them through the console while the site runs, or drop their declarations without cleanup",
        .deployments.len(),
        list_lines(.deployments)
    )]
    PlacedDeployments {
        node_name: String,
        deployments: Vec<PlacedDeployment>,
    },

    #[error(
        "forgetting the deployments of node {node_name:?} would break routes or applications that stay:{}{}\nedit them on the running manager first, then retry",
        shared_route_lines(.shared_routes),
        linked_application_lines(.linked_applications)
    )]
    RouteConflicts {
        node_name: String,
        shared_routes: Vec<String>,
        linked_applications: Vec<ApplicationRouteReference>,
    },

    #[error("the site without the node would be invalid: {0}")]
    InvalidSite(#[source] SiteRepositoryError),

    #[error("site.yaml could not be read: {0}")]
    SiteUnreadable(#[source] SiteRepositoryError),

    #[error("updated site.yaml could not be serialized: {0}")]
    SerializeSite(#[source] dotcore_proto::yaml::RestrictedYamlError),

    #[error(
        "node removal storage failed; site.yaml still declares the node, retry the removal: {0}"
    )]
    Storage(#[source] anyhow::Error),

    #[error(
        "node removal outcome is uncertain; retry the removal with the node id to confirm it: {0}"
    )]
    OutcomeUncertain(#[source] anyhow::Error),
}

fn select_node<'a>(site: &'a Site, selector: &str) -> Result<&'a SiteNode, NodeRemovalError> {
    let by_id = site.nodes.iter().find(|node| node.node_id == selector);
    let by_name = site.nodes.iter().find(|node| node.display_name == selector);
    match (by_id, by_name) {
        (Some(by_id), Some(by_name)) if by_id.node_id != by_name.node_id => {
            Err(NodeRemovalError::AmbiguousNode {
                selector: selector.to_owned(),
                id_node: by_id.node_id.clone(),
                name_node: by_name.node_id.clone(),
            })
        }
        (Some(node), _) | (None, Some(node)) => Ok(node),
        (None, None) => Err(NodeRemovalError::UnknownNode {
            selector: selector.to_owned(),
            declared: site
                .nodes
                .iter()
                .map(|node| format!("{:?} ({})", node.display_name, node.node_id))
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn is_placed_on(placement: Option<&ExactPlacement>, node_id: &str) -> bool {
    placement.is_some_and(|placement| placement.site_node_id == node_id)
}

fn placed_model(deployment: &ModelDeploymentDeclaration) -> PlacedDeployment {
    let name = if deployment.model_deployment_name.is_empty() {
        match deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                local,
            )) => local.model_id.clone(),
            _ => String::new(),
        }
    } else {
        deployment.model_deployment_name.clone()
    };
    let state = match ModelDeploymentDesiredState::try_from(deployment.desired_state) {
        Ok(ModelDeploymentDesiredState::Present) => "present",
        Ok(ModelDeploymentDesiredState::Paused) => "paused",
        Ok(ModelDeploymentDesiredState::Teardown) => "teardown",
        Ok(ModelDeploymentDesiredState::Unspecified) | Err(_) => "unspecified",
    };
    PlacedDeployment {
        kind: DeploymentKind::Model,
        id: deployment.model_deployment_id.clone(),
        name,
        state,
    }
}

fn placed_application(deployment: &ApplicationDeploymentDeclaration) -> PlacedDeployment {
    let state = match ApplicationDeploymentDesiredState::try_from(deployment.desired_state) {
        Ok(ApplicationDeploymentDesiredState::Present) => "present",
        Ok(ApplicationDeploymentDesiredState::Paused) => "paused",
        Ok(ApplicationDeploymentDesiredState::Teardown) => "teardown",
        Ok(ApplicationDeploymentDesiredState::Unspecified) | Err(_) => "unspecified",
    };
    PlacedDeployment {
        kind: DeploymentKind::Application,
        id: deployment.application_deployment_id.clone(),
        name: deployment.application_deployment_name.clone(),
        state,
    }
}

/// Node ids derived when a node joins look like `node-6c62272e07`.
fn looks_like_node_id(selector: &str) -> bool {
    selector
        .strip_prefix("node-")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn list_lines(deployments: &[PlacedDeployment]) -> String {
    deployments
        .iter()
        .map(|deployment| format!("\n  - {deployment}"))
        .collect()
}

fn shared_route_lines(routes: &[String]) -> String {
    routes
        .iter()
        .map(|name| format!("\n  - route {name:?} also has a deployment that stays"))
        .collect()
}

fn linked_application_lines(applications: &[ApplicationRouteReference]) -> String {
    applications
        .iter()
        .map(|application| {
            format!(
                "\n  - application {} ({}) uses route {:?}, which would be deleted",
                application.application_deployment_id,
                application.application_deployment_name,
                application.inference_route_name
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dotcore_proto::dotcore::{
        inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
        model::v1::{
            ModelDeploymentLifecycleTransition, ModelDeploymentSnapshot,
            model_deployment_snapshot::Source as SnapshotSource,
        },
        site::v1::execution_target::RuntimeBinding,
    };

    use super::*;
    use crate::{
        initial::{
            input_store::try_acquire_exclusive_site_lock, validation::SiteInputBundleValidator,
        },
        membership::join::test_fixtures::{
            InitializedSite, initialized_site, worker_node, worker_targets,
        },
        node::folder::CertificateFingerprint,
        validation::load_validated_site,
    };

    const WORKER: &str = "node-00000000aa";

    #[test]
    fn removing_an_empty_worker_drops_exactly_the_node_its_targets_and_its_pin() {
        let fixture = installed_worker();
        let pins_before = pin_names(&fixture.root);
        let lock = lock(&fixture);

        let plan = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            false,
        )
        .expect("plan");
        unpublish_node(&fixture.root, &lock, &plan).expect("apply");

        assert_eq!(plan.node.node_id, WORKER);
        assert_eq!(plan.execution_targets, worker_targets(WORKER).to_vec());
        assert_eq!(plan.forgotten_deployments, []);
        assert_eq!(plan.deleted_routes, []);
        assert_eq!(
            load_validated_site(
                &fixture.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules()
            )
            .expect("reload"),
            fixture.base.site
        );
        let mut expected_pins = pins_before;
        expected_pins.retain(|name| name != &format!("{WORKER}.sha256"));
        assert_eq!(pin_names(&fixture.root), expected_pins);
    }

    #[test]
    fn the_node_is_selected_by_display_name_or_node_id() {
        let fixture = installed_worker();

        let by_name = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            false,
        )
        .expect("name");
        let by_id = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            WORKER,
            false,
        )
        .expect("id");

        assert_eq!(by_name, by_id);
    }

    #[test]
    fn the_manager_node_is_refused() {
        let fixture = installed_worker();
        let manager = fixture.base.site.nodes[0].clone();

        for selector in [manager.display_name.as_str(), manager.node_id.as_str()] {
            let error = plan_node_removal(
                &fixture.site_with_worker,
                &crate::test_support::site_declaration_rules(),
                selector,
                true,
            )
            .expect_err("manager");
            assert_eq!(
                error.to_string(),
                format!(
                    "node {:?} is the site manager and cannot be removed; only worker nodes can leave the site",
                    manager.display_name
                )
            );
        }
    }

    #[test]
    fn an_unknown_node_lists_the_declared_nodes() {
        let fixture = installed_worker();
        let manager = &fixture.base.site.nodes[0];

        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "missing",
            false,
        )
        .expect_err("unknown");

        assert_eq!(
            error.to_string(),
            format!(
                "no node \"missing\" in this site; declared nodes: {:?} ({}), \"worker\" ({WORKER})",
                manager.display_name, manager.node_id
            )
        );
    }

    #[test]
    fn placed_deployments_are_counted_per_node() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();

        assert_eq!(
            placed_deployment_count(&fixture.site_with_worker, WORKER),
            3
        );
        assert_eq!(
            placed_deployment_count(&fixture.site_with_worker, "node-other"),
            0
        );
    }

    #[test]
    fn placed_deployments_refuse_the_removal_and_write_nothing() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();
        fixture.write_site();
        let site_bytes = fs::read(fixture.root.join(SITE_FILE)).expect("site bytes");
        let pins = pin_names(&fixture.root);

        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            false,
        )
        .expect_err("placed deployments");

        assert_eq!(
            error.to_string(),
            "node \"worker\" still has 3 deployment(s) placed on it:\n  \
             - model deployment worker-model (Worker model, present)\n  \
             - model deployment torn-down (fixture-model, teardown)\n  \
             - application deployment worker-app (Worker app, present)\n\
             remove them through the console while the site runs, or drop their declarations without cleanup"
        );
        assert_eq!(
            fs::read(fixture.root.join(SITE_FILE)).expect("site bytes"),
            site_bytes
        );
        assert_eq!(pin_names(&fixture.root), pins);
    }

    #[test]
    fn forgetting_drops_placed_deployments_and_the_routes_only_they_serve() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();
        let site = &mut fixture.site_with_worker;
        site.inference_routes = vec![
            route("sole", vec![single("worker-model")]),
            route("group", vec![round_robin(&["second-model", "third-model"])]),
            route("keep", vec![single("upstream")]),
        ];
        site.model_deployments.push(placed_model(
            "second-model",
            WORKER,
            &worker_model_target(),
            "present",
        ));
        site.model_deployments.push(placed_model(
            "third-model",
            WORKER,
            &worker_model_target(),
            "present",
        ));
        site.application_deployments[0].inference_route_id = Some("sole".to_owned());
        let before_allocator = site.model_deployment_identity_allocator.clone();
        let lock = lock(&fixture);

        let plan = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            true,
        )
        .expect("plan");
        unpublish_node(&fixture.root, &lock, &plan).expect("apply");

        let written = load_validated_site(
            &fixture.root.join(SITE_FILE),
            &crate::test_support::site_declaration_rules(),
        )
        .expect("reload");
        assert_eq!(&written, plan.updated_site());
        assert_eq!(
            plan.forgotten_deployments
                .iter()
                .map(|deployment| deployment.id.as_str())
                .collect::<Vec<_>>(),
            [
                "worker-model",
                "torn-down",
                "second-model",
                "third-model",
                "worker-app"
            ]
        );
        assert_eq!(
            plan.deleted_routes
                .iter()
                .map(|route| route.name.as_str())
                .collect::<Vec<_>>(),
            ["sole", "group"]
        );
        assert_eq!(
            written
                .model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["upstream"]
        );
        assert_eq!(written.application_deployments, []);
        assert_eq!(
            written
                .inference_routes
                .iter()
                .map(|route| route.name.as_str())
                .collect::<Vec<_>>(),
            ["keep"]
        );
        assert_eq!(
            written.model_deployment_identity_allocator,
            before_allocator
        );
    }

    #[test]
    fn a_route_with_a_surviving_member_refuses_the_removal() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();
        fixture.site_with_worker.inference_routes = vec![route(
            "shared",
            vec![single("worker-model"), single("upstream")],
        )];

        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            true,
        )
        .expect_err("shared route");

        assert_eq!(
            error.to_string(),
            "forgetting the deployments of node \"worker\" would break routes or applications that stay:\n  \
             - route \"shared\" also has a deployment that stays\n\
             edit them on the running manager first, then retry"
        );
    }

    #[test]
    fn a_surviving_application_on_a_deleted_route_refuses_but_a_forgotten_one_does_not() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();
        fixture.site_with_worker.inference_routes =
            vec![route("sole", vec![single("worker-model")])];
        let mut survivor = fixture.site_with_worker.application_deployments[0].clone();
        survivor.application_deployment_id = "manager-app".to_owned();
        survivor.application_deployment_name = "Manager app".to_owned();
        let manager_target = fixture.manager_application_target();
        let placement = ExactPlacement {
            site_node_id: fixture.base.site.nodes[0].node_id.clone(),
            execution_target_id: manager_target.execution_target_id.clone(),
        };
        survivor.placement = Some(placement.clone());
        if let Some(snapshot) = survivor.snapshot.as_mut() {
            snapshot.placement = Some(placement);
            snapshot.platform = manager_target.platform.clone();
        }
        survivor.inference_route_id = Some("sole".to_owned());
        fixture.site_with_worker.application_deployments[0].inference_route_id =
            Some("sole".to_owned());

        // Both the forgotten app and the survivor use the route: only the survivor blocks.
        fixture
            .site_with_worker
            .application_deployments
            .push(survivor);
        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            true,
        )
        .expect_err("linked survivor");
        assert_eq!(
            error.to_string(),
            "forgetting the deployments of node \"worker\" would break routes or applications that stay:\n  \
             - application manager-app (Manager app) uses route \"sole\", which would be deleted\n\
             edit them on the running manager first, then retry"
        );

        // Without the survivor, the forgotten app alone does not block.
        fixture.site_with_worker.application_deployments.pop();
        let plan = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            true,
        )
        .expect("plan");
        assert_eq!(
            plan.deleted_routes,
            [DeletedRoute {
                id: "sole".to_owned(),
                name: "sole".to_owned()
            }]
        );
    }

    #[test]
    fn a_shared_route_and_a_linked_application_are_reported_together() {
        let mut fixture = installed_worker();
        fixture.place_on_worker();
        fixture.site_with_worker.inference_routes = vec![
            route("shared", vec![single("worker-model"), single("upstream")]),
            route("sole", vec![single("torn-down")]),
        ];
        let mut survivor = fixture.site_with_worker.application_deployments[0].clone();
        survivor.application_deployment_id = "manager-app".to_owned();
        survivor.application_deployment_name = "Manager app".to_owned();
        let manager_target = fixture.manager_application_target();
        let placement = ExactPlacement {
            site_node_id: fixture.base.site.nodes[0].node_id.clone(),
            execution_target_id: manager_target.execution_target_id.clone(),
        };
        survivor.placement = Some(placement.clone());
        if let Some(snapshot) = survivor.snapshot.as_mut() {
            snapshot.placement = Some(placement);
            snapshot.platform = manager_target.platform.clone();
        }
        survivor.inference_route_id = Some("sole".to_owned());
        fixture
            .site_with_worker
            .application_deployments
            .push(survivor);

        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            "worker",
            true,
        )
        .expect_err("both conflicts");

        let message = error.to_string();
        assert!(
            message.contains("route \"shared\" also has a deployment that stays")
                && message.contains("application manager-app (Manager app) uses route \"sole\""),
            "{message}"
        );
    }

    #[test]
    fn a_retry_after_a_failed_swap_finishes_the_removal() {
        let fixture = installed_worker();
        // The pin was removed, then replacing site.yaml failed: the node is still declared.
        fs::remove_file(pin_dir(&fixture.root).join(format!("{WORKER}.sha256"))).expect("pin");
        let site =
            OfflineSite::open(&fixture.root, &SiteInputBundleValidator::for_test()).expect("open");

        let NodeRemoval::Planned(plan) = site.plan_node_removal("worker", false).expect("plan")
        else {
            panic!("the node is still declared");
        };
        site.remove_node(&plan)
            .expect("retry tolerates the missing pin");

        assert_eq!(
            load_validated_site(
                &fixture.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules()
            )
            .expect("reload"),
            fixture.base.site
        );
    }

    #[test]
    fn a_retry_by_id_after_the_swap_landed_reports_already_removed() {
        let fixture = installed_worker();
        {
            let site = OfflineSite::open(&fixture.root, &SiteInputBundleValidator::for_test())
                .expect("open");
            let NodeRemoval::Planned(plan) = site.plan_node_removal(WORKER, false).expect("plan")
            else {
                panic!("declared");
            };
            site.remove_node(&plan).expect("remove");
        }
        let site = OfflineSite::open(&fixture.root, &SiteInputBundleValidator::for_test())
            .expect("reopen");

        assert_eq!(
            site.plan_node_removal(WORKER, false).expect("by id"),
            NodeRemoval::AlreadyRemoved {
                node_id: WORKER.to_owned()
            }
        );
        assert!(matches!(
            site.plan_node_removal("worker", false),
            Err(NodeRemovalError::UnknownNode { .. })
        ));
    }

    #[test]
    fn a_selector_that_is_one_nodes_id_and_another_nodes_name_is_refused() {
        let mut fixture = installed_worker();
        let manager_id = fixture.base.site.nodes[0].node_id.clone();
        fixture.site_with_worker.nodes[1].display_name = manager_id.clone();

        let error = plan_node_removal(
            &fixture.site_with_worker,
            &crate::test_support::site_declaration_rules(),
            &manager_id,
            false,
        )
        .expect_err("ambiguous");

        assert_eq!(
            error.to_string(),
            format!(
                "{manager_id:?} is the id of node {manager_id} and also the display name of node {WORKER}; select the node to remove by the other one's id"
            )
        );
        // The other node's id is unambiguous.
        assert_eq!(
            plan_node_removal(
                &fixture.site_with_worker,
                &crate::test_support::site_declaration_rules(),
                WORKER,
                false
            )
            .expect("by id")
            .node
            .node_id,
            WORKER
        );
    }

    #[test]
    fn an_unreadable_pin_location_is_an_error_not_an_already_removed_node() {
        let fixture = installed_worker();
        let site =
            OfflineSite::open(&fixture.root, &SiteInputBundleValidator::for_test()).expect("open");
        let pins = pin_dir(&fixture.root);
        fs::remove_dir_all(&pins).expect("remove pins");
        fs::write(&pins, "not a directory").expect("block the pin directory");

        let result = site.plan_node_removal("node-0123456789abcdef", false);

        let Err(NodeRemovalError::Storage(error)) = result else {
            panic!("expected a storage error, got {result:?}");
        };
        assert!(
            format!("{error:#}").starts_with("inspect the node pin "),
            "{error:#}"
        );
    }

    /// An initialized site with one worker, declared in `site.yaml` and pinned on disk.
    struct WorkerFixture {
        base: InitializedSite,
        root: PathBuf,
        site_with_worker: Site,
    }

    impl WorkerFixture {
        fn place_on_worker(&mut self) {
            let target = worker_model_target();
            let site = &mut self.site_with_worker;
            site.model_deployments = vec![
                placed_model("worker-model", WORKER, &target, "present"),
                placed_model("torn-down", WORKER, &target, "teardown"),
                upstream_model("upstream"),
            ];
            site.application_deployments = vec![placed_application(
                "worker-app",
                WORKER,
                &worker_application_target(),
                self.base
                    .site
                    .execution_targets
                    .iter()
                    .chain(&site.execution_targets),
            )];
            self.write_site();
        }

        fn write_site(&self) {
            validate_site(
                &self.site_with_worker,
                &crate::test_support::site_declaration_rules(),
            )
            .expect("fixture site is valid");
            fs::write(
                self.root.join(SITE_FILE),
                yaml::to_string(&self.site_with_worker).expect("yaml"),
            )
            .expect("write site.yaml");
        }

        fn manager_application_target(&self) -> ExecutionTarget {
            self.base
                .site
                .execution_targets
                .iter()
                .find(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(RuntimeBinding::ApplicationRuntime(_))
                    )
                })
                .expect("manager application target")
                .clone()
        }
    }

    fn installed_worker() -> WorkerFixture {
        let base = initialized_site();
        let root = base.root.clone();
        let mut site = base.site.clone();
        site.nodes.push(worker_node(WORKER, "worker", "10.0.0.7"));
        site.execution_targets
            .extend(worker_targets(WORKER).iter().cloned());
        let fixture = WorkerFixture {
            base,
            root,
            site_with_worker: site,
        };
        fixture.write_site();
        fs::write(
            pin_dir(&fixture.root).join(format!("{WORKER}.sha256")),
            format!(
                "{}\n",
                CertificateFingerprint::for_certificate(b"leaf").as_str()
            ),
        )
        .expect("worker pin");
        fixture
    }

    fn lock(fixture: &WorkerFixture) -> SiteInputLock {
        try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free")
    }

    fn pin_dir(root: &Path) -> PathBuf {
        root.join(SITE_TRUST_DIRECTORY).join(WORKER_PIN_DIRECTORY)
    }

    fn pin_names(root: &Path) -> Vec<String> {
        let mut names = fs::read_dir(pin_dir(root))
            .expect("pins")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn worker_model_target() -> String {
        format!("{WORKER}-model-target")
    }

    fn worker_application_target() -> String {
        format!("{WORKER}-application-target")
    }

    fn placed_model(
        id: &str,
        node_id: &str,
        target_id: &str,
        state: &str,
    ) -> ModelDeploymentDeclaration {
        let (desired_state, transition) = if state == "teardown" {
            (
                ModelDeploymentDesiredState::Teardown,
                ModelDeploymentLifecycleTransition::Teardown,
            )
        } else {
            (
                ModelDeploymentDesiredState::Present,
                ModelDeploymentLifecycleTransition::Create,
            )
        };
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: if id == "torn-down" {
                String::new()
            } else {
                "Worker model".to_owned()
            },
            placement: Some(ExactPlacement {
                site_node_id: node_id.to_owned(),
                execution_target_id: target_id.to_owned(),
            }),
            requested_parallel_shape: dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One
                as i32,
            snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
                "fixture-model",
            )),
            desired_state: desired_state as i32,
            lifecycle_generation: 1,
            lifecycle_transition: transition as i32,
            ..Default::default()
        }
    }

    fn upstream_model(id: &str) -> ModelDeploymentDeclaration {
        use dotcore_proto::dotcore::{
            catalog::v1::{
                UpstreamModelBoundary, UpstreamModelDataEgressClass, UpstreamModelLocality,
                UpstreamModelTemplate, UpstreamWireProtocol,
            },
            model::v1::{
                OpenAiCompatibleEndpointConfiguration, UpstreamModelDeploymentSnapshot,
                UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
            },
        };

        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(SnapshotSource::Upstream(UpstreamModelDeploymentSnapshot {
                    upstream_model_template: Some(UpstreamModelTemplate {
                        upstream_model_template_id: "openai-compatible".to_owned(),
                        display_name: "OpenAI-compatible".to_owned(),
                        wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
                    }),
                })),
            }),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: Some(UpstreamModelDesiredConfiguration {
                upstream_model_id: "remote".to_owned(),
                api_key: "key".to_owned(),
                input_capabilities: Some(UpstreamModelInputCapabilities {
                    supports_text: true,
                    supports_image: false,
                }),
                endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                    base_url: "https://endpoint.example/v1".to_owned(),
                    allow_insecure_http: false,
                }),
                boundary: Some(UpstreamModelBoundary {
                    locality: UpstreamModelLocality::PublicExternal as i32,
                    trust_domain: "endpoint.example".to_owned(),
                    data_egress_class: UpstreamModelDataEgressClass::PublicInternet as i32,
                }),
            }),
            lifecycle_generation: 1,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Create as i32,
            ..Default::default()
        }
    }

    fn placed_application<'a>(
        id: &str,
        node_id: &str,
        target_id: &str,
        targets: impl Iterator<Item = &'a ExecutionTarget>,
    ) -> ApplicationDeploymentDeclaration {
        use dotcore_proto::dotcore::application::v1::ResolvedApplicationDeploymentSnapshot;

        let targets = targets
            .map(|target| (target.execution_target_id.as_str(), target))
            .collect::<BTreeMap<_, _>>();
        let placement = ExactPlacement {
            site_node_id: node_id.to_owned(),
            execution_target_id: target_id.to_owned(),
        };
        ApplicationDeploymentDeclaration {
            application_deployment_id: id.to_owned(),
            application_deployment_name: "Worker app".to_owned(),
            placement: Some(placement.clone()),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                placement: Some(placement),
                platform: targets[target_id].platform.clone(),
                ..Default::default()
            }),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            lifecycle_generation: 1,
            ..Default::default()
        }
    }

    fn route(name: &str, fallback_groups: Vec<FallbackGroup>) -> InferenceRoute {
        InferenceRoute {
            inference_route_id: name.to_owned(),
            name: name.to_owned(),
            enabled: true,
            fallback_groups,
            generation: 1,
        }
    }

    fn single(member: &str) -> FallbackGroup {
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![member.to_owned()],
        }
    }

    fn round_robin(members: &[&str]) -> FallbackGroup {
        FallbackGroup {
            selection: FallbackGroupSelection::RoundRobin as i32,
            model_deployment_ids: members.iter().map(|member| (*member).to_owned()).collect(),
        }
    }
}
