//! Reconstructs controller-owned resolved launch values from retained snapshots.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_artifact::{
    EngineArtifactPreparation, EngineArtifactSource, ModelArtifactSource, OciPlatform,
};
use dotcore_catalog::{CatalogError, CatalogModelId, validate_parameter_values};
use dotcore_domain::{
    MemoryArchitecture,
    deployment::artifact::{
        LaunchIdentityResolution, node_resolved_selection, validate_launch_identity,
        validate_nix_launch_identity,
    },
    oci::render_reference,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, EngineRelease, HardwareRequirements, HuggingFaceArtifactSource,
        ModelInputCapabilities, ParameterValues, StorageBinding,
        engine_artifact::Kind as EngineArtifactKind,
        hugging_face_artifact_source::Content as HuggingFaceContent, nix_artifact::Provides,
    },
    deployment::v1::ArtifactPreparationPolicy,
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentSnapshot, PortableModelLaunchPlan,
        ResolvedLocalModelDeploymentSnapshot, ResolvedSizing, model_deployment_snapshot::Source,
        portable_model_artifact_binding::Artifact as BoundArtifact,
        portable_model_launch_argument_token::Value as ArgumentValue,
    },
    telemetry::v1::WorkloadResourceInputs,
};

/// Fully resolved immutable values required to materialize one runtime launch.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedModelLaunch {
    pub model_id: CatalogModelId,
    pub profile_id: String,
    pub artifact: ModelArtifactSource,
    pub artifacts: BTreeMap<String, ModelArtifactSource>,
    pub hardware: ResolvedHardwareRequirementsValue,
    pub engine: ResolvedEngineArtifact,
    pub input_capabilities: ResolvedModelInputCapabilities,
    pub accelerator_memory: MemoryArchitecture,
    pub plan: PortableModelLaunchPlan,
    pub(crate) snapshot: ResolvedLocalModelDeploymentSnapshot,
}

impl ResolvedModelLaunch {
    /// Converts a local deployment declaration to the materialization inputs every launch uses:
    /// its retained snapshot with the engine launch identity its hosting node resolved, if any.
    pub(crate) fn from_declaration(
        declaration: &ModelDeploymentDeclaration,
    ) -> Result<Self, CatalogError> {
        let snapshot = crate::model::engine_pin::launch_snapshot(declaration)
            .ok_or_else(|| invalid("deployment declaration has no immutable model snapshot"))?;
        Self::from_snapshot(&snapshot)
    }

    /// Converts a self-contained immutable deployment snapshot to materialization inputs.
    pub fn from_snapshot(snapshot: &ModelDeploymentSnapshot) -> Result<Self, CatalogError> {
        let Some(Source::Local(local)) = snapshot.source.as_ref() else {
            return Err(invalid("deployment snapshot is not local"));
        };
        Self::from_local_snapshot(local)
    }

    pub(crate) fn from_local_snapshot(
        local: &ResolvedLocalModelDeploymentSnapshot,
    ) -> Result<Self, CatalogError> {
        validate_local_snapshot(local)?;
        let engine = local
            .engine_artifact
            .as_ref()
            .ok_or_else(|| invalid("local snapshot has no engine artifact"))?;
        let engine_artifact = engine
            .artifact
            .as_ref()
            .ok_or_else(|| invalid("local snapshot engine has no engine artifact"))?;
        let source = ResolvedEngineSource::from_artifact(engine_artifact)?;
        if !engine_artifact.selectors.is_empty() {
            return Err(invalid(
                "local snapshot engine artifact retains selectors instead of matched target facts",
            ));
        }
        let platform = engine
            .platform
            .as_ref()
            .ok_or_else(|| invalid("local snapshot engine has no matched platform"))?;
        let platform = dotcore_domain::ExecutionPlatform::try_from(platform)
            .map_err(|source| CatalogError::Invalid(anyhow::Error::new(source)))?;
        let artifact_source = local
            .model_artifact
            .as_ref()
            .and_then(|value| value.source.as_ref())
            .ok_or_else(|| invalid("local snapshot has no model artifact source"))?;
        let artifact = model_artifact_source(artifact_source)?;
        let auxiliary = |id: &str| {
            let source = local
                .auxiliary_artifacts
                .iter()
                .find(|artifact| artifact.auxiliary_artifact_id == id)
                .and_then(|artifact| artifact.source.as_ref())
                .ok_or_else(|| invalid("local snapshot has no bound auxiliary artifact source"))?;
            model_artifact_source(source)
        };
        let plan = local
            .desired_launch_plan
            .as_ref()
            .ok_or_else(|| invalid("local snapshot has no desired launch plan"))?
            .clone();
        let mut artifacts = BTreeMap::new();
        for binding in &plan.artifact_bindings {
            let bound = binding
                .artifact
                .as_ref()
                .ok_or_else(|| invalid("local snapshot has an incomplete artifact binding"))?;
            let resolved = match bound {
                BoundArtifact::ModelArtifactId(_) => artifact.clone(),
                BoundArtifact::AuxiliaryArtifactId(id) => auxiliary(id)?,
            };
            artifacts.insert(binding.binding_id.clone(), resolved);
        }
        Ok(Self {
            model_id: CatalogModelId(local.model_id.clone()),
            profile_id: local.profile_id.clone(),
            artifact,
            artifacts,
            hardware: ResolvedHardwareRequirementsValue::single_device(
                &engine.accelerator,
                local
                    .hardware_support
                    .as_ref()
                    .and_then(|support| support.hardware_requirements.as_ref()),
            ),
            engine: ResolvedEngineArtifact {
                release_id: engine.engine_release_id.clone(),
                kind: engine.engine_kind,
                version: engine.engine_version.clone(),
                artifact_id: engine_artifact.artifact_id.clone(),
                platform: ResolvedEnginePlatformValue {
                    operating_system: platform.operating_system().to_owned(),
                    architecture: platform.architecture().to_owned(),
                    variant: None,
                    accelerator: engine.accelerator.clone(),
                },
                minimum_nvidia_driver_major_version: engine.minimum_nvidia_driver_major_version,
                source,
                preparation_policy: validated_engine_preparation_policy(
                    engine,
                    LaunchIdentityResolution::Retained,
                )?,
                launch_identity: engine.launch_identity.clone(),
            },
            input_capabilities: ResolvedModelInputCapabilities::from(
                local.input_capabilities.as_ref(),
            ),
            accelerator_memory: selected_target_memory_architecture(local)?,
            plan,
            snapshot: local.clone(),
        })
    }

