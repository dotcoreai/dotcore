//! The joining node's proposal: the name, address and execution targets it asks the manager to
//! record, for example `gpu-box` with private port `7443`, one NVIDIA GPU target on linux/arm64
//! and a Docker applications target. The address is optional: without it the manager records the
//! source address it observes.
//!
//! The same value is built by the joining node and decoded and checked by the join host, so both
//! sides agree on what an acceptable proposal is before the site is consulted. A declared node that
//! joins again proposes only its address and private port; everything else comes from the site.

use std::collections::BTreeSet;

use dotcore_domain::{
    APPLE_GPU_DEVICE_ID, ExecutionPlatform, SiteNodeHost, names_every_cdi_device,
};
use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    node::v1::{
        JoinAppleModelTarget, JoinApplicationKind, JoinApplicationTarget, JoinModelTarget,
        JoinNvidiaModelTarget, JoinRequest, JoinSimulatedModelTarget, NodeObservationConfiguration,
        join_model_target,
    },
    observation::v1::AcceleratorVendor,
    site::v1::NativeWorkloadLifetime,
};

use super::JoinSecret;
use crate::{
    initial::{
        inputs::{
            ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices,
            application_accelerator_device_ids, same_gpu_selectors,
        },
        request::InitialApplicationRuntime,
    },
    validation::{MAX_DISPLAY_NAME_CHARS, is_valid_display_name},
    view::contract::MAX_EXECUTION_TARGETS,
};

/// What a joining node proposes about itself; the manager derives every identity from it.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinProposal {
    /// SiteNode display name, unique within the site.
    pub display_name: String,

    /// IPv4 address or DNS host name the manager dials for this node; `None` lets the manager
    /// record the source address it observes on the join connection.
    pub host: Option<SiteNodeHost>,

    /// Port of the node's private listener.
    pub private_port: u16,

    /// Execution targets the node declares, possibly none.
    pub targets: NodeTargetChoices,
}

impl JoinProposal {
    /// Checks the proposal on its own, without any site: a valid name, a concrete port, and
    /// targets that the site can declare side by side.
    pub fn validate(&self) -> Result<(), JoinProposalError> {
        if !is_valid_display_name(&self.display_name)
            || self.display_name.trim() != self.display_name
        {
            return Err(JoinProposalError::DisplayName);
        }
        if self.private_port == 0 {
            return Err(JoinProposalError::PrivatePort);
        }
        if self.targets.target_count() > MAX_EXECUTION_TARGETS {
            return Err(JoinProposalError::TooManyTargets);
        }
        self.validate_model_targets()?;
        let mut applications = BTreeSet::new();
        if !self
            .targets
            .application_targets
            .iter()
            .all(|target| applications.insert(target.applications))
        {
            return Err(JoinProposalError::DuplicateTarget);
        }
        self.validate_accelerator_facts()
    }

    /// Describes every proposed model target for operator review, for example
    /// `simulated (GPU-1f0d, linux/amd64)`, `nvidia (nvidia.com/gpu=0, linux/amd64)` or
    /// `apple (apple-gpu-0, ephemeral)`, or `none` when the node proposes no model target.
    pub fn inference_summary(&self) -> String {
        summary(
            self.targets
                .model_targets
                .iter()
                .map(|target| match target {
                    ModelTargetChoice::Simulated {
                        platform,
                        device_id,
                    } => format!("simulated ({device_id}, {platform})"),
                    ModelTargetChoice::Nvidia { selector, platform } => {
                        format!("nvidia ({selector}, {platform})")
                    }
                    ModelTargetChoice::Apple {
                        device_id,
                        lifetime,
                    } => format!(
                        "apple ({device_id}, {})",
                        match lifetime {
                            NativeWorkloadLifetime::Ephemeral => "ephemeral",
                            _ => "persistent",
                        }
                    ),
                }),
        )
    }

    /// Describes every proposed application target for operator review, for example
    /// `docker (nvidia.com/gpu=0, linux/amd64), simulated (linux/amd64)`, or `none`.
    pub fn applications_summary(&self) -> String {
        summary(self.targets.application_targets.iter().map(|target| {
            let runtime = match target.applications {
                InitialApplicationRuntime::Simulated => "simulated",
                InitialApplicationRuntime::Docker => "docker",
            };
            let devices = target
                .accelerator_device_ids
                .iter()
                .map(|device| format!("{device}, "))
                .collect::<String>();
            format!("{runtime} ({devices}{})", target.platform)
        }))
    }

