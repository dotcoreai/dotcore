//! Recommends catalog models for a site: for each use case asked, one ranking of the models the
//! master catalog recommends for the site's hardware, across all its model execution targets.
//!
//! Each target reads the list of the most specific hardware class that has one for the use case,
//! and each entry is judged with the same profile resolution and memory fit as the compatibility
//! read, the entry's own minimums added to its configuration's. An entry is sized for one request
//! using its whole total context, at the entry's context per request or else the profile's
//! default. Entries that can't run on a target or exceed its total memory are left out there, and
//! the rest merge across targets by their best declared position and context per request. Each
//! target names the model's running or starting deployment of the same profile there, whatever its
//! context. Asked for what can start now, it keeps only reachable targets that are deployed or fit
//! free memory, deployed rows first. The answer is advisory: it never admits, reserves, or places a
//! deployment.

use std::{cmp::Ordering, collections::BTreeMap};

use dotcore_catalog::{Catalog, CatalogModelId, SupportedHardwareClass};
use dotcore_proto::dotcore::{
    catalog::v1::{
        HardwareClass, HardwareClassRecommendations, Model, ModelProfile, ModelRecommendation,
        UseCase,
    },
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{
        ModelCompatibilityStatus, ModelDeployment, SizingScenarioUnsupportedReason,
        model_deployment_snapshot,
    },
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::TargetCapabilityInventory;
use tracing::{debug, warn};

mod machine;

pub use machine::{IdleMachine, IdleMachineError};

use crate::{
    hardware::{
        capacity::WorkloadPhase,
        selection::{
            HardwareClassSelectionError, SelectedTargetCapabilities, select_hardware_class,
        },
    },
    model::{
        compatibility::{
            CheckedTarget, ProfileEvaluation, ProfileRequest, StaleTargets, check_targets,
            evaluate_profile, node_workloads, workload_phase,
        },
        engine::EngineIntegrationRegistry,
        sizing_scenario::{
            ProfileSizing, SizingScenario, default_context_per_request, size_profile,
        },
    },
};

/// One use case's recommended model profiles for a site, best first.
#[derive(Clone, Debug, PartialEq)]
pub struct UseCaseRecommendations {
    pub use_case: UseCase,

    pub rows: Vec<Recommendation>,
}

/// One recommended model profile and the targets that can run it.
#[derive(Clone, Debug, PartialEq)]
pub struct Recommendation {
    pub model_id: String,

    pub profile_id: String,

    /// Context per request in tokens the profile is sized with: the entry's own, else the
    /// profile's default.
    pub context_tokens_per_request: u64,

    /// Targets whose list has the profile and that can run it, by node then target; deployed
    /// targets come first when only what can start now is asked for.
    pub targets: Vec<TargetRecommendation>,
}

/// How a recommended profile fits one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRecommendation {
    pub placement: ExactPlacement,

    pub fit: RecommendationFit,

    /// The running or starting model deployment of the same model and profile on the target,
    /// whatever its context per request.
    pub model_deployment_id: Option<String>,

    /// Total context in tokens a deployment of the profile starts from there; absent without a
    /// largest fit or a profile default.
    pub starting_total_context_tokens: Option<u64>,
}

/// A recommended profile's memory fit on a target whose total memory can hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecommendationFit {
    /// It fits beside what runs there now.
    FitsNow,

    /// It fits the target's memory only once some of what runs there is stopped.
    NeedsMemoryFreed,

    /// What is free there is unknown.
    Unknown,

    /// The target stopped reporting, so it is judged on the hardware it last reported and what
    /// is free there is unknown.
    Unreachable,
}

