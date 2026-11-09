//! Recommends catalog models for a site: for each use case asked, one ranking of the models the
//! master catalog recommends for the site's hardware, across all its model execution targets.
//!
//! Each target reads the list of the most specific hardware class that has one for the use case,
//! and each entry is judged with the same profile resolution and memory fit as the compatibility
//! read, the entry's own minimums added to its configuration's. Entries that can't run on a target
//! or exceed its total memory are left out there, and the rest merge across targets by their best
//! declared position. The answer is advisory: it never admits, reserves, or places a deployment.

use std::{cmp::Ordering, collections::BTreeMap, time::SystemTime};

use dotcore_catalog::{Catalog, CatalogModelId, SupportedHardwareClass};
use dotcore_proto::dotcore::{
    catalog::v1::{
        HardwareClass, HardwareClassRecommendations, Model, ModelProfile, ModelRecommendation,
        UseCase,
    },
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{ModelCompatibilityStatus, ModelDeployment, model_deployment_snapshot},
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::TargetCapabilityInventory;
use tracing::{debug, warn};

mod machine;

pub use machine::{IdleMachine, IdleMachineError};

use crate::{
    hardware::selection::{
        HardwareClassSelectionError, SelectedTargetCapabilities, select_hardware_class,
    },
    model::{
        compatibility::{
            CheckedTarget, ProfileEvaluation, ProfileRequest, StaleTargets, check_targets,
            evaluate_profile, node_workloads,
        },
        engine::EngineIntegrationRegistry,
        sizing_scenario::{ProfileSizing, size_profile},
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

    /// Targets whose list has the profile and that can run it, by node then target.
    pub targets: Vec<TargetRecommendation>,
}

/// How a recommended profile fits one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRecommendation {
    pub placement: ExactPlacement,

    pub fit: RecommendationFit,

    /// Whether a model deployment of the same model and profile is placed on the target.
    pub deployed: bool,

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
/// leaves the site. `deployments` are the site's model deployments, which hold memory and mark the
/// profiles already deployed.
pub(crate) fn recommend_models(
    catalog: &Catalog,
    targets: &[ExecutionTarget],
    inventory: Option<&TargetCapabilityInventory>,
    deployments: &[ModelDeployment],
    use_cases: &[UseCase],
    now: SystemTime,
) -> Vec<UseCaseRecommendations> {
    let workloads = node_workloads(deployments, inventory);
    let checked = check_targets(
        targets,
        inventory,
        &workloads,
        now,
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
        .map(|&use_case| UseCaseRecommendations {
            use_case,
            rows: recommender.rank(use_case, &checked, deployments),
        })
        .collect()
}

/// What one call reads for every use case: the catalog, its declarations indexed by id, and each
/// profile's sizing once it has been needed.
struct Recommender<'a> {
    catalog: &'a Catalog,
    declarations: &'a Declarations,
    registry: EngineIntegrationRegistry,
    sizings: BTreeMap<(String, String), ProfileSizing>,
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
        let mut rows = BTreeMap::<(String, String), RankedRow>::new();
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
                let Some((fit, starting_total_context_tokens)) =
                    self.judge(model, profile, entry, checked.target, selected)
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
                    .entry((entry.model_id.clone(), entry.profile_id.clone()))
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
                    deployed: deployed(deployments, &placement, entry),
                    placement,
                    fit,
                    starting_total_context_tokens,
                });
            }
        }
        ranked(rows)
    }

    /// Judges one entry on one target with the compatibility read's evaluation, its minimums
    /// added to its configuration's: its fit and starting total context there, or none when it is
    /// left out there.
    fn judge(
        &mut self,
        model: &Model,
        profile: &ModelProfile,
        entry: &ModelRecommendation,
        target: &ExecutionTarget,
        selected: &SelectedTargetCapabilities,
    ) -> Option<(RecommendationFit, Option<u64>)> {
        let sizing = self
            .sizings
            .entry((entry.model_id.clone(), entry.profile_id.clone()))
            .or_insert_with(|| size_profile(self.catalog, model, profile, None));
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

/// The fit of an evaluated entry on a target and the total context a deployment starts from
/// there, or none when the entry is left out there: its profile can't run there, or its memory
/// fit, with the entry's minimums, exceeds the target's total.
fn target_fit(evaluation: &ProfileEvaluation) -> Option<(RecommendationFit, Option<u64>)> {
    let running = match evaluation {
        ProfileEvaluation::Runs(running) => running,
        // Observations expired between the target check and resolution.
        ProfileEvaluation::Fails((ModelCompatibilityStatus::Unknown, _)) => {
            return Some((RecommendationFit::Unknown, None));
        }
        ProfileEvaluation::Fails(_) => return None,
    };
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

/// Whether a model deployment of the entry's model and profile is placed at `placement`.
fn deployed(
    deployments: &[ModelDeployment],
    placement: &ExactPlacement,
    entry: &ModelRecommendation,
) -> bool {
    deployments.iter().any(|deployment| {
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
        placed && same_profile
    })
}

/// Orders merged rows by best declared position, then higher intelligence score, then model and
/// profile id.
fn ranked(rows: BTreeMap<(String, String), RankedRow>) -> Vec<Recommendation> {
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
        .map(|((model_id, profile_id), row)| Recommendation {
            model_id,
            profile_id,
            targets: row.targets,
        })
        .collect()
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
