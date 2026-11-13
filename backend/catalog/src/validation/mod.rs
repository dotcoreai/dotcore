//! Validates the raw catalog records loaded from YAML and turns them into the
//! deterministic, indexed view the catalog is built from.
//!
//! The models, engine releases, hardware classes, model recommendations, applications, and upstream
//! model templates are checked against each other, and each configuration's sizing is compiled for
//! every hardware class and binding. Every failure is surfaced as a located diagnostic rather than
//! stopping the pass, so all problems are reported and the load fails only as a whole, never
//! publishing a partial catalog.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_proto::dotcore::catalog::v1::{
    Application, EngineRelease, HardwareClass, HardwareClassRecommendations, Model,
    ModelEngineConfiguration, ParameterDefinition, ParameterValues, ParameterVisibility,
    UpstreamModelTemplate,
};

use super::{
    shared_parameters::CONTEXT_PER_REQUEST_PARAMETER_ID,
    sizing::{self, CompiledSizing, SizingKey},
    source::CatalogSourceRecords,
};
pub use application::{
    valid_configuration_value, valid_environment_variable, validate_application_with_policy,
};
pub use model::{
    is_secret_parameter, overlay_parameter_defaults, valid_pinned_hugging_face_artifact,
    validate_parameter_values, validate_parameter_values_before_secrets,
    validate_supplied_parameter_values,
};
pub(super) use sensitive_literal::validate_sensitive_literals;
pub use upstream_model_template::validate_upstream_model_template;

mod application;
mod common;
mod engine_release;
mod graph;
mod hardware_class;
mod model;
mod nix;
mod recommendation;
mod sensitive_literal;
mod service;
mod storage;
mod upstream_model_template;

/// Validates engine-owned declaration semantics during catalog loading.
///
/// Implementations must return every independent problem they find so catalog loading can
/// report complete diagnostics without publishing a partially validated view.
pub trait EngineDeclarationValidator: Send + Sync {
    fn validate_engine_release(&self, engine: &EngineRelease) -> Vec<anyhow::Error>;

    /// Validates one configuration's engine payload. `hardware_classes` are the shared classes
    /// its hardware support entries reference, in entry order, so the engine can check the
    /// accelerator vendors their selectors name.
    fn validate_model_configuration(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        hardware_classes: &[&HardwareClass],
        engine: &EngineRelease,
    ) -> Vec<anyhow::Error>;

    /// Whether the engine's launch consumes catalog sizing, so every binding must declare an
    /// accelerator memory footprint estimate.
    fn launch_requires_sizing(&self, _engine: &EngineRelease) -> bool {
        false
    }

    /// Engine parameter defaults the engine derives from one configuration. They take the place
    /// of the release's defaults for the parameters they cover, while profile and supplied values
    /// still win over them.
    fn configuration_parameter_defaults(
        &self,
        _model: &Model,
        _configuration: &ModelEngineConfiguration,
        _engine: &EngineRelease,
    ) -> ParameterValues {
        ParameterValues::default()
    }
}

