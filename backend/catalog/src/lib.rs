//! Immutable catalog declarations for models, engine releases, hardware classes, applications,
//! and upstream model templates, plus the read-only view the workload controllers query.
//!
//! Declarations are read from one or two YAML roots and validated before they are frozen
//! into an immutable value, so callers always read one complete, validated source rather
//! than the raw files. Lookups against that value are deterministic, and a captured
//! deployment snapshot is a fully resolved, immutable view of one selected profile and
//! artifact binding.
//!
//! The catalog sits below every controller: it imports only domain primitives and protocol
//! bindings, and engine-owned payload rules reach it through an injected validator.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

use std::{collections::BTreeMap, error::Error as StdError, fmt, path::Path};

use dotcore_proto::dotcore::catalog::v1::{
    Application, EngineArtifact, EngineParameterValues, EngineRelease, HardwareClass,
    HardwareClassRecommendations, HardwareRequirements, Model, ModelArtifact, ModelArtifactBinding,
    ModelEngineConfiguration, ModelOptionValues, ModelProfile, UpstreamModelTemplate,
};
#[cfg(test)]
use dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot;

pub mod options;
mod selector;
pub mod service;
pub mod shared_parameters;
mod sizing;
mod source;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod typed_parameters;
mod validation;

pub use selector::{SelectorRelation, selector_relation};
pub use sizing::{
    ACCELERATOR_MEMORY_RESOURCE, HOST_MEMORY_RESOURCE, ScalarResourceEstimate, SizingError,
    SizingFitModel,
};
use sizing::{CompiledSizing, SizingKey};
use source::{CatalogSourceRecords, CatalogSourceRole, load_catalog_source};
pub use validation::CatalogDiagnostic;
pub use validation::EngineDeclarationValidator;
pub use validation::{
    overlay_parameter_defaults, valid_configuration_value, valid_environment_variable,
    valid_pinned_hugging_face_artifact, validate_application_with_policy,
    validate_parameter_values, validate_supplied_parameter_values,
    validate_upstream_model_template,
};

/// A complete, immutable catalog validated before it is exposed.
#[derive(Clone)]
pub struct Catalog {
    models: BTreeMap<String, Model>,
    engines: BTreeMap<String, EngineRelease>,
    hardware_classes: BTreeMap<String, HardwareClass>,
    recommendations: BTreeMap<String, HardwareClassRecommendations>,
    applications: BTreeMap<String, Application>,
    upstream_model_templates: BTreeMap<String, UpstreamModelTemplate>,
    sizing: BTreeMap<SizingKey, CompiledSizing>,
    engine_defaults: validation::ConfigurationEngineDefaults,
}

impl Catalog {
    /// Loads and validates one catalog root into an immutable read view.
    pub fn load(
        catalog_root: &Path,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        Self::load_with_policy(
            catalog_root,
            OciArtifactPolicy::DigestRequired,
            engine_validator,
        )
    }

    /// Loads one catalog root under an explicit OCI artifact policy.
    pub fn load_with_policy(
        catalog_root: &Path,
        oci_artifact_policy: OciArtifactPolicy,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        Self::load_from_sources(
            &[("", catalog_root, CatalogSourceRole::Master)],
            oci_artifact_policy,
            engine_validator,
        )
    }

    /// Builds one catalog view from the master and site declaration sources.
    pub fn load_sources(
        master_catalog_root: &Path,
        site_catalog_root: &Path,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        Self::load_sources_with_policy(
            master_catalog_root,
            site_catalog_root,
            OciArtifactPolicy::DigestRequired,
            engine_validator,
        )
    }

    /// Builds one catalog view from both sources under an explicit OCI artifact policy.
    pub fn load_sources_with_policy(
        master_catalog_root: &Path,
        site_catalog_root: &Path,
        oci_artifact_policy: OciArtifactPolicy,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        Self::load_from_sources(
            &[
                (
                    "master catalog",
                    master_catalog_root,
                    CatalogSourceRole::Master,
                ),
                ("site catalog", site_catalog_root, CatalogSourceRole::Layer),
            ],
            oci_artifact_policy,
            engine_validator,
        )
    }

    /// Builds one private development catalog view from immutable master and additive declarations.
    pub fn load_development_sources(
        master_catalog_root: &Path,
        development_catalog_root: &Path,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        Self::load_from_sources(
            &[
                (
                    "master catalog",
                    master_catalog_root,
                    CatalogSourceRole::Master,
                ),
                (
                    "development catalog",
                    development_catalog_root,
                    CatalogSourceRole::Layer,
                ),
            ],
            OciArtifactPolicy::Development,
            engine_validator,
        )
    }

    /// Returns a protobuf-only declaration projection for read-only consumers.
    ///
    /// This projection intentionally excludes private sizing. Clone the catalog or reload its YAML
    /// sources when a complete catalog copy is required.
    pub fn protobuf_records_without_sizing(&self) -> ProtobufCatalogRecords {
        ProtobufCatalogRecords {
            models: self.models.values().cloned().collect(),
            engine_releases: self.engines.values().cloned().collect(),
            hardware_classes: self.hardware_classes.values().cloned().collect(),
            applications: self.applications.values().cloned().collect(),
            upstream_model_templates: self.upstream_model_templates.values().cloned().collect(),
        }
    }

    /// Returns cloned model declarations in deterministic identity order.
    pub fn list_model_releases(&self) -> Vec<Model> {
        self.models.values().cloned().collect()
    }

    /// Returns cloned engine release declarations in deterministic identity order.
    pub fn list_engine_releases(&self) -> Vec<EngineRelease> {
        self.engines.values().cloned().collect()
    }

    /// Returns cloned hardware class declarations in deterministic identity order.
    pub fn list_hardware_classes(&self) -> Vec<HardwareClass> {
        self.hardware_classes.values().cloned().collect()
    }

    /// Returns the model recommendations of every hardware class that declares them, ordered by
    /// class id, each list in its declared priority order.
    pub fn list_hardware_class_recommendations(&self) -> Vec<HardwareClassRecommendations> {
        self.recommendations.values().cloned().collect()
    }

    /// Returns cloned application declarations in deterministic identity order.
    pub fn list_application_releases(&self) -> Vec<Application> {
        self.applications.values().cloned().collect()
    }

    /// Returns cloned upstream model templates in deterministic identity order.
    pub fn list_upstream_model_templates(&self) -> Vec<UpstreamModelTemplate> {
        self.upstream_model_templates.values().cloned().collect()
    }

    /// Returns one cloned upstream model template for controller-owned snapshot construction.
    pub fn upstream_model_template(
        &self,
        upstream_model_template_id: &str,
    ) -> Option<UpstreamModelTemplate> {
        self.upstream_model_templates
            .get(upstream_model_template_id)
            .cloned()
    }

    /// Returns model-local profiles for candidate selection.
    pub fn profiles_for_model(&self, model_id: &CatalogModelId) -> Vec<ModelProfile> {
        self.models
            .get(&model_id.0)
            .map(|model| model.profiles.clone())
            .unwrap_or_default()
    }

    /// Returns the profile-selected artifact binding and its engine parameter defaults: the
    /// profile's defaults, then the engine default of each parameter the profile leaves out. An
    /// engine default is the one the engine derives from the profile's configuration, else the
    /// release's. Parameters with neither stay out for resolution to fill or derive.
    pub fn local_defaults_for(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
    ) -> Option<(String, EngineParameterValues)> {
        let (model, profile) = self.selected_model_profile(model_id, profile_id)?;
        let engine_defaults = self
            .engine_defaults
            .get(&(model.model_id.clone(), profile.configuration_id.clone()))
            .cloned()
            .unwrap_or_default();
        Some((
            profile.default_model_artifact_binding_id.clone(),
            overlay_parameter_defaults(
                profile.engine_parameter_defaults.as_ref()?,
                &engine_defaults,
            ),
        ))
    }

    /// Returns sparse option defaults declared by one profile.
    pub fn local_option_defaults_for(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
    ) -> Option<Vec<ModelOptionValues>> {
        let (_, profile) = self.selected_model_profile(model_id, profile_id)?;
        Some(profile.option_defaults.clone())
    }

