//! Rules that decide whether an inference route's shape is valid and what happens to routes when
//! one of their model deployments is torn down.
//!
//! Both rules are pure: they read route declarations and never touch the site,
//! so the route manager and the site's locked acceptance apply the same policy.
//! For example, a route whose only member is the torn-down deployment is removed
//! with it, while a route that also uses other deployments blocks the teardown.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::inference::v1::{FallbackGroupSelection, InferenceRoute};

use crate::valid_id;

const MAX_FALLBACK_GROUPS: usize = 8;
const MAX_ROUND_ROBIN_MEMBERS: usize = 32;

/// Checks the fallback groups and members of one route declaration.
///
/// A route has one to eight groups. A SINGLE group has exactly one member, a ROUND_ROBIN group
/// has two to 32 members, and a model deployment occurs at most once in the whole route.
pub fn validate_route_shape(route: &InferenceRoute) -> Result<(), RouteShapeError> {
    let group_count = route.fallback_groups.len();
    if !(1..=MAX_FALLBACK_GROUPS).contains(&group_count) {
        return Err(RouteShapeError::GroupCount(group_count));
    }

    let mut members = BTreeSet::new();
    for group in &route.fallback_groups {
        let member_count = group.model_deployment_ids.len();
        match FallbackGroupSelection::try_from(group.selection) {
            Ok(FallbackGroupSelection::Single) if member_count == 1 => {}
            Ok(FallbackGroupSelection::Single) => {
                return Err(RouteShapeError::SingleMemberCount(member_count));
            }
            Ok(FallbackGroupSelection::RoundRobin)
                if (2..=MAX_ROUND_ROBIN_MEMBERS).contains(&member_count) => {}
            Ok(FallbackGroupSelection::RoundRobin) => {
                return Err(RouteShapeError::RoundRobinMemberCount(member_count));
            }
            Ok(FallbackGroupSelection::Unspecified) | Err(_) => {
                return Err(RouteShapeError::Selection);
            }
        }

        for deployment_id in &group.model_deployment_ids {
            if !valid_id(deployment_id) {
                return Err(RouteShapeError::MemberId);
            }
            if !members.insert(deployment_id) {
                return Err(RouteShapeError::DuplicateMember(deployment_id.clone()));
            }
        }
    }
    Ok(())
}

/// Splits the routes that reference one model deployment by what its teardown does to them.
///
/// Routes whose only member is the deployment are removed with it. Routes that also use other
/// deployments are shared and block the teardown. Routes that do not reference it are untouched.
pub fn classify_unload(routes: &[InferenceRoute], deployment_id: &str) -> RouteUnloadPlan {
    classify_unload_set(routes, &BTreeSet::from([deployment_id]))
}

/// Splits the routes that reference any of several model deployments by what withdrawing all of
/// them together does to them.
///
/// Routes whose members are all withdrawn, however many they are, are removed. Routes that also
/// have a member outside the set are shared and block the withdrawal. Other routes are untouched.
pub fn classify_unload_set(
    routes: &[InferenceRoute],
    deployment_ids: &BTreeSet<&str>,
) -> RouteUnloadPlan {
    let mut removed_route_ids = Vec::new();
    let mut shared_route_names = Vec::new();
    for route in routes {
        let mut members = route
            .fallback_groups
            .iter()
            .flat_map(|group| &group.model_deployment_ids);
        if !members
            .clone()
            .any(|member| deployment_ids.contains(member.as_str()))
        {
            continue;
        }
        if members.all(|member| deployment_ids.contains(member.as_str())) {
            removed_route_ids.push(route.inference_route_id.clone());
        } else {
            shared_route_names.push(route.name.clone());
        }
    }
    shared_route_names.sort();
    RouteUnloadPlan {
        removed_route_ids,
        shared_route_names,
    }
}

/// Outcome of applying the teardown rule to the current route declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteUnloadPlan {
    removed_route_ids: Vec<String>,
    shared_route_names: Vec<String>,
}

