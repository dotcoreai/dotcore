//! Validation for desired site configuration and its typed records.

use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::Path,
    time::Duration,
};

use anyhow::anyhow;
use dotcore_catalog::validate_upstream_model_template;
use dotcore_domain::SiteNodeHost;
use dotcore_domain::{
    APPLE_GPU_DEVICE_ID, AcceleratorDeviceSelector, ExecutionPlatform,
    inference_route::{is_valid_route_name, validate_route_shape},
    names_every_cdi_device, valid_id,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        ModelParallelShape, OciPlatform, UpstreamModelDataEgressClass, UpstreamModelLocality,
        UpstreamWireProtocol,
    },
    deployment::v1::ExactPlacement,
    inference::v1::InferenceRoute,
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentDesiredState,
        ModelDeploymentLifecycleTransition,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
    site::v1::{
        ExecutionTarget, NativeRuntimeAdapter, NativeWorkloadLifetime, Site, SiteNode,
        application_runtime_binding, execution_target, model_runtime_binding,
    },
};
use dotcore_proto::yaml::{self, RestrictedYamlError};
use url::Url;

use crate::repository::SiteRepositoryError;

/// Hardware refresh interval of a site that declares none.
/// Not a behavior duration: default of the site-declared hardware refresh interval.
pub(crate) const DEFAULT_HARDWARE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Shortest hardware refresh interval a site may declare.
/// Not a behavior duration: limit on a site-declared interval.
const MIN_HARDWARE_REFRESH_INTERVAL_SECONDS: u32 = 1;

/// Longest hardware refresh interval a site may declare.
/// Not a behavior duration: limit on a site-declared interval.
const MAX_HARDWARE_REFRESH_INTERVAL_SECONDS: u32 = 3_600;

/// Returns the validated site-wide current hardware refresh interval.
pub fn hardware_refresh_interval(site: &Site) -> Duration {
    site.hardware_observation
        .as_ref()
        .and_then(|configuration| configuration.refresh_interval_seconds)
        .map_or(DEFAULT_HARDWARE_REFRESH_INTERVAL, |seconds| {
            Duration::from_secs(u64::from(seconds))
        })
}

/// Owner rules site validation applies to declaration content it cannot judge itself, supplied by
/// composition and the CLI so the site names no owner.
#[derive(Clone, Copy, Debug)]
pub struct SiteDeclarationRules {
    /// Validates a local model deployment's immutable snapshot and its recorded node pins for
    /// resolution on the SiteNode hosting it.
    pub validate_local_model_declaration:
        fn(&ModelDeploymentDeclaration) -> Result<(), dotcore_catalog::CatalogError>,
}

/// Decodes and validates one desired site declaration without loading site store state.
pub fn load_validated_site(
    path: &Path,
    rules: &SiteDeclarationRules,
) -> Result<Site, SiteRepositoryError> {
    let site = decode_site(path)?;
    validate_site(&site, rules)?;
    reject_insecure_credential_file(path, &site)?;
    Ok(site)
}

/// Decodes one desired site declaration, reporting values the site schema does not know
/// separately from unreadable or malformed YAML.
pub fn decode_site(path: &Path) -> Result<Site, SiteRepositoryError> {
    yaml::from_file(path).map_err(|error| match error {
        RestrictedYamlError::ProtoJsonCodecError(source) => {
            SiteRepositoryError::SchemaMismatch(source)
        }
        other => SiteRepositoryError::Yaml(other),
    })
}

pub fn validate_site(site: &Site, rules: &SiteDeclarationRules) -> Result<(), SiteRepositoryError> {
    if !valid_id(&site.site_id) {
        return Err(SiteRepositoryError::InvalidSite(anyhow::anyhow!(
            "site.yaml siteId must match [a-z][a-z0-9-]{{0,62}}"
        )));
    }
    if let Some(seconds) = site
        .hardware_observation
        .as_ref()
        .and_then(|configuration| configuration.refresh_interval_seconds)
        && !(MIN_HARDWARE_REFRESH_INTERVAL_SECONDS..=MAX_HARDWARE_REFRESH_INTERVAL_SECONDS)
            .contains(&seconds)
    {
        return invalid_site(format!(
            "site.yaml hardwareObservation.refreshIntervalSeconds must be between {MIN_HARDWARE_REFRESH_INTERVAL_SECONDS} and {MAX_HARDWARE_REFRESH_INTERVAL_SECONDS}; got {seconds}"
        ));
    }
    validate_model_identity_allocator(site)?;
    let node_ids = validate_nodes(&site.nodes)?;
    let execution_targets = validate_execution_targets(&site.execution_targets, &node_ids)?;
    let mut deployment_ids = BTreeSet::new();
    let mut route_deployments = BTreeMap::new();
    for deployment in &site.model_deployments {
        if !valid_id(&deployment.model_deployment_id) {
            return invalid_site("site.yaml active deployment has an invalid identifier");
        }
        if !deployment_ids.insert(&deployment.model_deployment_id) {
            return invalid_site(format!(
                "site.yaml has duplicate active deployment id {:?}",
                deployment.model_deployment_id
            ));
        }
        validate_model_lifecycle_identity(deployment)?;
        match ModelDeploymentDesiredState::try_from(deployment.desired_state) {
            Ok(
                ModelDeploymentDesiredState::Present
                | ModelDeploymentDesiredState::Paused
                | ModelDeploymentDesiredState::Teardown,
            ) => {}
            Err(_) => {
                return invalid_site("site.yaml active deployment has an invalid desired state");
            }
            Ok(ModelDeploymentDesiredState::Unspecified) => {
                return invalid_site(
                    "site.yaml active deployment must declare PRESENT, PAUSED, or TEARDOWN",
                );
            }
        }
        match deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(ModelDeploymentSnapshotSource::Upstream(_)) => {
                validate_upstream_deployment(deployment)?;
                route_deployments.insert(
                    deployment.model_deployment_id.clone(),
                    RouteDeployment {
                        desired_state: deployment.desired_state,
                    },
                );
                continue;
            }
            Some(ModelDeploymentSnapshotSource::Local(_)) => {
                if deployment.upstream_desired_configuration.is_some() {
                    return invalid_site(
                        "site.yaml local model deployment has upstream private configuration",
                    );
                }
            }
            None => {
                return invalid_site("site.yaml active deployment has no immutable model snapshot");
            }
        }
        // Validate exact placement for local deployments
        match deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(ModelDeploymentSnapshotSource::Local(_)) => {
                let placement = deployment.placement.as_ref().ok_or_else(|| {
                    SiteRepositoryError::invalid_site(format!(
                        "site.yaml local model deployment {:?} requires exact placement",
                        deployment.model_deployment_id
                    ))
                })?;
                validate_exact_placement(placement, &execution_targets, WorkloadKind::Model)?;
            }
            Some(ModelDeploymentSnapshotSource::Upstream(_)) => {
                if deployment.placement.is_some() {
                    return invalid_site(format!(
                        "site.yaml upstream model deployment {:?} must not have placement",
                        deployment.model_deployment_id
                    ));
                }
            }
            None => {
                return invalid_site("site.yaml active deployment has no immutable model snapshot");
            }
        }
        let shape = ModelParallelShape::try_from(deployment.requested_parallel_shape)
            .ok()
            .filter(|shape| *shape == ModelParallelShape::One)
            .ok_or_else(|| {
                SiteRepositoryError::InvalidSite(anyhow::anyhow!(
                    "site.yaml active deployment {:?} requests an unsupported parallel shape",
                    deployment.model_deployment_id
                ))
            })?;
        let Some(snapshot) = deployment.snapshot.as_ref() else {
            return invalid_site("site.yaml active deployment has no immutable model snapshot");
        };
        let profile_supports_shape = snapshot
            .source
            .as_ref()
            .and_then(|source| match source {
                ModelDeploymentSnapshotSource::Local(local) => {
                    local.execution_capabilities.as_ref()
                }
                ModelDeploymentSnapshotSource::Upstream(_) => None,
            })
            .is_some_and(|capabilities| {
                capabilities
                    .supported_parallel_shapes
                    .contains(&(shape as i32))
            });
        // The hosting node resolves a local-development engine itself, on every node.
        (rules.validate_local_model_declaration)(deployment).map_err(|source| {
            SiteRepositoryError::InvalidSite(anyhow::Error::new(InvalidModelSnapshotError {
                deployment_id: deployment.model_deployment_id.clone(),
                source,
            }))
        })?;
        if !profile_supports_shape {
            return invalid_site(format!(
                "site.yaml active deployment {:?} has an immutable model snapshot that does not support its parallel shape",
                deployment.model_deployment_id
            ));
        }
        route_deployments.insert(
            deployment.model_deployment_id.clone(),
            RouteDeployment {
                desired_state: deployment.desired_state,
            },
        );
    }
    validate_application_deployments(&site.application_deployments, &execution_targets)?;
    validate_inference_routes(&site.inference_routes, &route_deployments)?;
    validate_application_route_bindings(&site.application_deployments, &site.inference_routes)
}

