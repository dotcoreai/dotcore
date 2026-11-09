//! Resolves one local catalog selection into an immutable launch snapshot.
//!
//! Resolution is pure and controller-owned. It overlays generic values, dispatches
//! engine-owned realization and translation, then returns one self-contained snapshot.

use dotcore_artifact::{ArtifactManager, LocalImageIdentityResolutionError};
use dotcore_catalog::{
    ACCELERATOR_MEMORY_RESOURCE, Catalog, CatalogError, CatalogModelId, HOST_MEMORY_RESOURCE,
    ScalarResourceEstimate, SupportedHardwareClass, options::resolve_model_option_values,
    overlay_parameter_defaults, shared_parameters, validate_parameter_values,
    validate_supplied_parameter_values,
};
use dotcore_domain::deployment::artifact::immutable_remote_launch_identity;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, EngineParameterSchema, EngineParameterValue, EngineParameterValues,
        EngineRelease, Model, ModelArtifact, ModelArtifactBinding, ModelEngineConfiguration,
        ModelOptionValues, ModelProfile, NixArtifact, engine_artifact::Kind as EngineArtifactKind,
        engine_parameter_definition::ParameterType as EngineParameterType,
        engine_parameter_value::Value as EngineParameterValueKind,
        model_option_parameter_value::Value as OptionParameterValueKind,
    },
    deployment::v1::ArtifactPreparationPolicy,
    model::v1::{
        HardwareClassResourceEstimate, LocalModelDeploymentConfiguration, ModelDeploymentSnapshot,
        ResolvedEngineArtifact, ResolvedHardwareSupport, ResolvedLocalModelDeploymentSnapshot,
        ResolvedModelOption, ResolvedSelectedTarget, ResolvedSizing,
        ScalarResourceEstimate as ProtoScalarResourceEstimate, UpstreamModelDeploymentSnapshot,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
};
use dotcore_site::node::target_inventory::TargetCapabilityInventory;

use crate::{
    hardware::selection::{
        HardwareClassSelectionError, SelectedTargetCapabilities, TargetCapabilitySelectionError,
        select_hardware_class, select_target_capabilities,
    },
    model::{
        engine::{EngineIntegrationRegistry, engine_artifact_kinds},
        resolved::ResolvedHardwareRequirementsValue,
        selection::{
            EngineArtifactSelectionError, MatchedEngineTarget, SelectedEngineArtifact,
            select_approved_engine_artifact,
        },
    },
};

/// Generic catalog selection and normalized values resolved before engine realization.
///
/// Engine parameter values are complete: supplied values plus those derived from catalog
/// sizing. Estimates are evaluated over the complete values with the binding's artifacts.
/// Only resource estimates keep values a launch would reject, with the reason beside them.
pub(crate) struct ResolvedLocalModelValues {
    pub(crate) selection: LocalModelSelection,
    pub(crate) engine_parameter_values: EngineParameterValues,
    pub(crate) derived_engine_parameter_values: EngineParameterValues,
    pub(crate) engine_parameter_schema: EngineParameterSchema,
    pub(crate) engine_parameter_schema_id: String,
    pub(crate) option_values: Vec<ModelOptionValues>,
    pub(crate) estimates: Vec<ScalarResourceEstimate>,
    pub(crate) launch_rejection: Option<ModelLaunchResolutionError>,
}

impl ResolvedLocalModelValues {
    /// These values, or the reason a launch rejects them.
    fn into_launchable(mut self) -> Result<Self, ModelLaunchResolutionError> {
        match self.launch_rejection.take() {
            Some(rejection) => Err(rejection),
            None => Ok(self),
        }
    }

    /// Engine-neutral sizing retained by snapshots, absent when sizing produced nothing.
    pub(crate) fn resolved_sizing(&self) -> Option<ResolvedSizing> {
        let estimate = |resource: &str| {
            self.estimates
                .iter()
                .find(|estimate| estimate.resource == resource)
                .map(|estimate| estimate.value)
        };
        let accelerator_memory_bytes = estimate(ACCELERATOR_MEMORY_RESOURCE);
        let host_memory_bytes = estimate(HOST_MEMORY_RESOURCE);
        let derived_engine_parameter_ids = self
            .derived_engine_parameter_values
            .values
            .iter()
            .map(|value| value.parameter_id.clone())
            .collect::<Vec<_>>();
        (accelerator_memory_bytes.is_some()
            || host_memory_bytes.is_some()
            || !derived_engine_parameter_ids.is_empty())
        .then_some(ResolvedSizing {
            accelerator_memory_bytes,
            derived_engine_parameter_ids,
            host_memory_bytes,
        })
    }