pub(super) fn validate_catalog(
    records: CatalogSourceRecords,
    oci_artifact_policy: super::OciArtifactPolicy,
    engine_validator: &dyn EngineDeclarationValidator,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> ValidatedCatalog {
    let CatalogSourceRecords {
        models,
        sizing_sources,
        engines,
        hardware_classes,
        recommendations,
        applications,
        upstream_model_templates,
    } = records;
    let duplicate_models =
        report_duplicates(&models, "model", |value| &value.model_id, diagnostics);
    report_duplicates(
        &engines,
        "engine release",
        |value| &value.engine_release_id,
        diagnostics,
    );
    let duplicate_applications = report_duplicates(
        &applications,
        "application",
        |value| &value.application_id,
        diagnostics,
    );
    let engine_index = validate_declarations(
        engines,
        |engine| engine.engine_release_id.clone(),
        |engine| {
            let mut errors =
                engine_release::validate_engine_release_with_policy(engine, oci_artifact_policy);
            errors.extend(engine_validator.validate_engine_release(engine));
            errors
        },
        &BTreeSet::new(),
        diagnostics,
    );
    validate_shared_parameter_schemas(&engine_index, diagnostics);
    let duplicate_hardware_classes = report_duplicates(
        &hardware_classes,
        "hardware class",
        |value| &value.hardware_class_id,
        diagnostics,
    );
    let hardware_class_index = validate_declarations(
        hardware_classes,
        |hardware_class| hardware_class.hardware_class_id.clone(),
        hardware_class::validate_hardware_class,
        &duplicate_hardware_classes,
        diagnostics,
    );
    let model_index = validate_declarations(
        models,
        |model| model.model_id.clone(),
        |model| {
            model::validate_model(
                model,
                &engine_index,
                &hardware_class_index,
                engine_validator,
            )
        },
        &duplicate_models,
        diagnostics,
    );
    let sizing = compile_sizing(
        sizing_sources,
        &model_index,
        &engine_index,
        &hardware_class_index,
        diagnostics,
    );
    let engine_defaults =
        configuration_engine_defaults(&model_index, &engine_index, engine_validator);
    validate_sizing_coverage(
        &model_index,
        &engine_index,
        &sizing,
        &engine_defaults,
        engine_validator,
        diagnostics,
    );
    let duplicate_recommendations = report_duplicates(
        &recommendations,
        "model recommendations for hardware class",
        |value| &value.hardware_class_id,
        diagnostics,
    );
    recommendation::report_overlapping_recommendation_classes(
        &recommendations,
        &hardware_class_index,
        diagnostics,
    );
    let recommendation_index = validate_declarations(
        recommendations,
        |value| value.hardware_class_id.clone(),
        |value| {
            recommendation::validate_recommendations(value, &model_index, &hardware_class_index)
        },
        &duplicate_recommendations,
        diagnostics,
    );
    let application_index = validate_declarations(
        applications,
        |application| application.application_id.clone(),
        |application| {
            application::validate_application_with_policy(application, oci_artifact_policy)
        },
        &duplicate_applications,
        diagnostics,
    );
    let duplicate_upstream_model_templates = report_duplicates(
        &upstream_model_templates,
        "upstream model template",
        |value| &value.upstream_model_template_id,
        diagnostics,
    );
    let upstream_model_template_index = validate_declarations(
        upstream_model_templates,
        |template| template.upstream_model_template_id.clone(),
        upstream_model_template::validate_upstream_model_template,
        &duplicate_upstream_model_templates,
        diagnostics,
    );
    ValidatedCatalog {
        models: model_index,
        engines: engine_index,
        hardware_classes: hardware_class_index,
        recommendations: recommendation_index,
        applications: application_index,
        upstream_model_templates: upstream_model_template_index,
        sizing,
        engine_defaults,
    }
}

/// Engine parameter defaults of each configuration, keyed by model and configuration id.
pub(super) type ConfigurationEngineDefaults = BTreeMap<(String, String), ParameterValues>;

pub(super) struct ValidatedCatalog {
    pub(super) models: BTreeMap<String, Model>,
    pub(super) engines: BTreeMap<String, EngineRelease>,
    pub(super) hardware_classes: BTreeMap<String, HardwareClass>,
    pub(super) recommendations: BTreeMap<String, HardwareClassRecommendations>,
    pub(super) applications: BTreeMap<String, Application>,
    pub(super) upstream_model_templates: BTreeMap<String, UpstreamModelTemplate>,
    pub(super) sizing: BTreeMap<SizingKey, CompiledSizing>,
    pub(super) engine_defaults: ConfigurationEngineDefaults,
}

#[derive(Debug)]
pub struct CatalogDiagnostic {
    pub location: String,
    pub error: anyhow::Error,
}

impl CatalogDiagnostic {
    pub(super) fn new(location: impl Into<String>, error: anyhow::Error) -> Self {
        Self {
            location: location.into(),
            error,
        }
    }
}

impl std::fmt::Display for CatalogDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.location, self.error)
    }
}

