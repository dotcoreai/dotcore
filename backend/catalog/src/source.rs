//! Reads the typed catalog declarations out of a catalog root's YAML files and records
//! where each problem came from.
//!
//! It walks the model, engine, hardware class, recommendation, application, and upstream-template
//! directories and parses every file into a typed record, extracting each configuration's private
//! sizing layers along the way. Only the master catalog may declare model recommendations.
//! A file that cannot be read or parsed is reported as a located diagnostic rather than
//! aborting the whole load, so one bad file never hides the rest.

use std::{
    fs,
    path::{Path, PathBuf},
};

use dotcore_proto::{
    dotcore::catalog::v1::{
        Application, EngineRelease, HardwareClass, HardwareClassRecommendations, Model,
        UpstreamModelTemplate,
    },
    yaml,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::{
    sizing::{CatalogSizingSource, ConfigurationSizingSource, SupportSizingSource},
    validation::{CatalogDiagnostic, validate_sensitive_literals},
};

pub(super) fn load_catalog_source(
    catalog_root: &Path,
    source: &str,
    role: CatalogSourceRole,
    records: &mut CatalogSourceRecords,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    if !validate_catalog_root(catalog_root, source, role, diagnostics) {
        return;
    }
    load_models(
        catalog_root,
        source,
        &mut records.models,
        &mut records.sizing_sources,
        diagnostics,
    );
    load_engines(catalog_root, source, &mut records.engines, diagnostics);
    load_hardware_classes(
        catalog_root,
        source,
        &mut records.hardware_classes,
        diagnostics,
    );
    load_recommendations(
        catalog_root,
        source,
        role,
        &mut records.recommendations,
        diagnostics,
    );
    load_applications(
        catalog_root,
        source,
        role,
        &mut records.applications,
        diagnostics,
    );
    load_upstream_model_templates(
        catalog_root,
        source,
        &mut records.upstream_model_templates,
        diagnostics,
    );
}

/// What a catalog source is: the required master catalog, or an optional layer added on top of it,
/// such as a site or development catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CatalogSourceRole {
    Master,
    Layer,
}

#[derive(Default)]
pub(super) struct CatalogSourceRecords {
    pub(super) models: Vec<(String, Model)>,
    pub(super) sizing_sources: Vec<(String, CatalogSizingSource)>,
    pub(super) engines: Vec<(String, EngineRelease)>,
    pub(super) hardware_classes: Vec<(String, HardwareClass)>,
    pub(super) recommendations: Vec<(String, HardwareClassRecommendations)>,
    pub(super) applications: Vec<(String, Application)>,
    pub(super) upstream_model_templates: Vec<(String, UpstreamModelTemplate)>,
}

pub(super) const MODEL_DIRECTORY: &str = "models";
pub(super) const ENGINE_DIRECTORY: &str = "engines";
pub(super) const HARDWARE_CLASS_DIRECTORY: &str = "hardware-classes";
pub(super) const RECOMMENDATION_DIRECTORY: &str = "recommendations";
pub(super) const APPLICATION_DIRECTORY: &str = "applications";
pub(super) const UPSTREAM_MODEL_DIRECTORY: &str = "upstream";
pub(super) const UPSTREAM_MODEL_TEMPLATE_DIRECTORY: &str = "models/upstream";

pub(super) struct CatalogFile {
    pub(super) location: String,
    pub(super) path: PathBuf,
}

fn load_models(
    catalog_root: &Path,
    source: &str,
    models: &mut Vec<(String, Model)>,
    sizing_sources: &mut Vec<(String, CatalogSizingSource)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in catalog_files(
        catalog_root,
        source,
        MODEL_DIRECTORY,
        Some(UPSTREAM_MODEL_DIRECTORY),
        diagnostics,
    ) {
        match load_model(&file.path) {
            Ok((model, sizing)) => {
                sizing_sources.extend(
                    sizing
                        .into_iter()
                        .map(|sizing| (file.location.clone(), sizing)),
                );
                models.push((file.location, model));
            }
            Err(error) => diagnostics.push(CatalogDiagnostic::new(file.location, error)),
        }
    }
}