fn validate_model_identity_allocator(site: &Site) -> Result<(), SiteRepositoryError> {
    let Some(allocator) = &site.model_deployment_identity_allocator else {
        return invalid_site("site.yaml model deployment identity allocator is missing");
    };
    if allocator.namespace.len() != 16
        || !allocator
            .namespace
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || allocator.next_sequence == 0
    {
        return invalid_site("site.yaml model deployment identity allocator is invalid");
    }
    Ok(())
}

fn validate_model_lifecycle_identity(
    deployment: &ModelDeploymentDeclaration,
) -> Result<(), SiteRepositoryError> {
    if deployment.lifecycle_generation == 0 {
        return invalid_site("site.yaml model lifecycle identity is incomplete");
    }
    let transition = ModelDeploymentLifecycleTransition::try_from(deployment.lifecycle_transition)
        .map_err(|_| {
            SiteRepositoryError::invalid_site("site.yaml model lifecycle transition is invalid")
        })?;
    let desired =
        ModelDeploymentDesiredState::try_from(deployment.desired_state).map_err(|_| {
            SiteRepositoryError::invalid_site("site.yaml model desired state is invalid")
        })?;
    let compatible = matches!(
        (transition, desired),
        (
            ModelDeploymentLifecycleTransition::Create | ModelDeploymentLifecycleTransition::Resume,
            ModelDeploymentDesiredState::Present
        ) | (
            ModelDeploymentLifecycleTransition::Pause,
            ModelDeploymentDesiredState::Paused
        ) | (
            ModelDeploymentLifecycleTransition::Teardown,
            ModelDeploymentDesiredState::Teardown
        )
    );
    if !compatible {
        return invalid_site("site.yaml model lifecycle transition conflicts with desired state");
    }
    Ok(())
}
fn validate_application_route_bindings(
    applications: &[dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration],
    routes: &[InferenceRoute],
) -> Result<(), SiteRepositoryError> {
    let route_ids = routes
        .iter()
        .map(|route| route.inference_route_id.as_str())
        .collect::<BTreeSet<_>>();
    for application in applications {
        if let Some(inference_route_id) = application.inference_route_id.as_deref()
            && !route_ids.contains(inference_route_id)
        {
            return invalid_site(format!(
                "site.yaml application deployment {:?} references missing inference route {:?}",
                application.application_deployment_id, inference_route_id
            ));
        }
    }
    Ok(())
}

fn validate_inference_routes(
    routes: &[InferenceRoute],
    deployments: &BTreeMap<String, RouteDeployment>,
) -> Result<(), SiteRepositoryError> {
    let mut route_ids = BTreeSet::new();
    let mut route_names = BTreeSet::new();
    for route in routes {
        if !valid_id(&route.inference_route_id) {
            return invalid_site("site.yaml inference route has an invalid id");
        }
        if !is_valid_route_name(&route.name) {
            return invalid_site("site.yaml inference route has an invalid name");
        }
        if route.generation == 0 {
            return invalid_site("site.yaml inference route must have a positive generation");
        }
        if !route_ids.insert(&route.inference_route_id) {
            return invalid_site(format!(
                "site.yaml has duplicate inference route id {:?}",
                route.inference_route_id
            ));
        }
        if !route_names.insert(&route.name) {
            return invalid_site(format!(
                "site.yaml has duplicate inference route name {:?}",
                route.name
            ));
        }
        validate_route_shape(route).map_err(|error| {
            SiteRepositoryError::invalid_site_with_source(
                format!(
                    "site.yaml inference route {:?} has an invalid shape",
                    route.name
                ),
                error,
            )
        })?;
    }
    for route in routes {
        validate_route_member_references(route, deployments)?;
    }
    Ok(())
}