    /// Storage bindings of the engine release, retained in the deployment snapshot.
    pub(crate) fn engine_storage_bindings(&self) -> &[StorageBinding] {
        engine_storage_bindings(&self.snapshot)
    }

    /// Resolved launch inputs recorded with workload resource telemetry.
    pub(crate) fn telemetry_inputs(&self) -> WorkloadResourceInputs {
        let snapshot = &self.snapshot;
        WorkloadResourceInputs {
            model_id: snapshot.model_id.clone(),
            configuration_id: snapshot.configuration_id.clone(),
            hardware_class_id: snapshot
                .hardware_support
                .as_ref()
                .map(|support| support.hardware_class_id.clone())
                .unwrap_or_default(),
            profile_id: snapshot.profile_id.clone(),
            model_artifact_id: snapshot
                .model_artifact
                .as_ref()
                .map(|artifact| artifact.artifact_id.clone())
                .unwrap_or_default(),
            auxiliary_artifact_ids: snapshot
                .auxiliary_artifacts
                .iter()
                .map(|artifact| artifact.auxiliary_artifact_id.clone())
                .collect(),
            engine_release_id: self.engine.release_id.clone(),
            engine_artifact_id: self.engine.artifact_id.clone(),
            engine_artifact_reference: match &self.engine.source {
                ResolvedEngineSource::OciImage { reference } => reference.clone(),
                // A Nix engine's launch identity is its evaluated store path.
                ResolvedEngineSource::Nix { .. } | ResolvedEngineSource::NixDockerImage { .. } => {
                    self.engine.launch_identity.clone()
                }
            },
            engine_parameter_values: snapshot.engine_parameter_values.clone(),
            option_values: snapshot
                .resolved_options
                .iter()
                .filter_map(|option| option.values.clone())
                .collect(),
            custom_argument_tokens: crate::management_logs::mask_sensitive_arguments(
                &snapshot.custom_argument_tokens,
            ),
        }
    }
}

/// Storage bindings of the engine release a local snapshot retains, read without validating the
/// rest of the snapshot, so teardown can name the storage of a deployment that can't launch.
pub(crate) fn engine_storage_bindings(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> &[StorageBinding] {
    snapshot
        .engine_artifact
        .as_ref()
        .map_or(&[], |engine| engine.storage_bindings.as_slice())
}

/// Launch hardware requirements synthesized from the resolved hardware class and its support
/// entry's minimums.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHardwareRequirementsValue {
    pub accelerator_requirements: Vec<ResolvedAcceleratorRequirementValue>,
    pub minimum_system_memory_bytes: u64,
}

impl ResolvedHardwareRequirementsValue {
    /// Synthesizes the requirements of a launch on one device of the matched accelerator vendor,
    /// with the support entry's minimums. Hardware class selectors carry no device count, so they
    /// match single-device targets only.
    pub(crate) fn single_device(vendor: &str, requirements: Option<&HardwareRequirements>) -> Self {
        let requirements = requirements.copied().unwrap_or_default();
        Self {
            accelerator_requirements: vec![ResolvedAcceleratorRequirementValue {
                vendor: vendor.to_owned(),
                minimum_count: 1,
                minimum_memory_bytes_per_device: requirements.minimum_memory_bytes_per_device,
            }],
            minimum_system_memory_bytes: requirements.minimum_system_memory_bytes,
        }
    }

    /// Returns how many accelerator devices the launch uses: the devices its accelerator
    /// requirements count together.
    pub fn accelerator_device_count(&self) -> u32 {
        self.accelerator_requirements
            .iter()
            .fold(0, |count, requirement| {
                count.saturating_add(requirement.minimum_count)
            })
    }
}

/// Immutable requirement for one accelerator vendor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAcceleratorRequirementValue {
    pub vendor: String,
    pub minimum_count: u32,
    pub minimum_memory_bytes_per_device: u64,
}

/// Exact engine artifact selected for one model launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEngineArtifact {
    pub release_id: String,
    pub kind: i32,
    pub version: String,
    pub artifact_id: String,
    pub platform: ResolvedEnginePlatformValue,
    /// Minimum NVIDIA driver major version of the selected entry, zero for other vendors.
    pub minimum_nvidia_driver_major_version: u32,

    /// Catalog source the artifact was selected from, retained as provenance.
    pub source: ResolvedEngineSource,

    /// How the artifact manager prepares the artifact before a launch.
    pub preparation_policy: ArtifactPreparationPolicy,

    /// Immutable digest reference, local image ID, or Nix store path that every launch uses.
    pub launch_identity: String,
}