fn load_model(path: &Path) -> Result<(Model, Vec<CatalogSizingSource>), anyhow::Error> {
    let mut value = yaml::value_from_file(path).map_err(anyhow::Error::new)?;
    let model_id = value
        .get("modelId")
        .or_else(|| value.get("model_id"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    let sizing = extract_sizing(&model_id, &mut value)?;
    let model = yaml::from_value::<Model>(value).map_err(anyhow::Error::new)?;
    Ok((model, sizing))
}

/// Removes every configuration's private sizing layers from the model YAML before it is decoded:
/// the configuration-level `sizing` block and each hardware support entry's class and binding
/// constants. Every support entry joins the source, including one without constants, so merging
/// reports the constants it misses. Configurations without any sizing yield no sizing source.
fn extract_sizing(
    model_id: &str,
    value: &mut JsonValue,
) -> Result<Vec<CatalogSizingSource>, anyhow::Error> {
    let Some(configurations) = value
        .as_object_mut()
        .and_then(|model| model.get_mut("configurations"))
        .and_then(JsonValue::as_array_mut)
    else {
        return Ok(Vec::new());
    };
    let mut sizing = Vec::new();
    for configuration in configurations {
        let Some(configuration) = configuration.as_object_mut() else {
            continue;
        };
        let configuration_id = string_field(configuration, "configurationId", "configuration_id");
        let declaration = configuration
            .remove("sizing")
            .map(serde_json::from_value::<ConfigurationSizingSource>)
            .transpose()
            .map_err(|error| anyhow::Error::new(error).context("invalid catalog sizing block"))?;
        let mut support = Vec::new();
        let mut sets_constants = false;
        let support_key = field_key(configuration, "hardwareSupport", "hardware_support");
        if let Some(entries) = configuration
            .get_mut(support_key)
            .and_then(JsonValue::as_array_mut)
        {
            for entry in entries {
                let Some(entry) = entry.as_object_mut() else {
                    continue;
                };
                let hardware_class_id = string_field(entry, "hardwareClassId", "hardware_class_id");
                let mut constants = JsonMap::new();
                for key in ["parameters", "bindingSizing"] {
                    if let Some(constants_value) = entry.remove(key) {
                        constants.insert(key.to_owned(), constants_value);
                    }
                }
                sets_constants |= !constants.is_empty();
                let constants =
                    serde_json::from_value::<SupportSizingSource>(JsonValue::Object(constants))
                        .map_err(|error| {
                            anyhow::Error::new(error)
                                .context("invalid catalog hardware support sizing")
                        })?;
                support.push((hardware_class_id, constants));
            }
        }
        if declaration.is_some() || sets_constants {
            sizing.push(CatalogSizingSource {
                model_id: model_id.to_owned(),
                configuration_id,
                declaration,
                support,
            });
        }
    }
    Ok(sizing)
}

/// Returns the key a protobuf YAML object uses for a field, preferring the camel-case spelling.
fn field_key(
    object: &JsonMap<String, JsonValue>,
    camel_case: &'static str,
    snake_case: &'static str,
) -> &'static str {
    if object.contains_key(camel_case) {
        camel_case
    } else {
        snake_case
    }
}

fn string_field(
    object: &JsonMap<String, JsonValue>,
    camel_case: &'static str,
    snake_case: &'static str,
) -> String {
    object
        .get(field_key(object, camel_case, snake_case))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn load_engines(
    catalog_root: &Path,
    source: &str,
    engines: &mut Vec<(String, EngineRelease)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in catalog_files(catalog_root, source, ENGINE_DIRECTORY, None, diagnostics) {
        match yaml::from_file(&file.path) {
            Ok(engine) => engines.push((file.location, engine)),
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::Error::new(error),
            )),
        }
    }
}

fn load_hardware_classes(
    catalog_root: &Path,
    source: &str,
    hardware_classes: &mut Vec<(String, HardwareClass)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in optional_catalog_files(catalog_root, source, HARDWARE_CLASS_DIRECTORY, diagnostics)
    {
        match yaml::from_file(&file.path) {
            Ok(hardware_class) => hardware_classes.push((file.location, hardware_class)),
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::Error::new(error),
            )),
        }
    }
}

/// Loads the master catalog's model recommendations. A layer can't add or replace them, since a
/// file for a narrower class would shadow the master's list for that hardware, so each of its
/// recommendation files is reported instead.
fn load_recommendations(
    catalog_root: &Path,
    source: &str,
    role: CatalogSourceRole,
    recommendations: &mut Vec<(String, HardwareClassRecommendations)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in optional_catalog_files(catalog_root, source, RECOMMENDATION_DIRECTORY, diagnostics)
    {
        if role == CatalogSourceRole::Layer {
            diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::anyhow!("model recommendations can only be declared by the master catalog"),
            ));
            continue;
        }
        match yaml::from_file(&file.path) {
            Ok(value) => recommendations.push((file.location, value)),
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::Error::new(error),
            )),
        }
    }
}