fn validate_route_member_references(
    route: &InferenceRoute,
    deployments: &BTreeMap<String, RouteDeployment>,
) -> Result<(), SiteRepositoryError> {
    for group in &route.fallback_groups {
        for deployment_id in &group.model_deployment_ids {
            let deployment = deployments.get(deployment_id).ok_or_else(|| {
                SiteRepositoryError::invalid_site(format!(
                    "site.yaml inference route {:?} references missing model deployment {:?}",
                    route.name, deployment_id
                ))
            })?;
            if deployment.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
                return invalid_site(format!(
                    "site.yaml inference route {:?} references tearing-down model deployment {:?}",
                    route.name, deployment_id
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RouteDeployment {
    desired_state: i32,
}

pub(super) fn reject_insecure_credential_file(
    path: &Path,
    site: &Site,
) -> Result<(), SiteRepositoryError> {
    let has_credential = site.model_deployments.iter().any(|deployment| {
        deployment
            .upstream_desired_configuration
            .as_ref()
            .is_some_and(|configuration| !configuration.api_key.is_empty())
    });
    if has_credential {
        let mode = std::fs::metadata(path)
            .map_err(credential_metadata_error)?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return invalid_site(format!(
                "credential-bearing site.yaml {:?} must be owner-only (0600); run chmod 600 {:?}",
                path, path
            ));
        }
    }
    Ok(())
}

fn credential_metadata_error(source: std::io::Error) -> SiteRepositoryError {
    SiteRepositoryError::invalid_site_with_source(
        "inspect credential-bearing site.yaml permissions",
        source,
    )
}

fn validate_nodes(site_nodes: &[SiteNode]) -> Result<BTreeSet<String>, SiteRepositoryError> {
    if site_nodes.is_empty() {
        return invalid_site("site.yaml must declare at least one node");
    }

    let mut node_ids = BTreeSet::new();
    let mut display_names = BTreeSet::new();
    let mut site_manager_count = 0;
    for node in site_nodes {
        if !valid_id(&node.node_id) {
            return invalid_site(format!(
                "site.yaml node ID {:?} must match [a-z][a-z0-9-]{{0,62}}",
                node.node_id
            ));
        }
        if !node_ids.insert(node.node_id.clone()) {
            return invalid_site(format!(
                "site.yaml has duplicate node ID {:?}",
                node.node_id
            ));
        }
        SiteNodeHost::parse(&node.host).map_err(|error| {
            SiteRepositoryError::InvalidSite(anyhow!(
                "site.yaml node {:?} host {error}",
                node.node_id
            ))
        })?;
        validate_site_node_manager_host(node)?;

        if !is_valid_display_name(&node.display_name) {
            return invalid_site(format!(
                "site.yaml node {:?} has an invalid display name",
                node.node_id
            ));
        }
        if !display_names.insert(node.display_name.clone()) {
            return invalid_site(format!(
                "site.yaml has duplicate node display name {:?}",
                node.display_name
            ));
        }
        if node.site_manager.is_some() {
            site_manager_count += 1;
        }
    }
    if site_manager_count != 1 {
        return invalid_site("site.yaml must declare exactly one site manager");
    }
    Ok(node_ids)
}

/// Checks the address a node reaches the site manager at: empty selects the declared manager
/// host, and the manager node itself never declares one.
fn validate_site_node_manager_host(node: &SiteNode) -> Result<(), SiteRepositoryError> {
    if node.manager_host.is_empty() {
        return Ok(());
    }
    if node.site_manager.is_some() {
        return invalid_site(format!(
            "site.yaml site manager node {:?} must not declare a manager host",
            node.node_id
        ));
    }
    SiteNodeHost::parse(&node.manager_host).map_err(|error| {
        SiteRepositoryError::InvalidSite(anyhow!(
            "site.yaml node {:?} manager host {error}",
            node.node_id
        ))
    })?;
    Ok(())
}

/// Longest SiteNode or execution target display name, in characters once trimmed.
pub const MAX_DISPLAY_NAME_CHARS: usize = 128;

/// Returns whether `name` is a valid SiteNode or execution target display name: 1 to
/// [`MAX_DISPLAY_NAME_CHARS`] non-control characters once trimmed.
pub fn is_valid_display_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control)
}

/// An active local model deployment in `site.yaml` keeps an immutable model snapshot this build
/// cannot use, so the deployment has to be created again. It is the typed cause of the site's
/// validation refusal, so callers can recognize it in the error chain.
#[derive(Debug, thiserror::Error)]
#[error(
    "site.yaml active deployment {deployment_id:?} has an invalid immutable model snapshot; remove the deployment and create it again, or reset the site"
)]
pub struct InvalidModelSnapshotError {
    /// Model deployment whose snapshot is invalid.
    pub deployment_id: String,

    #[source]
    source: dotcore_catalog::CatalogError,
}

fn validate_upstream_deployment(
    deployment: &ModelDeploymentDeclaration,
) -> Result<(), SiteRepositoryError> {
    if !valid_id(&deployment.model_deployment_id)
        || deployment.placement.is_some()
        || deployment.requested_parallel_shape != ModelParallelShape::Unspecified as i32
        || !deployment.node_resolved_launch_identities.is_empty()
    {
        return invalid_site(
            "site.yaml upstream model deployment has local-only target configuration",
        );
    }
    let deployment_snapshot = deployment.snapshot.as_ref().ok_or_else(|| {
        SiteRepositoryError::invalid_site(
            "site.yaml upstream model deployment has no immutable upstream snapshot",
        )
    })?;
    if !matches!(
        deployment_snapshot.source,
        Some(ModelDeploymentSnapshotSource::Upstream(_))
    ) {
        return invalid_site("site.yaml upstream model snapshot contains local catalog data");
    }
    let snapshot = match deployment_snapshot.source.as_ref() {
        Some(ModelDeploymentSnapshotSource::Upstream(snapshot)) => snapshot,
        _ => unreachable!(),
    };
    let template = snapshot.upstream_model_template.as_ref().ok_or_else(|| {
        SiteRepositoryError::invalid_site(
            "site.yaml upstream model deployment has no immutable upstream model template snapshot",
        )
    })?;
    validate_upstream_model_template_snapshot(template)?;
    if template.wire_protocol != UpstreamWireProtocol::OpenaiCompatible as i32 {
        return invalid_site(
            "site.yaml upstream model template must use OPENAI_COMPATIBLE wire protocol",
        );
    }
    let configuration = deployment
        .upstream_desired_configuration
        .as_ref()
        .ok_or_else(|| {
            SiteRepositoryError::invalid_site(
                "site.yaml upstream model deployment has no private target configuration",
            )
        })?;
    if configuration.upstream_model_id.trim().is_empty() {
        return invalid_site("site.yaml upstream model deployment requires provider model ID");
    }
    if !configuration
        .input_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.supports_text)
    {
        return invalid_site("site.yaml upstream model deployment requires text input capability");
    }
    let endpoint = configuration.endpoint.as_ref().ok_or_else(|| {
        SiteRepositoryError::invalid_site(
            "site.yaml upstream model deployment requires endpoint configuration",
        )
    })?;
    validate_endpoint(endpoint)?;
    let boundary = configuration.boundary.as_ref().ok_or_else(|| {
        SiteRepositoryError::invalid_site(
            "site.yaml upstream model deployment requires an operator boundary",
        )
    })?;
    validate_boundary(boundary)
}