/// Ranks the master catalog's recommendations for each of `use_cases`, in the order asked, on
/// `targets`, which must be configured model execution targets. `inventory` is the current target
/// observation, or none when it couldn't be read, which leaves nothing to recommend; a target that
/// stopped reporting is judged on its last observations, its rows marked unreachable, until it
/// leaves the site. `deployments` are the site's model deployments, which hold memory and name the
/// profiles already deployed. With `can_start_now`, each list keeps only what can start now, as
/// `startable` describes.
pub(crate) fn recommend_models(
    catalog: &Catalog,
    targets: &[ExecutionTarget],
    inventory: Option<&TargetCapabilityInventory>,
    deployments: &[ModelDeployment],
    use_cases: &[UseCase],
    can_start_now: bool,
) -> Vec<UseCaseRecommendations> {
    let workloads = node_workloads(deployments, inventory);
    let checked = check_targets(
        targets,
        inventory,
        &workloads,
        StaleTargets::UseLastObserved,
    );
    let declarations = Declarations::read(catalog);
    let mut recommender = Recommender {
        catalog,
        declarations: &declarations,
        registry: EngineIntegrationRegistry::production(),
        sizings: BTreeMap::new(),
    };
    use_cases
        .iter()
        .map(|&use_case| {
            let rows = recommender.rank(use_case, &checked, deployments);
            UseCaseRecommendations {
                use_case,
                rows: if can_start_now { startable(rows) } else { rows },
            }
        })
        .collect()
}

/// What one call reads for every use case: the catalog, its declarations indexed by id, and each
/// profile's sizing at each context per request once it has been needed.
struct Recommender<'a> {
    catalog: &'a Catalog,
    declarations: &'a Declarations,
    registry: EngineIntegrationRegistry,
    sizings: BTreeMap<RowKey, ProfileSizing>,
}

impl Recommender<'_> {
    /// Ranks one use case's recommendations on the checked targets.
    fn rank(
        &mut self,
        use_case: UseCase,
        checked: &[CheckedTarget<'_>],
        deployments: &[ModelDeployment],
    ) -> Vec<Recommendation> {
        let declarations = self.declarations;
        let lists = declarations.lists(use_case);
        let supported = lists
            .iter()
            .map(|(class, _)| SupportedHardwareClass {
                hardware_class: (*class).clone(),
                hardware_requirements: None,
            })
            .collect::<Vec<_>>();
        let mut rows = BTreeMap::<RowKey, RankedRow>::new();
        for checked in checked {
            let Ok(selected) = &checked.check else {
                continue;
            };
            let entries = match select_hardware_class(supported.clone(), selected) {
                Ok(support) => lists
                    .iter()
                    .find(|(class, _)| {
                        class.hardware_class_id == support.hardware_class.hardware_class_id
                    })
                    .map_or(&[][..], |(_, entries)| entries),
                Err(error) => {
                    log_no_list(use_case, checked.target, &error);
                    continue;
                }
            };
            for (position, entry) in entries.iter().enumerate() {
                let Some((model, profile)) = declarations.profile(entry) else {
                    continue;
                };
                let Some(scenario) = entry_scenario(self.catalog, model, profile, entry) else {
                    continue;
                };
                let Some((fit, starting_total_context_tokens)) =
                    self.judge(model, profile, entry, scenario, checked.target, selected)
                else {
                    continue;
                };
                let fit = if checked.unreachable {
                    RecommendationFit::Unreachable
                } else {
                    fit
                };
                let placement = ExactPlacement {
                    site_node_id: checked.target.site_node_id.clone(),
                    execution_target_id: checked.target.execution_target_id.clone(),
                };
                let row = rows
                    .entry(row_key(entry, scenario))
                    .or_insert_with(|| RankedRow {
                        position,
                        intelligence: model
                            .intelligence
                            .as_ref()
                            .map_or(0.0, |score| score.overall),
                        targets: Vec::new(),
                    });
                row.position = row.position.min(position);
                row.targets.push(TargetRecommendation {
                    model_deployment_id: deployment_id(deployments, &placement, entry),
                    placement,
                    fit,
                    starting_total_context_tokens,
                });
            }
        }
        ranked(rows)
    }

    /// Judges one entry sized for `scenario` on one target with the compatibility read's
    /// evaluation, its minimums added to its configuration's: its fit and starting total context
    /// there, or none when it is left out there.
    fn judge(
        &mut self,
        model: &Model,
        profile: &ModelProfile,
        entry: &ModelRecommendation,
        scenario: SizingScenario,
        target: &ExecutionTarget,
        selected: &SelectedTargetCapabilities,
    ) -> Option<(RecommendationFit, Option<u64>)> {
        let sizing = self
            .sizings
            .entry(row_key(entry, scenario))
            .or_insert_with(|| size_profile(self.catalog, model, profile, Some(&scenario)));
        let evaluation = evaluate_profile(
            self.catalog,
            &self.registry,
            &ProfileRequest {
                model_id: &CatalogModelId(model.model_id.clone()),
                profile,
                sizing,
                extra_minimums: entry.hardware_requirements.as_ref(),
            },
            target,
            selected,
        );
        target_fit(&evaluation)
    }
}