    /// Returns the validated immutable declarations of one local selection, with every approved
    /// engine artifact and supported hardware class of its configuration. Choosing one artifact
    /// and one class for a target is the inference controller's selection.
    pub fn local_model_declarations(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
        binding_id: &str,
    ) -> Result<CatalogLocalModelDeclarations, CatalogError> {
        let (model, profile) = self
            .selected_model_profile(model_id, profile_id)
            .ok_or_else(|| {
                CatalogError::Invalid(anyhow::anyhow!("selected model profile is stale"))
            })?;
        let configuration = model
            .configurations
            .iter()
            .find(|value| value.configuration_id == profile.configuration_id)
            .ok_or_else(|| {
                CatalogError::Invalid(anyhow::anyhow!("selected engine configuration is stale"))
            })?;
        let binding = configuration
            .artifact_bindings
            .iter()
            .find(|value| value.binding_id == binding_id)
            .ok_or_else(|| {
                CatalogError::Invalid(anyhow::anyhow!(
                    "selected model artifact binding is not supported by the profile configuration"
                ))
            })?;
        let approved_engine_artifacts = configuration
            .engine_artifacts
            .iter()
            .map(|reference| {
                let engine_release =
                    self.engines
                        .get(&reference.engine_release_id)
                        .ok_or_else(|| {
                            CatalogError::Invalid(anyhow::anyhow!(
                                "selected engine release is stale"
                            ))
                        })?;
                let artifact = engine_release
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.artifact_id == reference.engine_artifact_id)
                    .ok_or_else(|| {
                        CatalogError::Invalid(anyhow::anyhow!(
                            "selected approved engine artifact is stale"
                        ))
                    })?;
                Ok(ApprovedEngineArtifact {
                    engine_release: engine_release.clone(),
                    artifact: artifact.clone(),
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;
        let engine_kind = approved_engine_artifacts
            .first()
            .ok_or_else(|| {
                CatalogError::Invalid(anyhow::anyhow!(
                    "selected engine configuration has no approved engine artifacts"
                ))
            })?
            .engine_release
            .engine_kind;
        let model_artifact = model
            .artifacts
            .iter()
            .find(|value| value.artifact_id == binding.model_artifact_id)
            .ok_or_else(|| {
                CatalogError::Invalid(anyhow::anyhow!("selected model artifact is stale"))
            })?;
        let hardware_support = configuration
            .hardware_support
            .iter()
            .map(|support| {
                let hardware_class = self
                    .hardware_classes
                    .get(&support.hardware_class_id)
                    .ok_or_else(|| {
                        CatalogError::Invalid(anyhow::anyhow!(
                            "selected configuration's hardware class is stale"
                        ))
                    })?;
                Ok(SupportedHardwareClass {
                    hardware_class: hardware_class.clone(),
                    hardware_requirements: support.hardware_requirements,
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;

        Ok(CatalogLocalModelDeclarations {
            model: model.clone(),
            profile: profile.clone(),
            configuration: configuration.clone(),
            binding: binding.clone(),
            engine_kind,
            approved_engine_artifacts,
            hardware_support,
            model_artifact: model_artifact.clone(),
        })
    }

    /// Derives the engine parameter values a local selection omitted on one hardware class, in
    /// parameter id order. A binding without sizing derives nothing.
    pub fn derive_local_parameters(
        &self,
        model_id: &str,
        configuration_id: &str,
        hardware_class_id: &str,
        binding_id: &str,
        engine_parameter_values: &EngineParameterValues,
        option_values: &[ModelOptionValues],
    ) -> Result<Vec<(String, u64)>, SizingError> {
        let key = SizingKey {
            model_id: model_id.to_owned(),
            configuration_id: configuration_id.to_owned(),
            hardware_class_id: hardware_class_id.to_owned(),
            binding_id: binding_id.to_owned(),
        };
        match self.sizing.get(&key) {
            Some(sizing) => sizing.derive(engine_parameter_values, option_values),
            None => Ok(Vec::new()),
        }
    }

    /// Evaluates private sizing from a concrete local selection on one hardware class and its
    /// normalized values.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_local_sizing_inputs(
        &self,
        model_id: &str,
        configuration_id: &str,
        hardware_class_id: &str,
        binding_id: &str,
        resolved_model_artifact_id: Option<&str>,
        resolved_auxiliary_artifact_ids: &[String],
        engine_parameter_values: &EngineParameterValues,
        option_values: &[ModelOptionValues],
    ) -> Result<Vec<ScalarResourceEstimate>, SizingError> {
        let key = SizingKey {
            model_id: model_id.to_owned(),
            configuration_id: configuration_id.to_owned(),
            hardware_class_id: hardware_class_id.to_owned(),
            binding_id: binding_id.to_owned(),
        };
        let Some(sizing) = self.sizing.get(&key) else {
            return Ok(Vec::new());
        };
        let binding = self
            .models
            .get(model_id)
            .and_then(|model| {
                model
                    .configurations
                    .iter()
                    .find(|configuration| configuration.configuration_id == configuration_id)
            })
            .and_then(|configuration| {
                configuration
                    .artifact_bindings
                    .iter()
                    .find(|binding| binding.binding_id == binding_id)
            })
            .ok_or_else(|| SizingError::evaluation("resolved sizing binding is unavailable"))?;
        if resolved_model_artifact_id != Some(binding.model_artifact_id.as_str())
            || !binding
                .auxiliary_artifact_ids
                .iter()
                .all(|id| resolved_auxiliary_artifact_ids.contains(id))
        {
            return Err(SizingError::evaluation(
                "resolved local selection does not match sizing binding",
            ));
        }

        sizing.evaluate(engine_parameter_values, option_values)
    }

    /// Builds a linear fit model for one binding's sizing on one hardware class, for fitting
    /// constants from telemetry or checking the stored ones against it. An inline equation may
    /// also use the binding's declared parameters and the extra symbols.
    #[allow(clippy::too_many_arguments)]
    pub fn sizing_fit(
        &self,
        model_id: &str,
        configuration_id: &str,
        hardware_class_id: &str,
        binding_id: &str,
        equation: SizingFitEquation<'_>,
        unknowns: &[String],
        extra_symbols: &[String],
    ) -> Result<SizingFitTarget, SizingError> {
        let configuration = self
            .models
            .get(model_id)
            .and_then(|model| {
                model
                    .configurations
                    .iter()
                    .find(|configuration| configuration.configuration_id == configuration_id)
            })
            .ok_or_else(|| SizingError::evaluation("model configuration is not in the catalog"))?;
        if !configuration
            .hardware_support
            .iter()
            .any(|support| support.hardware_class_id == hardware_class_id)
        {
            return Err(SizingError::evaluation(
                "model configuration does not support the hardware class",
            ));
        }
        let binding = configuration
            .artifact_bindings
            .iter()
            .find(|binding| binding.binding_id == binding_id)
            .ok_or_else(|| SizingError::evaluation("artifact binding is not in the catalog"))?;
        let compiled = self.sizing.get(&SizingKey {
            model_id: model_id.to_owned(),
            configuration_id: configuration_id.to_owned(),
            hardware_class_id: hardware_class_id.to_owned(),
            binding_id: binding_id.to_owned(),
        });
        let engine_release = validation::configuration_engine_release(configuration, &self.engines)
            .ok_or_else(|| SizingError::evaluation("model configuration has no engine release"))?;

        let model = match equation {
            SizingFitEquation::Estimate(resource) => compiled
                .ok_or_else(|| SizingError::evaluation("artifact binding declares no sizing"))?
                .fit_model(resource, unknowns)?,
            SizingFitEquation::Stored(resource) => compiled
                .ok_or_else(|| SizingError::evaluation("artifact binding declares no sizing"))?
                .check_model(resource)?,
            SizingFitEquation::Inline(equation) => sizing::inline_fit_model(
                equation,
                engine_release.parameter_schema.as_ref().ok_or_else(|| {
                    SizingError::evaluation("model configuration has no engine parameter schema")
                })?,
                configuration,
                compiled
                    .map(|sizing| sizing.parameters().clone())
                    .unwrap_or_default(),
                unknowns,
                extra_symbols,
            )?,
        };
        Ok(SizingFitTarget {
            model,
            binding: binding.clone(),
            configuration: configuration.clone(),
            engine_kind: engine_release.engine_kind,
        })
    }

    #[cfg(test)]
    fn evaluate_local_sizing(
        &self,
        local: &ResolvedLocalModelDeploymentSnapshot,
        binding_id: &str,
    ) -> Result<Vec<ScalarResourceEstimate>, SizingError> {
        let values = local.engine_parameter_values.as_ref().ok_or_else(|| {
            SizingError::evaluation("resolved local selection has no engine parameter values")
        })?;
        let option_values = local
            .resolved_options
            .iter()
            .map(|option| {
                option
                    .values
                    .clone()
                    .ok_or_else(|| SizingError::evaluation("resolved model option has no values"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let auxiliary_ids = local
            .auxiliary_artifacts
            .iter()
            .map(|artifact| artifact.auxiliary_artifact_id.clone())
            .collect::<Vec<_>>();
        let hardware_class_id = local
            .hardware_support
            .as_ref()
            .map(|support| support.hardware_class_id.as_str())
            .unwrap_or_default();
        self.evaluate_local_sizing_inputs(
            &local.model_id,
            &local.configuration_id,
            hardware_class_id,
            binding_id,
            local
                .model_artifact
                .as_ref()
                .map(|artifact| artifact.artifact_id.as_str()),
            &auxiliary_ids,
            values,
            &option_values,
        )
    }

    fn load_from_sources(
        sources: &[(&str, &Path, CatalogSourceRole)],
        oci_artifact_policy: OciArtifactPolicy,
        engine_validator: &dyn EngineDeclarationValidator,
    ) -> Result<Self, CatalogError> {
        let mut diagnostics = Vec::new();
        let mut records = CatalogSourceRecords::default();
        for (source, catalog_root, role) in sources {
            load_catalog_source(catalog_root, source, *role, &mut records, &mut diagnostics);
        }
        let validated = validation::validate_catalog(
            records,
            oci_artifact_policy,
            engine_validator,
            &mut diagnostics,
        );
        if diagnostics.is_empty() {
            Ok(Self {
                models: validated.models,
                engines: validated.engines,
                hardware_classes: validated.hardware_classes,
                recommendations: validated.recommendations,
                applications: validated.applications,
                upstream_model_templates: validated.upstream_model_templates,
                sizing: validated.sizing,
                engine_defaults: validated.engine_defaults,
            })
        } else {
            diagnostics.sort_by_key(|diagnostic| diagnostic.to_string());
            Err(CatalogError::diagnostics(diagnostics))
        }
    }

    fn selected_model_profile(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
    ) -> Option<(&Model, &dotcore_proto::dotcore::catalog::v1::ModelProfile)> {
        let model = self.models.get(&model_id.0)?;
        let profile = model
            .profiles
            .iter()
            .find(|value| value.profile_id == profile_id)?;
        Some((model, profile))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_model_artifact_digests_omitted(mut self, model_id: &str) -> Self {
        let clear =
            |source: &mut dotcore_proto::dotcore::catalog::v1::HuggingFaceArtifactSource| {
                use dotcore_proto::dotcore::catalog::v1::hugging_face_artifact_source::Content;
                match source.content.as_mut().expect("test artifact content") {
                    Content::File(file) => file.provenance_digest.clear(),
                    Content::Snapshot(snapshot) => {
                        for file in &mut snapshot.files {
                            file.provenance_digest.clear();
                        }
                    }
                }
            };
        let model = self.models.get_mut(model_id).expect("test catalog model");
        for artifact in &mut model.artifacts {
            clear(
                artifact
                    .source
                    .as_mut()
                    .expect("test model artifact source"),
            );
        }
        for artifact in &mut model.auxiliary_artifacts {
            clear(
                artifact
                    .source
                    .as_mut()
                    .expect("test auxiliary artifact source"),
            );
        }
        self
    }
}

impl fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Catalog")
            .field("model_ids", &self.models.keys().collect::<Vec<_>>())
            .field(
                "engine_release_ids",
                &self.engines.keys().collect::<Vec<_>>(),
            )
            .field(
                "hardware_class_ids",
                &self.hardware_classes.keys().collect::<Vec<_>>(),
            )
            .field(
                "recommendation_hardware_class_ids",
                &self.recommendations.keys().collect::<Vec<_>>(),
            )
            .field(
                "application_ids",
                &self.applications.keys().collect::<Vec<_>>(),
            )
            .field(
                "upstream_model_template_ids",
                &self.upstream_model_templates.keys().collect::<Vec<_>>(),
            )
            .field("sizing_keys", &self.sizing.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Immutable catalog declarations of one local selection, for controller-owned resolution.
///
/// The approved engine artifacts keep their declared order and the list is never empty. Their
/// releases may differ but share one engine kind and identical parameter definitions, so the
/// release of whichever artifact is selected defines the same parameters. The supported hardware
/// classes keep the configuration's order and are never empty either.
pub struct CatalogLocalModelDeclarations {
    pub model: Model,
    pub profile: ModelProfile,
    pub configuration: ModelEngineConfiguration,
    pub binding: ModelArtifactBinding,

    /// Engine kind shared by every approved release.
    pub engine_kind: i32,

    pub approved_engine_artifacts: Vec<ApprovedEngineArtifact>,
    pub hardware_support: Vec<SupportedHardwareClass>,
    pub model_artifact: ModelArtifact,
}

/// One approved engine artifact of a configuration, with the engine release that declares it.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovedEngineArtifact {
    pub engine_release: EngineRelease,
    pub artifact: EngineArtifact,
}

/// One hardware support entry of a configuration, with the shared hardware class it references.
#[derive(Clone, Debug, PartialEq)]
pub struct SupportedHardwareClass {
    pub hardware_class: HardwareClass,

    /// Advisory memory minimums of the entry, absent when it declares none.
    pub hardware_requirements: Option<HardwareRequirements>,
}

/// Equation fitted by a catalog sizing fit.
#[derive(Clone, Copy, Debug)]
pub enum SizingFitEquation<'a> {
    /// The binding's declared estimate for this resource.
    Estimate(&'a str),

    /// An ad hoc equation over the binding's sizing symbols.
    Inline(&'a str),

    /// The binding's declared estimate for this resource with every constant at its declared
    /// value, so nothing is fitted.
    Stored(&'a str),
}

/// A sizing fit model and the catalog selection whose telemetry it applies to.
#[derive(Clone, Debug)]
pub struct SizingFitTarget {
    pub model: SizingFitModel,
    pub binding: ModelArtifactBinding,
    pub configuration: ModelEngineConfiguration,
    /// Raw engine kind of the configuration's engine release.
    pub engine_kind: i32,
}

/// The immutable identity of one catalog model release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModelId(pub String);

/// A deterministic protobuf-only view of typed catalog declarations.
///
/// Private catalog extensions such as compiled sizing are intentionally absent.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtobufCatalogRecords {
    pub models: Vec<Model>,
    pub engine_releases: Vec<EngineRelease>,
    pub hardware_classes: Vec<HardwareClass>,
    pub applications: Vec<Application>,
    pub upstream_model_templates: Vec<UpstreamModelTemplate>,
}

/// Builds a concrete catalog through the same validation path used by sources.
#[derive(Default)]
pub struct CatalogBuilder {
    oci_artifact_policy: OciArtifactPolicy,
    engine_validator: Option<&'static dyn EngineDeclarationValidator>,
    models: Vec<(String, Model)>,
    engines: Vec<(String, EngineRelease)>,
    hardware_classes: Vec<(String, HardwareClass)>,
    recommendations: Vec<(String, HardwareClassRecommendations)>,
    applications: Vec<(String, Application)>,
    upstream_model_templates: Vec<(String, UpstreamModelTemplate)>,
}

struct RejectEngineDeclarations;

impl EngineDeclarationValidator for RejectEngineDeclarations {
    fn validate_engine_release(&self, engine: &EngineRelease) -> Vec<anyhow::Error> {
        vec![anyhow::anyhow!(
            "engine release {:?} requires an engine declaration validator",
            engine.engine_release_id
        )]
    }

    fn validate_model_configuration(
        &self,
        _model: &Model,
        configuration: &ModelEngineConfiguration,
        _hardware_classes: &[&HardwareClass],
        _engine: &EngineRelease,
    ) -> Vec<anyhow::Error> {
        vec![anyhow::anyhow!(
            "configuration {:?} requires an engine declaration validator",
            configuration.configuration_id
        )]
    }
}

static REJECT_ENGINE_DECLARATIONS: RejectEngineDeclarations = RejectEngineDeclarations;

impl CatalogBuilder {
    /// Starts an empty catalog declaration builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the OCI artifact policy used when the catalog is built.
    pub fn oci_artifact_policy(mut self, policy: OciArtifactPolicy) -> Self {
        self.oci_artifact_policy = policy;
        self
    }

    /// Supplies validation for engine-owned declaration payloads.
    pub fn engine_validator(mut self, validator: &'static dyn EngineDeclarationValidator) -> Self {
        self.engine_validator = Some(validator);
        self
    }

    /// Adds one model declaration identified by a deterministic source location.
    pub fn model(mut self, location: impl Into<String>, model: Model) -> Self {
        self.models.push((location.into(), model));
        self
    }

    /// Adds one engine release declaration identified by a deterministic source location.
    pub fn engine_release(mut self, location: impl Into<String>, release: EngineRelease) -> Self {
        self.engines.push((location.into(), release));
        self
    }

    /// Adds one hardware class declaration identified by a deterministic source location.
    pub fn hardware_class(
        mut self,
        location: impl Into<String>,
        hardware_class: HardwareClass,
    ) -> Self {
        self.hardware_classes
            .push((location.into(), hardware_class));
        self
    }

    /// Adds one hardware class's model recommendations identified by a deterministic source
    /// location.
    pub fn recommendations(
        mut self,
        location: impl Into<String>,
        recommendations: HardwareClassRecommendations,
    ) -> Self {
        self.recommendations
            .push((location.into(), recommendations));
        self
    }

    /// Adds one application declaration identified by a deterministic source location.
    pub fn application(mut self, location: impl Into<String>, application: Application) -> Self {
        self.applications.push((location.into(), application));
        self
    }

    /// Adds one upstream model template declaration identified by a deterministic source location.
    pub fn upstream_model_template(
        mut self,
        location: impl Into<String>,
        template: UpstreamModelTemplate,
    ) -> Self {
        self.upstream_model_templates
            .push((location.into(), template));
        self
    }

    /// Validates all declarations and returns an immutable catalog only when complete.
    pub fn build(self) -> Result<Catalog, CatalogError> {
        let mut diagnostics = Vec::new();
        let engine_validator = self.engine_validator.unwrap_or(&REJECT_ENGINE_DECLARATIONS);
        let validated = validation::validate_catalog(
            CatalogSourceRecords {
                models: self.models,
                sizing_sources: Vec::new(),
                engines: self.engines,
                hardware_classes: self.hardware_classes,
                recommendations: self.recommendations,
                applications: self.applications,
                upstream_model_templates: self.upstream_model_templates,
            },
            self.oci_artifact_policy,
            engine_validator,
            &mut diagnostics,
        );
        if diagnostics.is_empty() {
            Ok(Catalog {
                models: validated.models,
                engines: validated.engines,
                hardware_classes: validated.hardware_classes,
                recommendations: validated.recommendations,
                applications: validated.applications,
                upstream_model_templates: validated.upstream_model_templates,
                sizing: validated.sizing,
                engine_defaults: validated.engine_defaults,
            })
        } else {
            diagnostics.sort_by_key(|diagnostic| diagnostic.to_string());
            Err(CatalogError::diagnostics(diagnostics))
        }
    }
}

/// Catalog admission policy for structured OCI artifact references.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OciArtifactPolicy {
    /// Accept local mutable tags as well as immutable digest references.
    Development,

    /// Require every OCI artifact declaration to include an immutable digest.
    #[default]
    DigestRequired,
}

/// Failures produced while loading, validating, or resolving immutable catalog data.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("Catalog diagnostics: {0}")]
    Diagnostics(#[source] anyhow::Error),

    #[error("Invalid catalog: {0}")]
    Invalid(#[source] anyhow::Error),
}

impl CatalogError {
    fn diagnostics(diagnostics: Vec<CatalogDiagnostic>) -> Self {
        Self::Diagnostics(anyhow::Error::new(CatalogDiagnostics::new(diagnostics)))
    }
}

/// Ordered validation diagnostics retained by one catalog failure.
///
/// Its error implementation intentionally selects the first diagnostic as the
/// source while its display preserves every diagnostic in deterministic multiline order.
#[derive(Debug)]
pub struct CatalogDiagnostics {
    diagnostics: Vec<CatalogDiagnostic>,
}

impl CatalogDiagnostics {
    /// Iterates over every diagnostic and its retained error.
    pub fn iter(&self) -> impl Iterator<Item = &CatalogDiagnostic> {
        self.diagnostics.iter()
    }

    fn new(diagnostics: Vec<CatalogDiagnostic>) -> Self {
        Self { diagnostics }
    }
}

impl fmt::Display for CatalogDiagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            write!(formatter, "{diagnostic}")?;
        }
        Ok(())
    }
}

impl StdError for CatalogDiagnostics {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.diagnostics
            .first()
            .map(|diagnostic| diagnostic.error.as_ref())
    }
}

impl<'a> IntoIterator for &'a CatalogDiagnostics {
    type Item = &'a CatalogDiagnostic;
    type IntoIter = std::slice::Iter<'a, CatalogDiagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diagnostics.iter()
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs};

    use anyhow::anyhow;
    use dotcore_domain::test_support::oci::ServiceArtifactOci as _;
    use dotcore_proto::dotcore::{
        catalog::v1::{
            ApplicationDependencyCondition, ApplicationServiceDependency, EngineParameterValue,
            EngineParameterValues, EngineRelease, UpstreamWireProtocol, UseCase,
            engine_parameter_value::Value as ParameterValue,
        },
        engine::v1::EngineKind,
        model::v1::ResolvedLocalModelDeploymentSnapshot,
    };
    use dotcore_proto::yaml::RestrictedYamlError;

    use super::{
        Catalog, CatalogBuilder, CatalogDiagnostics, CatalogError, CatalogModelId,
        EngineDeclarationValidator, OciArtifactPolicy,
        test_support::{
            SIZING_ENGINE_CACHE_DEFAULT, UNPARAMETERIZED_BINDING_SIZING,
            approve_sizing_apple_artifact, create_empty_catalog_source,
            sizing_not_linear_in_total_context, temporary_catalog_root, write_sizing_engine,
            write_sizing_model, write_sizing_model_with,
        },
        validation::CatalogDiagnostic,
    };

    struct SyntheticEngineValidator;

    impl EngineDeclarationValidator for SyntheticEngineValidator {
        fn validate_engine_release(
            &self,
            engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            vec![anyhow::anyhow!(
                "synthetic validator rejected {:?}",
                engine.engine_release_id
            )]
        }

        fn validate_model_configuration(
            &self,
            _model: &dotcore_proto::dotcore::catalog::v1::Model,
            _configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
            _hardware_classes: &[&dotcore_proto::dotcore::catalog::v1::HardwareClass],
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            Vec::new()
        }
    }

    static SYNTHETIC_ENGINE_VALIDATOR: SyntheticEngineValidator = SyntheticEngineValidator;

    /// Accepts every engine-owned payload, so tests exercise only the catalog's own rules.
    struct AcceptEngineDeclarations;

    impl EngineDeclarationValidator for AcceptEngineDeclarations {
        fn validate_engine_release(
            &self,
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            Vec::new()
        }

        fn validate_model_configuration(
            &self,
            _model: &dotcore_proto::dotcore::catalog::v1::Model,
            _configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
            _hardware_classes: &[&dotcore_proto::dotcore::catalog::v1::HardwareClass],
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            Vec::new()
        }
    }

    static ACCEPT_ENGINE_DECLARATIONS: AcceptEngineDeclarations = AcceptEngineDeclarations;

    /// Rejects every engine release as an unknown engine kind, like an engine validator that
    /// knows no engine.
    struct UnsupportedEngineKindValidator;

    impl EngineDeclarationValidator for UnsupportedEngineKindValidator {
        fn validate_engine_release(
            &self,
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            vec![anyhow!("engine release has an unsupported engine kind")]
        }

        fn validate_model_configuration(
            &self,
            _model: &dotcore_proto::dotcore::catalog::v1::Model,
            _configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
            _hardware_classes: &[&dotcore_proto::dotcore::catalog::v1::HardwareClass],
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            Vec::new()
        }
    }

    static UNSUPPORTED_ENGINE_KIND_VALIDATOR: UnsupportedEngineKindValidator =
        UnsupportedEngineKindValidator;

    #[test]
    fn catalog_builder_uses_the_supplied_engine_validator() {
        let engine = fixture_release("linux");

        let error = CatalogBuilder::new()
            .engine_validator(&SYNTHETIC_ENGINE_VALIDATOR)
            .engine_release("synthetic", engine.clone())
            .build()
            .expect_err("synthetic validator must reject")
            .to_string();

        assert!(
            error.contains(&format!(
                "synthetic validator rejected {:?}",
                engine.engine_release_id
            )),
            "unexpected validation error: {error}"
        );
    }

    #[test]
    fn releases_of_one_engine_kind_sharing_a_schema_id_must_define_identical_parameters() {
        let release = |id: &str, kind: &str, cache_maximum: &str| -> EngineRelease {
            dotcore_proto::yaml::from_str(&format!(
                r#"engineReleaseId: "{id}"
engineKind: "{kind}"
version: "fixture"
parameterSchema:
  schemaId: "fixture-shared"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      exposed: true
      unsignedInteger: {{ minimum: "1024", maximum: "131072", unit: "tokens", defaultValue: "32768" }}
    - parameterId: "context-per-request"
      displayName: "Context per request"
      exposed: true
      unsignedInteger: {{ minimum: "1024", maximum: "131072", unit: "tokens" }}
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      exposed: true
      unsignedInteger: {{ minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" }}
    - parameterId: "cache-mib"
      displayName: "Cache"
      unsignedInteger: {{ minimum: "0", maximum: "{cache_maximum}", unit: "MiB" }}
artifacts:
  - artifactId: "linux-amd64"
    oci:
      repository:
        registry:
          host: "example.invalid"
        name: "fixture/engine"
      digest:
        algorithm: "sha256"
        encoded: "1111111111111111111111111111111111111111111111111111111111111111"
    selectors:
      - platform: {{ operatingSystem: "linux", architecture: "amd64" }}
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#
            ))
            .expect("synthetic engine release")
        };
        let build = |second: EngineRelease| {
            CatalogBuilder::new()
                .engine_validator(&ACCEPT_ENGINE_DECLARATIONS)
                .engine_release(
                    "first.yaml",
                    release("first", "ENGINE_KIND_LLAMA_CPP", "65536"),
                )
                .engine_release("second.yaml", second)
                .build()
        };

        let identical = build(release("second", "ENGINE_KIND_LLAMA_CPP", "65536"))
            .expect("identical shared definitions");
        assert_eq!(identical.list_engine_releases().len(), 2);
        build(release("second", "ENGINE_KIND_VLLM", "1024"))
            .expect("another engine kind may reuse the schema id");

        let error = build(release("second", "ENGINE_KIND_LLAMA_CPP", "1024"))
            .expect_err("differing shared definition")
            .to_string();
        assert_eq!(
            error,
            "Catalog diagnostics: engine release \"second\": engine releases \"first\" and \"second\" \
             share parameter schema \"fixture-shared\" but define parameter \"cache-mib\" differently"
        );

        let mut missing = release("second", "ENGINE_KIND_LLAMA_CPP", "65536");
        missing
            .parameter_schema
            .as_mut()
            .expect("parameter schema")
            .parameters
            .pop();
        assert_eq!(
            build(missing)
                .expect_err("missing shared definition")
                .to_string(),
            error
        );
    }

    #[test]
    fn one_configuration_approves_releases_of_one_engine_kind_with_identical_parameters() {
        let linux = approved_release("linux", "ENGINE_KIND_LLAMA_CPP", "fixture-shared", "65536");
        let build = |second: EngineRelease| {
            CatalogBuilder::new()
                .engine_validator(&ACCEPT_ENGINE_DECLARATIONS)
                .hardware_class(
                    "hardware-classes/nvidia.yaml",
                    super::test_support::vendor_hardware_class("nvidia"),
                )
                .engine_release("linux.yaml", linux.clone())
                .engine_release("second.yaml", second)
                .model("model.yaml", two_release_model())
                .build()
        };

        let catalog = build(approved_release(
            "second",
            "ENGINE_KIND_LLAMA_CPP",
            "fixture-shared",
            "65536",
        ))
        .expect("releases of one engine kind with identical parameters");
        let declarations = catalog
            .local_model_declarations(
                &CatalogModelId("fixture-model".to_owned()),
                "balanced",
                "default",
            )
            .expect("local declarations");
        assert_eq!(declarations.engine_kind, EngineKind::LlamaCpp as i32);
        assert_eq!(
            declarations
                .approved_engine_artifacts
                .iter()
                .map(|approved| (
                    approved.engine_release.engine_release_id.as_str(),
                    approved.engine_release.version.as_str(),
                    approved.artifact.artifact_id.as_str(),
                ))
                .collect::<Vec<_>>(),
            [
                ("linux", "linux-version", "linux-artifact"),
                ("second", "second-version", "second-artifact"),
            ]
        );

        for (label, second, expected) in [
            (
                "mixed engine kinds",
                approved_release("second", "ENGINE_KIND_VLLM", "fixture-shared", "65536"),
                "configuration \"local\" approves engine releases \"linux\" and \"second\" of different engine kinds",
            ),
            (
                "differing definitions",
                approved_release("second", "ENGINE_KIND_LLAMA_CPP", "fixture-other", "1024"),
                "configuration \"local\" approves engine releases \"linux\" and \"second\" with different parameter definitions",
            ),
            (
                "identical definitions under another schema id",
                approved_release("second", "ENGINE_KIND_LLAMA_CPP", "fixture-other", "65536"),
                "configuration \"local\" approves engine releases \"linux\" and \"second\" with different parameter definitions",
            ),
        ] {
            let error = build(second).expect_err(label).to_string();
            assert_eq!(
                error,
                format!("Catalog diagnostics: model.yaml: {expected}"),
                "{label}"
            );
        }
    }

    #[test]
    fn catalog_builder_fails_closed_and_accepts_tags_only_under_explicit_development_policy() {
        let mut application = super::test_support::application();
        let reference = application
            .graph
            .as_mut()
            .expect("application graph")
            .services[0]
            .artifact
            .as_mut()
            .expect("OCI artifact")
            .oci_mut()
            .expect("OCI reference");
        reference.tag = Some("dev".to_owned());
        reference.digest = None;

        let strict_error = CatalogBuilder::new()
            .application("application", application.clone())
            .build()
            .expect_err("default catalog policy must require a digest")
            .to_string();
        assert!(strict_error.contains("OCI artifact policy requires a digest"));

        CatalogBuilder::new()
            .oci_artifact_policy(OciArtifactPolicy::Development)
            .application("application", application)
            .build()
            .expect("explicit development catalog policy must accept a local tag");
    }

    #[test]
    fn development_source_compiles_class_keyed_sizing_and_clone_preserves_it() {
        let root = temporary_catalog_root();
        let master = root.join("master");
        let development = root.join("development");
        create_empty_catalog_source(&master);
        create_empty_catalog_source(&development);
        write_sizing_engine(&master);
        write_sizing_model_with(
            &development,
            r#"    sizing:
      bindingParameters: ["baseBytes"]
      parameters:
        bytesPerToken: 3
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${bytesPerToken} * ${total-context}"
        hostMemory:
          unit: bytes
          equation: "${baseBytes}"
"#,
            r#"        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 100
          - bindingId: "second"
            parameters:
              baseBytes: 200
"#,
            &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
        );

        let catalog =
            Catalog::load_development_sources(&master, &development, &ACCEPT_ENGINE_DECLARATIONS)
                .expect("development dynamic sizing catalog");
        let site_catalog = Catalog::load_sources_with_policy(
            &master,
            &development,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("Master and site dynamic sizing catalog");
        assert_eq!(catalog.sizing.len(), 2);
        assert_eq!(site_catalog.sizing.len(), 2);

        let model_id = super::CatalogModelId("fixture-model".to_owned());
        let (_, defaults) = catalog
            .local_defaults_for(&model_id, "balanced")
            .expect("profile defaults");
        let local = resolved_fixture_snapshot("sized-weights", "nvidia", defaults.clone());

        let expected = vec![
            super::ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 98_404,
                unit: "bytes".to_owned(),
            },
            super::ScalarResourceEstimate {
                resource: "hostMemory".to_owned(),
                value: 100,
                unit: "bytes".to_owned(),
            },
        ];
        assert_eq!(
            catalog
                .evaluate_local_sizing(&local, "sized")
                .expect("sizing evaluates"),
            expected
        );
        assert_eq!(
            catalog
                .clone()
                .evaluate_local_sizing(&local, "sized")
                .expect("cloned sizing evaluates"),
            expected
        );

        let second_local = resolved_fixture_snapshot("second-weights", "nvidia", defaults.clone());
        assert_eq!(
            catalog
                .evaluate_local_sizing(&second_local, "second")
                .expect("second binding evaluates"),
            vec![
                super::ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 98_504,
                    unit: "bytes".to_owned(),
                },
                super::ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 200,
                    unit: "bytes".to_owned(),
                },
            ]
        );
        let unsupported_class = resolved_fixture_snapshot("sized-weights", "apple", defaults);
        assert_eq!(
            catalog
                .evaluate_local_sizing(&unsupported_class, "sized")
                .expect("an unsupported class has no sizing"),
            Vec::new()
        );
        assert_eq!(
            catalog
                .evaluate_local_sizing(&second_local, "sized")
                .expect_err("resolved binding mismatch must reject")
                .to_string(),
            "Catalog sizing evaluation: resolved local selection does not match sizing binding"
        );

        let expanded = EngineParameterValues {
            values: vec![EngineParameterValue {
                parameter_id: "total-context".to_owned(),
                value: Some(ParameterValue::UnsignedInteger(50_177)),
            }],
        };
        let expanded_local = resolved_fixture_snapshot("sized-weights", "nvidia", expanded);
        assert_eq!(
            catalog
                .evaluate_local_sizing(&expanded_local, "sized")
                .expect("sizing evaluates"),
            vec![
                super::ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 150_631,
                    unit: "bytes".to_owned(),
                },
                super::ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 100,
                    unit: "bytes".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn class_and_binding_constants_merge_into_sizing_per_hardware_class() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        write_sizing_model_with(
            &root,
            r#"    sizing:
      classParameters: ["offsetBytes"]
      bindingParameters: ["baseBytes"]
      parameters:
        bytesPerToken: 2
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${offsetBytes} + ${bytesPerToken} * ${total-context}"
"#,
            r#"        parameters:
          offsetBytes: 7
        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 100
          - bindingId: "second"
            parameters:
              baseBytes: 200
      - hardwareClassId: "apple"
        parameters:
          offsetBytes: 1000
        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 300
          - bindingId: "second"
            parameters:
              baseBytes: 400
"#,
            &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
        );
        approve_sizing_apple_artifact(&root);
        let catalog = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("layered sizing catalog");
        let (_, defaults) = catalog
            .local_defaults_for(
                &super::CatalogModelId("fixture-model".to_owned()),
                "balanced",
            )
            .expect("profile defaults");

        let accelerator_memory = |class: &str, artifact: &str, binding: &str| {
            catalog
                .evaluate_local_sizing(
                    &resolved_fixture_snapshot(artifact, class, defaults.clone()),
                    binding,
                )
                .expect("layered sizing evaluates")
                .into_iter()
                .map(|estimate| estimate.value)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            accelerator_memory("nvidia", "sized-weights", "sized"),
            [65_643]
        );
        assert_eq!(
            accelerator_memory("nvidia", "second-weights", "second"),
            [65_743]
        );
        assert_eq!(
            accelerator_memory("apple", "sized-weights", "sized"),
            [66_836]
        );
        assert_eq!(
            accelerator_memory("apple", "second-weights", "second"),
            [66_936]
        );

        let default_fit = catalog
            .sizing_fit(
                "fixture-model",
                "local",
                "apple",
                "sized",
                super::SizingFitEquation::Estimate("acceleratorMemory"),
                &[],
                &[],
            )
            .expect("default fit over class and shared constants");
        assert_eq!(default_fit.model.unknowns(), ["baseBytes"]);
    }

    #[test]
    fn misplaced_sizing_constants_fail_the_catalog_load() {
        for (case, support_sizing, expected) in [
            (
                "undeclared",
                r#"        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 100
              extraBytes: 1
          - bindingId: "second"
            parameters:
              baseBytes: 200
"#,
                "models/fixture.yaml: model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" binding \"sized\" sets undeclared sizing constant \"extraBytes\"",
            ),
            (
                "missing-binding-constant",
                r#"        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 100
          - bindingId: "second"
"#,
                "models/fixture.yaml: model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" binding \"second\" does not set binding sizing constant \"baseBytes\"",
            ),
            (
                "binding-without-sizing",
                r#"        bindingSizing:
          - bindingId: "sized"
            parameters:
              baseBytes: 100
"#,
                "models/fixture.yaml: model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" has no sizing for binding \"second\"",
            ),
            (
                "support-entry-without-constants",
                "",
                "models/fixture.yaml: model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" has no sizing for binding \"second\"\nmodels/fixture.yaml: model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" has no sizing for binding \"sized\"",
            ),
        ] {
            let root = temporary_catalog_root().join(case);
            create_empty_catalog_source(&root);
            write_sizing_engine(&root);
            write_sizing_model_with(
                &root,
                r#"    sizing:
      bindingParameters: ["baseBytes"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${total-context}"
"#,
                support_sizing,
                &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
            );

            let error = Catalog::load_with_policy(
                &root,
                OciArtifactPolicy::Development,
                &ACCEPT_ENGINE_DECLARATIONS,
            )
            .expect_err("misplaced sizing constants must reject")
            .to_string();

            assert_eq!(error, format!("Catalog diagnostics: {expected}"), "{case}");
        }
    }

    #[test]
    fn hardware_support_must_reference_known_disjoint_or_nested_classes() {
        let unknown = support_errors(
            r#"      - hardwareClassId: "nvidia"
      - hardwareClassId: "tpu"
"#,
            &[],
        );
        assert_eq!(
            unknown,
            "Catalog diagnostics: models/fixture.yaml: model \"fixture-model\" configuration \"local\" references unknown hardware class \"tpu\""
        );

        let overlapping = support_errors(
            r#"      - hardwareClassId: "nvidia"
      - hardwareClassId: "linux-gpu"
"#,
            &[(
                "linux-gpu",
                "selector:\n  platform: { operatingSystem: \"linux\", architecture: \"amd64\" }\n  accelerators: [\"nvidia\", \"apple\"]\n",
            )],
        );
        assert_eq!(
            overlapping,
            "Catalog diagnostics: models/fixture.yaml: model \"fixture-model\" configuration \"local\" supports hardware classes \"nvidia\" and \"linux-gpu\", whose selectors overlap without one nesting in the other"
        );

        let repeated = support_errors(
            r#"      - hardwareClassId: "nvidia"
      - hardwareClassId: "nvidia"
"#,
            &[],
        );
        assert_eq!(
            repeated,
            "Catalog diagnostics: models/fixture.yaml: model \"fixture-model\" configuration \"local\" supports hardware class \"nvidia\" more than once"
        );

        let duplicate_class = support_errors(
            "      - hardwareClassId: \"nvidia\"\n",
            &[("copy", "selector:\n  accelerators: [\"nvidia\"]\n")],
        );
        assert!(
            duplicate_class.contains(
                "hardware-classes/nvidia.yaml: duplicate hardware class id \"nvidia\"; first declared by hardware-classes/copy.yaml"
            ),
            "{duplicate_class}"
        );

        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        write_hardware_class(
            &root,
            "gpu",
            "selector:\n  accelerators: [\"nvidia\", \"apple\"]\n",
        );
        write_support_model(
            &root,
            r#"      - hardwareClassId: "gpu"
      - hardwareClassId: "nvidia"
"#,
        );
        let catalog = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("nested classes are accepted");
        assert_eq!(
            catalog
                .list_hardware_classes()
                .into_iter()
                .map(|class| class.hardware_class_id)
                .collect::<Vec<_>>(),
            ["apple", "gpu", "nvidia"]
        );
        let declarations = catalog
            .local_model_declarations(
                &super::CatalogModelId("fixture-model".to_owned()),
                "balanced",
                "sized",
            )
            .expect("local declarations");
        assert_eq!(
            declarations
                .hardware_support
                .iter()
                .map(|support| support.hardware_class.hardware_class_id.as_str())
                .collect::<Vec<_>>(),
            ["gpu", "nvidia"]
        );
    }

    #[test]
    fn dynamic_sizing_keeps_restricted_yaml_and_unknown_field_rejection() {
        for (case, sizing, expected) in [
            (
                "duplicate-parameter",
                r#"    sizing:
      parameters:
        baseBytes: 100
        baseBytes: 101
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes}"
"#,
                "YAML codec error",
            ),
            (
                "removed-valid-for",
                r#"    sizing:
      parameters:
        baseBytes: 100
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes}"
      validFor:
        total-context: [1024, 2048]
"#,
                "invalid catalog sizing block",
            ),
            (
                "unknown-sizing-field",
                r#"    sizing:
      parameters:
        baseBytes: 100
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes}"
      unexpected: true
"#,
                "invalid catalog sizing block",
            ),
            (
                "unknown-model-field",
                r#"    sizing:
      parameters:
        baseBytes: 100
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes}"
    unexpected: true
"#,
                "ProtoJSON codec error",
            ),
        ] {
            let root = temporary_catalog_root().join(case);
            create_empty_catalog_source(&root);
            write_sizing_engine(&root);
            write_sizing_model(&root, sizing);

            let error = Catalog::load_with_policy(
                &root,
                OciArtifactPolicy::Development,
                &ACCEPT_ENGINE_DECLARATIONS,
            )
            .expect_err("invalid dynamic sizing must reject")
            .to_string();
            assert!(error.contains(expected), "{case}: {error}");
        }
    }

    #[test]
    fn profile_omission_requires_every_binding_to_derive_the_parameter() {
        let profile_defaults = "      values: []\n";
        let root = temporary_catalog_root();
        let unsized_root = root.join("unsized");
        create_empty_catalog_source(&unsized_root);
        write_sizing_engine(&unsized_root);
        write_sizing_model_with(&unsized_root, "", "", profile_defaults);

        let error = Catalog::load_with_policy(
            &unsized_root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect_err("a binding without the derivation must reject the omission")
        .to_string();
        for binding_id in ["sized", "second"] {
            assert!(
                error.contains(&format!(
                    "profile \"balanced\" omits engine parameter \"prompt-cache-memory-mib\", which binding \"{binding_id}\" does not derive on hardware class \"nvidia\""
                )),
                "{error}"
            );
        }

        let complete = root.join("complete");
        create_empty_catalog_source(&complete);
        write_sizing_engine(&complete);
        write_sizing_model_with(
            &complete,
            r#"    sizing:
      derivedParameters:
        prompt-cache-memory-mib: "4096"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${prompt-cache-memory-mib}"
"#,
            super::test_support::UNPARAMETERIZED_BINDING_SIZING,
            profile_defaults,
        );
        let catalog = Catalog::load_with_policy(
            &complete,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("every binding derives the omitted parameter");
        assert_eq!(
            catalog
                .derive_local_parameters(
                    "fixture-model",
                    "local",
                    "nvidia",
                    "second",
                    &EngineParameterValues::default(),
                    &[],
                )
                .expect("derivation evaluates"),
            vec![("prompt-cache-memory-mib".to_owned(), 4096)]
        );
    }

    #[test]
    fn profile_defaults_overlay_the_engine_defaults_per_parameter() {
        let load = |profile_defaults: &str| {
            shared_parameter_catalog(
                "",
                "",
                &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}{profile_defaults}"),
            )
            .expect("profile omissions with engine defaults load")
        };
        let defaults = |catalog: &Catalog| {
            catalog
                .local_defaults_for(&CatalogModelId("fixture-model".to_owned()), "balanced")
                .expect("profile defaults")
        };

        assert_eq!(
            defaults(&load("")),
            (
                "sized".to_owned(),
                unsigned_values(&[
                    ("prompt-cache-memory-mib", 0),
                    ("total-context", 32_768),
                    ("parallel-requests", 4),
                ])
            )
        );
        assert_eq!(
            defaults(&load(
                "        - parameterId: \"parallel-requests\"\n          unsignedInteger: \"2\"\n"
            )),
            (
                "sized".to_owned(),
                unsigned_values(&[
                    ("prompt-cache-memory-mib", 0),
                    ("parallel-requests", 2),
                    ("total-context", 32_768),
                ])
            )
        );
    }

    #[test]
    fn profiles_cannot_default_the_total_context_or_the_context_per_request() {
        for parameter_id in ["total-context", "context-per-request"] {
            let error = shared_parameter_catalog(
                "",
                "",
                &format!(
                    "      values:\n{SIZING_ENGINE_CACHE_DEFAULT}        - parameterId: \"{parameter_id}\"\n          unsignedInteger: \"8192\"\n"
                ),
            )
            .expect_err("a profile default for context")
            .to_string();

            assert!(
                error.contains(&format!(
                    "profile \"balanced\" sets engine parameter \"{parameter_id}\", which profiles leave to the engine default and the deployment"
                )),
                "{error}"
            );
        }
    }

    #[test]
    fn an_engine_default_on_a_derived_parameter_is_rejected() {
        let error = shared_parameter_catalog(
            r#"    sizing:
      derivedParameters:
        parallel-requests: "2"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${total-context}"
"#,
            UNPARAMETERIZED_BINDING_SIZING,
            &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
        )
        .expect_err("an engine default on a derived parameter")
        .to_string();

        assert!(
            error.contains(
                "configuration \"local\" sizing derives engine parameter \"parallel-requests\", which its engine release also defaults; drop one of them"
            ),
            "{error}"
        );
    }

    #[test]
    fn the_linearity_guard_reports_sizing_that_bends_with_the_total_context() {
        let guard = |case: &str, equation: &str, allowance: &str| {
            let root = temporary_catalog_root().join(case);
            create_empty_catalog_source(&root);
            write_sizing_engine(&root);
            let allowance = if allowance.is_empty() {
                String::new()
            } else {
                format!("          allowance: \"{allowance}\"\n")
            };
            write_sizing_model(
                &root,
                &format!(
                    "    sizing:\n      estimates:\n        acceleratorMemory:\n          unit: bytes\n          equation: \"{equation}\"\n{allowance}"
                ),
            );
            sizing_not_linear_in_total_context(
                &Catalog::load_with_policy(
                    &root,
                    OciArtifactPolicy::Development,
                    &ACCEPT_ENGINE_DECLARATIONS,
                )
                .expect("sizing catalog"),
            )
        };
        let problem = |binding: &str, values: &str| {
            format!(
                "model \"fixture-model\" profile \"balanced\" hardware class \"nvidia\" binding \"{binding}\" \"acceleratorMemory\" isn't linear in the total context: {values} at 1024, 262144 and 524288 tokens"
            )
        };

        assert_eq!(
            guard(
                "linear",
                "1000 + 3 * ${total-context} + 2 * ${parallel-requests}",
                "500 + ${total-context}"
            ),
            Vec::<String>::new()
        );
        for (case, equation, allowance, values) in [
            (
                "square",
                "${total-context} * ${total-context}",
                "",
                "1048576, 68719476736 and 274877906944",
            ),
            (
                "condition",
                "if(${total-context} > 300000, 1000, 0) + ${total-context}",
                "",
                "1024, 262144 and 525288",
            ),
            // The context per request stops growing at the 262144-token maximum context.
            (
                "per-request",
                "${context-per-request} + ${total-context}",
                "",
                "2048, 524288 and 786432",
            ),
            (
                "allowance",
                "${total-context}",
                "if(${total-context} > 300000, 1000, 0)",
                "1024, 262144 and 525288",
            ),
        ] {
            assert_eq!(
                guard(case, equation, allowance),
                [problem("second", values), problem("sized", values)],
                "{case}"
            );
        }
    }

    #[test]
    fn the_linearity_guard_evaluates_every_declared_option_choice() {
        let guard = |case: &str, equation: &str, compatible_when: &str| {
            let root = temporary_catalog_root().join(case);
            create_empty_catalog_source(&root);
            write_sizing_engine(&root);
            write_sizing_model(
                &root,
                &format!(
                    "    sizing:\n      estimates:\n        acceleratorMemory:\n          unit: bytes\n          equation: '{equation}'\n"
                ),
            );
            let path = root.join("models/fixture.yaml");
            let model = std::fs::read_to_string(&path)
                .expect("sizing model fixture")
                .replace(
                    "maximumContextTokens: 262144\n",
                    "maximumContextTokens: 262144\noptions:\n  - { optionId: \"kv-cache\", displayName: \"KV cache precision\", kvCachePrecision: {} }\n",
                )
                .replace(
                    "      supportedModalities: [\"MODEL_INPUT_MODALITY_TEXT\"]\n",
                    r#"      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
    options:
      - optionId: "kv-cache"
        parameters:
          - { parameterId: "precision", displayName: "Precision", stringEnum: { values: ["f16", "q8_0"] } }
        defaultValues:
          optionId: "kv-cache"
          values:
            - { parameterId: "precision", string: "f16" }
        compatibleWhen: 'COMPATIBLE_WHEN'
"#
                    .replace("COMPATIBLE_WHEN", compatible_when)
                    .as_str(),
                );
            std::fs::write(&path, model).expect("sizing model fixture with an option");
            sizing_not_linear_in_total_context(
                &Catalog::load_with_policy(
                    &root,
                    OciArtifactPolicy::Development,
                    &ACCEPT_ENGINE_DECLARATIONS,
                )
                .expect("sizing catalog"),
            )
        };

        assert_eq!(
            guard(
                "linear",
                r#"if(${options.kv-cache.precision} == "q8_0", 2, 3) * ${total-context}"#,
                "true"
            ),
            Vec::<String>::new()
        );
        let problem = |binding: &str| {
            format!(
                "model \"fixture-model\" profile \"balanced\" with kv-cache.precision = \"q8_0\" hardware class \"nvidia\" binding \"{binding}\" \"acceleratorMemory\" isn't linear in the total context: 1048576, 68719476736 and 274877906944 at 1024, 262144 and 524288 tokens"
            )
        };
        assert_eq!(
            guard(
                "quantized-square",
                r#"if(${options.kv-cache.precision} == "q8_0", ${total-context} * ${total-context}, ${total-context})"#,
                "true"
            ),
            [problem("second"), problem("sized")]
        );

        // A choice the compatibility rule rejects can't be deployed, so its sizing isn't checked.
        assert_eq!(
            guard(
                "incompatible-square",
                r#"if(${options.kv-cache.precision} == "q8_0", ${total-context} * ${total-context}, ${total-context})"#,
                r#"${options.kv-cache.precision} == "f16""#
            ),
            Vec::<String>::new()
        );

        // A choice whose compatibility rule fails to evaluate is reported, not skipped.
        let unresolved = |binding: &str| {
            format!(
                "model \"fixture-model\" profile \"balanced\" with kv-cache.precision = \"q8_0\" hardware class \"nvidia\" binding \"{binding}\" options don't resolve: Catalog sizing arithmetic failed: sizing expression at byte 88: multiplication overflow: sizing expression at byte 88: multiplication overflow"
            )
        };
        assert_eq!(
            guard(
                "overflowing-rule",
                "${total-context}",
                r#"if(${options.kv-cache.precision} == "q8_0", 18446744073709551615 * 18446744073709551615 * 18446744073709551615, 0) == 0"#
            ),
            [unresolved("second"), unresolved("sized")]
        );
    }

    #[test]
    fn engines_requiring_sizing_reject_bindings_without_a_footprint_estimate() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        write_sizing_model(
            &root,
            r#"    sizing:
      estimates:
        hostMemory:
          unit: bytes
          equation: "${total-context}"
"#,
        );

        let error = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &REQUIRE_SIZING_ENGINE_VALIDATOR,
        )
        .expect_err("missing footprint estimates must reject")
        .to_string();
        for binding_id in ["sized", "second"] {
            assert!(
                error.contains(&format!(
                    "configuration \"local\" hardware class \"nvidia\" binding \"{binding_id}\" requires a \"acceleratorMemory\" sizing estimate for its engine"
                )),
                "{binding_id}: {error}"
            );
        }

        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("engines not requiring sizing accept bindings without a footprint estimate");
    }

    struct RequireSizingEngineValidator;

    impl EngineDeclarationValidator for RequireSizingEngineValidator {
        fn validate_engine_release(
            &self,
            engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            ACCEPT_ENGINE_DECLARATIONS.validate_engine_release(engine)
        }

        fn validate_model_configuration(
            &self,
            model: &dotcore_proto::dotcore::catalog::v1::Model,
            configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
            hardware_classes: &[&dotcore_proto::dotcore::catalog::v1::HardwareClass],
            engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> Vec<anyhow::Error> {
            ACCEPT_ENGINE_DECLARATIONS.validate_model_configuration(
                model,
                configuration,
                hardware_classes,
                engine,
            )
        }

        fn launch_requires_sizing(
            &self,
            _engine: &dotcore_proto::dotcore::catalog::v1::EngineRelease,
        ) -> bool {
            true
        }
    }

    static REQUIRE_SIZING_ENGINE_VALIDATOR: RequireSizingEngineValidator =
        RequireSizingEngineValidator;

    const NVIDIA_RECOMMENDATIONS: &str = r#"hardwareClassId: "nvidia"
useCases:
  - useCase: "USE_CASE_CODING"
    entries:
      - modelId: "fixture-model"
        profileId: "balanced"
        hardwareRequirements:
          minimumMemoryBytesPerDevice: "32000000000"
      - modelId: "fixture-model"
        profileId: "fast"
  - useCase: "USE_CASE_CHAT_ASSISTANT"
    entries:
      - modelId: "fixture-model"
        profileId: "fast"
"#;

    #[test]
    fn master_catalog_recommendations_load_in_declared_order() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        super::test_support::write_sizing_engine(&root);
        super::test_support::write_sizing_model(&root, "");
        super::test_support::add_fast_sizing_profile(&root);
        super::test_support::write_recommendations(&root, "nvidia", NVIDIA_RECOMMENDATIONS);

        let catalog = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("catalog with recommendations loads");

        let recommendations = catalog.list_hardware_class_recommendations();
        let lists = recommendations
            .iter()
            .flat_map(|class| {
                class.use_cases.iter().map(|list| {
                    (
                        class.hardware_class_id.as_str(),
                        list.use_case(),
                        list.entries
                            .iter()
                            .map(|entry| {
                                (
                                    entry.profile_id.as_str(),
                                    entry.hardware_requirements.as_ref().map(|requirements| {
                                        requirements.minimum_memory_bytes_per_device
                                    }),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lists,
            [
                (
                    "nvidia",
                    UseCase::Coding,
                    vec![("balanced", Some(32_000_000_000)), ("fast", None)]
                ),
                ("nvidia", UseCase::ChatAssistant, vec![("fast", None)]),
            ]
        );
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn catalog_without_recommendations_loads_with_none() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        super::test_support::write_sizing_engine(&root);
        super::test_support::write_sizing_model(&root, "");

        let catalog = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("catalog without recommendations loads");

        assert_eq!(catalog.list_hardware_class_recommendations(), []);
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn catalog_layers_cannot_declare_recommendations() {
        let root = temporary_catalog_root();
        let master = root.join("master");
        let layer = root.join("layer");
        create_empty_catalog_source(&master);
        super::test_support::write_sizing_engine(&master);
        super::test_support::write_sizing_model(&master, "");
        create_empty_catalog_source(&layer);
        super::test_support::write_recommendations(&layer, "nvidia", NVIDIA_RECOMMENDATIONS);

        let development =
            Catalog::load_development_sources(&master, &layer, &ACCEPT_ENGINE_DECLARATIONS)
                .expect_err("a development layer must not declare recommendations");
        let site = Catalog::load_sources_with_policy(
            &master,
            &layer,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect_err("a site layer must not declare recommendations");

        assert_eq!(
            development.to_string(),
            "Catalog diagnostics: development catalog recommendations/nvidia.yaml: model recommendations can only be declared by the master catalog"
        );
        assert_eq!(
            site.to_string(),
            "Catalog diagnostics: site catalog recommendations/nvidia.yaml: model recommendations can only be declared by the master catalog"
        );
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn recommendations_on_wider_and_narrower_classes_than_the_supported_one_are_valid() {
        let root = recommendation_fixture("      - hardwareClassId: \"nvidia\"\n");
        for (class, selector) in [
            (
                "gpu",
                "selector:\n  accelerators: [\"nvidia\", \"apple\"]\n",
            ),
            (
                "nvidia-dedicated",
                "selector:\n  accelerators: [\"nvidia\"]\n  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_DEDICATED\"\n",
            ),
        ] {
            write_hardware_class(&root, class, selector);
            super::test_support::write_recommendations(
                &root,
                class,
                &format!(
                    "hardwareClassId: \"{class}\"\nuseCases:\n  - useCase: \"USE_CASE_CHAT_ASSISTANT\"\n    entries:\n      - modelId: \"fixture-model\"\n        profileId: \"balanced\"\n"
                ),
            );
        }

        let catalog = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect("recommendations on related classes are valid");

        assert_eq!(
            catalog
                .list_hardware_class_recommendations()
                .into_iter()
                .map(|class| class.hardware_class_id)
                .collect::<Vec<_>>(),
            ["gpu", "nvidia-dedicated"]
        );
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn invalid_recommendations_are_rejected_with_their_file_use_case_and_entry() {
        let list = |use_case: &str, entries: &[(&str, &str)]| {
            let mut list = String::new();
            if !use_case.is_empty() {
                list.push_str(&format!("  - useCase: \"{use_case}\"\n    entries:\n"));
            } else {
                list.push_str("  - entries:\n");
            }
            for (model, profile) in entries {
                list.push_str(&format!(
                    "      - modelId: \"{model}\"\n        profileId: \"{profile}\"\n"
                ));
            }
            list
        };
        let file = |class: &str, lists: &[String]| {
            format!(
                "hardwareClassId: \"{class}\"\nuseCases:\n{}",
                lists.concat()
            )
        };
        let assistant = |entries: &[(&str, &str)]| list("USE_CASE_CHAT_ASSISTANT", entries);
        let balanced = [("fixture-model", "balanced")];
        let entry = "use case USE_CASE_CHAT_ASSISTANT entry 1 (model \"fixture-model\" profile \"balanced\")";
        let nvidia_support = "      - hardwareClassId: \"nvidia\"\n";
        let minimum_support = "      - hardwareClassId: \"nvidia\"\n        hardwareRequirements:\n          minimumMemoryBytesPerDevice: \"16000000000\"\n";

        for (case, support, files, expected) in [
            (
                "unknown class",
                nvidia_support,
                vec![("missing", file("missing", &[assistant(&balanced)]))],
                "recommendations/missing.yaml: model recommendations reference unknown hardware class \"missing\"".to_owned(),
            ),
            (
                "unspecified use case",
                nvidia_support,
                vec![("nvidia", file("nvidia", &[list("", &balanced)]))],
                "recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" require a use case on every list".to_owned(),
            ),
            (
                "repeated use case",
                nvidia_support,
                vec![("nvidia", file("nvidia", &[assistant(&balanced), assistant(&[("fixture-model", "fast")])]))],
                "recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" list use case USE_CASE_CHAT_ASSISTANT more than once".to_owned(),
            ),
            (
                "unknown model",
                nvidia_support,
                vec![("nvidia", file("nvidia", &[assistant(&[("missing", "balanced")])]))],
                "recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" use case USE_CASE_CHAT_ASSISTANT entry 1 (model \"missing\" profile \"balanced\") references an unknown model".to_owned(),
            ),
            (
                "unknown profile",
                nvidia_support,
                vec![("nvidia", file("nvidia", &[assistant(&[("fixture-model", "missing")])]))],
                "recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" use case USE_CASE_CHAT_ASSISTANT entry 1 (model \"fixture-model\" profile \"missing\") references an unknown profile".to_owned(),
            ),
            (
                "unrelated class",
                nvidia_support,
                vec![("apple", file("apple", &[assistant(&balanced)]))],
                format!("recommendations/apple.yaml: model recommendations for hardware class \"apple\" {entry} runs a configuration that supports neither this hardware class, a class containing it, nor a class nested in it"),
            ),
            (
                "entry minimum below the configuration's",
                minimum_support,
                vec![(
                    "nvidia",
                    file("nvidia", &[assistant(&balanced)]).replace(
                        "profileId: \"balanced\"\n",
                        "profileId: \"balanced\"\n        hardwareRequirements:\n          minimumMemoryBytesPerDevice: \"8000000000\"\n",
                    ),
                )],
                format!("recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" {entry} sets minimum memory per device 8000000000 bytes, below its configuration's 16000000000 bytes"),
            ),
            (
                "repeated entry",
                nvidia_support,
                vec![("nvidia", file("nvidia", &[assistant(&[balanced[0], balanced[0]])]))],
                "recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" use case USE_CASE_CHAT_ASSISTANT entry 2 (model \"fixture-model\" profile \"balanced\") repeats an earlier entry".to_owned(),
            ),
            (
                "class declared twice",
                nvidia_support,
                vec![
                    ("copy", file("nvidia", &[assistant(&balanced)])),
                    ("nvidia", file("nvidia", &[assistant(&balanced)])),
                ],
                "recommendations/nvidia.yaml: duplicate model recommendations for hardware class id \"nvidia\"; first declared by recommendations/copy.yaml".to_owned(),
            ),
            (
                "overlapping classes",
                nvidia_support,
                vec![
                    ("mixed", "hardwareClassId: \"mixed\"\n".to_owned()),
                    ("nvidia", file("nvidia", &[assistant(&balanced)])),
                ],
                "recommendations/nvidia.yaml: model recommendations for hardware classes \"mixed\" and \"nvidia\" overlap without one nesting in the other".to_owned(),
            ),
        ] {
            let root = recommendation_fixture(support);
            write_hardware_class(
                &root,
                "mixed",
                "selector:\n  accelerators: [\"nvidia\", \"apple\"]\n  memoryArchitecture: \"ACCELERATOR_MEMORY_ARCHITECTURE_DEDICATED\"\n",
            );
            for (file_name, body) in &files {
                super::test_support::write_recommendations(&root, file_name, body);
            }

            let error = Catalog::load_with_policy(
                &root,
                OciArtifactPolicy::Development,
                &ACCEPT_ENGINE_DECLARATIONS,
            )
            .expect_err(case)
            .to_string();

            assert_eq!(error, format!("Catalog diagnostics: {expected}"), "{case}");
            fs::remove_dir_all(root).expect("Catalog fixture cleanup");
        }
    }

    #[test]
    fn coding_entries_require_a_configuration_accepting_chat_requests() {
        let root = recommendation_fixture("      - hardwareClassId: \"nvidia\"\n");
        let path = root.join("models/fixture.yaml");
        let model = fs::read_to_string(&path).expect("fixture model");
        fs::write(
            &path,
            model.replace(
                "requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']",
                "requestKinds: ['INFERENCE_REQUEST_KIND_DECISION']",
            ),
        )
        .expect("decision-only fixture model");
        super::test_support::write_recommendations(
            &root,
            "nvidia",
            "hardwareClassId: \"nvidia\"\nuseCases:\n  - useCase: \"USE_CASE_CODING\"\n    entries:\n      - modelId: \"fixture-model\"\n        profileId: \"balanced\"\n",
        );

        let error = Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect_err("a coding entry on a decision-only configuration must reject")
        .to_string();

        assert_eq!(
            error,
            "Catalog diagnostics: recommendations/nvidia.yaml: model recommendations for hardware class \"nvidia\" use case USE_CASE_CODING entry 1 (model \"fixture-model\" profile \"balanced\") runs a configuration that doesn't accept INFERENCE_REQUEST_KIND_CHAT requests"
        );
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    /// Writes a catalog with the sizing fixture model, its `balanced` and `fast` profiles and the
    /// given hardware support entries, ready for recommendation files.
    fn recommendation_fixture(hardware_support: &str) -> std::path::PathBuf {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        write_support_model(&root, hardware_support);
        super::test_support::add_fast_sizing_profile(&root);
        root
    }

    #[test]
    fn development_source_cannot_shadow_a_master_catalog_identity() {
        let root = temporary_catalog_root();
        let master = root.join("master");
        let development = root.join("development");
        create_empty_catalog_source(&master);
        create_empty_catalog_source(&development);
        let model = two_release_model();
        write_model(&master, "master.yaml", &model);
        write_model(&development, "development.yaml", &model);

        let error =
            Catalog::load_development_sources(&master, &development, &ACCEPT_ENGINE_DECLARATIONS)
                .expect_err("development catalog must not replace a master catalog identity");

        assert!(error.to_string().contains(
            "development catalog models/development.yaml: duplicate model id \"fixture-model\"; first declared by master catalog models/master.yaml"
        ));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn source_precedence_rejects_a_site_declaration_that_replaces_a_master_identity() {
        let root = temporary_catalog_root();
        let master = root.join("master");
        let site = root.join("site");
        create_empty_catalog_source(&master);
        create_empty_catalog_source(&site);
        let application = super::test_support::application();
        write_application(&master, "master.yaml", &application);
        write_application(&site, "site.yaml", &application);

        let error = Catalog::load_sources_with_policy(
            &master,
            &site,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect_err("Site catalog must not replace a master catalog identity");

        assert!(error.to_string().contains(
            "site catalog applications/site.yaml: duplicate application id \"fixture-application\"; first declared by master catalog applications/master.yaml"
        ));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn complete_validation_prevents_partial_catalog_publication() {
        let valid = super::test_support::application();
        let mut invalid = valid.clone();
        invalid.application_id = "Invalid".to_owned();

        let result = CatalogBuilder::new()
            .application("valid", valid)
            .application("invalid", invalid)
            .build();

        assert!(matches!(result, Err(CatalogError::Diagnostics(_))));
    }

    #[test]
    fn diagnostic_output_is_deterministic_across_declaration_order() {
        let mut first = super::test_support::application();
        first.application_id = "Invalid-first".to_owned();
        let mut second = super::test_support::application();
        second.application_id = "Invalid-second".to_owned();

        let forward = CatalogBuilder::new()
            .application("z-location", first.clone())
            .application("a-location", second.clone())
            .build()
            .expect_err("invalid catalog")
            .to_string();
        let reverse = CatalogBuilder::new()
            .application("a-location", second)
            .application("z-location", first)
            .build()
            .expect_err("invalid catalog")
            .to_string();

        assert_eq!(forward, reverse);
    }

    #[test]
    fn invalid_profile_ids_and_missing_references_are_both_reported() {
        let mut model = two_release_model();
        let profile = model.profiles.first_mut().expect("model profile");
        profile.profile_id = "Invalid-profile".to_owned();
        model.configurations[0].engine_artifacts[0].engine_release_id =
            "missing-release".to_owned();

        let error = builder_with_engines()
            .model("invalid-model", model)
            .build()
            .expect_err("invalid profile")
            .to_string();

        assert!(error.contains("profile \"Invalid-profile\" has invalid provenance"));
        assert!(error.contains("references a missing engine release"));
    }

    #[test]
    fn duplicate_profiles_report_duplicate_and_nested_profile_errors() {
        let mut model = two_release_model();
        let mut duplicate = model.profiles[0].clone();
        duplicate
            .engine_parameter_defaults
            .as_mut()
            .expect("parameter defaults")
            .values[0]
            .value = Some(
            dotcore_proto::dotcore::catalog::v1::engine_parameter_value::Value::UnsignedInteger(
                1_000_000,
            ),
        );
        model.profiles.push(duplicate);

        let error = builder_with_engines()
            .model("duplicate-profile", model)
            .build()
            .expect_err("duplicate profile")
            .to_string();

        assert!(error.contains("has invalid provenance"));
        assert!(error.contains(
            "profile \"balanced\" engine parameter \"cache-mib\" value 1000000 is outside its range 0..=65536"
        ));
    }

    #[test]
    fn duplicate_bindings_and_services_preserve_independent_nested_errors() {
        let mut application = super::test_support::application();
        let graph = application.graph.as_mut().expect("Application graph");
        let service = &mut graph.services[0];
        service
            .configuration_bindings
            .push(service.configuration_bindings[0].clone());
        let mut duplicate = service.clone();
        let reference = duplicate
            .artifact
            .as_mut()
            .expect("artifact")
            .oci_mut()
            .expect("reference");
        reference.tag = Some("latest".to_owned());
        reference.digest = None;
        graph.services.push(duplicate);

        let error = CatalogBuilder::new()
            .application("duplicate-application", application)
            .build()
            .expect_err("duplicate nested declarations")
            .to_string();

        assert!(error.contains("duplicate application service name"));
        assert!(error.contains("duplicate application binding name"));
        assert!(error.contains("OCI artifact policy requires a digest"));
    }

    #[test]
    fn duplicate_services_suppress_only_ambiguous_cross_reference_diagnostics() {
        let mut application = super::test_support::application();
        let graph = application.graph.as_mut().expect("Application graph");
        let mut duplicate = graph.services[0].clone();
        duplicate.dependencies.push(ApplicationServiceDependency {
            service_name: "missing-service".to_owned(),
            condition: ApplicationDependencyCondition::Started as i32,
        });
        graph.services.push(duplicate);
        graph
            .primary_endpoint
            .as_mut()
            .expect("primary endpoint")
            .port_name = "missing-port".to_owned();

        let error = CatalogBuilder::new()
            .application("ambiguous-application", application)
            .build()
            .expect_err("duplicate services")
            .to_string();

        assert!(error.contains("duplicate application service name"));
        assert!(!error.contains("missing dependency service"));
        assert!(!error.contains("primary endpoint references missing port"));
        assert!(!error.contains("dependencies must be acyclic"));
    }

    #[test]
    fn engine_release_validation_aggregates_all_independent_errors() {
        let mut engine = fixture_release("linux");
        engine.engine_release_id = "Invalid-engine".to_owned();
        engine.engine_kind = EngineKind::Unspecified as i32;
        engine.version.clear();
        let artifact = engine.artifacts.first_mut().expect("Engine artifact");
        artifact.artifact_id = "Invalid-artifact".to_owned();
        artifact.kind = None;

        let error = CatalogBuilder::new()
            .engine_validator(&UNSUPPORTED_ENGINE_KIND_VALIDATOR)
            .engine_release("invalid-engine", engine)
            .build()
            .expect_err("invalid engine release")
            .to_string();

        for message in [
            "engine release id",
            "unsupported engine kind",
            "engine release version must not be empty",
            "engine artifact id",
            "requires an OCI or Nix artifact",
        ] {
            assert!(error.contains(message), "missing diagnostic: {message}");
        }
    }

    #[test]
    fn upstream_model_template_validation_accepts_the_openai_compatible_template() {
        let template = super::test_support::openai_compatible_template();

        CatalogBuilder::new()
            .upstream_model_template("openai-compatible.yaml", template)
            .build()
            .expect("OpenAI-compatible upstream model template");
    }

    #[test]
    fn upstream_model_template_validation_rejects_invalid_generic_template_fields() {
        let mut template = super::test_support::openai_compatible_template();
        template.upstream_model_template_id = "different-template".to_owned();
        template.display_name = "Different Endpoint".to_owned();
        template.wire_protocol = UpstreamWireProtocol::Unspecified as i32;

        let error = CatalogBuilder::new()
            .upstream_model_template("invalid.yaml", template)
            .build()
            .expect_err("invalid upstream model template")
            .to_string();

        assert!(error.contains("template id must be \"openai-compatible\""));
        assert!(error.contains("template displayName must be \"OpenAI-compatible\""));
        assert!(error.contains("wireProtocol must be OPENAI_COMPATIBLE"));
    }

    #[test]
    fn catalog_source_rejects_the_removed_top_level_template_directory() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        fs::create_dir(root.join("upstream-provider-templates"))
            .expect("legacy template directory");

        let error = Catalog::load(&root, &ACCEPT_ENGINE_DECLARATIONS)
            .expect_err("legacy top-level template directory must fail")
            .to_string();

        assert!(error.contains("upstream-provider-templates"));
        assert!(error.contains(
            "expected \"models\", \"engines\", \"hardware-classes\", \"recommendations\", or \"applications\" directory"
        ));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn catalog_source_rejects_unrecognized_model_subdirectories() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        fs::create_dir(root.join("models/other")).expect("unsupported model directory");

        let error = Catalog::load(&root, &ACCEPT_ENGINE_DECLARATIONS)
            .expect_err("unsupported model subdirectory must fail")
            .to_string();

        assert!(error.contains("models: unsupported catalog entry \"other\""));
        assert!(error.contains("expected a .yaml file or \"upstream\" directory"));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn upstream_model_template_source_rejects_old_yaml_fields() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        fs::create_dir(root.join("models/upstream")).expect("upstream model template directory");
        fs::write(
            root.join("models/upstream/legacy.yaml"),
            "providerTemplateId: openai-compatible\ndisplayName: OpenAI-compatible\nwireProtocol: UPSTREAM_WIRE_PROTOCOL_OPENAI_COMPATIBLE\n",
        )
        .expect("legacy upstream model template");

        let error = Catalog::load(&root, &ACCEPT_ENGINE_DECLARATIONS)
            .expect_err("legacy template field must fail");
        let diagnostics = error
            .source()
            .and_then(|source| source.downcast_ref::<CatalogDiagnostics>())
            .expect("Catalog diagnostics aggregate");

        assert!(error.to_string().contains("models/upstream/legacy.yaml"));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .error
                .chain()
                .filter_map(|source| source.downcast_ref::<RestrictedYamlError>())
                .any(|source| source.to_string() == "ProtoJSON codec error")
        }));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn upstream_model_template_source_rejects_non_yaml_entries() {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        fs::create_dir(root.join("models/upstream")).expect("upstream model template directory");
        fs::write(root.join("models/upstream/readme.txt"), "unsupported")
            .expect("unsupported template source");

        let error = Catalog::load(&root, &ACCEPT_ENGINE_DECLARATIONS)
            .expect_err("non-YAML upstream template source must fail")
            .to_string();

        assert!(error.contains("models/upstream: unsupported catalog entry \"readme.txt\""));
        assert!(error.contains("expected a .yaml file"));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn catalog_yaml_failures_retain_the_yaml_source_chain() {
        let root = temporary_catalog_root();
        fs::create_dir_all(root.join("models")).expect("model directory");
        fs::create_dir_all(root.join("engines")).expect("engine directory");
        fs::write(root.join("models/broken.yaml"), "modelId: [\n")
            .expect("broken model declaration");

        let error = Catalog::load(&root, &ACCEPT_ENGINE_DECLARATIONS)
            .expect_err("broken catalog YAML must fail");
        let diagnostics = error
            .source()
            .and_then(|source| source.downcast_ref::<CatalogDiagnostics>())
            .expect("Catalog diagnostics aggregate");
        let found_yaml_error = diagnostics.iter().any(|diagnostic| {
            let mut cause: Option<&(dyn std::error::Error + 'static)> =
                Some(diagnostic.error.as_ref());
            while let Some(value) = cause {
                if value.downcast_ref::<RestrictedYamlError>().is_some() {
                    return true;
                }
                cause = value.source();
            }
            false
        });
        assert!(
            found_yaml_error,
            "Catalog source chain must retain YAML failure"
        );
        assert!(error.to_string().contains("models/broken.yaml"));
        fs::remove_dir_all(root).expect("Catalog fixture cleanup");
    }

    #[test]
    fn catalog_errors_retain_all_diagnostic_sources() {
        let error = CatalogError::diagnostics(vec![
            CatalogDiagnostic::new("first", anyhow!("first diagnostic")),
            CatalogDiagnostic::new("second", anyhow!("second diagnostic")),
        ]);

        assert_eq!(
            error.to_string(),
            "Catalog diagnostics: first: first diagnostic\nsecond: second diagnostic"
        );
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<CatalogDiagnostics>())
                .expect("Catalog diagnostics aggregate")
                .iter()
                .map(|diagnostic| diagnostic.error.to_string())
                .collect::<Vec<_>>(),
            ["first diagnostic", "second diagnostic"]
        );
        assert!(error.source().is_some_and(|source| {
            source
                .downcast_ref::<CatalogDiagnostics>()
                .is_some_and(|diagnostics| diagnostics.iter().count() == 2)
        }));

        let aggregate = error
            .source()
            .and_then(|source| source.downcast_ref::<CatalogDiagnostics>())
            .expect("Catalog diagnostics aggregate");
        assert_eq!(
            aggregate
                .source()
                .expect("first diagnostic source")
                .to_string(),
            "first diagnostic"
        );
        assert_eq!(aggregate.iter().count(), 2);
    }

    #[test]
    fn catalog_fallback_display_does_not_render_anyhow_chain_or_backtrace() {
        let error = CatalogError::diagnostics(vec![CatalogDiagnostic::new(
            "catalog.yaml",
            anyhow!("leaf failure").context("catalog fallback context"),
        )]);

        let display = error.to_string();
        assert_eq!(
            display,
            "Catalog diagnostics: catalog.yaml: catalog fallback context"
        );
        assert!(!display.contains("leaf failure"));
        assert!(!display.contains("Caused by"));
        assert!(!display.contains("backtrace"));
        assert!(!display.contains("Backtrace"));
    }

    #[test]
    fn invalid_catalog_errors_name_the_catalog_category() {
        let error = CatalogError::Invalid(anyhow!("invalid catalog declaration"));

        assert_eq!(
            error.to_string(),
            "Invalid catalog: invalid catalog declaration"
        );
    }

    fn fixture_release(id: &str) -> EngineRelease {
        approved_release(id, "ENGINE_KIND_LLAMA_CPP", "fixture-schema", "65536")
    }

    /// Engine release `id` with one Linux NVIDIA artifact named after it, so releases never
    /// share an artifact id, and a two-parameter schema whose cache maximum varies.
    fn approved_release(
        id: &str,
        kind: &str,
        schema_id: &str,
        cache_maximum: &str,
    ) -> EngineRelease {
        let architecture = if id == "linux" { "amd64" } else { "arm64" };
        dotcore_proto::yaml::from_str(&format!(
            r#"engineReleaseId: "{id}"
engineKind: "{kind}"
version: "{id}-version"
parameterSchema:
  schemaId: "{schema_id}"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      exposed: true
      unsignedInteger: {{ minimum: "1024", maximum: "131072", unit: "tokens", defaultValue: "32768" }}
    - parameterId: "context-per-request"
      displayName: "Context per request"
      exposed: true
      unsignedInteger: {{ minimum: "1024", maximum: "131072", unit: "tokens" }}
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      exposed: true
      unsignedInteger: {{ minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" }}
    - parameterId: "cache-mib"
      displayName: "Cache"
      unsignedInteger: {{ minimum: "0", maximum: "{cache_maximum}", unit: "MiB" }}
artifacts:
  - artifactId: "{id}-artifact"
    oci:
      repository:
        registry:
          host: "example.invalid"
        name: "fixture/engine"
      digest:
        algorithm: "sha256"
        encoded: "1111111111111111111111111111111111111111111111111111111111111111"
    selectors:
      - platform: {{ operatingSystem: "linux", architecture: "{architecture}" }}
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#
        ))
        .expect("synthetic engine release")
    }

    /// Model whose one configuration approves the artifacts of releases `linux` and `second`.
    fn two_release_model() -> dotcore_proto::dotcore::catalog::v1::Model {
        dotcore_proto::yaml::from_str(
            r#"modelId: "fixture-model"
displayName: "Fixture model"
version: "fixture"
maximumContextTokens: 262144
artifacts:
  - artifactId: "weights"
    weightQuantization: "FIXTURE"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "weights.gguf"
        provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222"
configurations:
  - configurationId: "local"
    engineArtifacts:
      - engineReleaseId: "linux"
        engineArtifactId: "linux-artifact"
      - engineReleaseId: "second"
        engineArtifactId: "second-artifact"
    artifactBindings:
      - bindingId: "default"
        modelArtifactId: "weights"
    hardwareSupport:
      - hardwareClassId: "nvidia"
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp:
        fixedSettings:
          gpuLayers: 1
    executionCapabilities:
      supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"]
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
profiles:
  - profileId: "balanced"
    configurationId: "local"
    defaultModelArtifactBindingId: "default"
    engineParameterDefaults:
      values:
        - parameterId: "cache-mib"
          unsignedInteger: "0"
"#,
        )
        .expect("synthetic model")
    }

    /// Loads the sizing fixture model on the sizing engine, accepting every engine
    /// declaration.
    fn shared_parameter_catalog(
        sizing: &str,
        support_sizing: &str,
        profile_defaults: &str,
    ) -> Result<Catalog, CatalogError> {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        write_sizing_model_with(&root, sizing, support_sizing, profile_defaults);
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
    }

    fn unsigned_values(values: &[(&str, u64)]) -> EngineParameterValues {
        EngineParameterValues {
            values: values
                .iter()
                .map(|(parameter_id, value)| EngineParameterValue {
                    parameter_id: (*parameter_id).to_owned(),
                    value: Some(ParameterValue::UnsignedInteger(*value)),
                })
                .collect(),
        }
    }

    fn builder_with_engines() -> CatalogBuilder {
        ["linux", "second"].into_iter().fold(
            CatalogBuilder::new().engine_validator(&ACCEPT_ENGINE_DECLARATIONS),
            |builder, id| builder.engine_release(format!("engine/{id}"), fixture_release(id)),
        )
    }

    /// Builds the resolved snapshot fields sizing evaluation reads for the fixture model's
    /// `local` configuration on one hardware class, without auxiliary artifacts or options.
    fn resolved_fixture_snapshot(
        model_artifact_id: &str,
        hardware_class_id: &str,
        engine_parameter_values: EngineParameterValues,
    ) -> ResolvedLocalModelDeploymentSnapshot {
        ResolvedLocalModelDeploymentSnapshot {
            model_id: "fixture-model".to_owned(),
            configuration_id: "local".to_owned(),
            hardware_support: Some(dotcore_proto::dotcore::model::v1::ResolvedHardwareSupport {
                hardware_class_id: hardware_class_id.to_owned(),
                ..Default::default()
            }),
            model_artifact: Some(dotcore_proto::dotcore::catalog::v1::ModelArtifact {
                artifact_id: model_artifact_id.to_owned(),
                ..Default::default()
            }),
            engine_parameter_values: Some(engine_parameter_values),
            ..Default::default()
        }
    }

    /// Loads the sizing fixture model with the given hardware support entries and extra hardware
    /// classes, and returns the load error.
    fn support_errors(hardware_support: &str, extra_classes: &[(&str, &str)]) -> String {
        let root = temporary_catalog_root();
        create_empty_catalog_source(&root);
        write_sizing_engine(&root);
        for (file_name, selector) in extra_classes {
            write_hardware_class(&root, file_name, selector);
        }
        write_support_model(&root, hardware_support);
        Catalog::load_with_policy(
            &root,
            OciArtifactPolicy::Development,
            &ACCEPT_ENGINE_DECLARATIONS,
        )
        .expect_err("invalid hardware support must reject")
        .to_string()
    }

    /// Writes the unsized fixture model with its hardware support entries replaced.
    fn write_support_model(root: &std::path::Path, hardware_support: &str) {
        write_sizing_model(root, "");
        let path = root.join("models/fixture.yaml");
        let model = fs::read_to_string(&path).expect("fixture model");
        fs::write(
            &path,
            model.replace(
                "    hardwareSupport:\n      - hardwareClassId: \"nvidia\"\n",
                &format!("    hardwareSupport:\n{hardware_support}"),
            ),
        )
        .expect("fixture model with hardware support");
    }

    /// Writes one extra hardware class file whose id is `nvidia` for a `copy` file and the file
    /// name otherwise.
    fn write_hardware_class(root: &std::path::Path, file_name: &str, selector: &str) {
        let id = if file_name == "copy" {
            "nvidia"
        } else {
            file_name
        };
        fs::write(
            root.join(format!("hardware-classes/{file_name}.yaml")),
            format!("hardwareClassId: \"{id}\"\ndisplayName: \"{id}\"\n{selector}"),
        )
        .expect("hardware class fixture");
    }

    fn write_model(
        root: &std::path::Path,
        name: &str,
        model: &dotcore_proto::dotcore::catalog::v1::Model,
    ) {
        fs::write(
            root.join("models").join(name),
            serde_yaml_ng::to_string(model).expect("Model YAML"),
        )
        .expect("write model");
    }

    fn write_application(
        root: &std::path::Path,
        name: &str,
        application: &dotcore_proto::dotcore::catalog::v1::Application,
    ) {
        fs::write(
            root.join("applications").join(name),
            dotcore_proto::yaml::to_string(application).expect("Application YAML"),
        )
        .expect("Application fixture");
    }
}