fn validate_upstream_model_template_snapshot(
    template: &dotcore_proto::dotcore::catalog::v1::UpstreamModelTemplate,
) -> Result<(), SiteRepositoryError> {
    if let Some(error) = validate_upstream_model_template(template)
        .into_iter()
        .next()
    {
        return Err(SiteRepositoryError::InvalidSite(error.context(
            "site.yaml immutable upstream model template snapshot is invalid",
        )));
    }
    Ok(())
}

fn validate_endpoint(
    endpoint: &dotcore_proto::dotcore::model::v1::OpenAiCompatibleEndpointConfiguration,
) -> Result<(), SiteRepositoryError> {
    let Some(url) = Url::parse(&endpoint.base_url).ok().filter(|url| {
        url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    }) else {
        return invalid_site(
            "site.yaml upstream model endpoint requires an absolute HTTP(S) base URL",
        );
    };
    match url.scheme() {
        "https" if !endpoint.allow_insecure_http => Ok(()),
        "http" if endpoint.allow_insecure_http => Ok(()),
        _ => invalid_site("site.yaml upstream model endpoint has invalid insecure-HTTP opt-in"),
    }
}

fn validate_boundary(
    boundary: &dotcore_proto::dotcore::catalog::v1::UpstreamModelBoundary,
) -> Result<(), SiteRepositoryError> {
    let matches_locality = matches!((boundary.locality, boundary.data_egress_class),
        (locality, egress) if locality == UpstreamModelLocality::Local as i32 && egress == UpstreamModelDataEgressClass::Local as i32
        || locality == UpstreamModelLocality::Private as i32 && egress == UpstreamModelDataEgressClass::PrivateNetwork as i32
        || locality == UpstreamModelLocality::PublicExternal as i32 && egress == UpstreamModelDataEgressClass::PublicInternet as i32);
    if !matches_locality || boundary.trust_domain.trim().is_empty() {
        return invalid_site("site.yaml upstream model endpoint has an invalid operator boundary");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkloadKind {
    Model,
    Application,
}

fn validate_application_deployments(
    deployments: &[dotcore_proto::dotcore::application::v1::ApplicationDeploymentDeclaration],
    execution_targets: &BTreeMap<String, &ExecutionTarget>,
) -> Result<(), SiteRepositoryError> {
    let mut deployment_ids = BTreeSet::new();
    for deployment in deployments {
        if !valid_id(&deployment.application_deployment_id)
            || !deployment_ids.insert(&deployment.application_deployment_id)
        {
            return invalid_site("site.yaml application deployments require unique valid IDs");
        }
        if deployment.lifecycle_generation == 0 {
            return invalid_site(format!(
                "site.yaml application deployment {:?} requires a lifecycle generation of at least 1",
                deployment.application_deployment_id
            ));
        }
        let placement = deployment.placement.as_ref().ok_or_else(|| {
            SiteRepositoryError::invalid_site(format!(
                "site.yaml application deployment {:?} requires exact placement",
                deployment.application_deployment_id
            ))
        })?;
        let target =
            validate_exact_placement(placement, execution_targets, WorkloadKind::Application)?;
        let snapshot = deployment.snapshot.as_ref().ok_or_else(|| {
            SiteRepositoryError::invalid_site(format!(
                "site.yaml application deployment {:?} requires an immutable snapshot",
                deployment.application_deployment_id
            ))
        })?;
        if snapshot.placement.as_ref() != Some(placement) || snapshot.platform != target.platform {
            return invalid_site(format!(
                "site.yaml application deployment {:?} snapshot does not match its exact placement",
                deployment.application_deployment_id
            ));
        }
    }
    Ok(())
}

fn validate_exact_placement<'a>(
    placement: &ExactPlacement,
    execution_targets: &'a BTreeMap<String, &'a ExecutionTarget>,
    workload_kind: WorkloadKind,
) -> Result<&'a ExecutionTarget, SiteRepositoryError> {
    if !valid_id(&placement.site_node_id) || !valid_id(&placement.execution_target_id) {
        return invalid_site(
            "site.yaml exact placement requires valid node and execution-target IDs",
        );
    }
    let target = execution_targets
        .get(&placement.execution_target_id)
        .copied()
        .ok_or_else(|| {
            SiteRepositoryError::invalid_site(format!(
                "site.yaml exact placement references missing execution target {:?}",
                placement.execution_target_id
            ))
        })?;
    if target.site_node_id != placement.site_node_id {
        return invalid_site(format!(
            "site.yaml exact placement node {:?} does not own execution target {:?}",
            placement.site_node_id, placement.execution_target_id
        ));
    }
    let actual_kind = match target.runtime_binding {
        Some(execution_target::RuntimeBinding::ModelRuntime(_)) => WorkloadKind::Model,
        Some(execution_target::RuntimeBinding::ApplicationRuntime(_)) => WorkloadKind::Application,
        None => unreachable!("execution targets were validated before placements"),
    };
    if actual_kind != workload_kind {
        return invalid_site(format!(
            "site.yaml exact placement execution target {:?} has the wrong workload runtime binding",
            placement.execution_target_id
        ));
    }
    Ok(target)
}

