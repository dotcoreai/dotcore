//! Immutable desired-route snapshots and transient availability projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use dotcore_catalog::shared_parameters::CONTEXT_PER_REQUEST_PARAMETER_ID;
use dotcore_proto::dotcore::{
    catalog::v1::{InferenceRequestKind, ModelInputModality},
    inference::v1::{
        FallbackGroupAvailability, InferenceRoute, InferenceRouteAvailability,
        InferenceRouteMemberAvailability, InferenceRouteMemberExclusionReason,
        InferenceRouteUnavailabilityReason,
    },
    model::v1::{ModelDeploymentDeclaration, model_deployment_snapshot::Source},
};

use crate::model::controller::serving_targets::{
    ServingTarget, ServingTargetEligibility, ServingTargetsSource, declared_request_kinds,
};

/// One complete immutable set of compiled desired route declarations, with the request kinds
/// and the context per request each route's members declare.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CompiledRouteSnapshot {
    routes: Arc<[InferenceRoute]>,

    /// Request kinds any member of a route declares, keyed by route id, whether or not the
    /// member is ready.
    request_kinds: Arc<BTreeMap<String, BTreeSet<InferenceRequestKind>>>,

    /// Smallest context per request a route's members declare, keyed by route id, whether or not
    /// the member is ready. A route none of whose members declares one has no entry.
    context_per_request: Arc<BTreeMap<String, u64>>,
}

impl CompiledRouteSnapshot {
    /// Compiles all accepted desired declarations into one immutable snapshot, taking member
    /// request kinds and context per request from the declared model deployments.
    pub(crate) fn new(
        mut routes: Vec<InferenceRoute>,
        deployments: &[ModelDeploymentDeclaration],
    ) -> Self {
        routes.sort_by(|left, right| left.name.cmp(&right.name));
        let deployments = deployments
            .iter()
            .map(|deployment| (deployment.model_deployment_id.as_str(), deployment))
            .collect::<BTreeMap<_, _>>();
        let members = |route: &InferenceRoute| {
            route
                .fallback_groups
                .iter()
                .flat_map(|group| &group.model_deployment_ids)
                .filter_map(|model_deployment_id| deployments.get(model_deployment_id.as_str()))
                .copied()
                .collect::<Vec<_>>()
        };
        let mut request_kinds = BTreeMap::new();
        let mut context_per_request = BTreeMap::new();
        for route in &routes {
            let members = members(route);
            let kinds = members
                .iter()
                .flat_map(|deployment| declared_request_kinds(deployment))
                .collect();
            request_kinds.insert(route.inference_route_id.clone(), kinds);
            if let Some(context) = members
                .iter()
                .filter_map(|deployment| declared_context_per_request(deployment))
                .min()
            {
                context_per_request.insert(route.inference_route_id.clone(), context);
            }
        }
        Self {
            routes: Arc::from(routes),
            request_kinds: Arc::new(request_kinds),
            context_per_request: Arc::new(context_per_request),
        }
    }

    /// Returns routes in callable-name order for management projection.
    pub(crate) fn routes(&self) -> &[InferenceRoute] {
        &self.routes
    }

    /// Finds one route by its opaque management identity.
    pub(crate) fn by_id(&self, inference_route_id: &str) -> Option<&InferenceRoute> {
        self.routes
            .iter()
            .find(|route| route.inference_route_id == inference_route_id)
    }

