//! Recommendations for one described machine that belongs to no site.
//!
//! The machine gets the execution targets site setup would declare for its hardware, observed
//! with nothing running, and the site recommender ranks the catalog's models on them. Each target
//! can also be named with its hardware class, so a machine's hardware reads the way Dotcore sees
//! it. Nothing here reads the local host or a site: the description is the only input.

use dotcore_catalog::{Catalog, SupportedHardwareClass};
use dotcore_domain::{real_time, timing::NodeControlTiming};
use dotcore_node_hardware::{MachineDescription, MachineError};
use dotcore_proto::dotcore::{
    application::v1::ApplicationTargetOption,
    catalog::v1::{Application, HardwareClass, UseCase},
    site::v1::{ExecutionTarget, execution_target::RuntimeBinding},
};
use dotcore_site::{
    initial::inputs::{InitialSiteCompositionError, described_execution_targets},
    node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityInventory,
        TargetCapabilityReconciler,
    },
};
use tracing::debug;

use super::{UseCaseRecommendations, recommend_models};
use crate::{
    application::recommend_application_targets,
    hardware::selection::select_hardware_class,
    model::compatibility::{StaleTargets, check_targets, node_workloads},
};

/// A described machine as an idle node of its own: the execution targets site setup would declare
/// for its hardware, and their capabilities as freshly observed with nothing running.
#[derive(Debug)]
pub struct IdleMachine {
    targets: Vec<ExecutionTarget>,
    inventory: TargetCapabilityInventory,
}

impl IdleMachine {
    /// Declares the targets of `machine`'s hardware and observes them idle now, under the node
    /// control timing that bounds how long capability observations stay fresh. The machine stands
    /// for a node named after it.
    pub fn new(
        machine: &MachineDescription,
        timing: NodeControlTiming,
    ) -> Result<Self, IdleMachineError> {
        let node_id = machine.name();
        let targets =
            described_execution_targets(node_id, machine).map_err(IdleMachineError::Targets)?;
        let observed_at = real_time::wall_now();
        let snapshot = machine
            .render(&[], observed_at)
            .map_err(IdleMachineError::Hardware)?;
        let accelerators = NodeAcceleratorObservation::from_hardware_snapshot(
            node_id, &snapshot, /* stale */ false,
        );
        let platforms = targets
            .iter()
            .filter_map(|target| {
                let platform = target.platform.as_ref()?.try_into().ok()?;
                Some(RuntimePlatformObservation::available_at(
                    target.execution_target_id.clone(),
                    platform,
                    observed_at,
                ))
            })
            .collect::<Vec<_>>();
        let inventory = TargetCapabilityReconciler::reconcile(
            &targets,
            platforms,
            [accelerators],
            timing.capability_max_age,
            observed_at,
        );
        Ok(Self { targets, inventory })
    }

    /// Returns the machine's execution targets: its model targets, then its application targets.
    pub fn targets(&self) -> &[ExecutionTarget] {
        &self.targets
    }

    /// Returns each execution target with the most specific of `catalog`'s hardware classes that
    /// matches it. Application targets, and model targets no class matches, have none.
    pub fn hardware_classes(
        &self,
        catalog: &Catalog,
    ) -> Vec<(&ExecutionTarget, Option<HardwareClass>)> {
        let classes = catalog
            .list_hardware_classes()
            .into_iter()
            .map(|hardware_class| SupportedHardwareClass {
                hardware_class,
                hardware_requirements: None,
            })
            .collect::<Vec<_>>();
        let model_targets = self.model_targets();
        let workloads = node_workloads(&[], Some(&self.inventory));
        let checked = check_targets(
            &model_targets,
            Some(&self.inventory),
            &workloads,
            StaleTargets::Fail,
        );
        self.targets
            .iter()
            .map(|target| {
                let selected = checked
                    .iter()
                    .find(|checked| {
                        checked.target.execution_target_id == target.execution_target_id
                    })
                    .and_then(|checked| checked.check.as_ref().ok());
                let class = selected.and_then(|selected| {
                    select_hardware_class(classes.clone(), selected)
                        .inspect_err(|error| {
                            debug!(
                                execution_target_id = %target.execution_target_id,
                                %error,
                                "no hardware class matches the described target"
                            );
                        })
                        .ok()
                });
                (target, class.map(|support| support.hardware_class))
            })
            .collect()
    }

    /// Ranks the catalog's recommendations for each of `use_cases`, in the order asked, on the
    /// machine's model targets with nothing running.
    pub fn recommend(
        &self,
        catalog: &Catalog,
        use_cases: &[UseCase],
    ) -> Vec<UseCaseRecommendations> {
        recommend_models(
            catalog,
            &self.model_targets(),
            Some(&self.inventory),
            &[],
            use_cases,
            false,
        )
    }

