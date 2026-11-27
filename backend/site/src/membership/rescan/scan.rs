//! The node's own capability scan for a target re-scan: it probes this machine for every capability
//! the node can offer and builds the whole replacement target set through the same
//! target-construction path site initialization uses.
//!
//! A re-scan offers each real capability this host has, so a capability the machine gained since
//! the node was declared, such as Docker on a Mac first set up without it, adds its targets. A kind
//! of workload the node runs simulated stays simulated: simulated hardware stands in for the real
//! one of that kind, so the scan never adds a real capability beside it. A committed capability the
//! machine no longer offers keeps its committed targets in the replacement: a later outage is
//! reported as unavailable and removes no targets, and the whole replacement stays valid.

use std::collections::BTreeSet;

use dotcore_proto::dotcore::{
    node::v1::DeclaredMachine,
    site::v1::{
        ExecutionTarget, NativeWorkloadLifetime, application_runtime_binding, execution_target,
        model_runtime_binding,
    },
};

use crate::initial::{
    inputs::{NodeTargetChoices, build_execution_targets},
    probe::InitialSiteProbeReport,
    request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
};

/// One capability a node's execution targets offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RescanCapability {
    /// Model workloads simulated on the node's declared machine.
    SimulatedModels,

    /// Model workloads for one NVIDIA GPU through Docker.
    NvidiaModels,

    /// Model workloads on the Apple GPU of a macOS node.
    AppleModels,

    /// Application workloads in local Docker containers.
    DockerApplications,

    /// Application workloads of a simulated site.
    SimulatedApplications,
}

impl RescanCapability {
    /// Returns the capability one committed target offers, or `None` for a target whose runtime
    /// binding is absent or names a runtime a node does not build.
    pub fn of(target: &ExecutionTarget) -> Option<Self> {
        match target.runtime_binding.as_ref()? {
            execution_target::RuntimeBinding::ModelRuntime(binding) => {
                match binding.adapter.as_ref()? {
                    model_runtime_binding::Adapter::SimulatedRuntime(_) => {
                        Some(Self::SimulatedModels)
                    }
                    model_runtime_binding::Adapter::DockerRuntime(_) => Some(Self::NvidiaModels),
                    model_runtime_binding::Adapter::NativeRuntime(_) => Some(Self::AppleModels),
                }
            }
            execution_target::RuntimeBinding::ApplicationRuntime(binding) => {
                match binding.adapter.as_ref()? {
                    application_runtime_binding::Adapter::DockerRuntime(_) => {
                        Some(Self::DockerApplications)
                    }
                    application_runtime_binding::Adapter::SimulatedRuntime(_) => {
                        Some(Self::SimulatedApplications)
                    }
                }
            }
        }
    }

    /// Returns whether `probe` finds this capability on the machine. Simulated capabilities
    /// need no hardware, so they always hold.
    fn offered_by(self, probe: &InitialSiteProbeReport) -> bool {
        match self {
            Self::SimulatedModels | Self::SimulatedApplications => true,
            Self::NvidiaModels => {
                probe.nvidia().is_some()
                    && probe.docker().is_some_and(|docker| docker.cdi_enabled())
            }
            Self::AppleModels => probe.apple().is_some(),
            Self::DockerApplications => probe.docker().is_some(),
        }
    }

    /// Names the capability as the operator names it in command flags.
    pub fn label(self) -> &'static str {
        match self {
            Self::SimulatedModels => "simulated inference",
            Self::NvidiaModels => "nvidia inference",
            Self::AppleModels => "apple inference",
            Self::DockerApplications => "docker applications",
            Self::SimulatedApplications => "simulated applications",
        }
    }
}

/// The whole replacement target set one node's scan produced, and the committed targets the scan
/// could not rebuild.
#[derive(Clone, Debug, PartialEq)]
pub struct RescanScan {
    /// Whole replacement target set, built through the site initialization target path.
    pub targets: Vec<ExecutionTarget>,

    /// Committed targets kept as declared because this machine no longer offers their capability,
    /// each with the capability it kept them for.
    pub unavailable: Vec<(RescanCapability, Vec<ExecutionTarget>)>,
}

/// Returns the request that probes this machine for every capability a node with `committed`
/// targets can offer, which the caller probes before rebuilding the targets.
///
/// A node with simulated inference needs its declared machine to rebuild its targets, so its
/// simulated hardware choices carry over. So does the workload lifetime a native model target
/// declares, which is the node's policy rather than its hardware.
pub fn scan_request(
    committed: &[ExecutionTarget],
    declared: Option<&DeclaredMachine>,
) -> InitialSiteRequest {
    request_for(
        &candidates_of(committed),
        declared,
        workload_lifetime_of(committed),
    )
}