    /// Checks model targets: simulated targets each on a distinct GPU, at most one Apple target
    /// on the observed Apple GPU, and NVIDIA selectors that each name one individual, distinct GPU.
    fn validate_model_targets(&self) -> Result<(), JoinProposalError> {
        let mut kinds = BTreeSet::new();
        let mut simulated_devices = BTreeSet::new();
        let mut selectors = Vec::new();
        for target in &self.targets.model_targets {
            match target {
                ModelTargetChoice::Apple { .. } if !kinds.insert(target.inference()) => {
                    return Err(JoinProposalError::DuplicateTarget);
                }
                ModelTargetChoice::Simulated { device_id, .. } => {
                    if device_id.is_empty()
                        || device_id.chars().count() > MAX_DISPLAY_NAME_CHARS
                        || device_id.chars().any(char::is_control)
                    {
                        return Err(JoinProposalError::SimulatedDevice);
                    }
                    if !simulated_devices.insert(device_id) {
                        return Err(JoinProposalError::DuplicateTarget);
                    }
                }
                ModelTargetChoice::Apple { device_id, .. } if device_id != APPLE_GPU_DEVICE_ID => {
                    return Err(JoinProposalError::AppleWithoutDevice);
                }
                ModelTargetChoice::Nvidia { selector, .. } => {
                    if selector.trim().is_empty() {
                        return Err(JoinProposalError::DeviceSelectors);
                    }
                    if names_every_cdi_device(selector) {
                        return Err(JoinProposalError::EveryDeviceSelector);
                    }
                    if selectors.contains(selector) {
                        return Err(JoinProposalError::DuplicateTarget);
                    }
                    selectors.push(selector.clone());
                }
                ModelTargetChoice::Apple { .. } => {}
            }
        }
        let nvidia_facts = self
            .targets
            .accelerator_facts
            .iter()
            .filter(|fact| fact.vendor == AcceleratorVendor::Nvidia as i32)
            .cloned()
            .collect::<Vec<_>>();
        if same_gpu_selectors(&selectors, &nvidia_facts).is_some() {
            return Err(JoinProposalError::SameGpuSelectors);
        }
        Ok(())
    }

    /// Checks the facts that name the model execution targets: a bounded device count, and
    /// models and device ids that a display name can hold.
    fn validate_accelerator_facts(&self) -> Result<(), JoinProposalError> {
        let facts = &self.targets.accelerator_facts;
        let valid = facts.len() <= MAX_PROPOSED_ACCELERATORS
            && facts.iter().all(|fact| {
                (fact.model.is_empty() || is_valid_display_name(&fact.model))
                    && fact.device_id.chars().count() <= MAX_DISPLAY_NAME_CHARS
                    && !fact.device_id.chars().any(char::is_control)
            });
        if valid {
            Ok(())
        } else {
            Err(JoinProposalError::AcceleratorFacts)
        }
    }

    /// Builds the wire request carrying this proposal, the join secret, the manager address the
    /// joining node dialed, and the signing request.
    pub(super) fn to_request(
        &self,
        secret: &JoinSecret,
        manager_host: &SiteNodeHost,
        csr_der: Vec<u8>,
    ) -> JoinRequest {
        JoinRequest {
            join_secret: secret.as_bytes().to_vec(),
            display_name: self.display_name.clone(),
            host: self
                .host
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            private_port: u32::from(self.private_port),
            model_targets: self
                .targets
                .model_targets
                .iter()
                .map(model_target_to_wire)
                .collect(),
            application_targets: self
                .targets
                .application_targets
                .iter()
                .map(|target| JoinApplicationTarget {
                    kind: match target.applications {
                        InitialApplicationRuntime::Simulated => JoinApplicationKind::Simulated,
                        InitialApplicationRuntime::Docker => JoinApplicationKind::Docker,
                    } as i32,
                    platform: Some(target.platform.to_oci_platform()),
                })
                .collect(),
            accelerator_facts: self.targets.accelerator_facts.clone(),
            csr_der,
            manager_host: manager_host.to_string(),
            node_certificate_der: Vec::new(),
            node_observation: None,
        }
    }