    /// Returns the machine's application targets that can run `application`, best first, with
    /// nothing deployed on them.
    pub fn recommend_application(&self, application: &Application) -> Vec<ApplicationTargetOption> {
        let targets = self
            .targets
            .iter()
            .filter(|target| {
                matches!(
                    target.runtime_binding,
                    Some(RuntimeBinding::ApplicationRuntime(_))
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        recommend_application_targets(application, &targets, None, &self.inventory, &[])
    }

    /// The targets that run models, which recommendations and hardware classes apply to.
    fn model_targets(&self) -> Vec<ExecutionTarget> {
        self.targets
            .iter()
            .filter(|target| {
                matches!(
                    target.runtime_binding,
                    Some(RuntimeBinding::ModelRuntime(_))
                )
            })
            .cloned()
            .collect()
    }
}

/// A described machine whose targets or idle hardware can't be worked out.
#[derive(Debug, thiserror::Error)]
pub enum IdleMachineError {
    #[error("the machine's execution targets can't be chosen")]
    Targets(#[source] InitialSiteCompositionError),

    #[error("the machine's idle hardware can't be rendered")]
    Hardware(#[source] MachineError),
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::Catalog;
    use dotcore_domain::timing::Timing;
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::catalog::v1::UseCase;

    use super::IdleMachine;
    use crate::model::recommendation::{
        UseCaseRecommendations,
        tests::{MAC_64G, NVIDIA_X_32G, catalog},
    };

    #[test]
    fn an_idle_mac_gets_its_native_target_classed_and_ranked() {
        let catalog = catalog();
        let mac = idle("mac64", MAC_64G);

        assert_eq!(
            classes(&mac, &catalog),
            ["Fixture Apple GPU: apple", "Docker applications: none"]
        );
        assert_eq!(
            rows(
                &mac,
                &mac.recommend(&catalog, &[UseCase::Coding, UseCase::ChatAssistant])
            ),
            [
                vec![
                    "big/q4 on Fixture Apple GPU: FitsNow from 355328",
                    "mid/q4 on Fixture Apple GPU: FitsNow from 454656",
                    "small/q4 on Fixture Apple GPU: FitsNow from 494592",
                ],
                vec![
                    "mid/q4 on Fixture Apple GPU: FitsNow from 454656",
                    "small/q4 on Fixture Apple GPU: FitsNow from 494592",
                ],
            ]
        );
    }

    #[test]
    fn an_idle_nvidia_machine_gets_its_gpu_model_class_and_its_list_first() {
        let catalog = catalog();
        let gpu = idle("x32", NVIDIA_X_32G);

        assert_eq!(
            classes(&gpu, &catalog),
            ["Fixture GPU X: nvidia-x", "Docker applications: none"]
        );
        assert_eq!(
            rows(
                &gpu,
                &gpu.recommend(&catalog, &[UseCase::Coding, UseCase::ChatAssistant])
            ),
            [
                vec![
                    "x-only/x on Fixture GPU X: FitsNow from 293888",
                    "big/q4 on Fixture GPU X: FitsNow from 174080",
                    "mid/q4 on Fixture GPU X: FitsNow from 274432",
                    "small/q4 on Fixture GPU X: FitsNow from 314368",
                ],
                vec![
                    "mid/q4 on Fixture GPU X: FitsNow from 274432",
                    "small/q4 on Fixture GPU X: FitsNow from 314368",
                ],
            ]
        );
    }

    fn idle(name: &str, yaml: &str) -> IdleMachine {
        let machine = MachineDescription::from_yaml(name, yaml).expect("described machine");
        IdleMachine::new(&machine, Timing::test().node_control).expect("idle machine")
    }

    /// Each target as `name: class`.
    fn classes(machine: &IdleMachine, catalog: &Catalog) -> Vec<String> {
        machine
            .hardware_classes(catalog)
            .into_iter()
            .map(|(target, class)| {
                let class = class.map_or("none".to_owned(), |class| class.hardware_class_id);
                format!("{}: {class}", target.display_name)
            })
            .collect()
    }

    /// Each use case's rows, one line per row and target: `model/profile on target: fit from
    /// starting total context`. Nothing is deployed on an idle machine.
    fn rows(machine: &IdleMachine, recommendations: &[UseCaseRecommendations]) -> Vec<Vec<String>> {
        recommendations
            .iter()
            .map(|list| {
                list.rows
                    .iter()
                    .flat_map(|row| {
                        row.targets.iter().map(move |target| {
                            assert_eq!(target.model_deployment_id, None);
                            let name = machine
                                .targets()
                                .iter()
                                .find(|candidate| {
                                    candidate.execution_target_id
                                        == target.placement.execution_target_id
                                })
                                .map_or("unknown target", |candidate| &candidate.display_name);
                            format!(
                                "{}/{} on {name}: {:?} from {}",
                                row.model_id,
                                row.profile_id,
                                target.fit,
                                target.starting_total_context_tokens.unwrap_or_default()
                            )
                        })
                    })
                    .collect()
            })
            .collect()
    }
}