impl ResolvedEngineArtifact {
    /// Returns the Docker image reference a container launch of this artifact runs: the launch
    /// identity of an OCI image, or the reference derived from a Docker-delivered Nix artifact's
    /// pinned loader store path. `None` for a native Nix package or an unusable store path.
    pub(crate) fn container_image(&self) -> Option<String> {
        match &self.source {
            ResolvedEngineSource::OciImage { .. } => Some(self.launch_identity.clone()),
            ResolvedEngineSource::NixDockerImage { .. } => {
                dotcore_domain::nix::docker_launch_reference(&self.launch_identity)
            }
            ResolvedEngineSource::Nix { .. } => None,
        }
    }

    /// Returns the artifact manager input that prepares this exact engine artifact.
    pub(crate) fn preparation(&self) -> EngineArtifactPreparation {
        EngineArtifactPreparation {
            source: match &self.source {
                ResolvedEngineSource::OciImage { .. } => EngineArtifactSource::OciImage,
                ResolvedEngineSource::Nix { flake, package, .. } => EngineArtifactSource::Nix {
                    flake: flake.clone(),
                    package: package.clone(),
                },
                ResolvedEngineSource::NixDockerImage { flake, package } => {
                    EngineArtifactSource::NixDockerImage {
                        flake: flake.clone(),
                        package: package.clone(),
                    }
                }
            },
            platform: OciPlatform {
                operating_system: self.platform.operating_system.clone(),
                architecture: self.platform.architecture.clone(),
                variant: self.platform.variant.clone(),
            },
            preparation_policy: self.preparation_policy,
            launch_identity: self.launch_identity.clone(),
        }
    }
}

/// Catalog source of one selected engine artifact; the launch identity, not the source, decides
/// what runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedEngineSource {
    /// Rendered OCI reference, which may be a mutable local-development tag.
    OciImage { reference: String },

    /// Nix flake package the store path was built from, and the program it runs.
    Nix {
        flake: String,
        package: String,
        executable: String,
    },

    /// Docker image derivation package the store path was built from; its loader realizes the
    /// image the Docker runtime launches.
    NixDockerImage { flake: String, package: String },
}

impl ResolvedEngineSource {
    /// Packaging format of the source, which decides the runtimes able to launch it.
    pub(crate) fn kind(&self) -> dotcore_domain::WorkloadArtifactKind {
        match self {
            Self::OciImage { .. } => dotcore_domain::WorkloadArtifactKind::Oci,
            Self::Nix { .. } => dotcore_domain::WorkloadArtifactKind::Nix,
            // The Docker runtime launches the image the loader realizes.
            Self::NixDockerImage { .. } => dotcore_domain::WorkloadArtifactKind::Oci,
        }
    }

    fn from_artifact(artifact: &EngineArtifact) -> Result<Self, CatalogError> {
        match artifact.kind.as_ref() {
            Some(EngineArtifactKind::Oci(reference)) => Ok(Self::OciImage {
                reference: render_reference(reference)
                    .map_err(|error| CatalogError::Invalid(anyhow::Error::new(error)))?,
            }),
            Some(EngineArtifactKind::Nix(nix)) => {
                let flake = nix.flake.clone();
                let package = nix
                    .package
                    .clone()
                    .ok_or_else(|| invalid("local snapshot Nix engine artifact has no package"))?;
                match nix.provides.as_ref() {
                    Some(Provides::Executable(executable)) => Ok(Self::Nix {
                        flake,
                        package,
                        executable: executable.clone(),
                    }),
                    Some(Provides::DockerImage(_)) => Ok(Self::NixDockerImage { flake, package }),
                    None => Err(invalid(
                        "local snapshot Nix engine artifact names no program it provides",
                    )),
                }
            }
            None => Err(invalid("local snapshot engine has no OCI or Nix artifact")),
        }
    }
}

/// Immutable platform data for one selected engine artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEnginePlatformValue {
    pub operating_system: String,
    pub architecture: String,
    pub variant: Option<String>,
    pub accelerator: String,
}

/// Immutable request-input capabilities for one resolved configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModelInputCapabilities {
    pub image: Option<ResolvedModelImageInputCapacity>,
}

/// Immutable image-input limits for one resolved configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModelImageInputCapacity {
    pub maximum_image_count: u32,
    pub supported_media_types: Vec<i32>,
    pub maximum_encoded_bytes: u64,
    pub maximum_decoded_bytes: u64,
    pub maximum_width: u32,
    pub maximum_height: u32,
    pub maximum_pixels: u64,
    pub fixed_visual_tokens: u32,
    pub fixed_detail: i32,
}

impl From<Option<&ModelInputCapabilities>> for ResolvedModelInputCapabilities {
    fn from(capabilities: Option<&ModelInputCapabilities>) -> Self {
        Self {
            image: capabilities
                .and_then(|value| value.image_input_capacity.as_ref())
                .map(|capacity| ResolvedModelImageInputCapacity {
                    maximum_image_count: capacity.maximum_image_count,
                    supported_media_types: capacity.supported_media_types.clone(),
                    maximum_encoded_bytes: capacity.maximum_encoded_bytes,
                    maximum_decoded_bytes: capacity.maximum_decoded_bytes,
                    maximum_width: capacity.maximum_width,
                    maximum_height: capacity.maximum_height,
                    maximum_pixels: capacity.maximum_pixels,
                    fixed_visual_tokens: capacity.fixed_visual_tokens,
                    fixed_detail: capacity.fixed_detail,
                }),
        }
    }
}

/// Validates one retained local snapshot without consulting the catalog; its local-development
/// engine identity may be left for the hosting SiteNode when `resolution` allows it.
pub(crate) fn validate_deployment_snapshot_with(
    snapshot: &ModelDeploymentSnapshot,
    resolution: LaunchIdentityResolution,
) -> Result<(), CatalogError> {
    let Some(Source::Local(local)) = snapshot.source.as_ref() else {
        return Err(invalid("deployment snapshot is not local"));
    };
    validate_local_snapshot_with(local, resolution)
}