    /// Decodes and validates the proposal in a wire request; the secret, the dialed manager
    /// address, and the signing request are left to the caller.
    pub(super) fn from_request(request: &JoinRequest) -> Result<Self, JoinProposalError> {
        let host = decode_host(&request.host)?;
        let private_port =
            u16::try_from(request.private_port).map_err(|_| JoinProposalError::PrivatePort)?;
        let model_targets = request
            .model_targets
            .iter()
            .map(model_target_from_wire)
            .collect::<Result<Vec<_>, _>>()?;
        let application_targets = request
            .application_targets
            .iter()
            .map(|target| {
                let applications = match JoinApplicationKind::try_from(target.kind) {
                    Ok(JoinApplicationKind::Simulated) => InitialApplicationRuntime::Simulated,
                    Ok(JoinApplicationKind::Docker) => InitialApplicationRuntime::Docker,
                    Ok(JoinApplicationKind::Unspecified) | Err(_) => {
                        return Err(JoinProposalError::ApplicationKind);
                    }
                };
                Ok(ApplicationTargetChoice {
                    applications,
                    platform: decode_platform(target.platform.as_ref())?,
                    // The wire carries no application devices: the manager derives them from
                    // the proposed model targets with the rule site initialization uses.
                    accelerator_device_ids: application_accelerator_device_ids(
                        applications,
                        &model_targets,
                    ),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let proposal = Self {
            display_name: request.display_name.clone(),
            host,
            private_port,
            targets: NodeTargetChoices {
                model_targets,
                application_targets,
                accelerator_facts: request.accelerator_facts.clone(),
                // The declared machine stays on the joining node.
                declared_machine: None,
            },
        };
        proposal.validate()?;
        Ok(proposal)
    }
}

/// Joins target descriptions for review, or `none` without any.
fn summary(targets: impl Iterator<Item = String>) -> String {
    let targets = targets.collect::<Vec<_>>();
    if targets.is_empty() {
        "none".to_owned()
    } else {
        targets.join(", ")
    }
}

fn model_target_to_wire(target: &ModelTargetChoice) -> JoinModelTarget {
    JoinModelTarget {
        target: Some(match target {
            ModelTargetChoice::Simulated {
                platform,
                device_id,
            } => join_model_target::Target::Simulated(JoinSimulatedModelTarget {
                platform: Some(platform.to_oci_platform()),
                device_id: device_id.clone(),
            }),
            ModelTargetChoice::Nvidia { selector, platform } => {
                join_model_target::Target::Nvidia(JoinNvidiaModelTarget {
                    device_selector: selector.clone(),
                    platform: Some(platform.to_oci_platform()),
                })
            }
            ModelTargetChoice::Apple {
                device_id,
                lifetime,
            } => join_model_target::Target::Apple(JoinAppleModelTarget {
                device_id: device_id.clone(),
                native_workload_lifetime: *lifetime as i32,
            }),
        }),
    }
}

fn model_target_from_wire(
    target: &JoinModelTarget,
) -> Result<ModelTargetChoice, JoinProposalError> {
    match &target.target {
        Some(join_model_target::Target::Simulated(simulated)) => Ok(ModelTargetChoice::Simulated {
            platform: decode_platform(simulated.platform.as_ref())?,
            device_id: simulated.device_id.clone(),
        }),
        Some(join_model_target::Target::Nvidia(nvidia)) => Ok(ModelTargetChoice::Nvidia {
            selector: nvidia.device_selector.clone(),
            platform: decode_platform(nvidia.platform.as_ref())?,
        }),
        Some(join_model_target::Target::Apple(apple)) => Ok(ModelTargetChoice::Apple {
            device_id: apple.device_id.clone(),
            lifetime: match NativeWorkloadLifetime::try_from(apple.native_workload_lifetime) {
                Ok(NativeWorkloadLifetime::Unspecified | NativeWorkloadLifetime::Persistent) => {
                    NativeWorkloadLifetime::Persistent
                }
                Ok(NativeWorkloadLifetime::Ephemeral) => NativeWorkloadLifetime::Ephemeral,
                Err(_) => return Err(JoinProposalError::WorkloadLifetime),
            },
        }),
        None => Err(JoinProposalError::ModelTarget),
    }
}

/// Decodes a proposed target platform, which must be present and supported.
fn decode_platform(platform: Option<&OciPlatform>) -> Result<ExecutionPlatform, JoinProposalError> {
    platform
        .ok_or(JoinProposalError::Platform)
        .and_then(|platform| {
            ExecutionPlatform::try_from(platform).map_err(|_| JoinProposalError::Platform)
        })
}

/// What a declared node proposes when it joins again from its intact node folder: where the
/// manager reaches it and the port it listens on. Its name, targets, and identity come from the
/// site and its certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejoinProposal {
    /// IPv4 address or DNS host name the manager dials for this node; `None` keeps the host the
    /// site declares for it.
    pub host: Option<SiteNodeHost>,

    /// Port of the node's private listener.
    pub private_port: u16,
}

impl RejoinProposal {
    /// Checks the proposal on its own: a concrete port.
    pub fn validate(&self) -> Result<(), JoinProposalError> {
        if self.private_port == 0 {
            return Err(JoinProposalError::PrivatePort);
        }
        Ok(())
    }

    /// Builds the wire request carrying this proposal, the join secret, the manager address the
    /// joining node dialed, the signing request bound to the secret, the node's certificate, and
    /// the observation configuration its folder holds.
    pub(super) fn to_request(
        &self,
        secret: &JoinSecret,
        manager_host: &SiteNodeHost,
        csr_der: Vec<u8>,
        node_certificate_der: Vec<u8>,
        node_observation: NodeObservationConfiguration,
    ) -> JoinRequest {
        JoinRequest {
            join_secret: secret.as_bytes().to_vec(),
            host: self
                .host
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            private_port: u32::from(self.private_port),
            csr_der,
            manager_host: manager_host.to_string(),
            node_certificate_der,
            node_observation: Some(node_observation),
            ..JoinRequest::default()
        }
    }

    /// Decodes and validates a rejoin request, which carries nothing a new node proposes.
    fn from_request(request: &JoinRequest) -> Result<Self, JoinProposalError> {
        let rejoin_fields = JoinRequest {
            join_secret: request.join_secret.clone(),
            host: request.host.clone(),
            private_port: request.private_port,
            csr_der: request.csr_der.clone(),
            manager_host: request.manager_host.clone(),
            node_certificate_der: request.node_certificate_der.clone(),
            node_observation: request.node_observation.clone(),
            ..JoinRequest::default()
        };
        if *request != rejoin_fields {
            return Err(JoinProposalError::RejoinWithNewNodeFields);
        }
        if request.node_observation.is_none() {
            return Err(JoinProposalError::RejoinWithoutObservation);
        }
        let proposal = Self {
            host: decode_host(&request.host)?,
            private_port: u16::try_from(request.private_port)
                .map_err(|_| JoinProposalError::PrivatePort)?,
        };
        proposal.validate()?;
        Ok(proposal)
    }
}

/// A decoded join request: a new node's proposal, or a declared node's rejoin, which the request
/// tells apart by carrying the node's certificate.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ProposedJoin {
    New(Box<JoinProposal>),

    Rejoin(RejoinProposal),
}

impl ProposedJoin {
    /// Decodes and validates the proposal of either kind in a wire request.
    pub(super) fn from_request(request: &JoinRequest) -> Result<Self, JoinProposalError> {
        if request.node_certificate_der.is_empty() {
            JoinProposal::from_request(request).map(|proposal| Self::New(Box::new(proposal)))
        } else {
            RejoinProposal::from_request(request).map(Self::Rejoin)
        }
    }
}

/// Response metadata key on which the join host names why it refused a proposal, so the
/// joining node can tell refusals apart without reading the message.
pub(super) const REJECTION_METADATA_KEY: &str = "dotcore-join-rejection";

/// [`REJECTION_METADATA_KEY`] value: another node already has the proposed display name.
pub(super) const DUPLICATE_NAME_REJECTION: &str = "duplicate-name";

/// [`REJECTION_METADATA_KEY`] value: another node already uses the proposed host.
pub(super) const DUPLICATE_HOST_REJECTION: &str = "duplicate-host";

/// [`REJECTION_METADATA_KEY`] value: the joining node proposed no host, and the manager cannot
/// record the address it observed the connection from.
pub(super) const UNUSABLE_HOST_REJECTION: &str = "unusable-host";

/// [`REJECTION_METADATA_KEY`] value: the manager could not reach the joining node by its key at
/// the host and private port it would record.
pub(super) const UNREACHABLE_NODE_REJECTION: &str = "unreachable-node";

/// [`REJECTION_METADATA_KEY`] value: a rejoining node's certificate names a node the site no
/// longer declares.
pub(super) const NODE_NOT_DECLARED_REJECTION: &str = "node-not-declared";

/// [`REJECTION_METADATA_KEY`] value: a rejoining node's certificate is not the one the site pins
/// for that node.
pub(super) const CERTIFICATE_NOT_PINNED_REJECTION: &str = "certificate-not-pinned";

/// [`REJECTION_METADATA_KEY`] value: a rejoining node's certificate has expired.
pub(super) const CERTIFICATE_EXPIRED_REJECTION: &str = "certificate-expired";

/// [`REJECTION_METADATA_KEY`] value: a rejoining node's certificate is not valid yet, as when the
/// manager's clock is behind.
pub(super) const CERTIFICATE_NOT_YET_VALID_REJECTION: &str = "certificate-not-yet-valid";

/// A join proposal is not acceptable on its own; each variant names the field to fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JoinProposalError {
    #[error(
        "node name must be 1 to 128 characters without control characters or surrounding spaces"
    )]
    DisplayName,

    #[error("node host must be a specific IPv4 address or a DNS host name")]
    Host,

    #[error("node private port must be between 1 and 65535")]
    PrivatePort,

    #[error(
        "node model or application platform is missing or not supported \
         (linux/amd64, linux/arm64, darwin/arm64)"
    )]
    Platform,

    #[error("node application runtime must be simulated or docker")]
    ApplicationKind,

    #[error("node model target must be simulated, nvidia or apple")]
    ModelTarget,

    #[error("node proposes at most 64 execution targets")]
    TooManyTargets,

    #[error("node proposes the same execution target twice")]
    DuplicateTarget,

    #[error("NVIDIA device selectors must be non-empty")]
    DeviceSelectors,

    #[error("NVIDIA device selectors must name individual GPUs, not every GPU")]
    EveryDeviceSelector,

    #[error("two NVIDIA device selectors name the same GPU")]
    SameGpuSelectors,

    #[error("Apple model target needs the Apple GPU device ID \"apple-gpu-0\"")]
    AppleWithoutDevice,

    #[error(
        "simulated model target needs a GPU ID of 1 to 128 characters without control characters"
    )]
    SimulatedDevice,

    #[error("native workload lifetime must be persistent or ephemeral")]
    WorkloadLifetime,

    #[error(
        "accelerator facts must describe at most 64 NVIDIA or Apple devices, with models of up to \
         128 characters without control characters, and device IDs of up to \
         128 characters without control characters"
    )]
    AcceleratorFacts,

    #[error("a rejoining node proposes no name or execution target")]
    RejoinWithNewNodeFields,

    #[error("a rejoining node must send the observation configuration its folder holds")]
    RejoinWithoutObservation,
}