/// Checks that engine releases of one engine kind sharing a parameter schema id define identical
/// parameters. The schema id is the identity profiles and deployments rely on, so every release
/// carrying it must mean the same parameters.
fn validate_shared_parameter_schemas(
    engines: &BTreeMap<String, EngineRelease>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    let mut first_releases = BTreeMap::new();
    for engine in engines.values() {
        let Some(schema) = engine.parameter_schema.as_ref() else {
            continue;
        };
        let Some(first) = first_releases
            .get(&(engine.engine_kind, schema.schema_id.as_str()))
            .copied()
        else {
            first_releases.insert((engine.engine_kind, schema.schema_id.as_str()), engine);
            continue;
        };
        let first_parameters = first
            .parameter_schema
            .as_ref()
            .map(|schema| schema.parameters.as_slice())
            .unwrap_or_default();
        let parameter_ids = first_parameters
            .iter()
            .chain(&schema.parameters)
            .map(|parameter| parameter.parameter_id.as_str())
            .collect::<BTreeSet<_>>();
        for parameter_id in parameter_ids {
            if parameter_definition(first_parameters, parameter_id)
                != parameter_definition(&schema.parameters, parameter_id)
            {
                diagnostics.push(CatalogDiagnostic::new(
                    format!("engine release {:?}", engine.engine_release_id),
                    anyhow::anyhow!(
                        "engine releases {:?} and {:?} share parameter schema {:?} but define parameter {parameter_id:?} differently",
                        first.engine_release_id,
                        engine.engine_release_id,
                        schema.schema_id
                    ),
                ));
            }
        }
    }
}