    /// Returns the request kinds any member of the route declares, ready or not; empty for an
    /// unknown route.
    pub(crate) fn request_kinds(&self, inference_route_id: &str) -> BTreeSet<InferenceRequestKind> {
        self.request_kinds
            .get(inference_route_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the largest context one request may use on the route, in tokens: the smallest
    /// context per request its members declare, ready or not. None when no member declares one
    /// or the route is unknown.
    pub(crate) fn context_per_request(&self, inference_route_id: &str) -> Option<u64> {
        self.context_per_request.get(inference_route_id).copied()
    }
}

/// Returns the context per request a local deployment's snapshot resolved, in tokens. Upstream
/// deployments and engines without a context per request declare none.
fn declared_context_per_request(declaration: &ModelDeploymentDeclaration) -> Option<u64> {
    let Some(Source::Local(local)) = declaration
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    else {
        return None;
    };
    crate::model::engine::unsigned_parameter_value(
        local.engine_parameter_values.as_ref()?,
        CONTEXT_PER_REQUEST_PARAMETER_ID,
    )
}

/// Private source for controller-published READY target capability evidence.
#[derive(Clone)]
pub(crate) struct RouteAvailabilityTargetsSource {
    source: ServingTargetsSource,
}

impl RouteAvailabilityTargetsSource {
    /// Creates a read-only target evidence source from the inference controller.
    pub(crate) fn new(source: ServingTargetsSource) -> Self {
        Self { source }
    }

    /// Derives the current availability of one immutable desired route.
    pub(crate) fn availability(&self, route: &InferenceRoute) -> InferenceRouteAvailability {
        let snapshot = self.source.snapshot();
        let targets = snapshot
            .targets
            .iter()
            .map(ServingTarget::route_eligibility)
            .collect::<Vec<_>>();
        availability_for_eligible_targets(route, &targets)
    }
}

fn availability_for_eligible_targets(
    route: &InferenceRoute,
    targets: &[ServingTargetEligibility],
) -> InferenceRouteAvailability {
    let eligible = eligible_modalities(targets);
    let fallback_groups = route
        .fallback_groups
        .iter()
        .enumerate()
        .map(|(index, group)| FallbackGroupAvailability {
            fallback_group_index: u32::try_from(index)
                .expect("validated route group count fits u32"),
            members: group
                .model_deployment_ids
                .iter()
                .map(|model_deployment_id| {
                    member_availability(model_deployment_id, eligible.get(model_deployment_id))
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let available_input_modalities = available_modalities(&fallback_groups);
    let has_text = available_input_modalities.contains(&(ModelInputModality::Text as i32));
    let exclusion_reasons = if !route.enabled {
        vec![InferenceRouteUnavailabilityReason::Disabled as i32]
    } else if !has_text {
        vec![InferenceRouteUnavailabilityReason::NoEligibleTextMember as i32]
    } else {
        Vec::new()
    };
    InferenceRouteAvailability {
        available: route.enabled && has_text,
        available_input_modalities,
        exclusion_reasons,
        fallback_groups,
    }
}

fn eligible_modalities(targets: &[ServingTargetEligibility]) -> BTreeMap<String, Vec<i32>> {
    targets
        .iter()
        .filter(|target| target.eligible)
        .map(|target| {
            (
                target.deployment_id.0.clone(),
                modalities(target.supports_text, target.supports_image),
            )
        })
        .collect()
}

fn member_availability(
    model_deployment_id: &str,
    modalities: Option<&Vec<i32>>,
) -> InferenceRouteMemberAvailability {
    match modalities {
        Some(available_input_modalities) => InferenceRouteMemberAvailability {
            model_deployment_id: model_deployment_id.to_owned(),
            eligible: true,
            available_input_modalities: available_input_modalities.clone(),
            exclusion_reasons: Vec::new(),
        },
        None => InferenceRouteMemberAvailability {
            model_deployment_id: model_deployment_id.to_owned(),
            eligible: false,
            available_input_modalities: Vec::new(),
            exclusion_reasons: vec![
                InferenceRouteMemberExclusionReason::ModelDeploymentNotReady as i32,
            ],
        },
    }
}

fn available_modalities(groups: &[FallbackGroupAvailability]) -> Vec<i32> {
    let mut available = Vec::new();
    for member in groups.iter().flat_map(|group| &group.members) {
        for modality in &member.available_input_modalities {
            if !available.contains(modality) {
                available.push(*modality);
            }
        }
    }
    available
}

fn modalities(supports_text: bool, supports_image: bool) -> Vec<i32> {
    let mut modalities = Vec::new();
    if supports_text {
        modalities.push(ModelInputModality::Text as i32);
    }
    if supports_image {
        modalities.push(ModelInputModality::Image as i32);
    }
    modalities
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::engine_parameter_value,
        inference::v1::{FallbackGroup, FallbackGroupSelection, InferenceRoute},
        model::v1::ModelDeploymentSnapshot,
    };
    use dotcore_runtime::LoopbackEndpoint;

    use super::*;
    use crate::model::{
        controller::serving_targets::ServingTargetEligibility, domain::ModelDeploymentId,
    };

    #[test]
    fn route_policy_uses_identical_ready_data_from_generic_targets_with_opaque_dispatch() {
        let route = InferenceRoute {
            inference_route_id: "route-id".to_owned(),
            name: "route".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec!["deployment".to_owned()],
            }],
            generation: 7,
        };
        let docker_dispatch = FakeReadyTarget {
            ready: ready_target(),
            dispatch: LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: 8080,
            },
        };
        let simulated_dispatch = FakeReadyTarget {
            ready: ready_target(),
            dispatch: "simulated dispatch detail".to_owned(),
        };

        let docker = availability_for(&route, &[docker_dispatch]);
        let simulated = availability_for(&route, &[simulated_dispatch]);

        assert_eq!(docker, simulated);
        assert!(docker.available);
    }

    #[test]
    fn route_policy_excludes_ineligible_deployment_members() {
        let route = InferenceRoute {
            inference_route_id: "route-id".to_owned(),
            name: "route".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec!["deployment".to_owned()],
            }],
            generation: 7,
        };
        let availability = availability_for_eligible_targets(
            &route,
            &[ServingTargetEligibility {
                deployment_id: ModelDeploymentId("deployment".to_owned()),
                eligible: false,
                supports_text: true,
                supports_image: true,
            }],
        );

        assert!(!availability.available);
        assert!(!availability.fallback_groups[0].members[0].eligible);
    }

