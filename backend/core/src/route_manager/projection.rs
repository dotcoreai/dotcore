//! Immutable desired-route snapshots and transient availability projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use dotcore_proto::dotcore::{
    catalog::v1::{InferenceRequestKind, ModelInputModality},
    inference::v1::{
        FallbackGroupAvailability, InferenceRoute, InferenceRouteAvailability,
        InferenceRouteMemberAvailability, InferenceRouteMemberExclusionReason,
        InferenceRouteUnavailabilityReason,
    },
    model::v1::ModelDeploymentDeclaration,
};

use crate::model::controller::serving_targets::{
    ServingTarget, ServingTargetEligibility, ServingTargetsSource, declared_request_kinds,
};

/// One complete immutable set of compiled desired route declarations, with the request kinds
/// each route's members declare.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CompiledRouteSnapshot {
    routes: Arc<[InferenceRoute]>,

    /// Request kinds any member of a route declares, keyed by route id, whether or not the
    /// member is ready.
    request_kinds: Arc<BTreeMap<String, BTreeSet<InferenceRequestKind>>>,
}

impl CompiledRouteSnapshot {
    /// Compiles all accepted desired declarations into one immutable snapshot, taking member
    /// request kinds from the declared model deployments.
    pub(crate) fn new(
        mut routes: Vec<InferenceRoute>,
        deployments: &[ModelDeploymentDeclaration],
    ) -> Self {
        routes.sort_by(|left, right| left.name.cmp(&right.name));
        let deployment_kinds = deployments
            .iter()
            .map(|deployment| {
                (
                    deployment.model_deployment_id.as_str(),
                    declared_request_kinds(deployment),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let request_kinds = routes
            .iter()
            .map(|route| {
                let kinds = route
                    .fallback_groups
                    .iter()
                    .flat_map(|group| &group.model_deployment_ids)
                    .filter_map(|model_deployment_id| {
                        deployment_kinds.get(model_deployment_id.as_str())
                    })
                    .flatten()
                    .copied()
                    .collect();
                (route.inference_route_id.clone(), kinds)
            })
            .collect();
        Self {
            routes: Arc::from(routes),
            request_kinds: Arc::new(request_kinds),
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
    use dotcore_proto::dotcore::inference::v1::{
        FallbackGroup, FallbackGroupSelection, InferenceRoute,
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
}