/// A row's model id, profile id and context per request in tokens; also keys each sizing.
type RowKey = (String, String, u64);

/// One row while targets are merged: its best declared position, its model's intelligence score,
/// and the targets that can run it.
struct RankedRow {
    position: usize,
    intelligence: f64,
    targets: Vec<TargetRecommendation>,
}

/// The catalog declarations recommendations read, indexed once per call.
struct Declarations {
    models: BTreeMap<String, Model>,
    classes: BTreeMap<String, HardwareClass>,
    recommendations: Vec<HardwareClassRecommendations>,
}

impl Declarations {
    /// Indexes the catalog's models and hardware classes by id, beside its recommendations.
    fn read(catalog: &Catalog) -> Self {
        Self {
            models: catalog
                .list_model_releases()
                .into_iter()
                .map(|model| (model.model_id.clone(), model))
                .collect(),
            classes: catalog
                .list_hardware_classes()
                .into_iter()
                .map(|class| (class.hardware_class_id.clone(), class))
                .collect(),
            recommendations: catalog.list_hardware_class_recommendations(),
        }
    }

    /// Each hardware class with a list for `use_case`, with that list.
    fn lists(&self, use_case: UseCase) -> Vec<(&HardwareClass, &[ModelRecommendation])> {
        self.recommendations
            .iter()
            .filter_map(|recommendations| {
                let class = self.classes.get(&recommendations.hardware_class_id)?;
                let list = recommendations
                    .use_cases
                    .iter()
                    .find(|list| list.use_case() == use_case)?;
                Some((class, list.entries.as_slice()))
            })
            .collect()
    }

    /// The model and profile an entry names; catalog validation keeps both declared.
    fn profile(&self, entry: &ModelRecommendation) -> Option<(&Model, &ModelProfile)> {
        let model = self.models.get(&entry.model_id)?;
        let profile = model
            .profiles
            .iter()
            .find(|profile| profile.profile_id == entry.profile_id)?;
        Some((model, profile))
    }
}

/// The sizing scenario an entry is judged with: one request using the whole total context, both
/// the entry's context per request or else the profile's default. Returns none when the profile's
/// defaults don't resolve.
fn entry_scenario(
    catalog: &Catalog,
    model: &Model,
    profile: &ModelProfile,
    entry: &ModelRecommendation,
) -> Option<SizingScenario> {
    let context_tokens = entry
        .context_tokens_per_request
        .or_else(|| default_context_per_request(catalog, model, profile))?;
    // Catalog validation keeps an entry's context within the configuration's maximum context.
    let context_tokens = u32::try_from(context_tokens).ok()?;
    SizingScenario::new(context_tokens, Some(context_tokens)).ok()
}

/// The row an entry sized for `scenario` merges into.
fn row_key(entry: &ModelRecommendation, scenario: SizingScenario) -> RowKey {
    (
        entry.model_id.clone(),
        entry.profile_id.clone(),
        scenario.total_context_tokens(),
    )
}

/// The fit of an evaluated entry on a target and the total context a deployment starts from
/// there, or none when the entry is left out there: its profile can't run there or serve its
/// context there, or its memory fit, with the entry's minimums, exceeds the target's total.
fn target_fit(evaluation: &ProfileEvaluation) -> Option<(RecommendationFit, Option<u64>)> {
    let running = match evaluation {
        ProfileEvaluation::Runs(running) => running,
        // Observations expired between the target check and resolution.
        ProfileEvaluation::Fails((ModelCompatibilityStatus::Unknown, _)) => {
            return Some((RecommendationFit::Unknown, None));
        }
        ProfileEvaluation::Fails(_) => return None,
    };
    if running.fit.unsupported_reason != SizingScenarioUnsupportedReason::Unspecified {
        return None;
    }
    let fit = match (running.fit.memory_fit, running.fit.free_memory_fit) {
        (MemoryFit::Exceeds, _) => return None,
        (MemoryFit::Fits, MemoryFit::Fits) => RecommendationFit::FitsNow,
        (MemoryFit::Fits, MemoryFit::Exceeds) => RecommendationFit::NeedsMemoryFreed,
        _ => RecommendationFit::Unknown,
    };
    Some((
        fit,
        running
            .starting_total_context
            .map(|starting| starting.tokens()),
    ))
}

