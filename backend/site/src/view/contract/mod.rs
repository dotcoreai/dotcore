//! Validates the private node desired-view wire contract before any node intent changes.
//! It converts one view published on a node control stream into the lifecycle projection and
//! validates stream opens, acknowledgements, independently sequenced status, and cleanup receipts
//! without serving the stream.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::{
    application::private::v1::{
        ApplicationCleanupDisposition, ApplicationCleanupEvidence, ApplicationCleanupReceipt,
        ApplicationControllerStatusSection, ApplicationDeploymentStatus,
        ApplicationDesiredViewSection, ApplicationLifecycleObservation,
        ApplicationStatusFailureCode, ApplicationStatusStage,
    },
    deployment::v1::{
        AcceleratorMemoryObservation, AcceleratorMemoryUnavailable, accelerator_memory_observation,
        accelerator_memory_refresh_state,
    },
    model::{
        private::v1::{
            ModelBindingReadiness, ModelCleanupDisposition, ModelCleanupEvidence,
            ModelCleanupReceipt, ModelControllerStatusSection, ModelDeploymentStatus,
            ModelDesiredViewSection, ModelLifecycleObservation,
        },
        v1::{ModelDeploymentFailure, ObservedPlacement, model_deployment_snapshot},
    },
    site::{
        private::v1::{
            DesiredViewRejectionCode, DesiredViewSectionKind, NodeControlOpen,
            NodeControllerStatus, NodeControllerStatusAcceptance, NodeDesiredView,
            NodeViewAcknowledgement, NodeViewReference, SupportedDesiredViewSection,
            controller_status_receipt, controller_status_section, desired_view_section,
            node_view_acknowledgement,
        },
        store::v1::NodeModelCleanupEvidence,
        v1::{ExecutionTarget, execution_target, model_runtime_binding},
    },
};
use prost::Message;

use crate::validation::{binding_supports_platform, canonical_platform};
use crate::view::fencing::{
    AcceptedNodeViewIdentity, ManagerSession, NodeIncarnation, NodeViewDigest, NodeViewError,
    NodeViewProjection, NodeViewPublicationIdentity, NodeViewStream, ProcessIdentityError,
};

pub(crate) const NODE_VIEW_PROJECTION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MODEL_SECTION_SCHEMA_VERSION: u32 = 1;
pub(crate) const APPLICATION_SECTION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_NODE_VIEW_BYTES: usize = 4 * 1024 * 1024;
/// Sized for every assignment reporting a failure with complete diagnostics.
pub(crate) const MAX_CONTROLLER_STATUS_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_EXECUTION_TARGETS: usize = 64;
pub(crate) const MAX_MODEL_ASSIGNMENTS: usize = 256;
pub(crate) const MAX_APPLICATION_ASSIGNMENTS: usize = 256;
pub(crate) const MAX_CLEANUP_EVIDENCE: usize = 256;
pub(crate) const MAX_STATUS_MESSAGE_BYTES: usize = 512;
pub(crate) const MAX_FAILURE_CODE_BYTES: usize = 64;
pub(crate) const MAX_FAILURE_MESSAGE_BYTES: usize = 1024;
pub(crate) const MAX_FAILURE_DIAGNOSTICS: usize = 8;
/// Total bytes of one failure's diagnostics, room for an engine startup output tail.
pub(crate) const MAX_FAILURE_DIAGNOSTIC_BYTES: usize = 6 * 1024;
/// Devices one deployment's reported accelerator memory may list.
pub(crate) const MAX_ACCELERATOR_MEMORY_DEVICES: usize = 64;
pub(crate) const MAX_ACCELERATOR_DEVICE_ID_BYTES: usize = 256;

/// Code reported for an accelerator memory unavailable reason recorded without one.
const UNSPECIFIED_UNAVAILABLE_CODE: &str = "UNSPECIFIED";

/// Validated complete view ready for atomic installation by the lifecycle authority.
#[derive(Debug)]
pub(crate) struct ValidatedNodeView {
    pub(crate) identity: AcceptedNodeViewIdentity,
    pub(crate) projection: NodeViewProjection,
}

/// Validates one view published on a node control stream: its envelope names the stream's open
/// identity and manager session, and its content is complete, supported and within bounds.
/// Strictly increasing publication sequences are fenced by the node's view authority.
pub(crate) fn validate_published_view(
    open: &NodeControlOpen,
    stream: &NodeViewStream,
    view: &NodeDesiredView,
) -> Result<ValidatedNodeView, DesiredViewContractError> {
    if view.encoded_len() > MAX_NODE_VIEW_BYTES {
        return Err(DesiredViewContractError::LimitExceeded);
    }
    let identity = view
        .identity
        .as_ref()
        .ok_or(DesiredViewContractError::IdentityMismatch)?;
    if identity.site_id != open.site_id
        || identity.node_id != open.node_id
        || open.node_incarnation_id != stream.node_incarnation().as_str()
        || identity.node_incarnation_id != open.node_incarnation_id
        || identity.manager_session_id != stream.manager_session().as_str()
    {
        return Err(DesiredViewContractError::IdentityMismatch);
    }
    let publication = NodeViewPublicationIdentity::new(
        stream.manager_session().clone(),
        stream.node_incarnation().clone(),
        identity.publication_sequence,
    )?;
    validate_view(view, publication, &open.supported_sections)
}