    /// Advisory estimate of these values on their hardware class, with the derived engine
    /// parameter values; `include_estimates` false leaves the resource estimates out, for
    /// configurations whose footprint they can't predict.
    pub(crate) fn into_hardware_class_estimate(
        self,
        include_estimates: bool,
    ) -> HardwareClassResourceEstimate {
        HardwareClassResourceEstimate {
            hardware_class_id: self
                .selection
                .hardware_support
                .hardware_class
                .hardware_class_id,
            estimates: if include_estimates {
                self.estimates
                    .into_iter()
                    .map(|estimate| ProtoScalarResourceEstimate {
                        resource: estimate.resource,
                        value: estimate.value,
                        unit: estimate.unit,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            derived_engine_parameter_values: Some(self.derived_engine_parameter_values),
            launch_rejection: self
                .launch_rejection
                .as_ref()
                .map(ModelLaunchResolutionError::operator_reason),
        }
    }
}

/// Catalog declarations of one local selection with the one approved engine artifact and the
/// most specific supported hardware class chosen for the target. Without a target, it holds the
/// first approved artifact and one of the configuration's supported classes. The engine release
/// is the one declaring the chosen artifact.
pub(crate) struct LocalModelSelection {
    pub(crate) model: Model,
    pub(crate) profile: ModelProfile,
    pub(crate) configuration: ModelEngineConfiguration,
    pub(crate) binding: ModelArtifactBinding,
    pub(crate) engine_release: EngineRelease,
    pub(crate) engine_artifact: EngineArtifact,
    /// Concrete target facts the engine artifact matched; absent when no target is known.
    pub(crate) engine_target: Option<MatchedEngineTarget>,
    /// Supported hardware class whose sizing and requirements apply.
    pub(crate) hardware_support: SupportedHardwareClass,
    pub(crate) model_artifact: ModelArtifact,
}

/// Resolves one local catalog selection into its immutable portable launch snapshot.
///
/// `engine_identity` is the local image ID a caller resolved for a selected tag-only image, which
/// may instead be left to the hosting SiteNode. It must name the selected image and is rejected
/// for a digest-pinned image or a Nix artifact.
pub fn resolve_local_launch(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    target_inventory: &TargetCapabilityInventory,
    engine_identity: Option<&EngineLaunchIdentity>,
) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
    let selected_target = select_target_capabilities(target, target_inventory)
        .map_err(ModelLaunchResolutionError::TargetCapability)?;
    resolve_local_launch_on_selected_target(
        catalog,
        model_id,
        profile_id,
        configuration,
        target,
        &selected_target,
        engine_identity,
    )
}

/// Resolves one local catalog selection into its launch snapshot like [`resolve_local_launch`],
/// on target capabilities the caller already selected.
pub(crate) fn resolve_local_launch_on_selected_target(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    selected_target: &SelectedTargetCapabilities,
    engine_identity: Option<&EngineLaunchIdentity>,
) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
    let resolved = resolve_local_values_for_target(
        catalog,
        model_id,
        profile_id,
        configuration,
        selected_target,
    )?;
    let sizing = resolved.resolved_sizing();
    let selection = resolved.selection;
    let parameter_values = resolved.engine_parameter_values;
    let engine_parameter_schema_id = resolved.engine_parameter_schema_id;
    let engine_parameter_schema = resolved.engine_parameter_schema;
    let option_values = resolved.option_values;
    let engine_target =
        selection
            .engine_target
            .ok_or(ModelLaunchResolutionError::InvalidConfiguration(
                "selected engine artifact matched no target",
            ))?;
    validate_target_accelerator_devices(
        target,
        &ResolvedHardwareRequirementsValue::single_device(
            engine_target.accelerator.catalog_name(),
            selection.hardware_support.hardware_requirements.as_ref(),
        ),
    )?;
    let (engine_preparation_policy, engine_launch_identity, engine_artifact) =
        engine_launch_identity(selection.engine_artifact, engine_identity)?;
    let realized = crate::model::engine::realize_model(
        &selection.engine_release,
        &selection.model,
        &selection.configuration,
        &selection.binding,
        &option_values,
    )
    .map_err(ModelLaunchResolutionError::EngineRealization)?;
    let auxiliary_artifacts = realized
        .auxiliary_artifact_ids
        .iter()
        .map(|id| {
            selection
                .model
                .auxiliary_artifacts
                .iter()
                .find(|artifact| &artifact.auxiliary_artifact_id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("selected auxiliary artifact is stale"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(ModelLaunchResolutionError::EngineRealization)?;

    let mut local = ResolvedLocalModelDeploymentSnapshot {
        model_id: selection.model.model_id,
        model_display_name: selection.model.display_name,
        model_version: selection.model.version,
        configuration_id: selection.configuration.configuration_id,
        profile_id: selection.profile.profile_id,
        engine_artifact: Some(ResolvedEngineArtifact {
            engine_release_id: selection.engine_release.engine_release_id,
            engine_kind: selection.engine_release.engine_kind,
            engine_version: selection.engine_release.version,
            artifact: Some(engine_artifact),
            platform: Some(engine_target.platform.to_oci_platform()),
            accelerator: engine_target.accelerator.catalog_name().to_owned(),
            minimum_nvidia_driver_major_version: engine_target.minimum_nvidia_driver_major_version,
            preparation_policy: engine_preparation_policy as i32,
            launch_identity: engine_launch_identity,
            storage_bindings: selection.engine_release.storage_bindings,
        }),
        model_artifact: Some(selection.model_artifact),
        auxiliary_artifacts,
        hardware_support: Some(ResolvedHardwareSupport {
            hardware_class_id: selection.hardware_support.hardware_class.hardware_class_id,
            selector: selection.hardware_support.hardware_class.selector,
            hardware_requirements: selection.hardware_support.hardware_requirements,
        }),
        resolved_engine_state: Some(realized.state),
        execution_capabilities: selection.configuration.execution_capabilities,
        input_capabilities: Some(realized.input_capabilities),
        engine_parameter_values: Some(parameter_values),
        engine_parameter_schema_id,
        engine_parameter_schema: Some(engine_parameter_schema),
        desired_launch_plan: None,
        custom_argument_tokens: configuration.custom_argument_tokens.clone(),
        resolved_options: option_values
            .into_iter()
            .map(|values| ResolvedModelOption {
                values: Some(values),
            })
            .collect(),
        selected_target: Some(ResolvedSelectedTarget {
            accelerator_memory_architecture: selected_target.memory_architecture.to_proto() as i32,
            accelerator_memory_pool_total_bytes: selected_target.memory_pool_total_bytes(),
        }),
        sizing,
        request_kinds: selection.configuration.request_kinds,
    };
    local.desired_launch_plan = Some(
        EngineIntegrationRegistry::production()
            .translate(&local)
            .map_err(|error| ModelLaunchResolutionError::LaunchPlan(anyhow::Error::new(error)))?,
    );
    crate::model::resolved::validate_local_snapshot_with(
        &local,
        dotcore_domain::deployment::artifact::LaunchIdentityResolution::NodeResolved,
    )
    .map_err(ModelLaunchResolutionError::CatalogResolution)?;
    Ok(local)
}

/// Rejects a target whose accelerator devices don't number exactly the devices the launch's
/// synthesized hardware requirements use, so admission refuses it instead of the workload failing
/// at realization.
fn validate_target_accelerator_devices(
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    hardware: &ResolvedHardwareRequirementsValue,
) -> Result<(), ModelLaunchResolutionError> {
    let required = hardware.accelerator_device_count();
    let device_count = target.accelerator_device_ids.len();
    if device_count == required as usize {
        Ok(())
    } else {
        Err(ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
            execution_target_id: target.execution_target_id.clone(),
            device_count,
            required,
        })
    }
}

/// Selects the engine artifact one local catalog selection would launch on the target, without
/// resolving values or building a snapshot.
pub fn select_engine_artifact(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    target_inventory: &TargetCapabilityInventory,
) -> Result<EngineArtifactSelection, ModelLaunchResolutionError> {
    let target = select_target_capabilities(target, target_inventory)
        .map_err(ModelLaunchResolutionError::TargetCapability)?;
    let selection = select_local_model(
        catalog,
        model_id,
        profile_id,
        &configuration.model_artifact_binding_id,
        &target,
    )?;
    match selection.engine_artifact.kind.as_ref() {
        Some(EngineArtifactKind::Oci(_)) => {
            let (reference, immutable_identity) =
                engine_image_reference(&selection.engine_artifact)?;
            Ok(EngineArtifactSelection::OciImage {
                reference,
                tag_only: immutable_identity.is_none(),
            })
        }
        Some(EngineArtifactKind::Nix(_)) => Ok(EngineArtifactSelection::Nix(
            NixEngineSelection::from_selected(&selection.engine_artifact)?,
        )),
        None => Err(ModelLaunchResolutionError::InvalidConfiguration(
            "selected engine artifact has no OCI or Nix source",
        )),
    }
}

/// Catalog engine artifact selected for one local deployment, before its launch identity is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineArtifactSelection {
    /// OCI image with its rendered catalog reference. A mutable local-development tag launches
    /// only from an image ID resolved on the node that runs it.
    OciImage { reference: String, tag_only: bool },

    /// Nix flake package, built for the hosting node's own Nix system.
    Nix(NixEngineSelection),
}

impl EngineArtifactSelection {
    /// Rendered catalog reference of the selected artifact, for diagnostics.
    pub fn reference(&self) -> &str {
        match self {
            Self::OciImage { reference, .. } => reference,
            Self::Nix(nix) => &nix.flake,
        }
    }

    /// Resolves the launch identity the selected image needs before resolution for the development
    /// workload facade: the current local image ID for a tag-only reference, and none for a
    /// digest-pinned one or a Nix artifact, whose store path the facade pins after resolution.
    pub(crate) async fn resolve_launch_identity(
        &self,
        artifacts: &ArtifactManager,
    ) -> Result<Option<EngineLaunchIdentity>, EngineLaunchIdentityError> {
        match self {
            Self::OciImage {
                tag_only: false, ..
            } => Ok(None),
            Self::OciImage { reference, .. } => {
                let image_id = artifacts
                    .resolve_local_image_identity(reference.clone())
                    .await
                    .map_err(EngineLaunchIdentityError::LocalImage)?;
                Ok(Some(EngineLaunchIdentity {
                    reference: reference.clone(),
                    image_id,
                }))
            }
            Self::Nix(_) => Ok(None),
        }
    }
}

/// Nix engine artifact selected for one target, built for the hosting node's own Nix system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NixEngineSelection {
    /// Catalog flake reference, for example `self`.
    pub flake: String,

    /// Package attribute path the hosting node builds for its own Nix system.
    pub package: String,
}

impl NixEngineSelection {
    fn from_selected(artifact: &EngineArtifact) -> Result<Self, ModelLaunchResolutionError> {
        let nix = dotcore_domain::nix::engine_artifact_nix(artifact).ok_or(
            ModelLaunchResolutionError::InvalidConfiguration(
                "selected engine artifact has no Nix source",
            ),
        )?;
        Ok(Self {
            flake: nix.flake.clone(),
            package: nix.package.clone().ok_or(
                ModelLaunchResolutionError::InvalidConfiguration(
                    "selected Nix engine artifact names no package",
                ),
            )?,
        })
    }
}

/// Local image identity a caller resolved for a tag-only engine image before resolution, as the
/// development workload facade does on the node that launches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineLaunchIdentity {
    /// Rendered catalog reference the image ID was resolved from.
    pub reference: String,

    /// Docker image ID, `sha256:` followed by 64 lowercase hex digits.
    pub image_id: String,
}

/// Returns the preparation policy, immutable launch identity, and retained form of the selected
/// engine artifact, from its OCI or Nix source. A Nix artifact is retained whole, with the catalog
/// store paths a locked one carries, and leaves its identity to the hosting SiteNode.
fn engine_launch_identity(
    artifact: EngineArtifact,
    identity: Option<&EngineLaunchIdentity>,
) -> Result<(ArtifactPreparationPolicy, String, EngineArtifact), ModelLaunchResolutionError> {
    let (policy, launch_identity) = match artifact.kind.as_ref() {
        Some(EngineArtifactKind::Nix(nix)) => nix_launch_identity(nix, identity)?,
        _ => oci_launch_identity(&artifact, identity)?,
    };
    Ok((policy, launch_identity, artifact))
}

/// Returns the preparation policy of a selected Nix engine artifact and its empty launch identity.
/// The hosting SiteNode pins the store path of its own Nix system: from the catalog store paths a
/// locked artifact retains, or by evaluating a development-source (`self`) package. Neither takes a
/// caller-resolved identity.
fn nix_launch_identity(
    nix: &NixArtifact,
    identity: Option<&EngineLaunchIdentity>,
) -> Result<(ArtifactPreparationPolicy, String), ModelLaunchResolutionError> {
    if identity.is_some() {
        return Err(ModelLaunchResolutionError::EngineIdentityUnexpected {
            reference: nix.flake.clone(),
        });
    }
    let policy = dotcore_domain::nix::NixFlakeReference::parse(&nix.flake)
        .map_err(|error| {
            ModelLaunchResolutionError::CatalogResolution(CatalogError::Invalid(
                anyhow::Error::new(error),
            ))
        })?
        .preparation_policy();
    Ok((policy, String::new()))
}

/// Returns the preparation policy and launch identity of a selected OCI engine image.
fn oci_launch_identity(
    artifact: &EngineArtifact,
    identity: Option<&EngineLaunchIdentity>,
) -> Result<(ArtifactPreparationPolicy, String), ModelLaunchResolutionError> {
    let (reference, immutable_identity) = engine_image_reference(artifact)?;
    match (immutable_identity, identity) {
        (Some(launch_identity), None) => {
            Ok((ArtifactPreparationPolicy::ImmutableRemote, launch_identity))
        }
        (Some(_), Some(_)) => {
            Err(ModelLaunchResolutionError::EngineIdentityUnexpected { reference })
        }
        // The SiteNode named by the exact placement resolves it once.
        (None, None) => Ok((ArtifactPreparationPolicy::LocalDevelopment, String::new())),
        (None, Some(identity)) if identity.reference != reference => {
            Err(ModelLaunchResolutionError::EngineImageIdentityMismatch {
                reference,
                resolved_reference: identity.reference.clone(),
            })
        }
        (None, Some(identity)) => Ok((
            ArtifactPreparationPolicy::LocalDevelopment,
            identity.image_id.clone(),
        )),
    }
}

/// Returns the rendered catalog reference of an engine artifact and, when it is digest-pinned,
/// its canonical digest reference.
fn engine_image_reference(
    artifact: &EngineArtifact,
) -> Result<(String, Option<String>), ModelLaunchResolutionError> {
    let reference = dotcore_domain::oci::engine_artifact_reference(artifact).ok_or(
        ModelLaunchResolutionError::InvalidConfiguration(
            "selected engine artifact has no OCI reference",
        ),
    )?;
    let invalid_reference = |error| {
        ModelLaunchResolutionError::CatalogResolution(CatalogError::Invalid(anyhow::Error::new(
            error,
        )))
    };
    Ok((
        dotcore_domain::oci::render_reference(reference).map_err(invalid_reference)?,
        immutable_remote_launch_identity(reference).map_err(invalid_reference)?,
    ))
}

/// Evaluates generic selection and values without a target, once per hardware class the profile's
/// configuration supports and in declaration order, without invoking engine semantics or building
/// a snapshot. Each result takes the first approved engine artifact, whose release shares the
/// parameter schema of every approved release, and keeps the estimates of values a launch would
/// reject, with the reason.
pub(crate) fn estimate_local_values_per_class(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
) -> Result<Vec<ResolvedLocalModelValues>, ModelLaunchResolutionError> {
    select_local_model_per_class(
        catalog,
        model_id,
        profile_id,
        &configuration.model_artifact_binding_id,
    )?
    .into_iter()
    .map(|selection| evaluate_selected_values(catalog, profile_id, configuration, selection))
    .collect()
}

/// Outcome of resolving one selection on one supported hardware class, keyed by the class id.
pub(crate) type HardwareClassResolution = (
    String,
    Result<ResolvedLocalModelValues, ModelLaunchResolutionError>,
);

/// Resolves generic selection and values without a target like
/// [`estimate_local_values_per_class`], but rejects values a launch can't start and keeps each
/// supported hardware class's own outcome next to its identifier, so one class failing doesn't
/// hide the others. Fails only when the selection itself can't be read.
pub(crate) fn resolve_local_values_each_class(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
) -> Result<Vec<HardwareClassResolution>, ModelLaunchResolutionError> {
    Ok(select_local_model_per_class(
        catalog,
        model_id,
        profile_id,
        &configuration.model_artifact_binding_id,
    )?
    .into_iter()
    .map(|selection| {
        let hardware_class_id = selection
            .hardware_support
            .hardware_class
            .hardware_class_id
            .clone();
        (
            hardware_class_id,
            resolve_selected_values(catalog, profile_id, configuration, selection),
        )
    })
    .collect())
}

/// Resolves generic selection and values for one configured execution target, whose observed
/// facts pick the engine artifact and the hardware class, without building a snapshot.
pub(crate) fn resolve_local_values_on_target(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    target_inventory: &TargetCapabilityInventory,
) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
    let selected_target = select_target_capabilities(target, target_inventory)
        .map_err(ModelLaunchResolutionError::TargetCapability)?;
    resolve_local_values_for_target(
        catalog,
        model_id,
        profile_id,
        configuration,
        &selected_target,
    )
}

/// Evaluates values for resource estimates on target capabilities the caller already selected,
/// like [`resolve_local_values_for_target`], keeping the estimates of values a launch would
/// reject, with the reason.
pub(crate) fn estimate_local_values_on_target(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &SelectedTargetCapabilities,
) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
    let selection = select_local_model(
        catalog,
        model_id,
        profile_id,
        &configuration.model_artifact_binding_id,
        target,
    )?;
    evaluate_selected_values(catalog, profile_id, configuration, selection)
}

/// Completes a local configuration with the selected profile's catalog defaults before
/// resolution. An empty binding ID takes the profile's default binding. Engine parameter values
/// merge per parameter, for an absent or supplied set alike: a supplied value wins, then the
/// profile default, then the engine default; parameters with none stay out for resolution to fill
/// or derive from sizing.
pub(crate) fn apply_profile_defaults(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
) -> Result<LocalModelDeploymentConfiguration, ModelLaunchResolutionError> {
    let mut configuration = configuration.clone();
    let (default_binding_id, default_parameter_values) = catalog
        .local_defaults_for(model_id, profile_id)
        .ok_or_else(|| ModelLaunchResolutionError::ProfileDefaultsUnavailable {
            model_id: model_id.0.clone(),
            profile_id: profile_id.to_owned(),
        })?;
    if configuration.model_artifact_binding_id.is_empty() {
        configuration.model_artifact_binding_id = default_binding_id;
    }
    configuration.engine_parameter_values = Some(overlay_parameter_defaults(
        &configuration.engine_parameter_values.unwrap_or_default(),
        &default_parameter_values,
    ));
    Ok(configuration)
}

/// Resolves generic selection and values on target capabilities the caller already selected,
/// whose facts pick the engine artifact and the hardware class, without building a snapshot.
pub(crate) fn resolve_local_values_for_target(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    target: &SelectedTargetCapabilities,
) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
    let selection = select_local_model(
        catalog,
        model_id,
        profile_id,
        &configuration.model_artifact_binding_id,
        target,
    )?;
    resolve_selected_values(catalog, profile_id, configuration, selection)
}