fn load_applications(
    catalog_root: &Path,
    source: &str,
    role: CatalogSourceRole,
    applications: &mut Vec<(String, Application)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in optional_catalog_files(catalog_root, source, APPLICATION_DIRECTORY, diagnostics) {
        match yaml::from_file(&file.path) {
            Ok(application) => {
                // Site and development additions belong to their operators; only the master
                // catalog ships to every site.
                if role == CatalogSourceRole::Master {
                    diagnostics.extend(
                        validate_sensitive_literals(&application)
                            .into_iter()
                            .map(|error| CatalogDiagnostic::new(file.location.clone(), error)),
                    );
                }
                applications.push((file.location, application));
            }
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::Error::new(error),
            )),
        }
    }
}

fn load_upstream_model_templates(
    catalog_root: &Path,
    source: &str,
    upstream_model_templates: &mut Vec<(String, UpstreamModelTemplate)>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    for file in optional_catalog_files(
        catalog_root,
        source,
        UPSTREAM_MODEL_TEMPLATE_DIRECTORY,
        diagnostics,
    ) {
        match yaml::from_file(&file.path) {
            Ok(template) => upstream_model_templates.push((file.location, template)),
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                file.location,
                anyhow::Error::new(error),
            )),
        }
    }
}

fn validate_catalog_root(
    root: &Path,
    source: &str,
    role: CatalogSourceRole,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> bool {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            if role == CatalogSourceRole::Layer && error.kind() == std::io::ErrorKind::NotFound {
                return false;
            }
            diagnostics.push(CatalogDiagnostic::new(
                catalog_location(source, "catalog"),
                anyhow::Error::new(error).context("could not read catalog root"),
            ));
            return false;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(CatalogDiagnostic::new(
                    catalog_location(source, "catalog"),
                    anyhow::Error::new(error).context("could not read catalog root entry"),
                ));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(kind)
                if kind.is_dir()
                    && matches!(
                        name.as_str(),
                        MODEL_DIRECTORY
                            | ENGINE_DIRECTORY
                            | HARDWARE_CLASS_DIRECTORY
                            | RECOMMENDATION_DIRECTORY
                            | APPLICATION_DIRECTORY
                    ) =>
                {
            }
            Ok(_) => diagnostics.push(CatalogDiagnostic::new(
                catalog_location(source, name),
                anyhow::anyhow!("unexpected catalog root entry; expected \"models\", \"engines\", \"hardware-classes\", \"recommendations\", or \"applications\" directory"),
            )),
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                catalog_location(source, name),
                anyhow::Error::new(error).context("could not inspect catalog root entry"),
            )),
        }
    }
    true
}

fn optional_catalog_files(
    root: &Path,
    source: &str,
    directory: &str,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> Vec<CatalogFile> {
    if !root.join(directory).exists() {
        return Vec::new();
    }
    catalog_files(root, source, directory, None, diagnostics)
}

fn catalog_files(
    root: &Path,
    source: &str,
    directory: &str,
    allowed_subdirectory: Option<&str>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) -> Vec<CatalogFile> {
    let entries = match fs::read_dir(root.join(directory)) {
        Ok(entries) => entries,
        Err(error) => {
            let message = if error.kind() == std::io::ErrorKind::NotFound {
                "required catalog directory does not exist"
            } else {
                "could not read catalog directory"
            };
            diagnostics.push(CatalogDiagnostic::new(
                catalog_location(source, directory),
                anyhow::Error::new(error).context(message),
            ));
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(CatalogDiagnostic::new(
                    catalog_location(source, directory),
                    anyhow::Error::new(error).context("could not read catalog entry"),
                ));
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(kind) if kind.is_file() && path.extension().is_some_and(|value| value == "yaml") => {
                files.push(CatalogFile {
                    location: catalog_location(source, format!("{directory}/{name}")),
                    path,
                })
            }
            Ok(kind) if kind.is_dir() && allowed_subdirectory == Some(name.as_str()) => {}
            Ok(_) => {
                let expected = allowed_subdirectory.map_or_else(
                    || "a .yaml file".to_owned(),
                    |subdirectory| format!("a .yaml file or {subdirectory:?} directory"),
                );
                diagnostics.push(CatalogDiagnostic::new(
                    catalog_location(source, directory),
                    anyhow::anyhow!("unsupported catalog entry {name:?}; expected {expected}"),
                ));
            }
            Err(error) => diagnostics.push(CatalogDiagnostic::new(
                catalog_location(source, directory),
                anyhow::Error::new(error).context("could not inspect catalog entry"),
            )),
        }
    }
    files.sort_by(|left, right| left.location.cmp(&right.location));
    files
}

fn catalog_location(source: &str, location: impl std::fmt::Display) -> String {
    if source.is_empty() {
        location.to_string()
    } else {
        format!("{source} {location}")
    }
}