/// Validates one complete local snapshot that retains its engine launch identity.
pub(crate) fn validate_local_snapshot(
    local: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), CatalogError> {
    validate_local_snapshot_with(local, LaunchIdentityResolution::Retained)
}

/// Validates one complete local snapshot; with node resolution, a development engine leaves its
/// launch identity empty for the hosting SiteNode.
pub(crate) fn validate_local_snapshot_with(
    local: &ResolvedLocalModelDeploymentSnapshot,
    resolution: LaunchIdentityResolution,
) -> Result<(), CatalogError> {
    if local.model_id.is_empty()
        || local.configuration_id.is_empty()
        || local.profile_id.is_empty()
        || local.engine_artifact.is_none()
        || local.model_artifact.is_none()
        || local.hardware_support.is_none()
        || local.resolved_engine_state.is_none()
        || local.input_capabilities.is_none()
        || local.request_kinds.is_empty()
        || local.engine_parameter_values.is_none()
        || local.engine_parameter_schema.is_none()
        || local.engine_parameter_schema_id.is_empty()
        || local.desired_launch_plan.is_none()
    {
        return Err(invalid("local deployment snapshot is incomplete"));
    }
    let schema = local
        .engine_parameter_schema
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no engine parameter schema"))?;
    if schema.schema_id != local.engine_parameter_schema_id {
        return Err(invalid(
            "local deployment snapshot engine parameter schema identity does not match",
        ));
    }
    let values = local
        .engine_parameter_values
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no engine parameter values"))?;
    let mut parameter_errors = Vec::new();
    validate_parameter_values(
        values,
        &EngineRelease {
            parameter_schema: Some(schema.clone()),
            ..Default::default()
        },
        &local.profile_id,
        &mut parameter_errors,
    );
    if parameter_errors.into_iter().next().is_some() {
        return Err(invalid(
            "local deployment snapshot has invalid engine parameter values",
        ));
    }
    let engine = local
        .engine_artifact
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no engine artifact"))?;
    let state = local
        .resolved_engine_state
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no resolved engine state"))?;
    if engine.engine_kind != state.engine_kind {
        return Err(invalid(
            "local deployment engine kind and state payload disagree",
        ));
    }
    validated_engine_preparation_policy(engine, resolution)?;
    validate_hardware_support(local, engine)?;
    selected_target_memory_architecture(local)?;
    validate_sizing(local.sizing.as_ref(), values)?;
    validate_artifact_sources(local)?;
    validate_artifact_bindings(local)?;
    crate::model::engine::validate_snapshot(local).map_err(CatalogError::Invalid)?;
    Ok(())
}

/// Checks the retained hardware class: an identified class whose selector accepts the accelerator
/// vendor the engine artifact matched, since launch requirements are synthesized from both.
fn validate_hardware_support(
    local: &ResolvedLocalModelDeploymentSnapshot,
    engine: &dotcore_proto::dotcore::model::v1::ResolvedEngineArtifact,
) -> Result<(), CatalogError> {
    let support = local
        .hardware_support
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no hardware support"))?;
    let selector = support
        .selector
        .as_ref()
        .filter(|_| !support.hardware_class_id.is_empty())
        .ok_or_else(|| invalid("local deployment snapshot has an incomplete hardware class"))?;
    if !selector.accelerators.is_empty() && !selector.accelerators.contains(&engine.accelerator) {
        return Err(invalid(
            "local deployment snapshot hardware class does not select the matched accelerator",
        ));
    }
    Ok(())
}

/// Returns the retained engine preparation policy once its launch identity matches the retained
/// catalog source; a snapshot without that identity never launches from a tag or a flake. With
/// node resolution, a local-development tag may, and a development-source Nix package must, leave
/// its identity for the hosting SiteNode.
fn validated_engine_preparation_policy(
    engine: &dotcore_proto::dotcore::model::v1::ResolvedEngineArtifact,
    resolution: LaunchIdentityResolution,
) -> Result<ArtifactPreparationPolicy, CatalogError> {
    let invalid_identity = |error, context: &'static str| {
        CatalogError::Invalid(anyhow::Error::new(error).context(context))
    };
    let reference = match engine
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.kind.as_ref())
    {
        Some(EngineArtifactKind::Oci(reference)) => reference,
        Some(EngineArtifactKind::Nix(nix)) => {
            return validate_nix_launch_identity(
                engine.preparation_policy,
                nix,
                &engine.launch_identity,
                resolution,
            )
            .map_err(|error| {
                invalid_identity(
                    error,
                    "local deployment snapshot engine Nix store path is invalid",
                )
            });
        }
        None => {
            return Err(invalid(
                "local deployment snapshot engine artifact has no OCI or Nix source",
            ));
        }
    };
    if resolution == LaunchIdentityResolution::NodeResolved
        && node_resolved_selection(
            engine.preparation_policy,
            reference,
            &engine.launch_identity,
        )
    {
        return Ok(ArtifactPreparationPolicy::LocalDevelopment);
    }
    validate_launch_identity(
        engine.preparation_policy,
        reference,
        &engine.launch_identity,
    )
    .map_err(|error| {
        invalid_identity(
            error,
            "local deployment snapshot engine image launch identity is invalid",
        )
    })
}