/// Validates one stream open's identity and section support before the manager answers it.
pub(crate) fn validate_stream_open(open: &NodeControlOpen) -> Result<(), DesiredViewContractError> {
    if !valid_id(&open.site_id) || !valid_id(&open.node_id) {
        return Err(DesiredViewContractError::IdentityMismatch);
    }
    NodeIncarnation::parse(open.node_incarnation_id.clone())?;
    supported_sections(&open.supported_sections)?;
    Ok(())
}

/// Validates one complete view whose envelope already names this node, converting it into the
/// lifecycle projection under the `publication` it arrived with.
fn validate_view(
    view: &NodeDesiredView,
    publication: NodeViewPublicationIdentity,
    supported: &[SupportedDesiredViewSection],
) -> Result<ValidatedNodeView, DesiredViewContractError> {
    let identity = view
        .identity
        .as_ref()
        .ok_or(DesiredViewContractError::IdentityMismatch)?;
    if identity.view_revision == 0
        || identity.projection_schema_version != NODE_VIEW_PROJECTION_SCHEMA_VERSION
    {
        return Err(DesiredViewContractError::IdentityMismatch);
    }
    let supported = supported_sections(supported)?;
    let required = required_sections(view, &supported)?;
    let sections = owner_sections(view, &required)?;
    if view.execution_targets.len() > MAX_EXECUTION_TARGETS
        || sections.model.assignments.len() > MAX_MODEL_ASSIGNMENTS
        || sections
            .application
            .is_some_and(|application| application.assignments.len() > MAX_APPLICATION_ASSIGNMENTS)
    {
        return Err(DesiredViewContractError::LimitExceeded);
    }
    // A view naming an application target must state application intent explicitly, so a
    // node never mistakes an omitted section for the removal of every application.
    if sections.application.is_none() && has_application_target(&view.execution_targets) {
        return Err(DesiredViewContractError::InvalidRequiredSection);
    }
    let projection = model_projection(&identity.node_id, &view.execution_targets, sections.model)?
        .with_application_assignments(
            sections
                .application
                .map(|application| application.assignments.clone())
                .unwrap_or_default(),
        )
        .map_err(|_| DesiredViewContractError::InvalidSectionPayload)?;
    let content_digest = NodeViewDigest::parse(identity.content_sha256.clone())?;
    if projection.digest() != content_digest {
        return Err(DesiredViewContractError::ContentDigestMismatch);
    }
    Ok(ValidatedNodeView {
        identity: AcceptedNodeViewIdentity::new(
            identity.site_id.clone(),
            identity.node_id.clone(),
            publication,
            identity.view_revision,
            content_digest,
        ),
        projection,
    })
}

/// Validates one atomic view acknowledgement without inspecting desired payload values.
pub(crate) fn validate_acknowledgement(
    acknowledgement: &NodeViewAcknowledgement,
) -> Result<(), DesiredViewContractError> {
    validate_view_reference(
        acknowledgement
            .view
            .as_ref()
            .ok_or(DesiredViewContractError::IdentityMismatch)?,
    )?;
    if acknowledgement.acknowledgement_sequence == 0 {
        return Err(DesiredViewContractError::InvalidSequence);
    }
    match acknowledgement.outcome.as_ref() {
        Some(node_view_acknowledgement::Outcome::Accepted(_)) => Ok(()),
        Some(node_view_acknowledgement::Outcome::Rejected(rejection)) => {
            match DesiredViewRejectionCode::try_from(rejection.code) {
                Ok(DesiredViewRejectionCode::IdentityMismatch)
                | Ok(DesiredViewRejectionCode::RequiredSectionInvalid)
                | Ok(DesiredViewRejectionCode::RequiredSectionUnsupported)
                | Ok(DesiredViewRejectionCode::SectionPayloadInvalid)
                | Ok(DesiredViewRejectionCode::ContentConflict)
                | Ok(DesiredViewRejectionCode::LimitExceeded) => Ok(()),
                _ => Err(DesiredViewContractError::UnsafeDiagnostic),
            }
        }
        None => Err(DesiredViewContractError::MissingAcknowledgementOutcome),
    }
}

/// Validates one complete typed controller status envelope and its model section without
/// accepting it into manager state. The optional application section is validated separately by
/// [`validate_application_status`], so its problems never reject model status.
pub fn validate_controller_status(
    status: &NodeControllerStatus,
) -> Result<&ModelControllerStatusSection, DesiredViewContractError> {
    if status.encoded_len() > MAX_CONTROLLER_STATUS_BYTES {
        return Err(DesiredViewContractError::LimitExceeded);
    }
    let view = status
        .view
        .as_ref()
        .ok_or(DesiredViewContractError::IdentityMismatch)?;
    validate_view_reference(view)?;
    if status.status_sequence == 0 {
        return Err(DesiredViewContractError::InvalidSequence);
    }
    let (model, _) = status_sections(status)?;
    let model = model.ok_or(DesiredViewContractError::InvalidStatusSection)?;
    validate_model_status(view, model)?;
    Ok(model)
}