fn validate_execution_targets<'a>(
    targets: &'a [ExecutionTarget],
    node_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, &'a ExecutionTarget>, SiteRepositoryError> {
    let mut by_id = BTreeMap::new();
    let mut node_display_names = BTreeSet::new();
    for target in targets {
        validate_execution_target(target, node_ids)?;
        if by_id
            .insert(target.execution_target_id.clone(), target)
            .is_some()
        {
            return invalid_site(format!(
                "site.yaml has duplicate execution target ID {:?}",
                target.execution_target_id
            ));
        }
        if !node_display_names.insert((&target.site_node_id, target.display_name.trim())) {
            return invalid_site(format!(
                "site.yaml node {:?} has more than one execution target named {:?}",
                target.site_node_id, target.display_name
            ));
        }
    }
    Ok(by_id)
}

fn validate_execution_target(
    target: &ExecutionTarget,
    node_ids: &BTreeSet<String>,
) -> Result<(), SiteRepositoryError> {
    if !valid_id(&target.execution_target_id) {
        return invalid_site("site.yaml execution targets require valid executionTargetId values");
    }
    if !valid_id(&target.site_node_id) || !node_ids.contains(&target.site_node_id) {
        return invalid_site(format!(
            "site.yaml execution target {:?} references undeclared node {:?}",
            target.execution_target_id, target.site_node_id
        ));
    }
    if !is_valid_display_name(&target.display_name) {
        return invalid_site(format!(
            "site.yaml execution target {:?} has an invalid display name",
            target.execution_target_id
        ));
    }
    let Some(platform) = target.platform.as_ref().and_then(canonical_platform) else {
        return invalid_site(format!(
            "site.yaml execution target {:?} requires one concrete linux/amd64, linux/arm64, or darwin/arm64 platform without a variant",
            target.execution_target_id
        ));
    };
    if !binding_supports_platform(target.runtime_binding.as_ref(), &platform) {
        return invalid_site(format!(
            "site.yaml Docker execution target {:?} requires a linux platform; darwin/arm64 targets use a simulated or native runtime binding",
            target.execution_target_id
        ));
    }
    let mut devices = BTreeSet::new();
    if target
        .accelerator_device_ids
        .iter()
        .any(|id| id.trim().is_empty() || !devices.insert(id))
    {
        return invalid_site(format!(
            "site.yaml execution target {:?} has an invalid or duplicate accelerator device ID",
            target.execution_target_id
        ));
    }
    match target.runtime_binding.as_ref() {
        Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => {
            validate_model_runtime_binding(binding, target)?;
        }
        Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => {
            validate_application_runtime_binding(binding, target)?;
        }
        None => {
            return invalid_site(format!(
                "site.yaml execution target {:?} requires one runtime binding",
                target.execution_target_id
            ));
        }
    }
    Ok(())
}

/// Returns the canonical platform when `platform` is a supported value spelled canonically and
/// without a variant.
pub(crate) fn canonical_platform(platform: &OciPlatform) -> Option<ExecutionPlatform> {
    ExecutionPlatform::try_from(platform)
        .ok()
        .filter(|normalized| normalized.to_oci_platform() == *platform)
}

/// Returns whether the runtime binding can realize workloads on `platform`. Docker bindings run
/// Linux workloads only; simulated and native bindings accept every supported platform, the native
/// one realizing the platform of the host that runs it.
pub(crate) fn binding_supports_platform(
    binding: Option<&execution_target::RuntimeBinding>,
    platform: &ExecutionPlatform,
) -> bool {
    let docker = match binding {
        Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => matches!(
            binding.adapter,
            Some(model_runtime_binding::Adapter::DockerRuntime(_))
        ),
        Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => matches!(
            binding.adapter,
            Some(application_runtime_binding::Adapter::DockerRuntime(_))
        ),
        None => false,
    };
    !docker || platform.is_linux()
}

fn validate_model_runtime_binding(
    binding: &dotcore_proto::dotcore::site::v1::ModelRuntimeBinding,
    target: &ExecutionTarget,
) -> Result<(), SiteRepositoryError> {
    // A model target names the exact devices its workloads receive, which every-device names hide.
    if let Some(device) = target
        .accelerator_device_ids
        .iter()
        .find(|id| names_every_cdi_device(id))
    {
        return invalid_site(format!(
            "site.yaml model execution target {:?} names every device with {device:?}; name individual devices",
            target.execution_target_id
        ));
    }
    match binding.adapter.as_ref() {
        // A simulated target names GPUs of its node's declared machine, checked by the node.
        Some(model_runtime_binding::Adapter::SimulatedRuntime(settings)) => {
            validate_simulated_runtime(settings, &target.execution_target_id)
        }
        Some(model_runtime_binding::Adapter::NativeRuntime(settings)) => {
            validate_native_runtime(settings, target)
        }
        Some(model_runtime_binding::Adapter::DockerRuntime(settings)) => {
            AcceleratorDeviceSelector::for_docker_transport(
                settings.accelerator_transport,
                &target.accelerator_device_ids,
            )
            .map(|_| ())
            .map_err(|error| {
                SiteRepositoryError::invalid_site(format!(
                    "site.yaml Docker execution target {:?} {error}",
                    target.execution_target_id
                ))
            })
        }
        None => invalid_site(format!(
            "site.yaml model execution target {:?} requires one runtime adapter",
            target.execution_target_id
        )),
    }
}

fn validate_native_runtime(
    settings: &NativeRuntimeAdapter,
    target: &ExecutionTarget,
) -> Result<(), SiteRepositoryError> {
    if NativeWorkloadLifetime::try_from(settings.workload_lifetime).is_err() {
        return invalid_site(format!(
            "site.yaml native execution target {:?} has an unknown workload lifetime",
            target.execution_target_id
        ));
    }
    let darwin = target
        .platform
        .as_ref()
        .is_some_and(|platform| platform.operating_system == "darwin");
    if darwin && target.accelerator_device_ids != [APPLE_GPU_DEVICE_ID] {
        return invalid_site(format!(
            "site.yaml native darwin execution target {:?} requires exactly the Apple GPU device ID {APPLE_GPU_DEVICE_ID:?}",
            target.execution_target_id
        ));
    }
    Ok(())
}