/// Evaluates the selection's values and rejects those a launch can't start.
fn resolve_selected_values(
    catalog: &Catalog,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    selection: LocalModelSelection,
) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
    evaluate_selected_values(catalog, profile_id, configuration, selection)?.into_launchable()
}

/// Validates the configuration's engine parameter values against the selection's release and
/// evaluates the sizing of its hardware class: derived values, then estimates. Values each in
/// range that a launch rejects together still get estimates, with the rejection kept beside them.
fn evaluate_selected_values(
    catalog: &Catalog,
    profile_id: &str,
    configuration: &LocalModelDeploymentConfiguration,
    selection: LocalModelSelection,
) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
    let engine_parameter_values = configuration.engine_parameter_values.as_ref().ok_or(
        ModelLaunchResolutionError::InvalidConfiguration(
            "local deployment configuration has no engine parameter values",
        ),
    )?;
    let schema = selection.engine_release.parameter_schema.clone().ok_or(
        ModelLaunchResolutionError::InvalidConfiguration(
            "selected engine release has no parameter schema",
        ),
    )?;
    let mut parameter_errors = Vec::new();
    validate_supplied_parameter_values(
        engine_parameter_values,
        &selection.engine_release,
        profile_id,
        &mut parameter_errors,
    );
    if let Some(error) = parameter_errors.into_iter().next() {
        return Err(ModelLaunchResolutionError::InvalidParameterValues(error));
    }

    let option_values = resolve_model_option_values(
        &selection.model.options,
        &selection.configuration,
        &selection.profile,
        &configuration.option_overrides,
    )
    .map_err(ModelLaunchResolutionError::InvalidOptionValues)?;
    let sizing_unavailable =
        |source: anyhow::Error| ModelLaunchResolutionError::SizingUnavailable {
            binding_id: selection.binding.binding_id.clone(),
            options: describe_options(&option_values),
            source,
        };

    let hardware_class_id = selection
        .hardware_support
        .hardware_class
        .hardware_class_id
        .clone();
    // The filled context per request counts as derived, and sizing may read it.
    let filled_context_per_request = shared_parameters::filled_context_per_request(
        &schema,
        engine_parameter_values,
        shared_parameters::maximum_context_tokens(&selection.model, &selection.configuration),
    );
    let mut sizing_inputs = engine_parameter_values.clone();
    sizing_inputs
        .values
        .extend(filled_context_per_request.iter().cloned());
    let mut derived_values = catalog
        .derive_local_parameters(
            &selection.model.model_id,
            &selection.configuration.configuration_id,
            &hardware_class_id,
            &selection.binding.binding_id,
            &sizing_inputs,
            &option_values,
        )
        .map_err(|error| sizing_unavailable(anyhow::Error::new(error)))?
        .into_iter()
        .map(|(parameter_id, value)| EngineParameterValue {
            parameter_id,
            value: Some(EngineParameterValueKind::UnsignedInteger(value)),
        })
        .chain(filled_context_per_request)
        .collect::<Vec<_>>();
    derived_values.sort_by(|left, right| left.parameter_id.cmp(&right.parameter_id));
    let derived_engine_parameter_values = EngineParameterValues {
        values: derived_values,
    };
    validate_derived_ranges(&derived_engine_parameter_values, &schema)?;
    let mut complete_values = engine_parameter_values.clone();
    complete_values
        .values
        .extend(derived_engine_parameter_values.values.iter().cloned());
    let mut parameter_errors = Vec::new();
    validate_parameter_values(
        &complete_values,
        &selection.engine_release,
        profile_id,
        &mut parameter_errors,
    );
    if let Some(error) = parameter_errors.into_iter().next() {
        return Err(ModelLaunchResolutionError::InvalidParameterValues(error));
    }
    let launch_rejection = validate_launch_values(&selection, &complete_values).err();

    let estimates = catalog
        .evaluate_local_sizing_inputs(
            &selection.model.model_id,
            &selection.configuration.configuration_id,
            &hardware_class_id,
            &selection.binding.binding_id,
            Some(selection.model_artifact.artifact_id.as_str()),
            &selection.binding.auxiliary_artifact_ids,
            &complete_values,
            &option_values,
        )
        .map_err(|error| sizing_unavailable(anyhow::Error::new(error)))
        .and_then(|estimates| {
            let requires_estimate = crate::model::engine::declaration_validator()
                .launch_requires_sizing(&selection.engine_release);
            if requires_estimate
                && !estimates
                    .iter()
                    .any(|estimate| estimate.resource == ACCELERATOR_MEMORY_RESOURCE)
            {
                return Err(sizing_unavailable(anyhow::anyhow!(
                    "the engine requires a {ACCELERATOR_MEMORY_RESOURCE:?} estimate, but none applies"
                )));
            }
            Ok(estimates)
        });
    // Values a launch rejects may have no sizing at all; their rejection says more.
    let (estimates, launch_rejection) = match (estimates, launch_rejection) {
        (Ok(estimates), launch_rejection) => (estimates, launch_rejection),
        (Err(_), Some(rejection)) => return Err(rejection),
        (Err(error), None) => return Err(error),
    };
    let engine_parameter_schema_id = schema.schema_id.clone();

    Ok(ResolvedLocalModelValues {
        selection,
        engine_parameter_values: complete_values,
        derived_engine_parameter_values,
        engine_parameter_schema_id,
        engine_parameter_schema: schema,
        option_values,
        estimates,
        launch_rejection,
    })
}

/// Reads one local selection's catalog declarations and chooses its approved engine artifact and
/// most specific supported hardware class for the target.
fn select_local_model(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    binding_id: &str,
    target: &SelectedTargetCapabilities,
) -> Result<LocalModelSelection, ModelLaunchResolutionError> {
    let declarations = catalog
        .local_model_declarations(model_id, profile_id, binding_id)
        .map_err(ModelLaunchResolutionError::CatalogResolution)?;
    let mut approved = declarations.approved_engine_artifacts;
    let SelectedEngineArtifact {
        position,
        artifact: engine_artifact,
        target: engine_target,
    } = select_approved_engine_artifact(
        approved.iter().map(|approved| approved.artifact.clone()),
        target,
        engine_artifact_kinds(declarations.engine_kind),
    )
    .map_err(ModelLaunchResolutionError::EngineArtifactSelection)?;
    // The selected artifact pins its own release, since approved artifacts may come from several
    // releases of the engine.
    let engine_release = approved.swap_remove(position).engine_release;
    let hardware_support =
        select_hardware_class(declarations.hardware_support, target).map_err(|source| {
            ModelLaunchResolutionError::HardwareClassSelection {
                model_id: model_id.0.clone(),
                profile_id: profile_id.to_owned(),
                source: Box::new(source),
            }
        })?;

    Ok(LocalModelSelection {
        model: declarations.model,
        profile: declarations.profile,
        configuration: declarations.configuration,
        binding: declarations.binding,
        engine_release,
        engine_artifact,
        engine_target: Some(engine_target),
        hardware_support,
        model_artifact: declarations.model_artifact,
    })
}

/// Reads one local selection's catalog declarations without a target: one selection per supported
/// hardware class in declaration order, each with the first approved engine artifact.
fn select_local_model_per_class(
    catalog: &Catalog,
    model_id: &CatalogModelId,
    profile_id: &str,
    binding_id: &str,
) -> Result<Vec<LocalModelSelection>, ModelLaunchResolutionError> {
    let declarations = catalog
        .local_model_declarations(model_id, profile_id, binding_id)
        .map_err(ModelLaunchResolutionError::CatalogResolution)?;
    let first = declarations
        .approved_engine_artifacts
        .into_iter()
        .next()
        .ok_or(ModelLaunchResolutionError::InvalidConfiguration(
            "selected engine configuration has no approved engine artifacts",
        ))?;

    Ok(declarations
        .hardware_support
        .into_iter()
        .map(|hardware_support| LocalModelSelection {
            model: declarations.model.clone(),
            profile: declarations.profile.clone(),
            configuration: declarations.configuration.clone(),
            binding: declarations.binding.clone(),
            engine_release: first.engine_release.clone(),
            engine_artifact: first.artifact.clone(),
            engine_target: None,
            hardware_support,
            model_artifact: declarations.model_artifact.clone(),
        })
        .collect())
}

/// Captures one upstream template in an immutable controller-owned deployment snapshot.
pub fn resolve_upstream_snapshot(
    catalog: &Catalog,
    upstream_model_template_id: &str,
) -> Option<ModelDeploymentSnapshot> {
    let template = catalog.upstream_model_template(upstream_model_template_id)?;
    Some(ModelDeploymentSnapshot {
        source: Some(ModelDeploymentSnapshotSource::Upstream(
            UpstreamModelDeploymentSnapshot {
                upstream_model_template: Some(template),
            },
        )),
    })
}

/// Failures while resolving the launch identity of a selected engine artifact for the
/// development workload facade.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineLaunchIdentityError {
    #[error(transparent)]
    LocalImage(LocalImageIdentityResolutionError),
}

/// Failures while producing one immutable local launch snapshot.
#[derive(Debug, thiserror::Error)]
pub enum ModelLaunchResolutionError {
    #[error("target capability selection failed: {0}")]
    TargetCapability(#[source] TargetCapabilitySelectionError),

    #[error("approved engine artifact selection failed: {0}")]
    EngineArtifactSelection(#[source] EngineArtifactSelectionError),

    #[error(
        "profile {profile_id:?} of catalog model {model_id:?} cannot run on the target: {source}"
    )]
    HardwareClassSelection {
        model_id: String,
        profile_id: String,
        #[source]
        source: Box<HardwareClassSelectionError>,
    },

    #[error(
        "execution target {execution_target_id:?} has {device_count} accelerator devices, but the profile's launch uses {required}"
    )]
    AcceleratorDeviceCountMismatch {
        execution_target_id: String,
        device_count: usize,
        required: u32,
    },

    #[error("could not resolve catalog model: {0}")]
    CatalogResolution(#[source] CatalogError),

    #[error("local deployment configuration is invalid: {0}")]
    InvalidConfiguration(&'static str),

    #[error(
        "Catalog model {model_id:?} profile {profile_id:?} declares no defaults for the omitted engine configuration; supply the model artifact binding and engine parameter values"
    )]
    ProfileDefaultsUnavailable {
        model_id: String,
        profile_id: String,
    },

    #[error("could not realize local engine state: {0}")]
    EngineRealization(#[source] anyhow::Error),

    /// The configuration's engine parameter values break a rule of the engine release or the
    /// engine; the operator fixes it by changing the values.
    #[error("invalid engine parameter values: {0}")]
    InvalidParameterValues(#[source] anyhow::Error),

    /// The configuration's model option overrides don't fit the model's options; the operator
    /// fixes it by changing the overrides.
    #[error("invalid model option values: {0}")]
    InvalidOptionValues(#[source] anyhow::Error),

    #[error("could not translate local model launch plan: {0}")]
    LaunchPlan(#[source] anyhow::Error),

    #[error(
        "Engine artifact {reference:?} is digest-pinned or a Nix artifact and takes no resolved launch identity"
    )]
    EngineIdentityUnexpected { reference: String },

    #[error(
        "Engine image {reference:?} was selected, but the local image identity was resolved for {resolved_reference:?}"
    )]
    EngineImageIdentityMismatch {
        reference: String,
        resolved_reference: String,
    },

    #[error(
        "Catalog sizing is unavailable for binding {binding_id:?} with options {options}: {source}"
    )]
    SizingUnavailable {
        binding_id: String,
        options: String,
        #[source]
        source: anyhow::Error,
    },

    #[error(
        "derived engine parameter {parameter_id:?} value {value} is outside its range {minimum}..={maximum}; adjust the parameters it derives from or supply it explicitly"
    )]
    DerivedParameterOutOfRange {
        parameter_id: String,
        value: u64,
        minimum: u64,
        maximum: u64,
    },

    #[error(
        "engine parameter values give each request {context_tokens} tokens of context, above the maximum context of {maximum_context_tokens} tokens of configuration {configuration_id:?} of catalog model {model_id:?}; reduce the context per request"
    )]
    ContextAboveMaximum {
        model_id: String,
        configuration_id: String,
        context_tokens: u64,
        maximum_context_tokens: u32,
    },
}