/// Returns the retained execution-target memory architecture, rejecting absent or unspecified values.
pub(crate) fn selected_target_memory_architecture(
    local: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<MemoryArchitecture, CatalogError> {
    let selected_target = local
        .selected_target
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no selected target"))?;
    MemoryArchitecture::from_proto(selected_target.accelerator_memory_architecture()).ok_or_else(
        || invalid("local deployment selected target has no accelerator memory architecture"),
    )
}

/// Requires every derived parameter id to name one retained engine parameter value.
fn validate_sizing(
    sizing: Option<&ResolvedSizing>,
    values: &ParameterValues,
) -> Result<(), CatalogError> {
    let Some(sizing) = sizing else {
        return Ok(());
    };
    let mut derived_ids = BTreeSet::new();
    if !sizing
        .derived_engine_parameter_ids
        .iter()
        .all(|parameter_id| {
            derived_ids.insert(parameter_id)
                && values
                    .values
                    .iter()
                    .any(|value| &value.parameter_id == parameter_id)
        })
    {
        return Err(invalid(
            "local deployment sizing names a derived engine parameter without a retained value",
        ));
    }
    Ok(())
}

fn validate_artifact_sources(
    local: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), CatalogError> {
    let model_source = local
        .model_artifact
        .as_ref()
        .expect("checked model artifact")
        .source
        .as_ref();
    let invalid_model_source = model_source
        .is_none_or(|source| !dotcore_catalog::valid_pinned_hugging_face_artifact(source));
    let invalid_auxiliary_source = local.auxiliary_artifacts.iter().any(|artifact| {
        artifact
            .source
            .as_ref()
            .is_none_or(|source| !dotcore_catalog::valid_pinned_hugging_face_artifact(source))
    });
    if invalid_model_source || invalid_auxiliary_source {
        return Err(invalid("local snapshot has an invalid artifact source"));
    }
    Ok(())
}

fn validate_artifact_bindings(
    local: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), CatalogError> {
    let plan = local
        .desired_launch_plan
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no desired launch plan"))?;
    let model_id = local
        .model_artifact
        .as_ref()
        .ok_or_else(|| invalid("local deployment snapshot has no model artifact"))?
        .artifact_id
        .as_str();
    let auxiliary = local
        .auxiliary_artifacts
        .iter()
        .map(|artifact| artifact.auxiliary_artifact_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    for binding in &plan.artifact_bindings {
        let valid_artifact = match binding.artifact.as_ref() {
            Some(BoundArtifact::ModelArtifactId(id)) => id == model_id,
            Some(BoundArtifact::AuxiliaryArtifactId(id)) => auxiliary.contains(id.as_str()),
            None => false,
        };
        if !ids.insert(binding.binding_id.as_str())
            || binding.binding_id.is_empty()
            || !valid_artifact
            || !binding.read_only
        {
            return Err(invalid(
                "local deployment launch plan has an invalid artifact binding",
            ));
        }
    }
    if plan
        .arguments
        .iter()
        .any(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::ArtifactBindingId(id)) => !ids.contains(id.as_str()),
            Some(ArgumentValue::ServiceHost(name) | ArgumentValue::ServicePort(name)) => {
                name.is_empty()
            }
            Some(ArgumentValue::Literal(_) | ArgumentValue::EngineExecutable(_)) => false,
            None => true,
        })
    {
        return Err(invalid(
            "local deployment launch plan has an invalid argument",
        ));
    }
    Ok(())
}

/// Converts a retained Hugging Face source into the artifact manager's pinned source.
fn model_artifact_source(
    source: &HuggingFaceArtifactSource,
) -> Result<ModelArtifactSource, CatalogError> {
    match source.content.as_ref() {
        Some(HuggingFaceContent::File(file)) => Ok(ModelArtifactSource::HuggingFaceFile {
            repository: source.repository.clone(),
            revision: source.revision.clone(),
            path: file.path.clone(),
            provenance_digest: file.provenance_digest.clone(),
        }),
        Some(HuggingFaceContent::Snapshot(snapshot)) => {
            let files = snapshot
                .files
                .iter()
                .map(|file| (file.path.clone(), file.provenance_digest.clone()))
                .collect::<BTreeMap<_, _>>();
            if files.len() != snapshot.files.len() {
                return Err(invalid("local snapshot has duplicate artifact paths"));
            }
            Ok(ModelArtifactSource::HuggingFaceSnapshot {
                repository: source.repository.clone(),
                revision: source.revision.clone(),
                files,
            })
        }
        None => Err(invalid("local snapshot artifact source has no content")),
    }
}