fn validate_application_runtime_binding(
    binding: &dotcore_proto::dotcore::site::v1::ApplicationRuntimeBinding,
    target: &ExecutionTarget,
) -> Result<(), SiteRepositoryError> {
    match binding.adapter {
        Some(application_runtime_binding::Adapter::SimulatedRuntime(_)) => {
            if !target.accelerator_device_ids.is_empty() {
                return invalid_site(format!(
                    "site.yaml simulated application execution target {:?} cannot configure accelerator devices",
                    target.execution_target_id
                ));
            }
            Ok(())
        }
        Some(application_runtime_binding::Adapter::DockerRuntime(_)) => {
            // The application's Docker backend injects the target's whole device set through
            // CDI, so a target names the individual CDI devices its units receive.
            if let Some(device) = target
                .accelerator_device_ids
                .iter()
                .find(|id| names_every_cdi_device(id))
            {
                return invalid_site(format!(
                    "site.yaml application execution target {:?} names every device with {device:?}; name individual devices",
                    target.execution_target_id
                ));
            }
            AcceleratorDeviceSelector::for_docker_transport(
                dotcore_proto::dotcore::site::v1::DockerAcceleratorTransport::Cdi as i32,
                &target.accelerator_device_ids,
            )
            .map(|_| ())
            .map_err(|error| {
                SiteRepositoryError::invalid_site(format!(
                    "site.yaml Docker application execution target {:?} {error}",
                    target.execution_target_id
                ))
            })
        }
        None => invalid_site(format!(
            "site.yaml application execution target {:?} requires one runtime adapter",
            target.execution_target_id
        )),
    }
}

fn validate_simulated_runtime(
    settings: &dotcore_proto::dotcore::site::v1::SimulatedRuntimeAdapter,
    execution_target_id: &str,
) -> Result<(), SiteRepositoryError> {
    if settings.default_scenario.is_none() {
        return invalid_site(format!(
            "site.yaml simulated execution target {execution_target_id:?} requires a default scenario"
        ));
    }
    let mut deployment_ids = BTreeSet::new();
    for deployment in &settings.deployment_scenarios {
        if !valid_id(&deployment.model_deployment_id)
            || deployment.scenario.is_none()
            || !deployment_ids.insert(&deployment.model_deployment_id)
        {
            return invalid_site(format!(
                "site.yaml simulated execution target {execution_target_id:?} has an invalid or duplicate deployment scenario"
            ));
        }
    }
    Ok(())
}