    #[test]
    fn a_route_context_per_request_is_the_smallest_its_members_declare() {
        let deployments = [
            local_deployment("large", 32_768),
            local_deployment("small", 16_384),
            upstream_deployment("upstream"),
        ];
        let routes = vec![
            route("mixed", &[&["large", "upstream"], &["small"]]),
            route("upstream-only", &[&["upstream", "undeclared"]]),
        ];

        let snapshot = CompiledRouteSnapshot::new(routes, &deployments);

        assert_eq!(snapshot.context_per_request("mixed-id"), Some(16_384));
        assert_eq!(snapshot.context_per_request("upstream-only-id"), None);
        assert_eq!(snapshot.context_per_request("unknown-id"), None);
    }

    struct FakeReadyTarget<D> {
        ready: ServingTargetEligibility,
        #[expect(
            dead_code,
            reason = "the contract proves route policy cannot observe opaque dispatch details"
        )]
        dispatch: D,
    }

    fn availability_for<D>(
        route: &InferenceRoute,
        targets: &[FakeReadyTarget<D>],
    ) -> InferenceRouteAvailability {
        availability_for_eligible_targets(
            route,
            &targets
                .iter()
                .map(|target| target.ready.clone())
                .collect::<Vec<_>>(),
        )
    }

    fn ready_target() -> ServingTargetEligibility {
        ServingTargetEligibility {
            deployment_id: ModelDeploymentId("deployment".to_owned()),
            eligible: true,
            supports_text: true,
            supports_image: false,
        }
    }

    fn route(name: &str, groups: &[&[&str]]) -> InferenceRoute {
        InferenceRoute {
            inference_route_id: format!("{name}-id"),
            name: name.to_owned(),
            enabled: true,
            fallback_groups: groups
                .iter()
                .map(|members| FallbackGroup {
                    selection: FallbackGroupSelection::Single as i32,
                    model_deployment_ids: members.iter().map(|id| (*id).to_owned()).collect(),
                })
                .collect(),
            generation: 1,
        }
    }

    /// A local deployment whose snapshot resolved `context` tokens of context per request.
    fn local_deployment(id: &str, context: u64) -> ModelDeploymentDeclaration {
        let mut snapshot = dotcore_domain::test_support::model::local_model_snapshot("model");
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local fixture snapshot");
        };
        let values = local
            .engine_parameter_values
            .as_mut()
            .expect("fixture parameter values");
        for value in &mut values.values {
            if value.parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID {
                value.value = Some(engine_parameter_value::Value::UnsignedInteger(context));
            }
        }
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            snapshot: Some(snapshot),
            ..Default::default()
        }
    }

    fn upstream_deployment(id: &str) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(Source::Upstream(Default::default())),
            }),
            ..Default::default()
        }
    }
}