/// Most accelerator devices a proposal may describe, bounding what a joining node can send.
const MAX_PROPOSED_ACCELERATORS: usize = 64;

/// Decodes an optional proposed host; empty means the manager records the observed address.
fn decode_host(host: &str) -> Result<Option<SiteNodeHost>, JoinProposalError> {
    match host {
        "" => Ok(None),
        host => SiteNodeHost::normalize(host)
            .map(Some)
            .map_err(|_| JoinProposalError::Host),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::observation::v1::AcceleratorFacts;

    use super::*;
    use crate::membership::join::JOIN_SECRET_BYTES;

    fn proposal() -> JoinProposal {
        JoinProposal {
            display_name: "gpu-box".to_owned(),
            host: Some(SiteNodeHost::parse("10.0.0.7").expect("host")),
            private_port: 7443,
            targets: NodeTargetChoices {
                model_targets: vec![nvidia("nvidia.com/gpu=0")],
                application_targets: vec![ApplicationTargetChoice {
                    accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
                    ..application(InitialApplicationRuntime::Docker)
                }],
                accelerator_facts: vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    device_id: "GPU-0".to_owned(),
                    vendor_index: Some(0),
                    ..AcceleratorFacts::default()
                }],
                declared_machine: None,
            },
        }
    }

    fn mixed_proposal() -> JoinProposal {
        let mut proposal = proposal();
        proposal.targets.model_targets.insert(
            0,
            ModelTargetChoice::Simulated {
                platform: ExecutionPlatform::linux_arm64(),
                device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
            },
        );
        proposal
            .targets
            .application_targets
            .push(application(InitialApplicationRuntime::Simulated));
        proposal
    }

    fn apple_proposal() -> JoinProposal {
        JoinProposal {
            targets: NodeTargetChoices {
                model_targets: vec![ModelTargetChoice::Apple {
                    device_id: "apple-gpu-0".to_owned(),
                    lifetime: NativeWorkloadLifetime::Ephemeral,
                }],
                application_targets: Vec::new(),
                accelerator_facts: Vec::new(),
                declared_machine: None,
            },
            ..proposal()
        }
    }

    fn empty_proposal() -> JoinProposal {
        JoinProposal {
            host: None,
            targets: NodeTargetChoices::default(),
            ..proposal()
        }
    }

    fn nvidia(selector: &str) -> ModelTargetChoice {
        ModelTargetChoice::Nvidia {
            selector: selector.to_owned(),
            platform: ExecutionPlatform::linux_arm64(),
        }
    }

    fn application(applications: InitialApplicationRuntime) -> ApplicationTargetChoice {
        ApplicationTargetChoice {
            applications,
            platform: ExecutionPlatform::linux_amd64(),
            accelerator_device_ids: Vec::new(),
        }
    }

    fn wire(proposal: &JoinProposal) -> JoinRequest {
        proposal.to_request(
            &JoinSecret::from_bytes([3; JOIN_SECRET_BYTES]),
            &SiteNodeHost::parse("100.64.0.10").expect("manager host"),
            b"csr".to_vec(),
        )
    }

    #[test]
    fn proposal_round_trips_through_the_wire_request() {
        let request = wire(&proposal());

        assert_eq!(request.join_secret, vec![3; JOIN_SECRET_BYTES]);
        assert_eq!(request.csr_der, b"csr");
        assert_eq!(request.manager_host, "100.64.0.10");
        assert_eq!(
            request.application_targets,
            [JoinApplicationTarget {
                kind: JoinApplicationKind::Docker as i32,
                platform: Some(ExecutionPlatform::linux_amd64().to_oci_platform()),
            }]
        );
        assert_eq!(
            request.model_targets,
            [JoinModelTarget {
                target: Some(join_model_target::Target::Nvidia(JoinNvidiaModelTarget {
                    device_selector: "nvidia.com/gpu=0".to_owned(),
                    platform: Some(ExecutionPlatform::linux_arm64().to_oci_platform()),
                })),
            }]
        );
        for proposal in [
            proposal(),
            mixed_proposal(),
            apple_proposal(),
            empty_proposal(),
        ] {
            assert_eq!(
                JoinProposal::from_request(&wire(&proposal)).expect("decode"),
                proposal
            );
        }
        assert_eq!(wire(&empty_proposal()).host, "");
    }

    #[test]
    fn the_docker_application_target_decodes_with_the_proposed_nvidia_gpus() {
        let mut two_gpus = proposal();
        two_gpus
            .targets
            .model_targets
            .push(nvidia("nvidia.com/gpu=1"));
        let second_gpu = AcceleratorFacts {
            device_id: "GPU-1".to_owned(),
            vendor_index: Some(1),
            ..two_gpus.targets.accelerator_facts[0].clone()
        };
        two_gpus.targets.accelerator_facts.push(second_gpu);
        let mut without_nvidia = proposal();
        without_nvidia.targets.model_targets = vec![ModelTargetChoice::Simulated {
            platform: ExecutionPlatform::linux_arm64(),
            device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
        }];
        without_nvidia.targets.accelerator_facts.clear();

        let devices = |proposal: &JoinProposal| {
            JoinProposal::from_request(&wire(proposal))
                .expect("decode")
                .targets
                .application_targets
                .into_iter()
                .map(|target| target.accelerator_device_ids)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            devices(&two_gpus),
            [vec![
                "nvidia.com/gpu=0".to_owned(),
                "nvidia.com/gpu=1".to_owned()
            ]]
        );
        assert_eq!(devices(&without_nvidia), [Vec::<String>::new()]);
        assert_eq!(devices(&mixed_proposal())[1], Vec::<String>::new());
    }

    #[test]
    fn apple_target_carries_device_and_lifetime_through_the_wire_request() {
        let request = wire(&apple_proposal());

        assert_eq!(
            request.model_targets,
            [JoinModelTarget {
                target: Some(join_model_target::Target::Apple(JoinAppleModelTarget {
                    device_id: "apple-gpu-0".to_owned(),
                    native_workload_lifetime: NativeWorkloadLifetime::Ephemeral as i32,
                })),
            }]
        );

        let mut unspecified = request;
        if let Some(join_model_target::Target::Apple(apple)) =
            &mut unspecified.model_targets[0].target
        {
            apple.native_workload_lifetime = NativeWorkloadLifetime::Unspecified as i32;
        }
        assert_eq!(
            JoinProposal::from_request(&unspecified)
                .expect("decode")
                .targets
                .model_targets,
            [ModelTargetChoice::Apple {
                device_id: "apple-gpu-0".to_owned(),
                lifetime: NativeWorkloadLifetime::Persistent,
            }]
        );
    }

    #[test]
    fn simulated_targets_each_hold_a_distinct_named_gpu() {
        let simulated = |device_id: &str| ModelTargetChoice::Simulated {
            platform: ExecutionPlatform::linux_amd64(),
            device_id: device_id.to_owned(),
        };
        let with = |model_targets: Vec<ModelTargetChoice>| JoinProposal {
            targets: NodeTargetChoices {
                model_targets,
                ..NodeTargetChoices::default()
            },
            ..proposal()
        };
        let two_gpus = with(vec![simulated("GPU-a"), simulated("GPU-b")]);

        assert_eq!(
            JoinProposal::from_request(&wire(&two_gpus)).expect("decode"),
            two_gpus
        );
        for (targets, expected) in [
            (
                vec![simulated("GPU-a"), simulated("GPU-a")],
                JoinProposalError::DuplicateTarget,
            ),
            (vec![simulated("")], JoinProposalError::SimulatedDevice),
            (vec![simulated("GPU\n")], JoinProposalError::SimulatedDevice),
        ] {
            let error = with(targets.clone()).validate().expect_err("invalid");
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected),
                "{targets:?}"
            );
        }
    }

    #[test]
    fn summaries_list_every_proposed_target() {
        assert_eq!(
            proposal().inference_summary(),
            "nvidia (nvidia.com/gpu=0, linux/arm64)"
        );
        assert_eq!(
            proposal().applications_summary(),
            "docker (nvidia.com/gpu=0, linux/amd64)"
        );
        assert_eq!(
            mixed_proposal().inference_summary(),
            "simulated (GPU-00000000-0000-0000-0000-000000005090, linux/arm64), nvidia \
             (nvidia.com/gpu=0, linux/arm64)"
        );
        assert_eq!(
            mixed_proposal().applications_summary(),
            "docker (nvidia.com/gpu=0, linux/amd64), simulated (linux/amd64)"
        );
        assert_eq!(
            apple_proposal().inference_summary(),
            "apple (apple-gpu-0, ephemeral)"
        );
        assert_eq!(empty_proposal().inference_summary(), "none");
        assert_eq!(empty_proposal().applications_summary(), "none");
    }

    #[test]
    fn proposed_host_names_are_normalized() {
        let mut request = wire(&proposal());
        request.host = "GPU-Box.Example.TS.net.".to_owned();

        let decoded = JoinProposal::from_request(&request).expect("decode");
        assert_eq!(
            decoded.host.map(|host| host.to_string()).as_deref(),
            Some("gpu-box.example.ts.net")
        );
    }

    #[test]
    fn a_request_with_a_certificate_decodes_as_a_rejoin_with_no_new_node_fields() {
        let secret = JoinSecret::from_bytes([3; JOIN_SECRET_BYTES]);
        let manager_host = SiteNodeHost::parse("10.0.0.1").expect("manager host");
        let rejoin = RejoinProposal {
            host: Some(SiteNodeHost::parse("10.0.0.8").expect("host")),
            private_port: 7443,
        };
        let request = rejoin.to_request(
            &secret,
            &manager_host,
            b"csr".to_vec(),
            b"leaf".to_vec(),
            NodeObservationConfiguration::default(),
        );

        assert_eq!(request.display_name, "");
        assert_eq!(request.node_certificate_der, b"leaf");
        assert_eq!(
            ProposedJoin::from_request(&request),
            Ok(ProposedJoin::Rejoin(rejoin.clone()))
        );
        assert_eq!(
            ProposedJoin::from_request(&wire(&proposal())),
            Ok(ProposedJoin::New(Box::new(proposal())))
        );
        assert_eq!(
            ProposedJoin::from_request(&wire(&empty_proposal())),
            Ok(ProposedJoin::New(Box::new(empty_proposal())))
        );

        let mut named = request.clone();
        named.display_name = "gpu-box".to_owned();
        let mut with_model_target = request.clone();
        with_model_target.model_targets = wire(&mixed_proposal()).model_targets;
        let mut with_application_target = request.clone();
        with_application_target.application_targets = wire(&proposal()).application_targets;
        let mut without_observation = request.clone();
        without_observation.node_observation = None;
        let mut without_port = request;
        without_port.private_port = 0;
        for (rejoin, expected) in [
            (named, JoinProposalError::RejoinWithNewNodeFields),
            (
                with_model_target,
                JoinProposalError::RejoinWithNewNodeFields,
            ),
            (
                with_application_target,
                JoinProposalError::RejoinWithNewNodeFields,
            ),
            (
                without_observation,
                JoinProposalError::RejoinWithoutObservation,
            ),
            (without_port, JoinProposalError::PrivatePort),
        ] {
            assert_eq!(ProposedJoin::from_request(&rejoin), Err(expected));
        }
    }

    #[test]
    fn the_target_bound_counts_model_and_application_targets_together() {
        let mut at_bound = proposal();
        at_bound.targets.model_targets = (0..MAX_EXECUTION_TARGETS - 1)
            .map(|index| nvidia(&format!("nvidia.com/gpu=MIG-{index}")))
            .collect();
        assert_eq!(at_bound.targets.target_count(), MAX_EXECUTION_TARGETS);
        assert_eq!(at_bound.validate(), Ok(()));

        let mut over = at_bound;
        over.targets
            .application_targets
            .push(application(InitialApplicationRuntime::Simulated));
        assert_eq!(over.validate(), Err(JoinProposalError::TooManyTargets));
    }

    #[test]
    fn invalid_proposals_name_the_field_to_fix() {
        let request = wire(&mixed_proposal());
        let cases: [(RequestEdit, JoinProposalError); 22] = [
            (
                |request| request.display_name = " padded".to_owned(),
                JoinProposalError::DisplayName,
            ),
            (
                |request| request.host = "0.0.0.0".to_owned(),
                JoinProposalError::Host,
            ),
            (
                |request| request.host = "::1".to_owned(),
                JoinProposalError::Host,
            ),
            (
                |request| request.host = "gpu_box.local".to_owned(),
                JoinProposalError::Host,
            ),
            (
                |request| request.private_port = 70_000,
                JoinProposalError::PrivatePort,
            ),
            (
                |request| {
                    if let Some(join_model_target::Target::Simulated(simulated)) =
                        &mut request.model_targets[0].target
                    {
                        simulated.platform = None;
                    }
                },
                JoinProposalError::Platform,
            ),
            (
                |request| request.application_targets[0].platform = None,
                JoinProposalError::Platform,
            ),
            (
                |request| {
                    request.application_targets[0].kind = JoinApplicationKind::Unspecified as i32
                },
                JoinProposalError::ApplicationKind,
            ),
            (
                |request| request.model_targets[0].target = None,
                JoinProposalError::ModelTarget,
            ),
            (
                |request| request.model_targets.push(request.model_targets[0].clone()),
                JoinProposalError::DuplicateTarget,
            ),
            (
                |request| request.model_targets.push(request.model_targets[1].clone()),
                JoinProposalError::DuplicateTarget,
            ),
            (
                |request| {
                    request
                        .application_targets
                        .push(request.application_targets[0].clone())
                },
                JoinProposalError::DuplicateTarget,
            ),
            (
                |request| request.model_targets[1] = wire_nvidia(""),
                JoinProposalError::DeviceSelectors,
            ),
            (
                |request| request.model_targets[1] = wire_nvidia("nvidia.com/gpu=all"),
                JoinProposalError::EveryDeviceSelector,
            ),
            (
                |request| {
                    request
                        .model_targets
                        .push(wire_nvidia("nvidia.com/gpu=gpu0"))
                },
                JoinProposalError::SameGpuSelectors,
            ),
            (
                |request| request.accelerator_facts[0].model = "RTX\u{7}5090".to_owned(),
                JoinProposalError::AcceleratorFacts,
            ),
            (
                |request| request.accelerator_facts[0].model = "   ".to_owned(),
                JoinProposalError::AcceleratorFacts,
            ),
            (
                |request| request.accelerator_facts[0].model = "R".repeat(129),
                JoinProposalError::AcceleratorFacts,
            ),
            (
                |request| {
                    let fact = request.accelerator_facts[0].clone();
                    request.accelerator_facts = vec![fact; 65];
                },
                JoinProposalError::AcceleratorFacts,
            ),
            (
                |request| request.model_targets.push(wire_apple("apple-gpu-1", 0)),
                JoinProposalError::AppleWithoutDevice,
            ),
            (
                |request| request.model_targets.push(wire_apple("apple-gpu-0", 99)),
                JoinProposalError::WorkloadLifetime,
            ),
            (
                |request| {
                    request.model_targets.push(wire_apple("apple-gpu-0", 0));
                    request.model_targets.push(wire_apple("apple-gpu-0", 2));
                },
                JoinProposalError::DuplicateTarget,
            ),
        ];
        for (edit, expected) in cases {
            let mut request = request.clone();
            edit(&mut request);
            assert_eq!(JoinProposal::from_request(&request), Err(expected));
        }
    }

    fn wire_nvidia(selector: &str) -> JoinModelTarget {
        JoinModelTarget {
            target: Some(join_model_target::Target::Nvidia(JoinNvidiaModelTarget {
                device_selector: selector.to_owned(),
                platform: Some(ExecutionPlatform::linux_arm64().to_oci_platform()),
            })),
        }
    }

    fn wire_apple(device_id: &str, native_workload_lifetime: i32) -> JoinModelTarget {
        JoinModelTarget {
            target: Some(join_model_target::Target::Apple(JoinAppleModelTarget {
                device_id: device_id.to_owned(),
                native_workload_lifetime,
            })),
        }
    }

    /// Breaks one field of a valid request.
    type RequestEdit = fn(&mut JoinRequest);
}