fn invalid_site<T>(message: impl Into<String>) -> Result<T, SiteRepositoryError> {
    Err(SiteRepositoryError::invalid_site(message))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dotcore_proto::dotcore::{
        catalog::v1::OciPlatform,
        deployment::v1::ExactPlacement,
        inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
        model::v1::ModelDeploymentDesiredState,
        site::v1::{
            ApplicationRuntimeBinding, DockerAcceleratorTransport, DockerApplicationRuntimeAdapter,
            DockerRuntimeAdapter, ExecutionTarget, ModelRuntimeBinding, NativeRuntimeAdapter,
            NativeWorkloadLifetime, SimulatedApplicationRuntimeAdapter, SimulatedRuntimeAdapter,
            SimulatedRuntimeScenario, Site, SiteManagerRole, SiteNode, application_runtime_binding,
            execution_target, model_runtime_binding,
        },
    };

    use super::{
        DEFAULT_HARDWARE_REFRESH_INTERVAL, RouteDeployment, hardware_refresh_interval,
        validate_inference_routes, validate_site,
    };

    #[test]
    fn accepts_two_typed_targets_on_one_node() {
        validate_site(&site(), &crate::test_support::site_declaration_rules())
            .expect("valid exact-target site");
    }

    #[test]
    fn node_hosts_are_specific_addresses_or_canonical_host_names() {
        let with_manager_host = |host: &str| {
            let mut site = site();
            site.nodes[0].host = host.to_owned();
            site
        };
        for valid in ["10.0.0.7", "localhost", "manager.example.ts.net"] {
            validate_site(
                &with_manager_host(valid),
                &crate::test_support::site_declaration_rules(),
            )
            .expect(valid);
        }

        for (invalid, reason) in [
            (
                "Manager.Example.TS.net",
                "must be a lowercase DNS host name without a trailing dot",
            ),
            (
                "manager.example.ts.net.",
                "must be a lowercase DNS host name without a trailing dot",
            ),
            (
                "0.0.0.0",
                "must be a specific non-wildcard IPv4 address or a DNS host name",
            ),
            (
                "::1",
                "must be an IPv4 address or a DNS host name, not an IPv6 address",
            ),
            ("desk_app", "must be an IPv4 address or a DNS host name"),
        ] {
            let error = validate_site(
                &with_manager_host(invalid),
                &crate::test_support::site_declaration_rules(),
            )
            .expect_err(invalid);
            assert_eq!(
                error.to_string(),
                format!("Invalid desired site: site.yaml node \"manager-node\" host {reason}"),
                "{invalid}"
            );
        }
    }

    #[test]
    fn manager_host_is_a_specific_host_or_empty_and_empty_on_the_manager() {
        let with_worker = |manager_host: &str| {
            let mut site = site();
            site.nodes.push(SiteNode {
                node_id: "worker-node".to_owned(),
                display_name: "Worker".to_owned(),
                site_manager: None,
                host: "10.0.0.7".to_owned(),
                manager_host: manager_host.to_owned(),
            });
            site
        };
        validate_site(
            &with_worker(""),
            &crate::test_support::site_declaration_rules(),
        )
        .expect("declared manager host");
        validate_site(
            &with_worker("100.64.0.10"),
            &crate::test_support::site_declaration_rules(),
        )
        .expect("node-specific manager host");
        validate_site(
            &with_worker("manager.example.ts.net"),
            &crate::test_support::site_declaration_rules(),
        )
        .expect("manager host name");

        for invalid in [
            "0.0.0.0",
            "Manager.example",
            "manager.example.",
            "::1",
            "manager_1",
        ] {
            let error = validate_site(
                &with_worker(invalid),
                &crate::test_support::site_declaration_rules(),
            )
            .expect_err(invalid);
            assert!(
                error
                    .to_string()
                    .contains("node \"worker-node\" manager host must be"),
                "{invalid}: {error}"
            );
        }

        let mut manager = site();
        manager.nodes[0].manager_host = "10.0.0.1".to_owned();
        let error = validate_site(&manager, &crate::test_support::site_declaration_rules())
            .expect_err("manager host on the manager node");
        assert!(
            error
                .to_string()
                .contains("site manager node \"manager-node\" must not declare a manager host"),
            "{error}"
        );
    }

    #[test]
    fn accepts_dotted_inference_route_names() {
        let route = InferenceRoute {
            inference_route_id: "route-one".to_owned(),
            name: "claude-3.5-sonnet".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec!["deployment-one".to_owned()],
            }],
            generation: 1,
        };
        let deployments = BTreeMap::from([(
            "deployment-one".to_owned(),
            RouteDeployment {
                desired_state: ModelDeploymentDesiredState::Present as i32,
            },
        )]);

        validate_inference_routes(&[route], &deployments)
            .expect("Site validation accepts a dotted callable route name");
    }

    #[test]
    fn accepts_darwin_arm64_simulated_model_target() {
        let mut darwin = site();
        darwin.execution_targets[0].platform = Some(platform("darwin", "arm64"));

        validate_site(&darwin, &crate::test_support::site_declaration_rules())
            .expect("darwin/arm64 simulated model target");
    }

    #[test]
    fn accepts_darwin_arm64_native_model_target_with_apple_device() {
        for lifetime in [
            NativeWorkloadLifetime::Unspecified,
            NativeWorkloadLifetime::Persistent,
            NativeWorkloadLifetime::Ephemeral,
        ] {
            let mut native = native_site(lifetime as i32);
            native.execution_targets[0].accelerator_device_ids = vec!["apple-gpu-0".to_owned()];

            validate_site(&native, &crate::test_support::site_declaration_rules())
                .expect("darwin/arm64 native model target");
        }
    }

    #[test]
    fn rejects_non_apple_devices_and_unknown_lifetime_on_native_targets() {
        let mut nvidia = native_site(NativeWorkloadLifetime::Persistent as i32);
        nvidia.execution_targets[0].accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
        let error = validate_site(&nvidia, &crate::test_support::site_declaration_rules())
            .expect_err("non-Apple device on darwin native target");
        assert!(
            error.to_string().contains(
                "native darwin execution target \"model-target\" requires exactly the Apple GPU device ID \"apple-gpu-0\""
            ),
            "{error}"
        );

        let mut empty = native_site(NativeWorkloadLifetime::Persistent as i32);
        empty.execution_targets[0].accelerator_device_ids.clear();
        let error = validate_site(&empty, &crate::test_support::site_declaration_rules())
            .expect_err("darwin native target without a device");
        assert!(
            error
                .to_string()
                .contains("requires exactly the Apple GPU device ID"),
            "{error}"
        );

        let error = validate_site(
            &native_site(99),
            &crate::test_support::site_declaration_rules(),
        )
        .expect_err("unknown lifetime");
        assert!(
            error.to_string().contains(
                "native execution target \"model-target\" has an unknown workload lifetime"
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_unsupported_target_platforms() {
        for (operating_system, architecture) in [
            ("darwin", "amd64"),
            ("windows", "arm64"),
            ("linux", "riscv64"),
        ] {
            let mut unsupported = site();
            unsupported.execution_targets[0].platform =
                Some(platform(operating_system, architecture));

            let error = validate_site(&unsupported, &crate::test_support::site_declaration_rules())
                .expect_err("unsupported platform");
            assert!(
                error.to_string().contains(
                    "requires one concrete linux/amd64, linux/arm64, or darwin/arm64 platform"
                ),
                "{operating_system}/{architecture}: {error}"
            );
        }
    }

    #[test]
    fn rejects_darwin_arm64_docker_targets_with_actionable_error() {
        let mut docker_model = site();
        docker_model.execution_targets[0].platform = Some(platform("darwin", "arm64"));
        docker_model.execution_targets[0].runtime_binding = Some(
            execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                    Default::default(),
                )),
            }),
        );
        let error = validate_site(
            &docker_model,
            &crate::test_support::site_declaration_rules(),
        )
        .expect_err("darwin Docker model target");
        assert!(
            error.to_string().contains(
                "Docker execution target \"model-target\" requires a linux platform; darwin/arm64 targets use a simulated or native runtime binding"
            ),
            "{error}"
        );

        let mut docker_application = site();
        docker_application.execution_targets[1].platform = Some(platform("darwin", "arm64"));
        docker_application.execution_targets[1].runtime_binding = Some(
            execution_target::RuntimeBinding::ApplicationRuntime(ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                    Default::default(),
                )),
            }),
        );
        assert!(
            validate_site(
                &docker_application,
                &crate::test_support::site_declaration_rules()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_and_unknown_target_ownership() {
        let mut duplicate = site();
        duplicate.execution_targets[1].execution_target_id = "model-target".to_owned();
        assert!(validate_site(&duplicate, &crate::test_support::site_declaration_rules()).is_err());

        let mut unknown = site();
        unknown.execution_targets[0].site_node_id = "missing-node".to_owned();
        assert!(validate_site(&unknown, &crate::test_support::site_declaration_rules()).is_err());
    }

    #[test]
    fn execution_target_display_names_are_valid_and_unique_per_node() {
        for name in ["", "   ", "bad\u{7}name", &"n".repeat(129)] {
            let mut invalid = site();
            invalid.execution_targets[0].display_name = name.to_owned();
            let error = validate_site(&invalid, &crate::test_support::site_declaration_rules())
                .expect_err("invalid display name");
            assert_eq!(
                error.to_string(),
                "Invalid desired site: site.yaml execution target \"model-target\" has an invalid display name",
                "{name:?}"
            );
        }

        let mut duplicate = site();
        duplicate.execution_targets[1].display_name = " model-target ".to_owned();
        let error = validate_site(&duplicate, &crate::test_support::site_declaration_rules())
            .expect_err("duplicate display name");
        assert_eq!(
            error.to_string(),
            "Invalid desired site: site.yaml node \"manager-node\" has more than one execution target named \" model-target \""
        );

        let mut other_node = site();
        other_node.nodes.push(SiteNode {
            node_id: "worker-node".to_owned(),
            display_name: "Worker".to_owned(),
            site_manager: None,
            host: "127.0.0.2".to_owned(),
            manager_host: String::new(),
        });
        let mut worker_target = other_node.execution_targets[0].clone();
        worker_target.execution_target_id = "worker-model-target".to_owned();
        worker_target.site_node_id = "worker-node".to_owned();
        other_node.execution_targets.push(worker_target);
        validate_site(&other_node, &crate::test_support::site_declaration_rules())
            .expect("same display name on another node");
    }

    #[test]
    fn model_targets_name_individual_devices_and_may_overlap() {
        let gpu_target = |id: &str, devices: &[&str]| ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: "manager-node".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: devices.iter().map(ToString::to_string).collect(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                        DockerRuntimeAdapter {
                            accelerator_transport: DockerAcceleratorTransport::Cdi as i32,
                        },
                    )),
                },
            )),
        };

        let mut overlapping = site();
        overlapping.execution_targets.extend([
            gpu_target("gpu0", &["nvidia.com/gpu=gpu0"]),
            gpu_target("gpu1", &["nvidia.com/gpu=gpu1"]),
            gpu_target("pair", &["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"]),
            gpu_target(
                "alias",
                &["nvidia.com/gpu=GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b"],
            ),
            gpu_target("mig", &["nvidia.com/gpu=0:1"]),
            gpu_target("same-gpu0", &["nvidia.com/gpu=gpu0"]),
        ]);
        validate_site(&overlapping, &crate::test_support::site_declaration_rules())
            .expect("individual and overlapping devices");

        let mut every = site();
        every.execution_targets.push(gpu_target(
            "every",
            &["nvidia.com/gpu=gpu0", "nvidia.com/gpu=all"],
        ));
        let error = validate_site(&every, &crate::test_support::site_declaration_rules())
            .expect_err("every-device selector");
        assert_eq!(
            error.to_string(),
            "Invalid desired site: site.yaml model execution target \"every\" names every device with \"nvidia.com/gpu=all\"; name individual devices"
        );
    }

    #[test]
    fn exact_placement_rejects_node_mismatch_and_wrong_workload_kind() {
        let site = site();
        let targets = super::validate_execution_targets(
            &site.execution_targets,
            &super::validate_nodes(&site.nodes).expect("nodes"),
        )
        .expect("targets");
        assert!(
            super::validate_exact_placement(
                &ExactPlacement {
                    site_node_id: "other-node".to_owned(),
                    execution_target_id: "model-target".to_owned(),
                },
                &targets,
                super::WorkloadKind::Model,
            )
            .is_err()
        );
        assert!(
            super::validate_exact_placement(
                &ExactPlacement {
                    site_node_id: "manager-node".to_owned(),
                    execution_target_id: "application-target".to_owned(),
                },
                &targets,
                super::WorkloadKind::Model,
            )
            .is_err()
        );
    }

    #[test]
    fn application_target_accelerators_follow_the_cdi_grammar() {
        // A simulated application target never names accelerator devices.
        let mut simulated = site();
        simulated.execution_targets[1].accelerator_device_ids = vec!["gpu-0".to_owned()];
        assert!(validate_site(&simulated, &crate::test_support::site_declaration_rules()).is_err());

        let docker_target = || {
            execution_target::RuntimeBinding::ApplicationRuntime(ApplicationRuntimeBinding {
                adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                    DockerApplicationRuntimeAdapter {},
                )),
            })
        };

        // A Docker application target names the individual CDI devices its units receive.
        let mut docker = site();
        docker.execution_targets[1].runtime_binding = Some(docker_target());
        docker.execution_targets[1].accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
        validate_site(&docker, &crate::test_support::site_declaration_rules())
            .expect("individual CDI device");

        // A legacy index or an every-device name is not a CDI device name.
        for device in ["0", "nvidia.com/gpu=all"] {
            docker.execution_targets[1].accelerator_device_ids = vec![device.to_owned()];
            assert!(
                validate_site(&docker, &crate::test_support::site_declaration_rules()).is_err(),
                "device {device:?} must be rejected"
            );
        }
    }

    #[test]
    fn execution_targets_on_one_node_keep_independent_runtime_configuration() {
        let mut independent = site();
        let mut second = independent.execution_targets[0].clone();
        second.execution_target_id = "model-target-two".to_owned();
        second.display_name = "model-target-two".to_owned();
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
            second.runtime_binding.as_mut()
        else {
            panic!("model target");
        };
        let Some(model_runtime_binding::Adapter::SimulatedRuntime(settings)) =
            binding.adapter.as_mut()
        else {
            panic!("simulated target");
        };
        settings
            .default_scenario
            .as_mut()
            .expect("default scenario")
            .ready_delay_millis = 99;
        independent.execution_targets.push(second);

        validate_site(&independent, &crate::test_support::site_declaration_rules())
            .expect("independent target runtime settings");
    }

    #[test]
    fn default_hardware_refresh_interval_is_stable() {
        assert_eq!(
            hardware_refresh_interval(&site()),
            DEFAULT_HARDWARE_REFRESH_INTERVAL
        );
    }

    fn platform(operating_system: &str, architecture: &str) -> OciPlatform {
        OciPlatform {
            operating_system: operating_system.to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        }
    }

    fn native_site(workload_lifetime: i32) -> Site {
        let mut native = site();
        native.execution_targets[0].platform = Some(platform("darwin", "arm64"));
        native.execution_targets[0].runtime_binding = Some(
            execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                    NativeRuntimeAdapter { workload_lifetime },
                )),
            }),
        );
        native
    }

    fn site() -> Site {
        let scenario = || SimulatedRuntimeAdapter {
            default_scenario: Some(SimulatedRuntimeScenario {
                ready_delay_millis: 1,
                stop_delay_millis: 1,
            }),
            deployment_scenarios: Vec::new(),
        };
        Site {
            site_id: "site-one".to_owned(),
            model_deployment_identity_allocator: Some(crate::test_support::identity_allocator()),
            nodes: vec![SiteNode {
                node_id: "manager-node".to_owned(),
                display_name: "Manager".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                host: "127.0.0.1".to_owned(),
                manager_host: String::new(),
            }],
            execution_targets: vec![
                ExecutionTarget {
                    execution_target_id: "model-target".to_owned(),
                    display_name: "model-target".to_owned(),
                    site_node_id: "manager-node".to_owned(),
                    platform: Some(OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    }),
                    accelerator_device_ids: Vec::new(),
                    runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                        ModelRuntimeBinding {
                            adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                                scenario(),
                            )),
                        },
                    )),
                },
                ExecutionTarget {
                    execution_target_id: "application-target".to_owned(),
                    display_name: "application-target".to_owned(),
                    site_node_id: "manager-node".to_owned(),
                    platform: Some(OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    }),
                    accelerator_device_ids: Vec::new(),
                    runtime_binding: Some(execution_target::RuntimeBinding::ApplicationRuntime(
                        ApplicationRuntimeBinding {
                            adapter: Some(application_runtime_binding::Adapter::SimulatedRuntime(
                                SimulatedApplicationRuntimeAdapter {},
                            )),
                        },
                    )),
                },
            ],
            ..Default::default()
        }
    }
}