/// Merges each configuration's sizing layers per hardware class and binding and compiles the
/// result. Layers of a configuration that failed validation, or of an unknown hardware class,
/// are skipped; their problems are already reported.
fn compile_sizing(
    sources: Vec<(String, sizing::CatalogSizingSource)>,
    models: &BTreeMap<String, Model>,
    engines: &BTreeMap<String, EngineRelease>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> BTreeMap<SizingKey, CompiledSizing> {
    let mut compiled = BTreeMap::new();
    for (location, source) in sources {
        let Some(configuration) = models.get(&source.model_id).and_then(|model| {
            model
                .configurations
                .iter()
                .find(|configuration| configuration.configuration_id == source.configuration_id)
        }) else {
            continue;
        };
        let Some(engine) = configuration_engine_release(configuration, engines) else {
            continue;
        };
        let binding_ids = configuration
            .artifact_bindings
            .iter()
            .map(|binding| binding.binding_id.as_str())
            .collect::<Vec<_>>();
        let (flattened, errors) = source.flatten(&binding_ids, |hardware_class_id| {
            hardware_classes.contains_key(hardware_class_id)
        });
        diagnostics.extend(
            errors
                .into_iter()
                .map(|error| CatalogDiagnostic::new(&location, error)),
        );
        for flat in flattened {
            let key = SizingKey {
                model_id: source.model_id.clone(),
                configuration_id: source.configuration_id.clone(),
                hardware_class_id: flat.hardware_class_id,
                binding_id: flat.binding_id,
            };
            let Some(schema) = engine.parameter_schema.as_ref() else {
                diagnostics.push(CatalogDiagnostic::new(
                    &location,
                    anyhow::anyhow!("containing engine release has no parameter schema"),
                ));
                continue;
            };
            match sizing::compile(&flat.source, schema, configuration) {
                Ok(sizing) => {
                    if compiled.insert(key, sizing).is_some() {
                        diagnostics.push(CatalogDiagnostic::new(
                            &location,
                            anyhow::anyhow!("duplicate catalog sizing declaration"),
                        ));
                    }
                }
                Err(error) => {
                    diagnostics.push(CatalogDiagnostic::new(&location, anyhow::Error::new(error)))
                }
            }
        }
    }
    compiled
}

/// Checks that sizing covers what configurations rely on: engines requiring sizing get an
/// accelerator memory estimate for every binding on every supported hardware class, no parameter
/// both has an engine default and is derived, and every engine parameter a profile omits without an
/// engine default, other than the filled context per request, is derived by every binding of the
/// profile's configuration on every supported class.
fn validate_sizing_coverage(
    models: &BTreeMap<String, Model>,
    engines: &BTreeMap<String, EngineRelease>,
    sizing: &BTreeMap<SizingKey, CompiledSizing>,
    engine_defaults: &ConfigurationEngineDefaults,
    engine_validator: &dyn EngineDeclarationValidator,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for model in models.values() {
        let location = format!("model {:?}", model.model_id);
        for configuration in &model.configurations {
            let Some(engine) = configuration_engine_release(configuration, engines) else {
                continue;
            };
            let binding_sizing = |hardware_class_id: &str, binding_id: &str| {
                sizing.get(&SizingKey {
                    model_id: model.model_id.clone(),
                    configuration_id: configuration.configuration_id.clone(),
                    hardware_class_id: hardware_class_id.to_owned(),
                    binding_id: binding_id.to_owned(),
                })
            };
            let class_bindings = || {
                configuration.hardware_support.iter().flat_map(|support| {
                    configuration
                        .artifact_bindings
                        .iter()
                        .map(move |binding| (support.hardware_class_id.as_str(), binding))
                })
            };

            if engine_validator.launch_requires_sizing(engine) {
                for (hardware_class_id, binding) in class_bindings() {
                    if !binding_sizing(hardware_class_id, &binding.binding_id).is_some_and(
                        |sizing| sizing.declares_estimate(sizing::ACCELERATOR_MEMORY_RESOURCE),
                    ) {
                        diagnostics.push(CatalogDiagnostic::new(
                            &location,
                            anyhow::anyhow!(
                                "configuration {:?} hardware class {hardware_class_id:?} binding {:?} requires a {:?} sizing estimate for its engine",
                                configuration.configuration_id,
                                binding.binding_id,
                                sizing::ACCELERATOR_MEMORY_RESOURCE
                            ),
                        ));
                    }
                }
            }

            let engine_defaults = engine_defaults
                .get(&(
                    model.model_id.clone(),
                    configuration.configuration_id.clone(),
                ))
                .cloned()
                .unwrap_or_default();
            for default in &engine_defaults.values {
                if class_bindings().any(|(hardware_class_id, binding)| {
                    binding_sizing(hardware_class_id, &binding.binding_id)
                        .is_some_and(|sizing| sizing.derives(&default.parameter_id))
                }) {
                    diagnostics.push(CatalogDiagnostic::new(
                        &location,
                        anyhow::anyhow!(
                            "configuration {:?} sizing derives engine parameter {:?}, which its engine release also defaults; drop one of them",
                            configuration.configuration_id,
                            default.parameter_id
                        ),
                    ));
                }
            }

            for profile in model
                .profiles
                .iter()
                .filter(|profile| profile.configuration_id == configuration.configuration_id)
            {
                let Some(values) = profile.engine_parameter_defaults.as_ref() else {
                    continue;
                };
                let defaulted = model::overlay_parameter_defaults(values, &engine_defaults);
                // Resolution fills the context per request and deployment creation generates
                // secrets, so no sizing needs to derive either.
                for parameter_id in model::missing_parameter_ids(&defaulted, engine)
                    .into_iter()
                    .filter(|parameter_id| {
                        parameter_id != CONTEXT_PER_REQUEST_PARAMETER_ID
                            && !model::is_secret_parameter(engine, parameter_id)
                    })
                {
                    let hidden = profile.engine_parameter_visibility_overrides.iter().any(
                        |visibility_override| {
                            visibility_override.parameter_id == parameter_id
                                && visibility_override.visibility() == ParameterVisibility::Hidden
                        },
                    );
                    // A hidden parameter is one the operator can never set, so it needs a value
                    // on every binding and class.
                    if hidden
                        && class_bindings().any(|(hardware_class_id, binding)| {
                            !binding_sizing(hardware_class_id, &binding.binding_id)
                                .is_some_and(|sizing| sizing.derives(&parameter_id))
                        })
                    {
                        diagnostics.push(CatalogDiagnostic::new(
                            &location,
                            anyhow::anyhow!(
                                "profile {:?} hides engine parameter {parameter_id:?}, which has no default and which sizing does not derive on every binding and hardware class",
                                profile.profile_id
                            ),
                        ));
                        continue;
                    }
                    for (hardware_class_id, binding) in class_bindings() {
                        if !binding_sizing(hardware_class_id, &binding.binding_id)
                            .is_some_and(|sizing| sizing.derives(&parameter_id))
                        {
                            diagnostics.push(CatalogDiagnostic::new(
                                &location,
                                anyhow::anyhow!(
                                    "profile {:?} omits engine parameter {parameter_id:?}, which binding {:?} does not derive on hardware class {hardware_class_id:?}",
                                    profile.profile_id,
                                    binding.binding_id
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn parameter_definition<'a>(
    parameters: &'a [ParameterDefinition],
    parameter_id: &str,
) -> Option<&'a ParameterDefinition> {
    parameters
        .iter()
        .find(|parameter| parameter.parameter_id == parameter_id)
}

/// Returns the engine parameter defaults of every configuration whose engine release is known: the
/// engine's configuration defaults, then the release default of each parameter they leave out.
fn configuration_engine_defaults(
    models: &BTreeMap<String, Model>,
    engines: &BTreeMap<String, EngineRelease>,
    engine_validator: &dyn EngineDeclarationValidator,
) -> ConfigurationEngineDefaults {
    models
        .values()
        .flat_map(|model| {
            model
                .configurations
                .iter()
                .filter_map(move |configuration| {
                    let engine = configuration_engine_release(configuration, engines)?;
                    let defaults = model::overlay_parameter_defaults(
                        &engine_validator.configuration_parameter_defaults(
                            model,
                            configuration,
                            engine,
                        ),
                        &model::engine_default_values(engine),
                    );
                    Some((
                        (
                            model.model_id.clone(),
                            configuration.configuration_id.clone(),
                        ),
                        defaults,
                    ))
                })
        })
        .collect()
}

/// Returns the engine release that stands for all of a configuration's approved releases: the
/// release of its first approved artifact. Configuration validation requires every approved
/// release to share one engine kind and identical parameter definitions, so for a validated
/// configuration any approved release defines the same engine parameters.
pub(super) fn configuration_engine_release<'a>(
    configuration: &ModelEngineConfiguration,
    engines: &'a BTreeMap<String, EngineRelease>,
) -> Option<&'a EngineRelease> {
    configuration
        .engine_artifacts
        .first()
        .and_then(|reference| engines.get(&reference.engine_release_id))
}

fn report_duplicates<T>(
    declarations: &[(String, T)],
    kind: &str,
    id: impl Fn(&T) -> &String,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> BTreeSet<String> {
    let mut first = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for (location, declaration) in declarations {
        let id = id(declaration);
        if let Some(first_location) = first.get(id) {
            duplicates.insert(id.clone());
            diagnostics.push(CatalogDiagnostic::new(
                location,
                anyhow::anyhow!("duplicate {kind} id {id:?}; first declared by {first_location}"),
            ));
        } else {
            first.insert(id, location);
        }
    }
    duplicates
}

fn validate_declarations<T>(
    declarations: Vec<(String, T)>,
    id: impl Fn(&T) -> String,
    validate: impl Fn(&T) -> Vec<anyhow::Error>,
    duplicates: &BTreeSet<String>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> BTreeMap<String, T> {
    let mut index = BTreeMap::new();
    for (location, declaration) in declarations {
        let errors = validate(&declaration);
        let declaration_id = id(&declaration);
        if errors.is_empty() && !duplicates.contains(&declaration_id) {
            index.entry(declaration_id).or_insert(declaration);
        } else {
            diagnostics.extend(
                errors
                    .into_iter()
                    .map(|error| CatalogDiagnostic::new(&location, error)),
            );
        }
    }
    index
}