/// Validates the optional application section of a status whose envelope passed
/// [`validate_controller_status`]; `None` means the node reported no application section.
pub(crate) fn validate_application_status(
    status: &NodeControllerStatus,
) -> Result<Option<&ApplicationControllerStatusSection>, DesiredViewContractError> {
    let view = status
        .view
        .as_ref()
        .ok_or(DesiredViewContractError::IdentityMismatch)?;
    let (_, application) = status_sections(status)?;
    if let Some(application) = application {
        validate_application_status_section(view, application)?;
    }
    Ok(application)
}

/// Validates per-entry cleanup dispositions against the exact status report that carried the
/// evidence. Every disposition must answer exactly one reported entry.
pub(crate) fn validate_status_receipts(
    status: &NodeControllerStatus,
    response: &NodeControllerStatusAcceptance,
) -> Result<(), DesiredViewContractError> {
    let model = validate_controller_status(status)?;
    if response.accepted_status_sequence != status.status_sequence {
        return Err(DesiredViewContractError::ReceiptMismatch);
    }
    let view = status
        .view
        .as_ref()
        .expect("status validation requires view");
    let evidence = model
        .cleanup_evidence
        .iter()
        .map(|evidence| (cleanup_key(evidence), evidence))
        .collect::<BTreeMap<_, _>>();
    let application_evidence = validate_application_status(status)?
        .map(|application| {
            application
                .cleanup_evidence
                .iter()
                .map(|evidence| {
                    (
                        (
                            evidence.application_deployment_id.clone(),
                            evidence.lifecycle_generation,
                        ),
                        evidence,
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut answered = BTreeSet::new();
    let mut answered_applications = BTreeSet::new();
    for receipt in &response.receipts {
        let receipt = match receipt.payload.as_ref() {
            Some(controller_status_receipt::Payload::Model(receipt)) => receipt,
            Some(controller_status_receipt::Payload::Application(receipt)) => {
                let key = (
                    receipt.application_deployment_id.clone(),
                    receipt.lifecycle_generation,
                );
                let matching = application_evidence
                    .get(&key)
                    .ok_or(DesiredViewContractError::ReceiptMismatch)?;
                if !answered_applications.insert(key)
                    || receipt.manager_session_id != view.manager_session_id
                    || receipt.node_incarnation_id != view.node_incarnation_id
                    || receipt.status_sequence != status.status_sequence
                    || receipt.evidence_sha256 != matching.evidence_sha256
                    || !matches!(
                        ApplicationCleanupDisposition::try_from(receipt.disposition),
                        Ok(ApplicationCleanupDisposition::Acknowledged
                            | ApplicationCleanupDisposition::Rejected)
                    )
                {
                    return Err(DesiredViewContractError::ReceiptMismatch);
                }
                continue;
            }
            None => return Err(DesiredViewContractError::ReceiptMismatch),
        };
        let key = (
            receipt.model_deployment_id.clone(),
            receipt.lifecycle_generation,
        );
        let matching = evidence
            .get(&key)
            .ok_or(DesiredViewContractError::ReceiptMismatch)?;
        if !answered.insert(key)
            || receipt.manager_session_id != view.manager_session_id
            || receipt.node_incarnation_id != view.node_incarnation_id
            || receipt.status_sequence != status.status_sequence
            || receipt.evidence_sha256 != matching.evidence_sha256
            || !matches!(
                ModelCleanupDisposition::try_from(receipt.disposition),
                Ok(ModelCleanupDisposition::Acknowledged | ModelCleanupDisposition::Rejected)
            )
        {
            return Err(DesiredViewContractError::ReceiptMismatch);
        }
    }
    Ok(())
}

/// Cuts a recorded status message to the contract size the manager accepts.
pub fn bounded_status_message(message: &str) -> String {
    truncated(message, MAX_STATUS_MESSAGE_BYTES).to_owned()
}

/// Cuts a recorded failure summary to the contract sizes the manager accepts: code and message
/// are truncated, and diagnostics are kept in order until the count or byte budget runs out, the
/// last kept one truncated to the remaining budget.
pub fn bounded_failure(failure: &ModelDeploymentFailure) -> ModelDeploymentFailure {
    let mut remaining = MAX_FAILURE_DIAGNOSTIC_BYTES;
    let diagnostics = failure
        .diagnostics
        .iter()
        .take(MAX_FAILURE_DIAGNOSTICS)
        .map_while(|diagnostic| {
            let kept = truncated(diagnostic, remaining);
            remaining -= kept.len();
            (!kept.is_empty() || diagnostic.is_empty()).then(|| kept.to_owned())
        })
        .collect();
    ModelDeploymentFailure {
        code: truncated(&failure.code, MAX_FAILURE_CODE_BYTES).to_owned(),
        message: truncated(&failure.message, MAX_FAILURE_MESSAGE_BYTES).to_owned(),
        diagnostics,
    }
}

/// Bounds an accelerator memory observation to what the manager accepts from `node_id`, so one
/// bad row never rejects the node's whole status. Usage rows on another node or with an empty or
/// oversized device ID are dropped and at most the device limit is kept; unavailable reasons are
/// cut to the failure code and message sizes, a reason without a code reported as `UNSPECIFIED`.
pub fn bounded_accelerator_memory(
    node_id: &str,
    memory: AcceleratorMemoryObservation,
) -> AcceleratorMemoryObservation {
    let bounded = |reason: AcceleratorMemoryUnavailable| AcceleratorMemoryUnavailable {
        code: match truncated(&reason.code, MAX_FAILURE_CODE_BYTES) {
            "" => UNSPECIFIED_UNAVAILABLE_CODE.to_owned(),
            code => code.to_owned(),
        },
        message: truncated(&reason.message, MAX_FAILURE_MESSAGE_BYTES).to_owned(),
    };
    let mut memory = memory;
    memory.result = match memory.result.take() {
        Some(accelerator_memory_observation::Result::Available(mut usage)) => {
            usage.devices.retain(|device| {
                device.managed_node_id == node_id
                    && !device.device_id.is_empty()
                    && device.device_id.len() <= MAX_ACCELERATOR_DEVICE_ID_BYTES
            });
            usage.devices.truncate(MAX_ACCELERATOR_MEMORY_DEVICES);
            Some(accelerator_memory_observation::Result::Available(usage))
        }
        Some(accelerator_memory_observation::Result::Unavailable(reason)) => Some(
            accelerator_memory_observation::Result::Unavailable(bounded(reason)),
        ),
        result => result,
    };
    if let Some(refresh) = memory.refresh.as_mut()
        && let Some(accelerator_memory_refresh_state::Outcome::Failed(reason)) =
            refresh.outcome.as_mut()
    {
        *reason = bounded(std::mem::take(reason));
    }
    memory
}

/// Bounds observed placements to what the manager accepts from `node_id`: placements on another
/// node are dropped, and so are empty accelerator device IDs.
pub fn bounded_observed_placements(
    node_id: &str,
    placements: &[ObservedPlacement],
) -> Vec<ObservedPlacement> {
    placements
        .iter()
        .filter(|placement| placement.managed_node_id == node_id)
        .map(|placement| ObservedPlacement {
            managed_node_id: placement.managed_node_id.clone(),
            accelerator_device_ids: placement
                .accelerator_device_ids
                .iter()
                .filter(|device_id| !device_id.is_empty())
                .cloned()
                .collect(),
        })
        .collect()
}

/// Projects node-retained cleanup evidence with its identity digest.
pub fn cleanup_evidence_from_state(evidence: &NodeModelCleanupEvidence) -> ModelCleanupEvidence {
    ModelCleanupEvidence {
        model_deployment_id: evidence.model_deployment_id.clone(),
        lifecycle_generation: evidence.lifecycle_generation,
        realization_generation: evidence.realization_generation,
        never_realized: evidence.never_realized,
        evidence_sha256: crate::store::cleanup_evidence_digest(
            &evidence.model_deployment_id,
            evidence.lifecycle_generation,
        ),
    }
}

/// Builds the manager disposition answering one reported cleanup evidence entry.
pub fn cleanup_receipt(
    manager_session: &ManagerSession,
    node_incarnation: &NodeIncarnation,
    status_sequence: u64,
    evidence: &ModelCleanupEvidence,
    disposition: ModelCleanupDisposition,
) -> ModelCleanupReceipt {
    ModelCleanupReceipt {
        manager_session_id: manager_session.as_str().to_owned(),
        node_incarnation_id: node_incarnation.as_str().to_owned(),
        model_deployment_id: evidence.model_deployment_id.clone(),
        lifecycle_generation: evidence.lifecycle_generation,
        status_sequence,
        evidence_sha256: evidence.evidence_sha256.clone(),
        disposition: disposition as i32,
    }
}

/// Builds application cleanup evidence for one withdrawn deployment generation with its identity
/// digest; evidence without a runtime launch generation reports that nothing was ever realized.
pub fn application_cleanup_evidence(
    application_deployment_id: &str,
    lifecycle_generation: u64,
    realization_generation: Option<u64>,
) -> ApplicationCleanupEvidence {
    ApplicationCleanupEvidence {
        application_deployment_id: application_deployment_id.to_owned(),
        lifecycle_generation,
        realization_generation,
        never_realized: realization_generation.is_none(),
        evidence_sha256: crate::store::application_cleanup_evidence_digest(
            application_deployment_id,
            lifecycle_generation,
        ),
    }
}

/// Builds the manager disposition answering one reported application cleanup evidence entry.
pub fn application_cleanup_receipt(
    manager_session: &ManagerSession,
    node_incarnation: &NodeIncarnation,
    status_sequence: u64,
    evidence: &ApplicationCleanupEvidence,
    disposition: ApplicationCleanupDisposition,
) -> ApplicationCleanupReceipt {
    ApplicationCleanupReceipt {
        manager_session_id: manager_session.as_str().to_owned(),
        node_incarnation_id: node_incarnation.as_str().to_owned(),
        application_deployment_id: evidence.application_deployment_id.clone(),
        lifecycle_generation: evidence.lifecycle_generation,
        status_sequence,
        evidence_sha256: evidence.evidence_sha256.clone(),
        disposition: disposition as i32,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DesiredViewContractError {
    #[error("desired-view identity or fencing metadata does not match")]
    IdentityMismatch,

    #[error("desired-view sequence must be nonzero")]
    InvalidSequence,

    #[error("desired-view required-section manifest is invalid")]
    InvalidRequiredSection,

    #[error("desired-view required section is unsupported")]
    UnsupportedRequiredSection,

    #[error("desired-view section payload is absent, duplicated, or mismatched")]
    InvalidSectionPayload,

    #[error("desired-view model assignment is invalid")]
    InvalidModelAssignment,

    #[error("controller status application section is invalid")]
    InvalidApplicationStatus,

    #[error("desired-view content digest does not match normalized content")]
    ContentDigestMismatch,

    #[error("desired-view acknowledgement has no atomic outcome")]
    MissingAcknowledgementOutcome,

    #[error("desired-view rejection diagnostic is not a known safe code")]
    UnsafeDiagnostic,

    #[error("controller status section is absent, duplicated, or invalid")]
    InvalidStatusSection,

    #[error("controller status cleanup receipt does not match accepted evidence")]
    ReceiptMismatch,

    #[error("private node control-plane message exceeds a protocol bound")]
    LimitExceeded,

    #[error(transparent)]
    ProcessIdentity(#[from] ProcessIdentityError),

    #[error(transparent)]
    NodeView(#[from] NodeViewError),
}

fn supported_sections(
    sections: &[SupportedDesiredViewSection],
) -> Result<BTreeMap<DesiredViewSectionKind, (u32, u32)>, DesiredViewContractError> {
    let mut supported = BTreeMap::new();
    for section in sections {
        let kind = DesiredViewSectionKind::try_from(section.kind)
            .map_err(|_| DesiredViewContractError::UnsupportedRequiredSection)?;
        if kind == DesiredViewSectionKind::Unspecified
            || section.minimum_schema_version == 0
            || section.minimum_schema_version > section.maximum_schema_version
            || supported
                .insert(
                    kind,
                    (
                        section.minimum_schema_version,
                        section.maximum_schema_version,
                    ),
                )
                .is_some()
        {
            return Err(DesiredViewContractError::InvalidRequiredSection);
        }
    }
    Ok(supported)
}

fn required_sections(
    view: &dotcore_proto::dotcore::site::private::v1::NodeDesiredView,
    supported: &BTreeMap<DesiredViewSectionKind, (u32, u32)>,
) -> Result<BTreeMap<DesiredViewSectionKind, u32>, DesiredViewContractError> {
    let mut required = BTreeMap::new();
    for section in &view.required_sections {
        let kind = DesiredViewSectionKind::try_from(section.kind)
            .map_err(|_| DesiredViewContractError::UnsupportedRequiredSection)?;
        let Some((minimum, maximum)) = supported.get(&kind) else {
            return Err(DesiredViewContractError::UnsupportedRequiredSection);
        };
        if kind == DesiredViewSectionKind::Unspecified
            || section.schema_version < *minimum
            || section.schema_version > *maximum
        {
            return Err(DesiredViewContractError::UnsupportedRequiredSection);
        }
        if required.insert(kind, section.schema_version).is_some() {
            return Err(DesiredViewContractError::InvalidRequiredSection);
        }
    }
    if required.get(&DesiredViewSectionKind::Model) != Some(&MODEL_SECTION_SCHEMA_VERSION) {
        return Err(DesiredViewContractError::InvalidRequiredSection);
    }
    Ok(required)
}

/// Typed owner payloads of one view, each present exactly once when its section is required.
struct OwnerSections<'a> {
    model: &'a ModelDesiredViewSection,
    application: Option<&'a ApplicationDesiredViewSection>,
}

fn owner_sections<'a>(
    view: &'a dotcore_proto::dotcore::site::private::v1::NodeDesiredView,
    required: &BTreeMap<DesiredViewSectionKind, u32>,
) -> Result<OwnerSections<'a>, DesiredViewContractError> {
    let mut model = None;
    let mut application = None;
    for section in &view.sections {
        let kind = DesiredViewSectionKind::try_from(section.kind)
            .map_err(|_| DesiredViewContractError::InvalidSectionPayload)?;
        let Some(required_version) = required.get(&kind) else {
            return Err(DesiredViewContractError::InvalidSectionPayload);
        };
        if section.schema_version != *required_version {
            return Err(DesiredViewContractError::InvalidSectionPayload);
        }
        match section.payload.as_ref() {
            Some(desired_view_section::Payload::Model(candidate))
                if kind == DesiredViewSectionKind::Model && model.is_none() =>
            {
                model = Some(candidate);
            }
            Some(desired_view_section::Payload::Application(candidate))
                if kind == DesiredViewSectionKind::Application && application.is_none() =>
            {
                application = Some(candidate);
            }
            _ => return Err(DesiredViewContractError::InvalidSectionPayload),
        }
    }
    if required.contains_key(&DesiredViewSectionKind::Application) != application.is_some() {
        return Err(DesiredViewContractError::InvalidSectionPayload);
    }
    Ok(OwnerSections {
        model: model.ok_or(DesiredViewContractError::InvalidSectionPayload)?,
        application,
    })
}

fn has_application_target(execution_targets: &[ExecutionTarget]) -> bool {
    execution_targets.iter().any(|target| {
        matches!(
            target.runtime_binding,
            Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
        )
    })
}

fn model_projection(
    node_id: &str,
    execution_targets: &[ExecutionTarget],
    model: &ModelDesiredViewSection,
) -> Result<NodeViewProjection, DesiredViewContractError> {
    if execution_targets.iter().any(|target| {
        target.site_node_id != node_id
            || target
                .platform
                .as_ref()
                .and_then(canonical_platform)
                .is_none_or(|platform| {
                    !binding_supports_platform(target.runtime_binding.as_ref(), &platform)
                })
            || !matches!(
                target.runtime_binding,
                Some(
                    execution_target::RuntimeBinding::ModelRuntime(_)
                        | execution_target::RuntimeBinding::ApplicationRuntime(_)
                )
            )
    }) {
        return Err(DesiredViewContractError::InvalidModelAssignment);
    }
    let model_target_ids = execution_targets
        .iter()
        .filter(|target| {
            matches!(
                target.runtime_binding,
                Some(execution_target::RuntimeBinding::ModelRuntime(_))
            )
        })
        .map(|target| target.execution_target_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut declarations = Vec::with_capacity(model.assignments.len());
    for assignment in &model.assignments {
        let declaration = assignment
            .declaration
            .as_ref()
            .ok_or(DesiredViewContractError::InvalidModelAssignment)?;
        let is_local = declaration
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
            .is_some_and(|source| matches!(source, model_deployment_snapshot::Source::Local(_)));
        if !is_local
            || declaration.upstream_desired_configuration.is_some()
            || declaration.lifecycle_generation == 0
            || declaration.placement.as_ref().is_none_or(|placement| {
                placement.site_node_id != node_id
                    || !model_target_ids.contains(placement.execution_target_id.as_str())
            })
        {
            return Err(DesiredViewContractError::InvalidModelAssignment);
        }
        declarations.push(declaration.clone());
    }
    let deployment_ids = declarations
        .iter()
        .map(|declaration| declaration.model_deployment_id.as_str())
        .collect::<BTreeSet<_>>();
    if execution_targets.iter().any(|target| {
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) =
            target.runtime_binding.as_ref()
        else {
            return false;
        };
        matches!(
            binding.adapter.as_ref(),
            Some(model_runtime_binding::Adapter::SimulatedRuntime(simulated))
                if simulated.deployment_scenarios.iter().any(|scenario| {
                    !deployment_ids.contains(scenario.model_deployment_id.as_str())
                })
        )
    }) {
        return Err(DesiredViewContractError::InvalidModelAssignment);
    }
    NodeViewProjection::new(execution_targets.to_vec(), declarations).map_err(Into::into)
}

fn validate_view_reference(reference: &NodeViewReference) -> Result<(), DesiredViewContractError> {
    if !valid_id(&reference.site_id)
        || !valid_id(&reference.node_id)
        || reference.view_revision == 0
    {
        return Err(DesiredViewContractError::IdentityMismatch);
    }
    ManagerSession::parse(reference.manager_session_id.clone())?;
    NodeIncarnation::parse(reference.node_incarnation_id.clone())?;
    NodeViewDigest::parse(reference.content_sha256.clone())?;
    Ok(())
}

fn validate_model_status(
    view: &NodeViewReference,
    model: &ModelControllerStatusSection,
) -> Result<(), DesiredViewContractError> {
    if model.deployments.len() > MAX_MODEL_ASSIGNMENTS
        || model.cleanup_evidence.len() > MAX_CLEANUP_EVIDENCE
    {
        return Err(DesiredViewContractError::LimitExceeded);
    }
    let mut deployments = BTreeSet::new();
    for deployment in &model.deployments {
        validate_deployment_status(view, deployment)?;
        if !deployments.insert(deployment.model_deployment_id.as_str()) {
            return Err(DesiredViewContractError::InvalidStatusSection);
        }
    }
    let mut cleanup = BTreeSet::new();
    for evidence in &model.cleanup_evidence {
        if !valid_id(&evidence.model_deployment_id)
            || evidence.lifecycle_generation == 0
            || evidence.evidence_sha256
                != crate::store::cleanup_evidence_digest(
                    &evidence.model_deployment_id,
                    evidence.lifecycle_generation,
                )
            || (evidence.never_realized && evidence.realization_generation.is_some())
            || !cleanup.insert(cleanup_key(evidence))
        {
            return Err(DesiredViewContractError::InvalidStatusSection);
        }
    }
    Ok(())
}

fn validate_deployment_status(
    view: &NodeViewReference,
    deployment: &ModelDeploymentStatus,
) -> Result<(), DesiredViewContractError> {
    let lifecycle = ModelLifecycleObservation::try_from(deployment.lifecycle)
        .map_err(|_| DesiredViewContractError::InvalidStatusSection)?;
    let readiness = ModelBindingReadiness::try_from(deployment.binding_readiness)
        .map_err(|_| DesiredViewContractError::InvalidStatusSection)?;
    if !valid_id(&deployment.model_deployment_id)
        || deployment.lifecycle_generation == 0
        || (deployment.state_revision == 0 && lifecycle != ModelLifecycleObservation::Reconciling)
        || lifecycle == ModelLifecycleObservation::Unspecified
        || readiness == ModelBindingReadiness::Unspecified
        || deployment.status_message.len() > MAX_STATUS_MESSAGE_BYTES
        || (lifecycle == ModelLifecycleObservation::Failed) != deployment.failure.is_some()
        || deployment
            .failure
            .as_ref()
            .is_some_and(|failure| !valid_failure(failure))
        || !valid_binding_evidence(readiness, deployment)
        || !valid_progress(lifecycle, deployment)
        || deployment
            .placement
            .as_ref()
            .is_none_or(|placement| placement.site_node_id != view.node_id)
        || deployment.observed_placements.iter().any(|placement| {
            placement.managed_node_id != view.node_id
                || placement
                    .accelerator_device_ids
                    .iter()
                    .any(|device_id| device_id.is_empty())
        })
        || deployment
            .accelerator_memory
            .as_ref()
            .is_none_or(|memory| !valid_accelerator_memory(&view.node_id, memory))
    {
        return Err(DesiredViewContractError::InvalidStatusSection);
    }
    Ok(())
}

/// A failure summary fits the sizes its hosting node bounds it to.
fn valid_failure(failure: &ModelDeploymentFailure) -> bool {
    failure.code.len() <= MAX_FAILURE_CODE_BYTES
        && failure.message.len() <= MAX_FAILURE_MESSAGE_BYTES
        && failure.diagnostics.len() <= MAX_FAILURE_DIAGNOSTICS
        && failure.diagnostics.iter().map(String::len).sum::<usize>()
            <= MAX_FAILURE_DIAGNOSTIC_BYTES
}

/// Reported accelerator memory has a result, lists usage only on the reporting node within the
/// device limits, and carries unavailable reasons within the failure code and message sizes.
fn valid_accelerator_memory(node_id: &str, memory: &AcceleratorMemoryObservation) -> bool {
    let valid_reason = |reason: &AcceleratorMemoryUnavailable| {
        !reason.code.is_empty()
            && reason.code.len() <= MAX_FAILURE_CODE_BYTES
            && reason.message.len() <= MAX_FAILURE_MESSAGE_BYTES
    };
    let valid_result = match memory.result.as_ref() {
        Some(accelerator_memory_observation::Result::Available(usage)) => {
            usage.devices.len() <= MAX_ACCELERATOR_MEMORY_DEVICES
                && usage.devices.iter().all(|device| {
                    device.managed_node_id == node_id
                        && !device.device_id.is_empty()
                        && device.device_id.len() <= MAX_ACCELERATOR_DEVICE_ID_BYTES
                })
        }
        Some(accelerator_memory_observation::Result::Unavailable(reason)) => valid_reason(reason),
        Some(accelerator_memory_observation::Result::NotApplicable(_)) => true,
        None => false,
    };
    let valid_refresh = memory
        .refresh
        .as_ref()
        .and_then(|refresh| refresh.outcome.as_ref())
        .is_none_or(|outcome| match outcome {
            accelerator_memory_refresh_state::Outcome::Failed(reason) => valid_reason(reason),
            _ => true,
        });
    valid_result && valid_refresh
}

/// A Ready binding names itself with a 64-character lowercase hex token and its runtime
/// generation; any other readiness carries no token.
fn valid_binding_evidence(
    readiness: ModelBindingReadiness,
    deployment: &ModelDeploymentStatus,
) -> bool {
    if readiness != ModelBindingReadiness::Ready {
        return deployment.binding_token.is_empty();
    }
    deployment.realization_generation.is_some()
        && deployment.binding_token.len() == 64
        && deployment
            .binding_token
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Byte progress belongs only to a Downloading status, completed never exceeding a non-zero
/// total.
fn valid_progress(
    lifecycle: ModelLifecycleObservation,
    deployment: &ModelDeploymentStatus,
) -> bool {
    deployment
        .preparation_progress
        .as_ref()
        .is_none_or(|progress| {
            lifecycle == ModelLifecycleObservation::Downloading
                && progress
                    .total_bytes
                    .is_none_or(|total| total != 0 && progress.completed_bytes <= total)
        })
}

/// Splits a status envelope into its owner sections; a model section is required by the caller
/// and each owner may report at most one section.
fn status_sections(
    status: &NodeControllerStatus,
) -> Result<
    (
        Option<&ModelControllerStatusSection>,
        Option<&ApplicationControllerStatusSection>,
    ),
    DesiredViewContractError,
> {
    let mut model = None;
    let mut application = None;
    for section in &status.sections {
        match section.payload.as_ref() {
            Some(controller_status_section::Payload::Model(candidate)) if model.is_none() => {
                model = Some(candidate);
            }
            Some(controller_status_section::Payload::Application(candidate))
                if application.is_none() =>
            {
                application = Some(candidate);
            }
            _ => return Err(DesiredViewContractError::InvalidStatusSection),
        }
    }
    Ok((model, application))
}

fn validate_application_status_section(
    view: &NodeViewReference,
    application: &ApplicationControllerStatusSection,
) -> Result<(), DesiredViewContractError> {
    if application.deployments.len() > MAX_APPLICATION_ASSIGNMENTS
        || application.cleanup_evidence.len() > MAX_CLEANUP_EVIDENCE
    {
        return Err(DesiredViewContractError::LimitExceeded);
    }
    let mut deployments = BTreeSet::new();
    for deployment in &application.deployments {
        validate_application_deployment_status(view, deployment)?;
        if !deployments.insert(deployment.application_deployment_id.as_str()) {
            return Err(DesiredViewContractError::InvalidApplicationStatus);
        }
    }
    let mut cleanup = BTreeSet::new();
    for evidence in &application.cleanup_evidence {
        if !valid_id(&evidence.application_deployment_id)
            || evidence.lifecycle_generation == 0
            || evidence.evidence_sha256
                != crate::store::application_cleanup_evidence_digest(
                    &evidence.application_deployment_id,
                    evidence.lifecycle_generation,
                )
            || (evidence.never_realized && evidence.realization_generation.is_some())
            || !cleanup.insert((
                evidence.application_deployment_id.as_str(),
                evidence.lifecycle_generation,
            ))
        {
            return Err(DesiredViewContractError::InvalidApplicationStatus);
        }
    }
    Ok(())
}

fn validate_application_deployment_status(
    view: &NodeViewReference,
    deployment: &ApplicationDeploymentStatus,
) -> Result<(), DesiredViewContractError> {
    let invalid = || DesiredViewContractError::InvalidApplicationStatus;
    let lifecycle =
        ApplicationLifecycleObservation::try_from(deployment.lifecycle).map_err(|_| invalid())?;
    ApplicationStatusStage::try_from(deployment.stage).map_err(|_| invalid())?;
    let failure_code =
        ApplicationStatusFailureCode::try_from(deployment.failure_code).map_err(|_| invalid())?;
    // Without a retained record a status either reconciles or reports an assignment the node
    // cannot realize, which is only a typed failure.
    if !valid_id(&deployment.application_deployment_id)
        || deployment.lifecycle_generation == 0
        || lifecycle == ApplicationLifecycleObservation::Unspecified
        || (deployment.state_revision == 0
            && lifecycle != ApplicationLifecycleObservation::Reconciling
            && failure_code == ApplicationStatusFailureCode::Unspecified)
        || deployment
            .placement
            .as_ref()
            .is_none_or(|placement| placement.site_node_id != view.node_id)
        || deployment.endpoint.as_ref().is_some_and(|endpoint| {
            endpoint.host.parse::<std::net::Ipv4Addr>().is_err()
                || endpoint.port == 0
                || endpoint.port > u32::from(u16::MAX)
        })
        || !valid_service_accelerator_memory(&view.node_id, deployment)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Each unit's reported accelerator memory names a distinct service and passes the same checks
/// as a model deployment's.
fn valid_service_accelerator_memory(
    node_id: &str,
    deployment: &ApplicationDeploymentStatus,
) -> bool {
    let mut services = BTreeSet::new();
    deployment.service_accelerator_memory.iter().all(|unit| {
        !unit.service_name.is_empty()
            && services.insert(unit.service_name.as_str())
            && unit
                .accelerator_memory
                .as_ref()
                .is_some_and(|memory| valid_accelerator_memory(node_id, memory))
    })
}

fn cleanup_key(evidence: &ModelCleanupEvidence) -> (String, u64) {
    (
        evidence.model_deployment_id.clone(),
        evidence.lifecycle_generation,
    )
}

/// Longest prefix of `text` within `limit` bytes that ends on a character boundary.
fn truncated(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests;