fn invalid(message: &'static str) -> CatalogError {
    CatalogError::Invalid(anyhow::anyhow!(message))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dotcore_artifact::{EngineArtifactPreparation, EngineArtifactSource, OciPlatform};
    use dotcore_catalog::CatalogError;
    use dotcore_proto::dotcore::{
        catalog::v1::{
            HuggingFaceArtifactSource, HuggingFaceSnapshotArtifact, HuggingFaceSnapshotFile,
            ModelImageInputCapacity, ModelOptionValues,
            hugging_face_artifact_source::Content as HuggingFaceContent,
        },
        deployment::v1::ArtifactPreparationPolicy,
        model::v1::{
            ModelDeploymentSnapshot, ResolvedModelOption, model_deployment_snapshot::Source,
        },
        observation::v1::AcceleratorMemoryArchitecture,
    };

    use super::{ResolvedEngineSource, ResolvedModelLaunch};
    use crate::test_support::local_model_snapshot;

    #[test]
    fn engine_preparation_carries_the_platform_policy_and_identity_of_the_artifact() {
        let mut engine = ResolvedModelLaunch::from_snapshot(&local_model_snapshot("fixture-model"))
            .expect("resolved launch")
            .engine;
        engine.platform.variant = Some("v8".to_owned());
        engine.preparation_policy = ArtifactPreparationPolicy::LocalDevelopment;
        engine.launch_identity = "sha256:".to_owned() + &"d".repeat(64);

        assert_eq!(
            engine.preparation(),
            EngineArtifactPreparation {
                source: EngineArtifactSource::OciImage,
                platform: OciPlatform {
                    operating_system: engine.platform.operating_system.clone(),
                    architecture: engine.platform.architecture.clone(),
                    variant: Some("v8".to_owned()),
                },
                preparation_policy: ArtifactPreparationPolicy::LocalDevelopment,
                launch_identity: "sha256:".to_owned() + &"d".repeat(64),
            }
        );

        engine.source = ResolvedEngineSource::Nix {
            flake: "self".to_owned(),
            package: "llama-cpp".to_owned(),
            executable: "llama-server".to_owned(),
        };
        assert_eq!(
            engine.preparation().source,
            EngineArtifactSource::Nix {
                flake: "self".to_owned(),
                package: "llama-cpp".to_owned(),
            }
        );
    }

    #[test]
    fn duplicate_snapshot_paths_are_rejected() {
        let source = HuggingFaceArtifactSource {
            repository: "dotcore/fixture".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            content: Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
                files: vec![
                    HuggingFaceSnapshotFile {
                        path: "config.json".to_owned(),
                        provenance_digest: "11".repeat(32),
                    },
                    HuggingFaceSnapshotFile {
                        path: "config.json".to_owned(),
                        provenance_digest: "22".repeat(32),
                    },
                ],
            })),
        };

        assert!(matches!(
            super::model_artifact_source(&source),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn resolved_launch_preserves_the_exact_snapshot_manifest() {
        let mut snapshot = local_model_snapshot("fixture");
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local snapshot");
        };
        local
            .model_artifact
            .as_mut()
            .expect("model artifact")
            .source
            .as_mut()
            .expect("artifact source")
            .content = Some(HuggingFaceContent::Snapshot(HuggingFaceSnapshotArtifact {
            files: vec![
                HuggingFaceSnapshotFile {
                    path: "tokenizer/config.json".to_owned(),
                    provenance_digest: "22".repeat(32),
                },
                HuggingFaceSnapshotFile {
                    path: "config.json".to_owned(),
                    provenance_digest: "11".repeat(32),
                },
            ],
        }));

        let resolved = ResolvedModelLaunch::from_snapshot(&snapshot).expect("resolved snapshot");

        assert_eq!(
            resolved.artifact,
            dotcore_artifact::ModelArtifactSource::HuggingFaceSnapshot {
                repository: "dotcore/fixture".to_owned(),
                revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                files: BTreeMap::from([
                    ("config.json".to_owned(), "11".repeat(32)),
                    ("tokenizer/config.json".to_owned(), "22".repeat(32)),
                ]),
            }
        );
    }

    #[test]
    fn retained_snapshot_validation_rejects_every_mutated_canonical_dimension() {
        assert_rejected(|local| {
            local.resolved_options.push(ResolvedModelOption {
                values: Some(ModelOptionValues {
                    option_id: "unexpected".to_owned(),
                    values: Vec::new(),
                }),
            });
        });
        assert_rejected(|local| {
            let state = local.resolved_engine_state.as_mut().expect("engine state");
            let dotcore_proto::dotcore::engine::v1::resolved_engine_model_state::State::LlamaCpp(
                llama,
            ) = state.state.as_mut().expect("llama state")
            else {
                panic!("llama state");
            };
            llama
                .fixed_settings
                .as_mut()
                .expect("fixed settings")
                .gpu_layers = 1;
        });
        assert_rejected(|local| {
            local
                .model_artifact
                .as_mut()
                .expect("model artifact")
                .artifact_id = "changed".to_owned();
        });
        assert_rejected(|local| {
            local
                .model_artifact
                .as_mut()
                .expect("model artifact")
                .source
                .as_mut()
                .expect("artifact source")
                .repository = "../unsafe".to_owned();
        });
        assert_rejected(|local| {
            let source = local
                .model_artifact
                .as_mut()
                .expect("model artifact")
                .source
                .as_mut()
                .expect("artifact source");
            let Some(HuggingFaceContent::File(file)) = source.content.as_mut() else {
                panic!("file source");
            };
            file.path = "../unsafe".to_owned();
        });
        assert_rejected(|local| {
            let source = local
                .model_artifact
                .as_mut()
                .expect("model artifact")
                .source
                .as_mut()
                .expect("artifact source");
            let Some(HuggingFaceContent::File(file)) = source.content.as_mut() else {
                panic!("file source");
            };
            file.provenance_digest = "invalid".to_owned();
        });
        assert_rejected(|local| {
            local
                .input_capabilities
                .as_mut()
                .expect("input capabilities")
                .image_input_capacity = Some(ModelImageInputCapacity::default());
        });
        assert_rejected(|local| {
            local
                .desired_launch_plan
                .as_mut()
                .expect("plan")
                .artifact_bindings[0]
                .binding_id = "changed".to_owned();
        });
        assert_rejected(|local| {
            local
                .desired_launch_plan
                .as_mut()
                .expect("plan")
                .arguments
                .swap(0, 1);
        });
        assert_rejected(|local| {
            local
                .desired_launch_plan
                .as_mut()
                .expect("plan")
                .format_version = 1;
        });
        assert_rejected(|local| {
            local.engine_artifact.as_mut().expect("engine").engine_kind = 0;
        });
        assert_rejected(|local| {
            local
                .resolved_engine_state
                .as_mut()
                .expect("engine state")
                .semantics_version += 1;
        });
        assert_rejected(|local| local.hardware_support = None);
        assert_rejected(|local| {
            local
                .hardware_support
                .as_mut()
                .expect("hardware support")
                .hardware_class_id
                .clear();
        });
        assert_rejected(|local| {
            local
                .hardware_support
                .as_mut()
                .expect("hardware support")
                .selector
                .as_mut()
                .expect("hardware class selector")
                .accelerators = vec!["apple".to_owned()];
        });
    }

    #[test]
    fn retained_llama_cpp_plan_in_an_older_format_reports_the_recreate_action() {
        let mut snapshot = local_model_snapshot("fixture-model");
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local source");
        };
        local
            .desired_launch_plan
            .as_mut()
            .expect("plan")
            .format_version = 1;

        assert!(
            format!(
                "{:#}",
                super::validate_deployment_snapshot_with(
                    &snapshot,
                    super::LaunchIdentityResolution::Retained
                )
                .expect_err("old plan format")
            )
            .ends_with(
                "llama.cpp deployment was created with launch plan format 1, which this Dotcore \
                 version no longer supports; delete and recreate the deployment"
            )
        );
    }

    #[test]
    fn retained_snapshot_requires_a_classified_selected_target_memory_architecture() {
        let rejection = |mutate: fn(
            &mut dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
        )| {
            let mut snapshot = local_model_snapshot("fixture-model");
            let Some(Source::Local(local)) = snapshot.source.as_mut() else {
                panic!("local source");
            };
            mutate(local);
            format!(
                "{:#}",
                super::validate_deployment_snapshot_with(
                    &snapshot,
                    super::LaunchIdentityResolution::Retained
                )
                .expect_err("snapshot rejected")
            )
        };

        assert!(
            rejection(|local| local.selected_target = None)
                .ends_with("local deployment snapshot has no selected target"),
        );
        assert!(
            rejection(|local| {
                local
                    .selected_target
                    .as_mut()
                    .expect("selected target")
                    .accelerator_memory_architecture =
                    AcceleratorMemoryArchitecture::Unspecified as i32;
            })
            .ends_with("local deployment selected target has no accelerator memory architecture"),
        );
    }

    fn assert_rejected(
        mutate: impl FnOnce(
            &mut dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot,
        ),
    ) {
        let mut snapshot = local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("local source") {
            Source::Local(local) => local,
            Source::Upstream(_) => panic!("local source"),
        };
        mutate(local);
        assert!(
            ResolvedModelLaunch::from_snapshot(&snapshot).is_err(),
            "mutated snapshot was accepted: {snapshot:?}"
        );
    }

    #[test]
    fn unsupported_retained_engine_kind_never_falls_back_to_llama_cpp() {
        let mut snapshot = local_model_snapshot("fixture-model");
        let local = match snapshot.source.as_mut().expect("local source") {
            Source::Local(local) => local,
            Source::Upstream(_) => panic!("local source"),
        };
        local.engine_artifact.as_mut().expect("engine").engine_kind = 0;
        local
            .resolved_engine_state
            .as_mut()
            .expect("engine state")
            .engine_kind = 0;

        let error = ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect_err("unsupported engine kind")
            .to_string();

        assert!(
            error.contains("unsupported engine kind"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn retained_snapshot_rejects_malformed_platform_components_without_panicking() {
        for (operating_system, architecture, variant, expected) in [
            ("windows", "arm64", None, "unsupported operating system"),
            ("linux", "riscv64", None, "unsupported CPU architecture"),
            ("linux", "arm64", Some("v8"), "unsupported CPU variant"),
        ] {
            let mut snapshot = local_model_snapshot("fixture-model");
            let Source::Local(local) = snapshot.source.as_mut().expect("local source") else {
                panic!("local snapshot");
            };
            let platform = local
                .engine_artifact
                .as_mut()
                .expect("engine")
                .platform
                .as_mut()
                .expect("matched platform");
            platform.operating_system = operating_system.to_owned();
            platform.architecture = architecture.to_owned();
            platform.variant = variant.map(str::to_owned);

            let error = ResolvedModelLaunch::from_snapshot(&snapshot)
                .expect_err("malformed retained platform")
                .to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn snapshot_retaining_selectors_requires_recreation_without_silent_rewrite() {
        let mut snapshot: ModelDeploymentSnapshot = local_model_snapshot("fixture-model");
        let Source::Local(local) = snapshot.source.as_mut().expect("local source") else {
            panic!("local snapshot");
        };
        local
            .engine_artifact
            .as_mut()
            .and_then(|engine| engine.artifact.as_mut())
            .expect("retained engine artifact")
            .selectors
            .push(dotcore_proto::dotcore::catalog::v1::Selector {
                platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: None,
                }),
                accelerators: vec!["nvidia".to_owned()],
                minimum_nvidia_driver_major_version: 580,
                ..Default::default()
            });

        let error = ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect_err("snapshot retaining selectors must require recreation")
            .to_string();
        assert_eq!(
            error,
            "Invalid catalog: local snapshot engine artifact retains selectors instead of matched target facts"
        );
        let Source::Local(local) = snapshot.source.expect("local source") else {
            panic!("local snapshot");
        };
        assert_eq!(
            local
                .engine_artifact
                .and_then(|engine| engine.artifact)
                .expect("retained engine artifact")
                .selectors
                .len(),
            1
        );
    }

    #[test]
    fn digest_pinned_engine_launches_from_its_canonical_digest_reference() {
        let engine = ResolvedModelLaunch::from_snapshot(&local_model_snapshot("fixture-model"))
            .expect("retained snapshot")
            .engine;

        assert_eq!(
            engine.source,
            ResolvedEngineSource::OciImage {
                reference: ENGINE_DIGEST_REFERENCE.to_owned()
            }
        );
        assert_eq!(engine.launch_identity, ENGINE_DIGEST_REFERENCE);
        assert_eq!(
            engine.preparation_policy,
            ArtifactPreparationPolicy::ImmutableRemote
        );
    }

    #[test]
    fn local_development_engine_launches_from_its_retained_image_id() {
        let mut snapshot = local_model_snapshot("fixture-model");
        retain_engine_identity(
            &mut snapshot,
            Some("dev"),
            ArtifactPreparationPolicy::LocalDevelopment as i32,
            ENGINE_IMAGE_ID,
        );

        let launch = ResolvedModelLaunch::from_snapshot(&snapshot).expect("retained snapshot");

        assert_eq!(launch.engine.launch_identity, ENGINE_IMAGE_ID);
        assert_eq!(
            launch.engine.preparation_policy,
            ArtifactPreparationPolicy::LocalDevelopment
        );
        assert_eq!(
            launch.engine.source,
            ResolvedEngineSource::OciImage {
                reference: "registry.dotcore.test/fixture:dev".to_owned()
            }
        );
        assert_eq!(
            launch.telemetry_inputs().engine_artifact_reference,
            "registry.dotcore.test/fixture:dev"
        );
    }

    #[test]
    fn nix_engine_telemetry_identifies_the_engine_and_the_resolved_hardware_class() {
        let snapshot = crate::test_support::nix::apple_metal_snapshot("fixture-model");

        let inputs = ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("retained snapshot")
            .telemetry_inputs();

        assert_eq!(
            (
                inputs.engine_release_id.as_str(),
                inputs.engine_artifact_id.as_str(),
                inputs.engine_artifact_reference.as_str(),
                inputs.hardware_class_id.as_str(),
            ),
            (
                "llama-cpp-2026-11-13",
                "metal",
                dotcore_domain::test_support::nix::STORE_PATH,
                "apple",
            )
        );
    }

    #[test]
    fn telemetry_records_custom_arguments_with_sensitive_values_masked() {
        let snapshot = local_model_snapshot("fixture-model");
        let mut launch = ResolvedModelLaunch::from_snapshot(&snapshot).expect("retained snapshot");
        launch.snapshot.custom_argument_tokens = ["--seed", "7", "--api-key", "sk-1", "--verbose"]
            .map(str::to_owned)
            .to_vec();

        let inputs = launch.telemetry_inputs();

        assert_eq!(
            inputs.custom_argument_tokens,
            ["--seed", "7", "--api-key", "<redacted>", "--verbose"]
        );
    }

    #[test]
    fn retained_snapshot_without_a_matching_engine_launch_identity_is_rejected() {
        let remote = ArtifactPreparationPolicy::ImmutableRemote as i32;
        let local = ArtifactPreparationPolicy::LocalDevelopment as i32;
        let cases = [
            (
                None,
                ArtifactPreparationPolicy::Unspecified as i32,
                "",
                "artifact selection retains no preparation policy",
            ),
            (
                Some("dev"),
                ArtifactPreparationPolicy::Unspecified as i32,
                "",
                "artifact selection retains no preparation policy",
            ),
            (
                Some("dev"),
                remote,
                "registry.dotcore.test/fixture:dev",
                "immutable remote artifact requires a digest reference",
            ),
            (
                Some("dev"),
                local,
                "sha256:abc",
                "local-development launch identity is not a SHA-256 image ID",
            ),
            (
                Some("dev"),
                local,
                "registry.dotcore.test/fixture:dev",
                "local-development launch identity is not a SHA-256 image ID",
            ),
            (
                None,
                remote,
                "registry.dotcore.test/other@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "immutable remote launch identity is not the canonical digest reference",
            ),
            (
                None,
                local,
                ENGINE_IMAGE_ID,
                "local-development artifact requires a tag-only reference",
            ),
        ];

        for (tag, policy, identity, cause) in cases {
            let mut snapshot = local_model_snapshot("fixture-model");
            retain_engine_identity(&mut snapshot, tag, policy, identity);

            let error = super::validate_deployment_snapshot_with(
                &snapshot,
                super::LaunchIdentityResolution::Retained,
            )
            .expect_err("snapshot rejected");
            assert_eq!(
                error.to_string(),
                "Invalid catalog: local deployment snapshot engine image launch identity is invalid"
            );
            assert_eq!(
                anyhow::Error::new(error).root_cause().to_string(),
                cause,
                "tag {tag:?}, policy {policy}, identity {identity:?}"
            );
            assert!(ResolvedModelLaunch::from_snapshot(&snapshot).is_err());
        }
    }

    #[test]
    fn canonical_retained_snapshot_is_accepted_without_catalog_access() {
        let snapshot: ModelDeploymentSnapshot = local_model_snapshot("fixture-model");
        let launch = ResolvedModelLaunch::from_snapshot(&snapshot).expect("retained snapshot");

        assert_eq!(launch.model_id.0, "fixture-model");
        assert_eq!(
            launch.plan,
            snapshot
                .source
                .and_then(|source| match source {
                    Source::Local(local) => local.desired_launch_plan,
                    Source::Upstream(_) => None,
                })
                .expect("stored plan")
        );
    }

    /// Replaces the retained engine reference with a tag-only one when a tag is given, then sets
    /// the retained policy and launch identity.
    fn retain_engine_identity(
        snapshot: &mut ModelDeploymentSnapshot,
        tag: Option<&str>,
        policy: i32,
        launch_identity: &str,
    ) {
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local snapshot");
        };
        let engine = local.engine_artifact.as_mut().expect("engine");
        if let Some(tag) = tag {
            let reference = engine
                .artifact
                .as_mut()
                .and_then(dotcore_domain::test_support::oci::reference_mut)
                .expect("engine OCI reference");
            reference.tag = Some(tag.to_owned());
            reference.digest = None;
        }
        engine.preparation_policy = policy;
        engine.launch_identity = launch_identity.to_owned();
    }

    const ENGINE_DIGEST_REFERENCE: &str = "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const ENGINE_IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
}