/// The first running or starting model deployment of the entry's model and profile placed at
/// `placement`, whatever its context per request. It shares the memory accounting's lifecycle
/// phases, so failed, paused, unavailable and unloading deployments don't count.
fn deployment_id(
    deployments: &[ModelDeployment],
    placement: &ExactPlacement,
    entry: &ModelRecommendation,
) -> Option<String> {
    deployments
        .iter()
        .find(|deployment| {
            let placed = deployment
                .intent
                .as_ref()
                .and_then(|intent| intent.placement.as_ref())
                == Some(placement);
            let same_profile = matches!(
                deployment.snapshot.as_ref().and_then(|snapshot| snapshot.source.as_ref()),
                Some(model_deployment_snapshot::Source::Local(local))
                    if local.model_id == entry.model_id && local.profile_id == entry.profile_id
            );
            let active = matches!(
                workload_phase(deployment),
                Some(WorkloadPhase::Starting | WorkloadPhase::Ready)
            );
            placed && same_profile && active
        })
        .map(|deployment| deployment.model_deployment_id.clone())
}

/// Orders merged rows by best declared position, then higher intelligence score, then model id,
/// profile id and context per request.
fn ranked(rows: BTreeMap<RowKey, RankedRow>) -> Vec<Recommendation> {
    let mut rows = rows.into_iter().collect::<Vec<_>>();
    rows.sort_by(|(left_id, left), (right_id, right)| {
        left.position
            .cmp(&right.position)
            .then_with(|| {
                right
                    .intelligence
                    .partial_cmp(&left.intelligence)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| left_id.cmp(right_id))
    });
    rows.into_iter()
        .map(
            |((model_id, profile_id, context_tokens_per_request), row)| Recommendation {
                model_id,
                profile_id,
                context_tokens_per_request,
                targets: row.targets,
            },
        )
        .collect()
}

/// Keeps what can start now from ranked rows: unreachable targets go first, deployed or not; a row
/// keeps its deployed targets and those that fit free memory now, and goes when none remain. Rows
/// with a deployed target come first, each group in rank order; within a row deployed targets
/// come first, then by node and target.
fn startable(rows: Vec<Recommendation>) -> Vec<Recommendation> {
    let mut rows = rows
        .into_iter()
        .filter_map(|mut row| {
            row.targets.retain(|target| {
                target.fit != RecommendationFit::Unreachable
                    && (target.model_deployment_id.is_some()
                        || target.fit == RecommendationFit::FitsNow)
            });
            row.targets.sort_by_key(|target| {
                (
                    target.model_deployment_id.is_none(),
                    target.fit != RecommendationFit::FitsNow,
                    target.placement.site_node_id.clone(),
                    target.placement.execution_target_id.clone(),
                )
            });
            (!row.targets.is_empty()).then_some(row)
        })
        .collect::<Vec<_>>();
    // A stable sort keeps the rank order within each group.
    rows.sort_by_key(|row| {
        !row.targets
            .iter()
            .any(|target| target.model_deployment_id.is_some())
    });
    rows
}

/// Logs why a target has no list for a use case: no class with one matches it, or, against
/// catalog validation, several match with none more specific.
fn log_no_list(use_case: UseCase, target: &ExecutionTarget, error: &HardwareClassSelectionError) {
    match error {
        HardwareClassSelectionError::NoMatch { .. } => debug!(
            use_case = use_case.as_str_name(),
            execution_target_id = %target.execution_target_id,
            "no hardware class with recommendations for the use case matches the target"
        ),
        HardwareClassSelectionError::Ambiguous { .. } => warn!(
            use_case = use_case.as_str_name(),
            execution_target_id = %target.execution_target_id,
            error = %error,
            "several recommendation hardware classes match the target"
        ),
    }
}

#[cfg(test)]
mod tests;