/// Rebuilds the whole replacement target set of `node_id` from `probe`: every candidate capability
/// the probe found gets its targets, and each committed capability it did not find keeps its
/// committed targets.
pub fn scan_replacement(
    node_id: &str,
    committed: &[ExecutionTarget],
    declared: Option<&DeclaredMachine>,
    probe: &InitialSiteProbeReport,
    host_architecture: &str,
) -> Result<RescanScan, RescanScanError> {
    let lifetime = workload_lifetime_of(committed);
    let offered = candidates_of(committed)
        .into_iter()
        .filter(|capability| capability.offered_by(probe))
        .collect::<BTreeSet<_>>();
    let request = request_for(&offered, declared, lifetime);

    let mut targets = Vec::new();
    if !offered.is_empty() {
        let choices = NodeTargetChoices::resolve(&request, probe, host_architecture)
            .map_err(RescanScanError::Composition)?;
        targets = build_execution_targets(node_id, &choices);
    }

    let mut unavailable = Vec::new();
    for capability in &capabilities_of(committed) - &offered {
        let kept = committed
            .iter()
            .filter(|target| RescanCapability::of(target) == Some(capability))
            .cloned()
            .collect::<Vec<_>>();
        // A capability the machine stopped offering keeps its committed targets unchanged, so the
        // replacement stays valid and the targets report unavailable when the node next serves.
        targets.extend(kept.iter().cloned());
        unavailable.push((capability, kept));
    }
    Ok(RescanScan {
        targets,
        unavailable,
    })
}

/// Returns the capabilities a re-scan of a node with `committed` targets looks for: the committed
/// ones, plus every real capability of each kind of workload the node does not run simulated.
fn candidates_of(committed: &[ExecutionTarget]) -> BTreeSet<RescanCapability> {
    let mut candidates = capabilities_of(committed);
    if !candidates.contains(&RescanCapability::SimulatedModels) {
        candidates.extend([
            RescanCapability::NvidiaModels,
            RescanCapability::AppleModels,
        ]);
    }
    if !candidates.contains(&RescanCapability::SimulatedApplications) {
        candidates.insert(RescanCapability::DockerApplications);
    }
    candidates
}

/// Returns the capabilities `committed` targets offer, without repeats.
fn capabilities_of(committed: &[ExecutionTarget]) -> BTreeSet<RescanCapability> {
    committed
        .iter()
        .filter_map(RescanCapability::of)
        .collect::<BTreeSet<_>>()
}

/// Composes the request that offers `capabilities`, with the simulated hardware `declared` names and
/// the native workload lifetime the node already runs under.
fn request_for(
    capabilities: &BTreeSet<RescanCapability>,
    declared: Option<&DeclaredMachine>,
    lifetime: NativeWorkloadLifetime,
) -> InitialSiteRequest {
    let mut inference = BTreeSet::new();
    let mut applications = BTreeSet::new();
    for capability in capabilities {
        match capability {
            RescanCapability::SimulatedModels => {
                inference.insert(InitialInferenceRuntime::Simulated);
            }
            RescanCapability::NvidiaModels => {
                inference.insert(InitialInferenceRuntime::Nvidia);
            }
            RescanCapability::AppleModels => {
                inference.insert(InitialInferenceRuntime::Apple);
            }
            RescanCapability::DockerApplications => {
                applications.insert(InitialApplicationRuntime::Docker);
            }
            RescanCapability::SimulatedApplications => {
                applications.insert(InitialApplicationRuntime::Simulated);
            }
        }
    }
    let mut request =
        InitialSiteRequest::new(inference, applications).with_native_workload_lifetime(lifetime);
    if request
        .inference()
        .contains(&InitialInferenceRuntime::Simulated)
    {
        request = request.with_simulated_hardware(
            declared.map_or_else(Vec::new, |machine| machine.choices.clone()),
        );
    }
    request
}

/// Returns the workload lifetime the node's native model targets declare, or the default when it
/// declares none.
fn workload_lifetime_of(committed: &[ExecutionTarget]) -> NativeWorkloadLifetime {
    committed
        .iter()
        .find_map(|target| match target.runtime_binding.as_ref()? {
            execution_target::RuntimeBinding::ModelRuntime(binding) => {
                match binding.adapter.as_ref()? {
                    model_runtime_binding::Adapter::NativeRuntime(native) => {
                        // An unset or unknown lifetime reads as persistent, the request default.
                        Some(
                            NativeWorkloadLifetime::try_from(native.workload_lifetime)
                                .unwrap_or(NativeWorkloadLifetime::Persistent),
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .unwrap_or(NativeWorkloadLifetime::Persistent)
}

/// Why a node's replacement target set could not be built.
#[derive(Debug, thiserror::Error)]
pub enum RescanScanError {
    /// The probed capabilities could not be resolved into targets.
    #[error("select this machine's replacement execution targets: {0}")]
    Composition(#[source] crate::initial::inputs::InitialSiteCompositionError),
}