impl RouteUnloadPlan {
    /// Returns the sorted names of shared routes that block the teardown.
    pub fn shared_route_names(&self) -> &[String] {
        &self.shared_route_names
    }

    /// Reports whether the teardown removes this route with its only member.
    pub fn removes(&self, route: &InferenceRoute) -> bool {
        self.removed_route_ids
            .iter()
            .any(|id| id == &route.inference_route_id)
    }
}

/// Explains why a route's fallback groups or members break the shape rules.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteShapeError {
    #[error("a route must have one to eight fallback groups, found {0}")]
    GroupCount(usize),

    #[error("a SINGLE group must have exactly one model deployment, found {0}")]
    SingleMemberCount(usize),

    #[error("a ROUND_ROBIN group must have two to 32 model deployments, found {0}")]
    RoundRobinMemberCount(usize),

    #[error("a fallback group must select SINGLE or ROUND_ROBIN")]
    Selection,

    #[error("a model deployment member ID must match [a-z][a-z0-9-]{{0,62}}")]
    MemberId,

    #[error("model deployment {0:?} occurs more than once in the route")]
    DuplicateMember(String),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{FallbackGroup, FallbackGroupSelection};

    use super::*;

    #[test]
    fn shape_accepts_single_and_round_robin_groups_up_to_their_limits() {
        let mut groups = vec![round_robin(&members("wide", 32))];
        groups.extend((0..7).map(|index| single(&format!("single-{index}"))));

        assert_eq!(validate_route_shape(&route(groups)), Ok(()));
    }

    #[test]
    fn shape_rejects_zero_or_nine_groups() {
        let nine = (0..9)
            .map(|index| single(&format!("single-{index}")))
            .collect();

        assert_rejected(route(Vec::new()), RouteShapeError::GroupCount(0));
        assert_rejected(route(nine), RouteShapeError::GroupCount(9));
        assert_eq!(
            RouteShapeError::GroupCount(9).to_string(),
            "a route must have one to eight fallback groups, found 9"
        );
    }

    #[test]
    fn shape_rejects_single_group_with_two_members() {
        let group = FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: members("member", 2),
        };

        assert_rejected(route(vec![group]), RouteShapeError::SingleMemberCount(2));
        assert_eq!(
            RouteShapeError::SingleMemberCount(2).to_string(),
            "a SINGLE group must have exactly one model deployment, found 2"
        );
    }

    #[test]
    fn shape_rejects_round_robin_group_with_one_or_33_members() {
        assert_rejected(
            route(vec![round_robin(&members("member", 1))]),
            RouteShapeError::RoundRobinMemberCount(1),
        );
        assert_rejected(
            route(vec![round_robin(&members("member", 33))]),
            RouteShapeError::RoundRobinMemberCount(33),
        );
        assert_eq!(
            RouteShapeError::RoundRobinMemberCount(33).to_string(),
            "a ROUND_ROBIN group must have two to 32 model deployments, found 33"
        );
    }

    #[test]
    fn shape_rejects_a_deployment_repeated_across_groups() {
        let groups = vec![
            round_robin(&["first".to_owned(), "second".to_owned()]),
            single("second"),
        ];

        assert_rejected(
            route(groups),
            RouteShapeError::DuplicateMember("second".to_owned()),
        );
        assert_eq!(
            RouteShapeError::DuplicateMember("second".to_owned()).to_string(),
            "model deployment \"second\" occurs more than once in the route"
        );
    }

    #[test]
    fn shape_rejects_unspecified_selection_and_invalid_member_ids() {
        let unspecified = FallbackGroup {
            selection: FallbackGroupSelection::Unspecified as i32,
            model_deployment_ids: members("member", 1),
        };

        assert_rejected(route(vec![unspecified]), RouteShapeError::Selection);
        assert_rejected(route(vec![single("Invalid ID")]), RouteShapeError::MemberId);
    }

    #[test]
    fn unload_removes_a_route_whose_only_member_is_the_deployment() {
        let sole = named_route("sole", vec![single("torn-down")]);

        let plan = classify_unload(std::slice::from_ref(&sole), "torn-down");

        assert!(plan.removes(&sole));
        assert!(plan.shared_route_names().is_empty());
    }

    #[test]
    fn unload_blocks_on_a_route_that_also_uses_another_deployment() {
        let shared = named_route(
            "shared",
            vec![round_robin(&["torn-down".to_owned(), "other".to_owned()])],
        );

        let plan = classify_unload(std::slice::from_ref(&shared), "torn-down");

        assert!(!plan.removes(&shared));
        assert_eq!(plan.shared_route_names(), ["shared"]);
    }

    #[test]
    fn unload_leaves_routes_without_the_deployment_untouched() {
        let unrelated = named_route("unrelated", vec![single("other")]);

        let plan = classify_unload(std::slice::from_ref(&unrelated), "torn-down");

        assert!(!plan.removes(&unrelated));
        assert!(plan.shared_route_names().is_empty());
    }

    #[test]
    fn unload_blocks_on_a_route_with_the_deployment_alone_in_one_of_several_groups() {
        let fallback = named_route("fallback", vec![single("torn-down"), single("other")]);

        let plan = classify_unload(std::slice::from_ref(&fallback), "torn-down");

        assert!(!plan.removes(&fallback));
        assert_eq!(plan.shared_route_names(), ["fallback"]);
    }

    #[test]
    fn set_unload_removes_a_route_served_only_by_several_withdrawn_deployments() {
        let group = named_route(
            "group",
            vec![round_robin(&["first".to_owned(), "second".to_owned()])],
        );
        let partly = named_route("partly", vec![single("first"), single("survivor")]);
        let routes = [group, partly];
        let withdrawn = BTreeSet::from(["first", "second"]);

        let plan = classify_unload_set(&routes, &withdrawn);

        assert!(plan.removes(&routes[0]));
        assert!(!plan.removes(&routes[1]));
        assert_eq!(plan.shared_route_names(), ["partly"]);
        assert_eq!(
            classify_unload(&routes[..1], "first").shared_route_names(),
            ["group"]
        );
    }

    #[test]
    fn unload_reports_shared_routes_sorted_by_name() {
        let routes = [
            named_route("zulu", vec![single("torn-down"), single("other")]),
            named_route("sole", vec![single("torn-down")]),
            named_route(
                "alpha",
                vec![round_robin(&["other".to_owned(), "torn-down".to_owned()])],
            ),
        ];

        let plan = classify_unload(&routes, "torn-down");

        assert_eq!(plan.shared_route_names(), ["alpha", "zulu"]);
        assert!(plan.removes(&routes[1]));
        assert!(!plan.removes(&routes[0]));
    }

    fn assert_rejected(route: InferenceRoute, expected: RouteShapeError) {
        assert_eq!(validate_route_shape(&route), Err(expected));
    }

    fn route(fallback_groups: Vec<FallbackGroup>) -> InferenceRoute {
        named_route("route", fallback_groups)
    }

    fn named_route(name: &str, fallback_groups: Vec<FallbackGroup>) -> InferenceRoute {
        InferenceRoute {
            inference_route_id: format!("{name}-id"),
            name: name.to_owned(),
            enabled: true,
            fallback_groups,
            generation: 1,
        }
    }

    fn single(deployment_id: &str) -> FallbackGroup {
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![deployment_id.to_owned()],
        }
    }

    fn round_robin(deployment_ids: &[String]) -> FallbackGroup {
        FallbackGroup {
            selection: FallbackGroupSelection::RoundRobin as i32,
            model_deployment_ids: deployment_ids.to_vec(),
        }
    }

    fn members(prefix: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("{prefix}-{index}"))
            .collect()
    }
}