impl ModelLaunchResolutionError {
    /// Why a launch rejects the values, in terms of the values an operator can change.
    fn operator_reason(&self) -> String {
        match self {
            Self::InvalidParameterValues(source) => source.to_string(),
            Self::ContextAboveMaximum {
                context_tokens,
                maximum_context_tokens,
                ..
            } => format!(
                "context per request of {context_tokens} tokens is above the model's maximum context of {maximum_context_tokens} tokens; lower the context per request"
            ),
            error => error.to_string(),
        }
    }
}

/// Rejects complete engine parameter values, each in range, that a launch can't start together.
fn validate_launch_values(
    selection: &LocalModelSelection,
    values: &EngineParameterValues,
) -> Result<(), ModelLaunchResolutionError> {
    validate_context_per_request(selection, values)?;
    crate::model::engine::validate_parameter_values(
        selection.engine_release.engine_kind,
        &selection.model,
        &selection.configuration,
        values,
    )
    .map_err(ModelLaunchResolutionError::InvalidParameterValues)
}

/// Rejects engine parameter values whose context per request is above the configuration's
/// maximum context: the model's maximum context, or the configuration's lower maximum.
fn validate_context_per_request(
    selection: &LocalModelSelection,
    values: &EngineParameterValues,
) -> Result<(), ModelLaunchResolutionError> {
    let Some(context_tokens) = crate::model::engine::unsigned_parameter_value(
        values,
        shared_parameters::CONTEXT_PER_REQUEST_PARAMETER_ID,
    ) else {
        return Ok(());
    };
    let maximum_context_tokens =
        shared_parameters::maximum_context_tokens(&selection.model, &selection.configuration);
    if context_tokens <= u64::from(maximum_context_tokens) {
        return Ok(());
    }
    Err(ModelLaunchResolutionError::ContextAboveMaximum {
        model_id: selection.model.model_id.clone(),
        configuration_id: selection.configuration.configuration_id.clone(),
        context_tokens,
        maximum_context_tokens,
    })
}

/// Rejects derived values outside their unsigned schema range with the offending value.
fn validate_derived_ranges(
    derived: &EngineParameterValues,
    schema: &EngineParameterSchema,
) -> Result<(), ModelLaunchResolutionError> {
    for value in &derived.values {
        let Some(EngineParameterValueKind::UnsignedInteger(derived_value)) = value.value else {
            continue;
        };
        let Some(EngineParameterType::UnsignedInteger(range)) = schema
            .parameters
            .iter()
            .find(|definition| definition.parameter_id == value.parameter_id)
            .and_then(|definition| definition.parameter_type.as_ref())
        else {
            continue;
        };
        if !(range.minimum..=range.maximum).contains(&derived_value) {
            return Err(ModelLaunchResolutionError::DerivedParameterOutOfRange {
                parameter_id: value.parameter_id.clone(),
                value: derived_value,
                minimum: range.minimum,
                maximum: range.maximum,
            });
        }
    }
    Ok(())
}

/// Renders option values as `option.parameter=value` pairs for sizing diagnostics.
fn describe_options(option_values: &[ModelOptionValues]) -> String {
    let pairs = option_values
        .iter()
        .flat_map(|option| {
            option.values.iter().map(move |parameter| {
                let value = match parameter.value.as_ref() {
                    Some(OptionParameterValueKind::Boolean(value)) => value.to_string(),
                    Some(OptionParameterValueKind::UnsignedInteger(value)) => value.to_string(),
                    Some(OptionParameterValueKind::String(value)) => value.clone(),
                    None => "unset".to_owned(),
                };
                format!("{}.{}={value}", option.option_id, parameter.parameter_id)
            })
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        "none".to_owned()
    } else {
        pairs.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::timing::Timing;
    use std::{fs, path::Path};

    use dotcore_catalog::test_support::UNPARAMETERIZED_BINDING_SIZING;
    use dotcore_catalog::{
        ACCELERATOR_MEMORY_RESOURCE, Catalog, CatalogModelId, OciArtifactPolicy,
        shared_parameters::{
            CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
            TOTAL_CONTEXT_PARAMETER_ID,
        },
    };
    use dotcore_domain::test_support::nix;
    use dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value as LaunchArgumentValue;
    use dotcore_proto::dotcore::site::v1::{
        ModelRuntimeBinding, execution_target, model_runtime_binding,
    };
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterValue, EngineParameterValues, HardwareRequirements,
            ModelOptionParameterValue, ModelOptionValues, NixSystemStorePath, Selector,
            StorageBinding, StorageEnvironmentVariable, StorageKind, StorageScope,
            engine_parameter_value, model_option_parameter_value,
        },
        engine::{
            llama_cpp::v1::LlamaCppResolvedModelState, v1::resolved_engine_model_state::State,
        },
        model::v1::{LocalModelDeploymentConfiguration, ResolvedHardwareSupport, ResolvedSizing},
    };
    use prost::Message;

    use super::{
        ArtifactPreparationPolicy, EngineArtifact, EngineArtifactKind, EngineArtifactSelection,
        EngineLaunchIdentity, ModelLaunchResolutionError, NixArtifact,
        ResolvedHardwareRequirementsValue, ResolvedLocalModelDeploymentSnapshot,
        ResolvedLocalModelValues, apply_profile_defaults, engine_launch_identity,
        estimate_local_values_per_class, resolve_local_launch, select_engine_artifact,
        validate_target_accelerator_devices,
    };
    use crate::model::engine::declaration_validator;
    use crate::test_support::TAG_ONLY_ENGINE_REFERENCE;

    #[test]
    fn resolved_snapshot_retains_the_selected_target_and_the_declared_request_kinds() {
        let catalog = fixture_catalog(&fixture_catalog_root());
        let (target, inventory) = crate::test_support::execution_target_capabilities_with_memory(
            dotcore_domain::ExecutionPlatform::linux_arm64(),
            dotcore_domain::MemoryArchitecture::Unified,
        );
        let snapshot = resolve_local_launch(
            &catalog,
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "fixture-artifact".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(30_000)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                custom_argument_tokens: Vec::new(),
                option_overrides: Vec::new(),
            },
            &target,
            &inventory,
            None,
        )
        .expect("unified snapshot");

        assert_eq!(
            snapshot
                .selected_target
                .expect("selected target")
                .accelerator_memory_architecture(),
            dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Unified
        );
        assert_eq!(
            snapshot.request_kinds,
            [dotcore_proto::dotcore::catalog::v1::InferenceRequestKind::Chat as i32]
        );
    }

    #[test]
    fn resolved_snapshot_is_the_same_whatever_memory_is_free() {
        let catalog = fixture_catalog(&fixture_catalog_root());
        let (target, _) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        );
        let resolve = |free_bytes: u64| {
            let inventory =
                dotcore_site::node::target_inventory::TargetCapabilityReconciler::reconcile(
                    [(
                        target.execution_target_id.as_str(),
                        target.site_node_id.as_str(),
                    )],
                    [
                        dotcore_site::node::target_inventory::RuntimePlatformObservation::available(
                            &target.execution_target_id,
                            dotcore_domain::ExecutionPlatform::linux_arm64(),
                        ),
                    ],
                    [
                        dotcore_site::node::target_inventory::NodeAcceleratorObservation::nvidia(
                            &target.site_node_id,
                            "580.173.02",
                        )
                        .with_device_memory(
                            "GPU-0",
                            Some(34_190_917_632),
                            Some(34_190_917_632 - free_bytes),
                        ),
                    ],
                    Timing::test().node_control.capability_max_age,
                    std::time::SystemTime::now(),
                );
            resolve_local_launch(
                &catalog,
                &CatalogModelId("fixture-model".to_owned()),
                "fixture-profile",
                &LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "fixture-artifact".to_owned(),
                    engine_parameter_values: Some(EngineParameterValues {
                        values: [prompt_cache_value(30_000)]
                            .into_iter()
                            .chain(context_and_request_values())
                            .collect(),
                    }),
                    custom_argument_tokens: Vec::new(),
                    option_overrides: Vec::new(),
                },
                &target,
                &inventory,
                None,
            )
            .expect("snapshot")
        };

        assert_eq!(resolve(1_000_000_000), resolve(30_000_000_000));
    }

    #[test]
    fn portable_launch_inputs_use_the_same_engine_validation_on_amd64_and_arm64() {
        let catalog = fixture_catalog(&fixture_catalog_root());
        let secret = "synthetic-required-launch-secret";
        for platform in [
            dotcore_domain::ExecutionPlatform::linux_amd64(),
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        ] {
            let (target, inventory) = crate::test_support::execution_target_capabilities(platform);
            let snapshot = resolve_local_launch(
                &catalog,
                &CatalogModelId("fixture-model".to_owned()),
                "fixture-profile",
                &LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "fixture-artifact".to_owned(),
                    engine_parameter_values: Some(EngineParameterValues {
                        values: [prompt_cache_value(30_000)]
                            .into_iter()
                            .chain(context_and_request_values())
                            .collect(),
                    }),
                    custom_argument_tokens: vec![
                        "--synthetic-secret".to_owned(),
                        secret.to_owned(),
                    ],
                    option_overrides: Vec::new(),
                },
                &target,
                &inventory,
                None,
            )
            .expect("manager admission");

            crate::model::resolved::validate_local_snapshot(&snapshot)
                .expect("worker retained-input validation");
            let encoded = snapshot.encode_to_vec();
            let round_trip =
                dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot::decode(
                    encoded.as_slice(),
                )
                .expect("portable snapshot round trip");
            assert_eq!(round_trip, snapshot);
            let worker = crate::model::resolved::ResolvedModelLaunch::from_snapshot(
                &dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot {
                    source: Some(
                        dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                            round_trip,
                        ),
                    ),
                },
            )
            .expect("worker realization input");
            assert_eq!(
                snapshot.custom_argument_tokens,
                ["--synthetic-secret", secret]
            );
            assert!(
                encoded
                    .windows(secret.len())
                    .any(|bytes| bytes == secret.as_bytes())
            );
            assert_eq!(
                worker.artifact,
                dotcore_artifact::ModelArtifactSource::HuggingFaceFile {
                    repository: "dotcore/fixture".to_owned(),
                    revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    path: "fixture-Q8_0.gguf".to_owned(),
                    provenance_digest: "0".repeat(64),
                }
            );
        }
    }

    #[test]
    fn credential_free_snapshot_artifact_round_trips_for_worker_realization() {
        let catalog = split_snapshot_catalog();
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        );
        let snapshot = resolve_local_launch(
            &catalog,
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "fixture-artifact".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(30_000)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
        .expect("snapshot artifact admission");

        let encoded = snapshot.encode_to_vec();
        let round_trip =
            dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot::decode(
                encoded.as_slice(),
            )
            .expect("portable snapshot round trip");
        assert_eq!(round_trip, snapshot);
        let worker = crate::model::resolved::ResolvedModelLaunch::from_snapshot(
            &dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot {
                source: Some(
                    dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
                        round_trip,
                    ),
                ),
            },
        )
        .expect("worker realization input");
        let dotcore_artifact::ModelArtifactSource::HuggingFaceSnapshot {
            repository,
            revision,
            files,
        } = worker.artifact
        else {
            panic!("snapshot artifact source")
        };
        assert_eq!(repository, "dotcore/fixture");
        assert_eq!(revision, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(files.len(), 3);
        assert_eq!(
            files.get("fixture-Q8_0-00002-of-00003.gguf"),
            Some(&"2".repeat(64))
        );
        assert!(files.keys().all(|path| !Path::new(path).is_absolute()));
    }

    #[test]
    fn omitted_parameters_are_derived_retained_and_sized() {
        let catalog = derived_sizing_catalog("4096");
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let configuration = LocalModelDeploymentConfiguration {
            model_artifact_binding_id: "sized".to_owned(),
            engine_parameter_values: Some(EngineParameterValues {
                values: context_and_request_values(),
            }),
            custom_argument_tokens: vec!["--verbose".to_owned()],
            option_overrides: Vec::new(),
        };

        let snapshot = resolve_local_launch(
            &catalog,
            &derived_model_id(),
            "balanced",
            &configuration,
            &target,
            &inventory,
            None,
        )
        .expect("derived snapshot");
        let values =
            resolve_local_values(&catalog, &derived_model_id(), "balanced", &configuration)
                .expect("derived values");

        let expected_values = EngineParameterValues {
            values: context_and_request_values()
                .into_iter()
                .chain([filled_context_per_request(), prompt_cache_value(4096)])
                .collect(),
        };
        assert_eq!(
            snapshot.engine_parameter_values,
            Some(expected_values.clone())
        );
        assert_eq!(
            snapshot.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(100 + 3 * 4096),
                derived_engine_parameter_ids: vec![
                    "context-per-request".to_owned(),
                    "prompt-cache-memory-mib".to_owned(),
                ],
                host_memory_bytes: None,
            })
        );
        assert_eq!(values.engine_parameter_values, expected_values);
        assert_eq!(
            values.derived_engine_parameter_values,
            EngineParameterValues {
                values: vec![filled_context_per_request(), prompt_cache_value(4096)],
            }
        );
        assert_eq!(values.resolved_sizing(), snapshot.sizing);
    }

    #[test]
    fn the_snapshot_retains_the_matched_hardware_class_and_launches_on_one_device() {
        let catalog = hardware_class_catalog(
            r#"        hardwareRequirements:
          minimumMemoryBytesPerDevice: "8000000000"
          minimumSystemMemoryBytes: "16000000000"
        bindingSizing:
          - bindingId: "sized"
          - bindingId: "second"
      - hardwareClassId: "apple"
        bindingSizing:
          - bindingId: "sized"
          - bindingId: "second"
"#,
        );
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );

        let snapshot = resolve_local_launch(
            &catalog,
            &derived_model_id(),
            "balanced",
            &context_and_request_configuration(),
            &target,
            &inventory,
            None,
        )
        .expect("snapshot on the nvidia class");
        let launch = ResolvedHardwareRequirementsValue::single_device(
            "nvidia",
            Some(&HardwareRequirements {
                minimum_memory_bytes_per_device: 8_000_000_000,
                minimum_system_memory_bytes: 16_000_000_000,
            }),
        );

        assert_eq!(
            snapshot.hardware_support,
            Some(ResolvedHardwareSupport {
                hardware_class_id: "nvidia".to_owned(),
                selector: Some(Selector {
                    platform: None,
                    accelerators: vec!["nvidia".to_owned()],
                    minimum_nvidia_driver_major_version: 0,
                    ..Default::default()
                }),
                hardware_requirements: Some(HardwareRequirements {
                    minimum_memory_bytes_per_device: 8_000_000_000,
                    minimum_system_memory_bytes: 16_000_000_000,
                }),
            })
        );
        assert_eq!(
            snapshot.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(100 + 3 * 4096),
                derived_engine_parameter_ids: vec![
                    "context-per-request".to_owned(),
                    "prompt-cache-memory-mib".to_owned(),
                ],
                host_memory_bytes: None,
            })
        );
        assert_eq!(
            crate::model::resolved::ResolvedModelLaunch::from_local_snapshot(&snapshot)
                .expect("launch inputs")
                .hardware,
            launch
        );
        assert_eq!(
            launch.accelerator_requirements,
            [
                crate::model::resolved::ResolvedAcceleratorRequirementValue {
                    vendor: "nvidia".to_owned(),
                    minimum_count: 1,
                    minimum_memory_bytes_per_device: 8_000_000_000,
                }
            ]
        );
    }

    #[test]
    fn a_target_matching_no_supported_hardware_class_cannot_run_the_profile() {
        let catalog = hardware_class_catalog(UNPARAMETERIZED_BINDING_SIZING);
        let unified_only =
            hardware_class_catalog_with_class("nvidia-unified", UNPARAMETERIZED_BINDING_SIZING);
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let resolve = |catalog: &Catalog| {
            resolve_local_launch(
                catalog,
                &derived_model_id(),
                "balanced",
                &context_and_request_configuration(),
                &target,
                &inventory,
                None,
            )
        };

        resolve(&catalog).expect("the nvidia class matches");
        let error =
            resolve(&unified_only).expect_err("a dedicated-memory target matches no unified class");

        assert!(matches!(
            error,
            ModelLaunchResolutionError::HardwareClassSelection { .. }
        ));
        assert_eq!(
            error.to_string(),
            "profile \"balanced\" of catalog model \"fixture-model\" cannot run on the target: no supported hardware class matches target platform linux/amd64 with nvidia accelerators and dedicated memory"
        );
    }

    #[test]
    fn target_independent_resolution_evaluates_every_supported_hardware_class_in_order() {
        let two_classes = hardware_class_catalog(
            r#"        bindingSizing:
          - bindingId: "sized"
          - bindingId: "second"
      - hardwareClassId: "apple"
        bindingSizing:
          - bindingId: "sized"
          - bindingId: "second"
"#,
        );

        let per_class = estimate_local_values_per_class(
            &two_classes,
            &derived_model_id(),
            "balanced",
            &context_and_request_configuration(),
        )
        .expect("values per class")
        .into_iter()
        .map(|values| {
            (
                values
                    .selection
                    .hardware_support
                    .hardware_class
                    .hardware_class_id,
                values.derived_engine_parameter_values.values,
                values
                    .estimates
                    .into_iter()
                    .map(|estimate| (estimate.resource, estimate.value))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();

        let expected = |class: &str| {
            (
                class.to_owned(),
                vec![filled_context_per_request(), prompt_cache_value(4096)],
                vec![(ACCELERATOR_MEMORY_RESOURCE.to_owned(), 100 + 3 * 4096)],
            )
        };
        assert_eq!(per_class, vec![expected("nvidia"), expected("apple")]);
    }

    #[test]
    fn resolved_sizing_retains_both_memory_estimates() {
        let sizing = resolved_sizing_for(
            r#"        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + 1"
        hostMemory:
          unit: bytes
          equation: "${baseBytes} * 2"
"#,
        );

        assert_eq!(
            sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(101),
                derived_engine_parameter_ids: vec![
                    "context-per-request".to_owned(),
                    "prompt-cache-memory-mib".to_owned(),
                ],
                host_memory_bytes: Some(200),
            })
        );
    }

    #[test]
    fn resolved_sizing_without_host_estimate_leaves_it_absent() {
        let sizing = resolved_sizing_for(
            r#"        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + 1"
"#,
        );

        assert_eq!(
            sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(101),
                derived_engine_parameter_ids: vec![
                    "context-per-request".to_owned(),
                    "prompt-cache-memory-mib".to_owned(),
                ],
                host_memory_bytes: None,
            })
        );
    }

    #[test]
    fn resolved_sizing_with_only_host_estimate_retains_host_value() {
        let sizing = resolved_sizing_for(
            r#"        hostMemory:
          unit: bytes
          equation: "${baseBytes} * 2"
"#,
        );

        assert_eq!(
            sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: None,
                derived_engine_parameter_ids: vec![
                    "context-per-request".to_owned(),
                    "prompt-cache-memory-mib".to_owned(),
                ],
                host_memory_bytes: Some(200),
            })
        );
    }

    #[test]
    fn supplied_values_take_precedence_over_derivations() {
        let catalog = derived_sizing_catalog("4096");
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );

        let snapshot = resolve_local_launch(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "sized".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(30_000)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                ..Default::default()
            },
            &target,
            &inventory,
            None,
        )
        .expect("supplied snapshot");

        assert_eq!(
            snapshot.engine_parameter_values,
            Some(EngineParameterValues {
                values: [prompt_cache_value(30_000)]
                    .into_iter()
                    .chain(context_and_request_values())
                    .chain([filled_context_per_request()])
                    .collect(),
            })
        );
        assert_eq!(
            snapshot.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(100 + 3 * 30_000),
                derived_engine_parameter_ids: vec!["context-per-request".to_owned()],
                host_memory_bytes: None,
            })
        );
    }

    #[test]
    fn derivation_failures_are_actionable() {
        for (derivation, expected) in [
            (
                "20000000",
                "derived engine parameter \"prompt-cache-memory-mib\" value 20000000 is outside its range 0..=65536; adjust the parameters it derives from or supply it explicitly",
            ),
            (
                "18446744073709551615 * 2",
                "Catalog sizing is unavailable for binding \"sized\" with options none: Catalog sizing arithmetic failed: sizing expression at byte 21: final result exceeds u64",
            ),
        ] {
            let error = resolve_local_values(
                &derived_sizing_catalog(derivation),
                &derived_model_id(),
                "balanced",
                &LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "sized".to_owned(),
                    engine_parameter_values: Some(EngineParameterValues {
                        values: context_and_request_values(),
                    }),
                    ..Default::default()
                },
            )
            .err()
            .expect("derivation must fail");

            assert_eq!(error.to_string(), expected, "{derivation}");
        }
    }

    #[test]
    fn omitted_engine_configuration_takes_the_profile_defaults_exactly() {
        let catalog = profile_defaults_catalog();
        let configuration = LocalModelDeploymentConfiguration {
            custom_argument_tokens: vec!["--verbose".to_owned()],
            ..Default::default()
        };

        let completed =
            apply_profile_defaults(&catalog, &derived_model_id(), "balanced", &configuration)
                .expect("profile defaults");
        let values = resolve_local_values(&catalog, &derived_model_id(), "balanced", &completed)
            .expect("default values");

        assert_eq!(
            completed,
            LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "sized".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(30_000)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                custom_argument_tokens: vec!["--verbose".to_owned()],
                option_overrides: Vec::new(),
            }
        );
        assert_eq!(
            values.engine_parameter_values,
            EngineParameterValues {
                values: [prompt_cache_value(30_000)]
                    .into_iter()
                    .chain(context_and_request_values())
                    .chain([filled_context_per_request()])
                    .collect(),
            }
        );
        assert_eq!(
            values.derived_engine_parameter_values,
            EngineParameterValues {
                values: vec![filled_context_per_request()],
            }
        );
    }

    #[test]
    fn supplied_values_overlay_the_profile_defaults_per_parameter() {
        let catalog = profile_defaults_catalog();
        let completed = |configuration: LocalModelDeploymentConfiguration| {
            apply_profile_defaults(&catalog, &derived_model_id(), "balanced", &configuration)
                .expect("supplied configuration")
        };

        let complete = LocalModelDeploymentConfiguration {
            model_artifact_binding_id: "second".to_owned(),
            engine_parameter_values: Some(EngineParameterValues {
                values: [prompt_cache_value(8)]
                    .into_iter()
                    .chain(context_and_request_values())
                    .collect(),
            }),
            ..Default::default()
        };
        assert_eq!(completed(complete.clone()), complete);

        let partial = completed(LocalModelDeploymentConfiguration {
            engine_parameter_values: Some(EngineParameterValues {
                values: vec![prompt_cache_value(8)],
            }),
            ..Default::default()
        });
        assert_eq!(
            partial,
            LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "sized".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(8)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                ..Default::default()
            }
        );
        assert_eq!(
            resolve_local_values(&catalog, &derived_model_id(), "balanced", &partial)
                .expect("overlaid values")
                .derived_engine_parameter_values,
            EngineParameterValues {
                values: vec![filled_context_per_request()],
            }
        );
    }

    #[test]
    fn a_profile_omitting_a_parameter_resolves_the_engine_default() {
        let catalog = shared_parameter_catalog("");
        let completed = apply_profile_defaults(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration::default(),
        )
        .expect("engine defaults");

        assert_eq!(
            completed.engine_parameter_values,
            Some(unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                (PARALLEL_REQUESTS_PARAMETER_ID, 4),
            ]))
        );
        assert_eq!(
            resolve_local_values(&catalog, &derived_model_id(), "balanced", &completed)
                .expect("engine default values")
                .engine_parameter_values,
            unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                (PARALLEL_REQUESTS_PARAMETER_ID, 4),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
            ])
        );
    }

    #[test]
    fn a_supplied_value_wins_over_profile_and_engine_defaults() {
        let catalog = shared_parameter_catalog(
            "        - parameterId: \"parallel-requests\"\n          unsignedInteger: \"2\"\n",
        );
        let defaults = |configuration: &LocalModelDeploymentConfiguration| {
            apply_profile_defaults(&catalog, &derived_model_id(), "balanced", configuration)
                .expect("profile and engine defaults")
                .engine_parameter_values
        };

        assert_eq!(
            defaults(&LocalModelDeploymentConfiguration::default()),
            Some(unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (PARALLEL_REQUESTS_PARAMETER_ID, 2),
                (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
            ]))
        );
        assert_eq!(
            defaults(&LocalModelDeploymentConfiguration {
                engine_parameter_values: Some(unsigned_values(&[
                    (PARALLEL_REQUESTS_PARAMETER_ID, 8),
                    (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                ])),
                ..Default::default()
            }),
            Some(unsigned_values(&[
                (PARALLEL_REQUESTS_PARAMETER_ID, 8),
                (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                ("prompt-cache-memory-mib", 0),
            ]))
        );
    }

    #[test]
    fn omitted_context_per_request_is_filled_from_the_maximum_and_the_total_context() {
        let catalog = shared_parameter_catalog("");
        let lowered = shared_parameter_catalog_with("", |model| {
            model.replacen(
                "  - configurationId: \"local\"\n",
                "  - configurationId: \"local\"\n    maximumContextTokens: 131072\n",
                1,
            )
        });
        let resolve = |catalog: &Catalog, total_context: u64| {
            let configuration = apply_profile_defaults(
                catalog,
                &derived_model_id(),
                "balanced",
                &LocalModelDeploymentConfiguration {
                    engine_parameter_values: Some(unsigned_values(&[(
                        TOTAL_CONTEXT_PARAMETER_ID,
                        total_context,
                    )])),
                    ..Default::default()
                },
            )
            .expect("defaults");
            let values =
                resolve_local_values(catalog, &derived_model_id(), "balanced", &configuration)
                    .expect("filled values");
            (
                values.derived_engine_parameter_values.clone(),
                values
                    .resolved_sizing()
                    .map(|sizing| sizing.derived_engine_parameter_ids),
            )
        };
        let filled = |context_per_request: u64| {
            (
                unsigned_values(&[(CONTEXT_PER_REQUEST_PARAMETER_ID, context_per_request)]),
                Some(vec![CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned()]),
            )
        };

        assert_eq!(resolve(&catalog, 65_536), filled(65_536));
        assert_eq!(resolve(&catalog, 1_048_576), filled(262_144));
        assert_eq!(resolve(&lowered, 1_048_576), filled(131_072));
        assert_eq!(resolve(&lowered, 65_536), filled(65_536));
    }

    #[test]
    fn single_micro_batch_total_context_defaults_to_and_is_capped_by_the_configuration_maximum() {
        let catalog = single_micro_batch_catalog(true);
        let defaults = apply_profile_defaults(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration::default(),
        )
        .expect("engine defaults");

        assert_eq!(
            defaults.engine_parameter_values,
            Some(unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (TOTAL_CONTEXT_PARAMETER_ID, 16_384),
                (PARALLEL_REQUESTS_PARAMETER_ID, 4),
            ]))
        );
        assert_eq!(
            resolve_local_values(&catalog, &derived_model_id(), "balanced", &defaults)
                .expect("default values")
                .engine_parameter_values,
            unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (TOTAL_CONTEXT_PARAMETER_ID, 16_384),
                (PARALLEL_REQUESTS_PARAMETER_ID, 4),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384),
            ])
        );
        assert_eq!(
            resolve_total_context(&catalog, 16_384).expect("the maximum"),
            16_384
        );
        assert_eq!(
            resolve_total_context(&catalog, 16_385)
                .expect_err("above the maximum")
                .to_string(),
            "invalid engine parameter values: total context of 16385 tokens is above the maximum context of 16384 tokens of configuration \"local\", which processes prompts in a single llama.cpp micro-batch; lower the total context"
        );
    }

    #[test]
    fn without_a_single_micro_batch_the_release_total_context_default_and_range_apply() {
        let catalog = single_micro_batch_catalog(false);
        let defaults = apply_profile_defaults(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration::default(),
        )
        .expect("engine defaults");

        assert_eq!(
            defaults.engine_parameter_values,
            Some(unsigned_values(&[
                ("prompt-cache-memory-mib", 0),
                (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                (PARALLEL_REQUESTS_PARAMETER_ID, 4),
            ]))
        );
        for total_context in [16_384, 16_385] {
            assert_eq!(
                resolve_total_context(&catalog, total_context).expect("accepted total context"),
                total_context
            );
        }
    }

    #[test]
    fn supplied_binding_with_omitted_parameters_takes_the_profile_parameter_defaults() {
        let catalog = profile_defaults_catalog();
        let completed = apply_profile_defaults(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "second".to_owned(),
                ..Default::default()
            },
        )
        .expect("profile parameter defaults");
        let values = resolve_local_values(&catalog, &derived_model_id(), "balanced", &completed)
            .expect("non-default binding values");
        assert_eq!(values.selection.binding.binding_id, "second");
        assert_eq!(
            values.engine_parameter_values,
            EngineParameterValues {
                values: [prompt_cache_value(30_000)]
                    .into_iter()
                    .chain(context_and_request_values())
                    .chain([filled_context_per_request()])
                    .collect(),
            }
        );

        let unsupported = apply_profile_defaults(
            &catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "other".to_owned(),
                ..Default::default()
            },
        )
        .expect("profile parameter defaults");
        let error = resolve_local_values(&catalog, &derived_model_id(), "balanced", &unsupported)
            .err()
            .expect("unsupported binding");
        assert_eq!(
            error.to_string(),
            "could not resolve catalog model: Invalid catalog: selected model artifact binding is not supported by the profile configuration"
        );
    }

    #[test]
    fn omitted_engine_configuration_of_a_missing_profile_is_actionable() {
        let error = apply_profile_defaults(
            &profile_defaults_catalog(),
            &derived_model_id(),
            "missing",
            &LocalModelDeploymentConfiguration::default(),
        )
        .expect_err("missing profile");
        assert_eq!(
            error.to_string(),
            "Catalog model \"fixture-model\" profile \"missing\" declares no defaults for the omitted engine configuration; supply the model artifact binding and engine parameter values"
        );
    }

    #[test]
    fn request_false_overrides_profile_true_without_vision_effects() {
        let catalog = development_catalog();
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let snapshot = resolve_local_launch(
            &catalog,
            &CatalogModelId("fixture-vision-model".to_owned()),
            "fixture-profile",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "fixture-artifact".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: [prompt_cache_value(30_000)]
                        .into_iter()
                        .chain(context_and_request_values())
                        .collect(),
                }),
                custom_argument_tokens: Vec::new(),
                option_overrides: vec![ModelOptionValues {
                    option_id: "vision".to_owned(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "enabled".to_owned(),
                        value: Some(model_option_parameter_value::Value::Boolean(false)),
                    }],
                }],
            },
            &target,
            &inventory,
            None,
        )
        .expect("disabled vision snapshot");

        assert_eq!(
            snapshot
                .auxiliary_artifacts
                .iter()
                .map(|artifact| artifact.auxiliary_artifact_id.as_str())
                .collect::<Vec<_>>(),
            ["mtp-draft"]
        );
        assert_eq!(
            snapshot
                .input_capabilities
                .as_ref()
                .expect("input capabilities")
                .supported_modalities,
            [dotcore_proto::dotcore::catalog::v1::ModelInputModality::Text as i32]
        );
        assert_eq!(
            llama_state(&snapshot)
                .option_realizations
                .iter()
                .map(|realization| realization.option_id.as_str())
                .collect::<Vec<_>>(),
            ["vision", "mtp"]
        );
        assert!(
            !snapshot
                .desired_launch_plan
                .as_ref()
                .expect("portable plan")
                .arguments
                .iter()
                .any(|argument| matches!(
                    argument.value.as_ref(),
                    Some(LaunchArgumentValue::Literal(value)) if value == "--mmproj"
                ))
        );
    }

    fn llama_state(
        snapshot: &dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
    ) -> &LlamaCppResolvedModelState {
        match snapshot
            .resolved_engine_state
            .as_ref()
            .and_then(|state| state.state.as_ref())
            .expect("resolved llama.cpp state")
        {
            State::LlamaCpp(state) => state,
            State::Vllm(_) | State::Ninfer(_) => panic!("resolved llama.cpp state"),
        }
    }

    #[test]
    fn digest_pinned_engine_retains_its_canonical_digest_reference_as_launch_identity() {
        let catalog = fixture_catalog(&fixture_catalog_root());
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );

        let snapshot = resolve_local_launch(
            &catalog,
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &crate::test_support::local_model_configuration(),
            &target,
            &inventory,
            None,
        )
        .expect("digest-pinned snapshot");

        let engine = snapshot.engine_artifact.expect("engine artifact");
        assert_eq!(
            engine.preparation_policy,
            dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy::ImmutableRemote
                as i32
        );
        assert_eq!(
            engine.launch_identity,
            "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn the_resolved_engine_artifact_retains_the_release_storage_bindings_exactly() {
        let snapshot =
            resolve_fixture(&engine_storage_catalog(), None).expect("engine storage snapshot");

        assert_eq!(
            snapshot
                .engine_artifact
                .expect("engine artifact")
                .storage_bindings,
            [
                StorageBinding {
                    binding_name: "compile-cache".to_owned(),
                    mount_path: "/cache".to_owned(),
                    kind: StorageKind::Cache as i32,
                    scope: StorageScope::EngineArtifact as i32,
                    environment: vec![
                        StorageEnvironmentVariable {
                            name: "CACHE_HOME".to_owned(),
                            relative_path: "home".to_owned(),
                        },
                        StorageEnvironmentVariable {
                            name: "CACHE_ROOT".to_owned(),
                            relative_path: String::new(),
                        },
                    ],
                },
                StorageBinding {
                    binding_name: "scratch".to_owned(),
                    mount_path: "/scratch".to_owned(),
                    kind: StorageKind::Cache as i32,
                    scope: StorageScope::Deployment as i32,
                    environment: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn tag_only_engine_selection_names_its_reference_and_digest_selection_does_not_need_an_identity()
     {
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let select = |catalog: &Catalog| {
            select_engine_artifact(
                catalog,
                &CatalogModelId("fixture-model".to_owned()),
                "fixture-profile",
                &crate::test_support::local_model_configuration(),
                &target,
                &inventory,
            )
            .expect("engine artifact selection")
        };

        assert_eq!(
            select(&crate::test_support::tag_only_engine_catalog()),
            EngineArtifactSelection::OciImage {
                reference: TAG_ONLY_ENGINE_REFERENCE.to_owned(),
                tag_only: true,
            }
        );
        assert_eq!(
            select(&fixture_catalog(&fixture_catalog_root())),
            EngineArtifactSelection::OciImage {
                reference: ENGINE_DIGEST_REFERENCE.to_owned(),
                tag_only: false,
            }
        );
    }

    #[test]
    fn tag_only_engine_with_a_resolved_image_id_retains_it_as_a_local_development_identity() {
        let snapshot = resolve_fixture(
            &crate::test_support::tag_only_engine_catalog(),
            Some(&EngineLaunchIdentity {
                reference: TAG_ONLY_ENGINE_REFERENCE.to_owned(),
                image_id: ENGINE_IMAGE_ID.to_owned(),
            }),
        )
        .expect("pinned tag-only snapshot");

        let engine = snapshot.engine_artifact.expect("engine artifact");
        assert_eq!(
            engine.preparation_policy(),
            ArtifactPreparationPolicy::LocalDevelopment
        );
        assert_eq!(engine.launch_identity, ENGINE_IMAGE_ID);
        assert_eq!(
            engine
                .artifact
                .as_ref()
                .and_then(dotcore_domain::oci::engine_artifact_reference)
                .and_then(|reference| reference.tag.clone()),
            Some("dev".to_owned())
        );
    }

    #[test]
    fn tag_only_engine_without_an_identity_is_left_for_the_hosting_node() {
        let local = resolve_fixture(&crate::test_support::tag_only_engine_catalog(), None)
            .expect("tag-only engine left for its node");

        let engine = local.engine_artifact.clone().expect("engine artifact");
        assert_eq!(
            engine.preparation_policy,
            ArtifactPreparationPolicy::LocalDevelopment as i32
        );
        assert_eq!(engine.launch_identity, "");
        assert!(crate::model::resolved::validate_local_snapshot(&local).is_err());
    }

    #[test]
    fn identity_resolved_for_another_reference_is_rejected() {
        let error = resolve_fixture(
            &crate::test_support::tag_only_engine_catalog(),
            Some(&EngineLaunchIdentity {
                reference: "registry.dotcore.test/other:dev".to_owned(),
                image_id: ENGINE_IMAGE_ID.to_owned(),
            }),
        )
        .expect_err("identity resolved for another selection");

        assert!(matches!(
            &error,
            ModelLaunchResolutionError::EngineImageIdentityMismatch {
                reference,
                resolved_reference,
            } if reference == TAG_ONLY_ENGINE_REFERENCE
                && resolved_reference == "registry.dotcore.test/other:dev"
        ));
    }

    #[test]
    fn digest_pinned_engine_rejects_a_resolved_local_identity() {
        let error = resolve_fixture(
            &fixture_catalog(&fixture_catalog_root()),
            Some(&EngineLaunchIdentity {
                reference: ENGINE_DIGEST_REFERENCE.to_owned(),
                image_id: ENGINE_IMAGE_ID.to_owned(),
            }),
        )
        .expect_err("digest-pinned engine with a local identity");

        assert!(matches!(
            &error,
            ModelLaunchResolutionError::EngineIdentityUnexpected { reference }
                if reference == ENGINE_DIGEST_REFERENCE
        ));
    }

    #[test]
    fn malformed_resolved_image_id_is_rejected_by_snapshot_validation() {
        let error = resolve_fixture(
            &crate::test_support::tag_only_engine_catalog(),
            Some(&EngineLaunchIdentity {
                reference: TAG_ONLY_ENGINE_REFERENCE.to_owned(),
                image_id: TAG_ONLY_ENGINE_REFERENCE.to_owned(),
            }),
        )
        .expect_err("tag retained as identity");

        let ModelLaunchResolutionError::CatalogResolution(source) = &error else {
            panic!("expected snapshot validation failure, got {error:?}");
        };
        let root_cause = std::iter::successors(Some(source as &dyn std::error::Error), |error| {
            error.source()
        })
        .last()
        .expect("error chain");
        assert_eq!(
            root_cause.to_string(),
            "local-development launch identity is not a SHA-256 image ID"
        );
    }

    #[test]
    fn locked_nix_engine_retains_its_catalog_store_paths_for_the_hosting_node() {
        let mut artifact = nix_artifact(&nix::locked_flake());
        let Some(EngineArtifactKind::Nix(source)) = artifact.kind.as_mut() else {
            panic!("Nix artifact");
        };
        source.store_paths = vec![NixSystemStorePath {
            system: "aarch64-darwin".to_owned(),
            store_path: nix::STORE_PATH.to_owned(),
        }];

        let (policy, launch_identity, retained) =
            engine_launch_identity(artifact.clone(), None).expect("locked launch");

        assert_eq!(policy, ArtifactPreparationPolicy::ImmutableRemote);
        assert_eq!(launch_identity, "");
        assert_eq!(retained, artifact);
    }

    #[test]
    fn development_source_nix_engine_leaves_its_launch_identity_for_the_hosting_node() {
        let (policy, launch_identity, retained) =
            engine_launch_identity(nix_artifact("self"), None).expect("development-source launch");

        assert_eq!(policy, ArtifactPreparationPolicy::DevelopmentSource);
        assert_eq!(launch_identity, "");
        assert_eq!(
            retained.kind,
            Some(EngineArtifactKind::Nix(NixArtifact {
                flake: "self".to_owned(),
                package: Some("engines.llama-cpp.native".to_owned()),
                provides: Some(
                    dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::Executable(
                        "llama-server".to_owned()
                    )
                ),
                store_paths: Vec::new(),
            }))
        );
    }

    #[test]
    fn docker_delivered_nix_engines_leave_their_store_path_to_the_hosting_node() {
        let docker_image = |flake: &str| {
            let mut artifact = nix_artifact(flake);
            let Some(EngineArtifactKind::Nix(nix)) = artifact.kind.as_mut() else {
                unreachable!("Nix fixture artifact");
            };
            nix.package = Some("engines.llama-cpp.docker".to_owned());
            nix.provides = Some(
                dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::DockerImage(
                    dotcore_proto::dotcore::catalog::v1::NixDockerImage {},
                ),
            );
            nix.store_paths = if flake == "self" {
                Vec::new()
            } else {
                ["x86_64-linux", "aarch64-linux", "aarch64-darwin"]
                    .map(
                        |system| dotcore_proto::dotcore::catalog::v1::NixSystemStorePath {
                            system: system.to_owned(),
                            store_path: nix::DOCKER_IMAGE_STORE_PATH.to_owned(),
                        },
                    )
                    .to_vec()
            };
            artifact
        };

        let locked = docker_image(&nix::locked_flake());
        assert_eq!(
            engine_launch_identity(locked.clone(), None).expect("locked launch"),
            (
                ArtifactPreparationPolicy::ImmutableRemote,
                String::new(),
                locked
            )
        );
        let development = docker_image("self");
        assert_eq!(
            engine_launch_identity(development.clone(), None).expect("development-source launch"),
            (
                ArtifactPreparationPolicy::DevelopmentSource,
                String::new(),
                development
            )
        );
    }

    #[test]
    fn nix_engine_identity_rejections_are_typed() {
        let image = EngineLaunchIdentity {
            reference: "self".to_owned(),
            image_id: format!("sha256:{}", "a".repeat(64)),
        };

        for flake in ["self".to_owned(), nix::locked_flake()] {
            assert!(matches!(
                engine_launch_identity(
                    nix_artifact(&flake),
                    Some(&image),
                ),
                Err(ModelLaunchResolutionError::EngineIdentityUnexpected { reference })
                    if reference == flake
            ));
        }
    }

    #[tokio::test]
    async fn facade_launch_identity_leaves_nix_engine_artifacts_to_be_pinned_after_resolution() {
        let cache_root = tempfile::tempdir().expect("cache root");
        let manager = dotcore_artifact::ArtifactManager::new(cache_root.path().to_path_buf(), None)
            .expect("manager")
            .with_nix_sources(dotcore_artifact::NixSources::new(
                dotcore_domain::nix::DotcoreSources {
                    development: Some("git+file:///src/dotcore".to_owned()),
                    packaged: None,
                },
            ));

        let identity = EngineArtifactSelection::Nix(super::NixEngineSelection {
            flake: "self".to_owned(),
            package: "engines.llama-cpp.native".to_owned(),
        })
        .resolve_launch_identity(&manager)
        .await
        .expect("Nix engine artifact");

        assert_eq!(identity, None);
    }

    #[test]
    fn targets_whose_device_count_differs_from_the_launch_are_rejected() {
        let catalog = fixture_catalog(&fixture_catalog_root());
        let (mut target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let resolve = |target: &dotcore_proto::dotcore::site::v1::ExecutionTarget| {
            resolve_local_launch(
                &catalog,
                &CatalogModelId("fixture-model".to_owned()),
                "fixture-profile",
                &crate::test_support::local_model_configuration(),
                target,
                &inventory,
                None,
            )
        };

        for (devices, device_count) in [(vec!["0", "1"], 2), (Vec::new(), 0)] {
            target.accelerator_device_ids = devices.iter().map(ToString::to_string).collect();
            let error = resolve(&target).expect_err("mismatched device count");
            assert!(matches!(
                &error,
                ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
                    execution_target_id,
                    device_count: actual,
                    required: 1,
                } if execution_target_id == "fixture-target" && *actual == device_count
            ));
            assert_eq!(
                error.to_string(),
                format!(
                    "execution target \"fixture-target\" has {device_count} accelerator devices, but the profile's launch uses 1"
                )
            );
        }

        target.accelerator_device_ids = vec!["0".to_owned()];
        resolve(&target).expect("one-device target");
    }

    #[test]
    fn the_launch_device_count_comes_from_the_synthesized_requirements_for_every_runtime() {
        let single_device = ResolvedHardwareRequirementsValue::single_device("nvidia", None);
        let mut two_devices = single_device.clone();
        two_devices.accelerator_requirements[0].minimum_count = 2;
        let docker = |devices: &[&str]| dotcore_proto::dotcore::site::v1::ExecutionTarget {
            execution_target_id: "target".to_owned(),
            accelerator_device_ids: devices.iter().map(ToString::to_string).collect(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..Default::default()
        };
        let simulated = |devices: &[&str]| dotcore_proto::dotcore::site::v1::ExecutionTarget {
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..docker(devices)
        };

        validate_target_accelerator_devices(&docker(&["0"]), &single_device)
            .expect("one-device target for a hardware class launch");
        assert!(matches!(
            validate_target_accelerator_devices(&docker(&["0", "1"]), &single_device),
            Err(ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
                device_count: 2,
                required: 1,
                ..
            })
        ));
        validate_target_accelerator_devices(&docker(&["0", "1"]), &two_devices)
            .expect("pair target for a two-device launch");

        // A simulated target names its declared machine's GPUs and passes the same check.
        validate_target_accelerator_devices(&simulated(&["GPU-0"]), &single_device)
            .expect("simulated target on one declared GPU");
        assert!(matches!(
            validate_target_accelerator_devices(&simulated(&[]), &single_device),
            Err(ModelLaunchResolutionError::AcceleratorDeviceCountMismatch {
                device_count: 0,
                required: 1,
                ..
            })
        ));
    }

    /// Nix artifact with one darwin/arm64 Apple selector.
    fn nix_artifact(flake: &str) -> EngineArtifact {
        nix::engine_artifact("metal", flake, "darwin", "arm64", "apple")
    }

    fn resolve_fixture(
        catalog: &Catalog,
        engine_identity: Option<&EngineLaunchIdentity>,
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        resolve_local_launch(
            catalog,
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &crate::test_support::local_model_configuration(),
            &target,
            &inventory,
            engine_identity,
        )
    }

    const ENGINE_DIGEST_REFERENCE: &str = "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ENGINE_IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn fixture_catalog_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/simulated-site/catalog")
    }

    fn fixture_catalog(root: &Path) -> Catalog {
        Catalog::load_with_policy(
            root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("fixture catalog")
    }

    fn estimates_catalog(estimates: &str) -> Catalog {
        let root = dotcore_catalog::test_support::temporary_catalog_root();
        dotcore_catalog::test_support::create_empty_catalog_source(&root);
        dotcore_catalog::test_support::write_sizing_engine(&root);
        let sizing = format!(
            "    sizing:\n      parameters:\n        baseBytes: 100\n      derivedParameters:\n        prompt-cache-memory-mib: \"4096\"\n      estimates:\n{estimates}"
        );
        dotcore_catalog::test_support::write_sizing_model_with(
            &root,
            &sizing,
            dotcore_catalog::test_support::UNPARAMETERIZED_BINDING_SIZING,
            "      values: []\n",
        );
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("estimates catalog")
    }

    fn resolved_sizing_for(estimates: &str) -> Option<ResolvedSizing> {
        resolve_local_values(
            &estimates_catalog(estimates),
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "sized".to_owned(),
                engine_parameter_values: Some(EngineParameterValues {
                    values: context_and_request_values(),
                }),
                ..Default::default()
            },
        )
        .expect("estimate values")
        .resolved_sizing()
    }

    /// Derived sizing catalog whose `nvidia` support entry carries `support_sizing`, which may
    /// also declare further support entries.
    /// Resolves a profile supporting one hardware class without a target.
    fn resolve_local_values(
        catalog: &Catalog,
        model_id: &CatalogModelId,
        profile_id: &str,
        configuration: &LocalModelDeploymentConfiguration,
    ) -> Result<ResolvedLocalModelValues, ModelLaunchResolutionError> {
        estimate_local_values_per_class(catalog, model_id, profile_id, configuration).and_then(
            |values| match <[ResolvedLocalModelValues; 1]>::try_from(values) {
                Ok([values]) => values.into_launchable(),
                Err(values) => panic!("expected one hardware class, got {}", values.len()),
            },
        )
    }

    fn hardware_class_catalog(support_sizing: &str) -> Catalog {
        hardware_class_catalog_with_class("nvidia", support_sizing)
    }

    /// Derived sizing catalog whose first support entry references `hardware_class_id`, among the
    /// fixture classes and an NVIDIA unified-memory class. A configuration supporting the `apple`
    /// class also approves the engine's Apple artifact.
    fn hardware_class_catalog_with_class(hardware_class_id: &str, support_sizing: &str) -> Catalog {
        let root = dotcore_catalog::test_support::temporary_catalog_root();
        dotcore_catalog::test_support::create_empty_catalog_source(&root);
        dotcore_catalog::test_support::write_sizing_engine(&root);
        std::fs::write(
            root.join("hardware-classes/nvidia-unified.yaml"),
            "hardwareClassId: \"nvidia-unified\"\ndisplayName: \"NVIDIA unified\"\nselector:\n  accelerators: [\"nvidia\"]\n  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_UNIFIED\"\n",
        )
        .expect("unified hardware class fixture");
        dotcore_catalog::test_support::write_sizing_model_with(
            &root,
            r#"    sizing:
      parameters:
        baseBytes: 100
      derivedParameters:
        prompt-cache-memory-mib: "4096"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + 3 * ${prompt-cache-memory-mib}"
"#,
            support_sizing,
            "      values: []\n",
        );
        let model_path = root.join("models/fixture.yaml");
        let model = std::fs::read_to_string(&model_path).expect("fixture model");
        std::fs::write(
            &model_path,
            model.replacen(
                "      - hardwareClassId: \"nvidia\"\n",
                &format!("      - hardwareClassId: \"{hardware_class_id}\"\n"),
                1,
            ),
        )
        .expect("fixture model class");
        if std::fs::read_to_string(&model_path)
            .expect("fixture model")
            .contains("hardwareClassId: \"apple\"")
        {
            dotcore_catalog::test_support::approve_sizing_apple_artifact(&root);
        }
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("hardware class catalog")
    }

    /// The `sized` binding with the total context and parallel requests, deriving the prompt cache.
    fn context_and_request_configuration() -> LocalModelDeploymentConfiguration {
        LocalModelDeploymentConfiguration {
            model_artifact_binding_id: "sized".to_owned(),
            engine_parameter_values: Some(EngineParameterValues {
                values: context_and_request_values(),
            }),
            ..Default::default()
        }
    }

    fn derived_sizing_catalog(derivation: &str) -> Catalog {
        let root = dotcore_catalog::test_support::temporary_catalog_root();
        dotcore_catalog::test_support::create_empty_catalog_source(&root);
        dotcore_catalog::test_support::write_sizing_engine(&root);
        let sizing = format!(
            r#"    sizing:
      parameters:
        baseBytes: 100
      derivedParameters:
        prompt-cache-memory-mib: "{derivation}"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${{baseBytes}} + 3 * ${{prompt-cache-memory-mib}}"
"#
        );
        dotcore_catalog::test_support::write_sizing_model_with(
            &root,
            &sizing,
            dotcore_catalog::test_support::UNPARAMETERIZED_BINDING_SIZING,
            "      values: []\n",
        );
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("derived sizing catalog")
    }

    /// Profile `balanced` defaults to binding `sized` with a prompt cache of 30000 MiB;
    /// bindings `sized` and `second` both derive the prompt cache.
    fn profile_defaults_catalog() -> Catalog {
        let root = dotcore_catalog::test_support::temporary_catalog_root();
        dotcore_catalog::test_support::create_empty_catalog_source(&root);
        dotcore_catalog::test_support::write_sizing_engine(&root);
        dotcore_catalog::test_support::write_sizing_model_with(
            &root,
            r#"    sizing:
      parameters:
        baseBytes: 100
      derivedParameters:
        prompt-cache-memory-mib: "4096"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + 3 * ${prompt-cache-memory-mib}"
"#,
            dotcore_catalog::test_support::UNPARAMETERIZED_BINDING_SIZING,
            "      values:\n        - parameterId: \"prompt-cache-memory-mib\"\n          unsignedInteger: \"30000\"\n",
        );
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("profile defaults catalog")
    }

    /// Profile `balanced` sets the prompt cache plus `profile_defaults` (eight-space list
    /// entries) on the sizing engine, which defaults the total context to 32768 tokens
    /// and the parallel requests to 4, in an unsized configuration of a 262144-token model.
    fn shared_parameter_catalog(profile_defaults: &str) -> Catalog {
        shared_parameter_catalog_with(profile_defaults, |model| model)
    }

    /// The shared parameter catalog under the engines' own declaration rules, with a 16384-token
    /// configuration maximum and, when `single_micro_batch` is set, the single micro-batch
    /// llama.cpp fixed setting.
    fn single_micro_batch_catalog(single_micro_batch: bool) -> Catalog {
        let root = shared_parameter_root("", |model| {
            let model = model.replacen(
                "  - configurationId: \"local\"\n",
                "  - configurationId: \"local\"\n    maximumContextTokens: 16384\n",
                1,
            );
            if single_micro_batch {
                model.replacen(
                    "          flashAttention: true\n",
                    "          flashAttention: true\n          singleMicroBatch: true\n",
                    1,
                )
            } else {
                model
            }
        });
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            declaration_validator(),
        )
        .expect("single micro-batch catalog")
    }

    /// Resolves the shared parameter fixture's profile with a supplied total context and returns
    /// the resolved total context.
    fn resolve_total_context(
        catalog: &Catalog,
        total_context: u64,
    ) -> Result<u64, ModelLaunchResolutionError> {
        let configuration = apply_profile_defaults(
            catalog,
            &derived_model_id(),
            "balanced",
            &LocalModelDeploymentConfiguration {
                engine_parameter_values: Some(unsigned_values(&[(
                    TOTAL_CONTEXT_PARAMETER_ID,
                    total_context,
                )])),
                ..Default::default()
            },
        )?;
        let values =
            resolve_local_values(catalog, &derived_model_id(), "balanced", &configuration)?;
        Ok(values
            .engine_parameter_values
            .values
            .iter()
            .find(|value| value.parameter_id == TOTAL_CONTEXT_PARAMETER_ID)
            .and_then(|value| match value.value {
                Some(engine_parameter_value::Value::UnsignedInteger(value)) => Some(value),
                _ => None,
            })
            .expect("resolved total context"))
    }

    /// The shared parameter catalog with its model file rewritten by `edit`.
    fn shared_parameter_catalog_with(
        profile_defaults: &str,
        edit: impl FnOnce(String) -> String,
    ) -> Catalog {
        Catalog::load_with_policy(
            &shared_parameter_root(profile_defaults, edit),
            OciArtifactPolicy::Development,
            &dotcore_catalog::test_support::ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("shared parameter catalog")
    }

    /// Writes the shared parameter catalog source, with its model file rewritten by `edit`, and
    /// returns its root.
    fn shared_parameter_root(
        profile_defaults: &str,
        edit: impl FnOnce(String) -> String,
    ) -> std::path::PathBuf {
        let root = dotcore_catalog::test_support::temporary_catalog_root();
        dotcore_catalog::test_support::create_empty_catalog_source(&root);
        dotcore_catalog::test_support::write_sizing_engine(&root);
        dotcore_catalog::test_support::write_sizing_model_with(
            &root,
            "",
            "",
            &format!(
                "      values:\n{}{profile_defaults}",
                dotcore_catalog::test_support::SIZING_ENGINE_CACHE_DEFAULT
            ),
        );
        let model_path = root.join("models/fixture.yaml");
        let model = fs::read_to_string(&model_path).expect("shared parameter model");
        fs::write(&model_path, edit(model)).expect("edited shared parameter model");
        root
    }

    fn unsigned_values(values: &[(&str, u64)]) -> EngineParameterValues {
        EngineParameterValues {
            values: values
                .iter()
                .map(|(parameter_id, value)| EngineParameterValue {
                    parameter_id: (*parameter_id).to_owned(),
                    value: Some(engine_parameter_value::Value::UnsignedInteger(*value)),
                })
                .collect(),
        }
    }

    fn derived_model_id() -> CatalogModelId {
        CatalogModelId("fixture-model".to_owned())
    }

    fn prompt_cache_value(value: u64) -> EngineParameterValue {
        EngineParameterValue {
            parameter_id: "prompt-cache-memory-mib".to_owned(),
            value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
        }
    }

    /// The context per request resolution fills from the fixtures' total context.
    fn filled_context_per_request() -> EngineParameterValue {
        EngineParameterValue {
            parameter_id: "context-per-request".to_owned(),
            value: Some(engine_parameter_value::Value::UnsignedInteger(32_768)),
        }
    }

    /// Total context and parallel request values the derivation fixtures supply, since their
    /// sizing derives the prompt cache instead.
    fn context_and_request_values() -> Vec<EngineParameterValue> {
        [("total-context", 32_768), ("parallel-requests", 4)]
            .into_iter()
            .map(|(parameter_id, value)| EngineParameterValue {
                parameter_id: parameter_id.to_owned(),
                value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
            })
            .collect()
    }

    fn development_catalog() -> Catalog {
        fixture_catalog(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/development-catalog"),
        )
    }

    /// The simulated-site fixture catalog with its model artifact declared as a three-shard
    /// snapshot of one repository instead of a single file.
    fn split_snapshot_catalog() -> Catalog {
        let root = copied_fixture_catalog();
        let model_path = root.path().join("models/fixture-model.yaml");
        let model = fs::read_to_string(&model_path).expect("fixture model");
        let single_file = format!(
            "      file:\n        path: \"fixture-Q8_0.gguf\"\n        provenanceDigest: \"{}\"\n",
            "0".repeat(64)
        );
        assert!(
            model.contains(&single_file),
            "fixture model single file source"
        );
        let shards = (1..=3)
            .map(|shard| {
                format!(
                    "          - path: \"fixture-Q8_0-0000{shard}-of-00003.gguf\"\n            provenanceDigest: \"{}\"\n",
                    shard.to_string().repeat(64)
                )
            })
            .collect::<String>();
        fs::write(
            &model_path,
            model.replace(
                &single_file,
                &format!("      snapshot:\n        files:\n{shards}"),
            ),
        )
        .expect("snapshot model fixture");

        fixture_catalog(root.path())
    }

    /// The simulated-site fixture catalog with its engine release declaring two storage bindings.
    fn engine_storage_catalog() -> Catalog {
        let root = copied_fixture_catalog();
        let engine_path = root.path().join("engines/fixture.yaml");
        let engine = fs::read_to_string(&engine_path).expect("fixture engine");
        fs::write(
            &engine_path,
            format!(
                "{engine}storageBindings:\n  - bindingName: \"compile-cache\"\n    mountPath: \"/cache\"\n    kind: \"STORAGE_KIND_CACHE\"\n    scope: \"STORAGE_SCOPE_ENGINE_ARTIFACT\"\n    environment:\n      - name: \"CACHE_HOME\"\n        relativePath: \"home\"\n      - name: \"CACHE_ROOT\"\n  - bindingName: \"scratch\"\n    mountPath: \"/scratch\"\n    kind: \"STORAGE_KIND_CACHE\"\n    scope: \"STORAGE_SCOPE_DEPLOYMENT\"\n"
            ),
        )
        .expect("engine storage fixture");

        fixture_catalog(root.path())
    }

    /// A writable copy of the simulated-site fixture catalog.
    fn copied_fixture_catalog() -> tempfile::TempDir {
        let fixture = fixture_catalog_root();
        let root = tempfile::tempdir().expect("catalog root");
        for directory in ["engines", "hardware-classes", "models"] {
            let target = root.path().join(directory);
            fs::create_dir_all(&target).expect("catalog directory");
            for entry in fs::read_dir(fixture.join(directory)).expect("fixture directory") {
                let entry = entry.expect("fixture entry");
                fs::copy(entry.path(), target.join(entry.file_name())).expect("fixture file");
            }
        }
        root
    }
}
