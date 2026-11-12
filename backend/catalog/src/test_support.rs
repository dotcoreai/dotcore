//! Catalog source fixtures shared by the unit tests of the catalog and of its consumers: a source
//! layout with the `nvidia` and `apple` hardware classes, a minimal llama.cpp engine release, with
//! its own parameters or the shared context parameters, and a model whose configuration carries
//! caller-supplied sizing, and an engine declaration validator that accepts every declaration.
//! It also checks that a loaded catalog's sizing is linear in the total context.

use std::{collections::BTreeMap, fs, path::PathBuf};

use dotcore_domain::test_support::{canonical_temp_dir, unique_suffix};
use dotcore_proto::dotcore::catalog::v1::{
    Application, EngineRelease, HardwareClass, HardwareSupport, Model, ModelEngineConfiguration,
    ModelOptionParameterValue, ModelOptionValues, ModelProfile, ParameterValue, Selector,
    UpstreamModelTemplate, UpstreamWireProtocol, model_option_parameter_definition,
    model_option_parameter_value, parameter_value,
};

use crate::{
    Catalog, CatalogModelId, EngineDeclarationValidator, options,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID, filled_context_per_request,
        maximum_context_tokens,
    },
    validation,
};

/// Engine declaration validator that accepts every engine release and model configuration, for
/// tests whose catalog behavior does not depend on engine-specific rules.
pub static ACCEPT_ENGINE_DECLARATIONS: AcceptEngineDeclarations = AcceptEngineDeclarations;

/// Engine declaration validator behind [`ACCEPT_ENGINE_DECLARATIONS`].
#[derive(Debug)]
pub struct AcceptEngineDeclarations;

impl EngineDeclarationValidator for AcceptEngineDeclarations {
    fn validate_engine_release(&self, _engine: &EngineRelease) -> Vec<anyhow::Error> {
        Vec::new()
    }

    fn validate_model_configuration(
        &self,
        _model: &Model,
        _configuration: &ModelEngineConfiguration,
        _hardware_classes: &[&HardwareClass],
        _engine: &EngineRelease,
    ) -> Vec<anyhow::Error> {
        Vec::new()
    }
}

/// Describes every sizing estimate and derived parameter of the catalog that isn't linear in the
/// total context, the shape the compatibility read's largest fitting total context relies on.
/// Each profile's defaults are evaluated at three totals, with the context per request filled as
/// resolution fills it, so one depending on the context per request bends at the maximum context.
/// Every combination of declared boolean and string-enum option values the profile accepts is
/// evaluated too, since operators may choose any of them.
pub fn sizing_not_linear_in_total_context(catalog: &Catalog) -> Vec<String> {
    let mut problems = Vec::new();
    for (key, sizing) in &catalog.sizing {
        let Some(model) = catalog.models.get(&key.model_id) else {
            continue;
        };
        let Some(configuration) = model
            .configurations
            .iter()
            .find(|configuration| configuration.configuration_id == key.configuration_id)
        else {
            continue;
        };
        let Some(schema) =
            validation::configuration_engine_release(configuration, &catalog.engines)
                .and_then(|engine| engine.parameter_schema.as_ref())
        else {
            continue;
        };
        let maximum = maximum_context_tokens(model, configuration);
        let middle = u64::from(maximum).max(2048);
        let totals = [1024, middle, 2 * middle];
        for profile in model
            .profiles
            .iter()
            .filter(|profile| profile.configuration_id == key.configuration_id)
        {
            for (choice, options) in option_variants(model, configuration, profile) {
                let label = format!(
                    "model {:?} profile {:?}{choice} hardware class {:?} binding {:?}",
                    key.model_id, profile.profile_id, key.hardware_class_id, key.binding_id
                );
                let options = match options {
                    Ok(options) => options,
                    Err(error) => {
                        problems.push(format!("{label} options don't resolve: {error:#}"));
                        continue;
                    }
                };
                let evaluated = totals.map(|total| {
                    let (_, defaults) = catalog
                        .local_defaults_for(
                            &CatalogModelId(key.model_id.clone()),
                            &profile.profile_id,
                        )
                        .ok_or_else(|| anyhow::anyhow!("profile defaults don't resolve"))?;
                    let mut values = defaults;
                    values.values.retain(|value| {
                        value.parameter_id != TOTAL_CONTEXT_PARAMETER_ID
                            && value.parameter_id != CONTEXT_PER_REQUEST_PARAMETER_ID
                    });
                    values
                        .values
                        .push(unsigned_value(TOTAL_CONTEXT_PARAMETER_ID, total));
                    let filled = filled_context_per_request(schema, &values, maximum);
                    values.values.extend(filled);
                    let derived = sizing.derive(&values, &options)?;
                    values.values.extend(
                        derived
                            .iter()
                            .map(|(parameter_id, value)| unsigned_value(parameter_id, *value)),
                    );
                    let results = derived
                        .into_iter()
                        .chain(
                            sizing
                                .evaluate(&values, &options)?
                                .into_iter()
                                .map(|estimate| (estimate.resource, estimate.value)),
                        )
                        .collect::<BTreeMap<_, _>>();
                    anyhow::Ok(results)
                });
                let [Ok(first), Ok(second), Ok(third)] = evaluated else {
                    problems.push(format!("{label} doesn't evaluate at every total context"));
                    continue;
                };
                for (name, first_value) in &first {
                    let (Some(second_value), Some(third_value)) =
                        (second.get(name), third.get(name))
                    else {
                        problems.push(format!(
                            "{label} {name:?} isn't evaluated at every total context"
                        ));
                        continue;
                    };
                    let [t1, t2, t3] = totals.map(i128::from);
                    let [v1, v2, v3] = [*first_value, *second_value, *third_value].map(i128::from);
                    if (v2 - v1) * (t3 - t2) != (v3 - v2) * (t2 - t1) {
                        problems.push(format!(
                            "{label} {name:?} isn't linear in the total context: {v1}, {v2} and {v3} at {t1}, {t2} and {t3} tokens"
                        ));
                    }
                }
            }
        }
    }
    problems
}

/// Hardware class whose selector names exactly the given accelerator vendor, with the vendor as
/// its id, such as the `nvidia` class.
pub fn vendor_hardware_class(vendor: &str) -> HardwareClass {
    HardwareClass {
        hardware_class_id: vendor.to_owned(),
        display_name: vendor.to_uppercase(),
        description: String::new(),
        selector: Some(Selector {
            platform: None,
            accelerators: vec![vendor.to_owned()],
            minimum_nvidia_driver_major_version: 0,
            ..Default::default()
        }),
    }
}

/// Hardware support for one class without memory minimums.
pub fn hardware_support(hardware_class_id: &str) -> Vec<HardwareSupport> {
    vec![HardwareSupport {
        hardware_class_id: hardware_class_id.to_owned(),
        hardware_requirements: None,
    }]
}

/// Minimal valid application: one `web` service pinned by digest with one literal configuration
/// binding, an HTTP port and its primary endpoint.
pub fn application() -> Application {
    dotcore_proto::yaml::from_str(
        r#"applicationId: "fixture-application"
displayName: "Fixture application"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        oci:
          repository:
            registry:
              host: "example.invalid"
            name: "fixture/web"
          digest:
            algorithm: "sha256"
            encoded: "1111111111111111111111111111111111111111111111111111111111111111"
        supportedPlatforms:
          - operatingSystem: "linux"
            architecture: "amd64"
      ports:
        - portName: "http"
          portNumber: 8080
      configurationBindings:
        - bindingName: "fixture-flag"
          environmentVariable: "FIXTURE_FLAG"
          literalValue: "false"
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
    )
    .expect("synthetic application")
}

/// The one upstream model template catalogs accept: the OpenAI-compatible wire protocol.
pub fn openai_compatible_template() -> UpstreamModelTemplate {
    UpstreamModelTemplate {
        upstream_model_template_id: "openai-compatible".to_owned(),
        display_name: "OpenAI-compatible".to_owned(),
        wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
    }
}

pub fn temporary_catalog_root() -> PathBuf {
    canonical_temp_dir().join(format!("dotcore-catalog-error-{}", unique_suffix()))
}

pub fn create_empty_catalog_source(root: &std::path::Path) {
    for directory in ["models", "engines", "applications"] {
        fs::create_dir_all(root.join(directory)).expect("Catalog source directory");
    }
}

/// Adds a `fast` profile to the sizing fixture model, defaulting to its second binding, for
/// fixtures that need two profiles of one configuration.
pub fn add_fast_sizing_profile(root: &std::path::Path) {
    let path = root.join("models/fixture.yaml");
    let mut model = fs::read_to_string(&path).expect("sizing model fixture");
    model.push_str(&format!(
        r#"
  - profileId: "fast"
    configurationId: "local"
    defaultModelArtifactBindingId: "second"
    engineParameterDefaults:
      values:
{SIZING_ENGINE_CACHE_DEFAULT}"#
    ));
    fs::write(&path, model).expect("sizing model fixture with a fast profile");
}

/// Writes one hardware class's model recommendations file with the given YAML body.
pub fn write_recommendations(root: &std::path::Path, file_name: &str, body: &str) {
    let directory = root.join("recommendations");
    fs::create_dir_all(&directory).expect("recommendation directory");
    fs::write(directory.join(format!("{file_name}.yaml")), body).expect("recommendation fixture");
}

/// Writes the `nvidia` and `apple` hardware classes, each matching one accelerator vendor.
pub fn write_hardware_classes(root: &std::path::Path) {
    let directory = root.join("hardware-classes");
    fs::create_dir_all(&directory).expect("hardware class directory");
    for (vendor, display_name) in [("nvidia", "NVIDIA"), ("apple", "Apple Silicon")] {
        fs::write(
            directory.join(format!("{vendor}.yaml")),
            format!(
                "hardwareClassId: \"{vendor}\"\ndisplayName: \"{display_name}\"\nselector:\n  accelerators: [\"{vendor}\"]\n"
            ),
        )
        .expect("hardware class fixture");
    }
}

/// Profile default value for the sizing engine's prompt cache, which neither has an engine
/// default nor is derived.
pub const SIZING_ENGINE_CACHE_DEFAULT: &str = r#"        - parameterId: "prompt-cache-memory-mib"
          unsignedInteger: "0"
"#;

/// Writes the master declarations the sizing fixture model needs: its llama.cpp engine release,
/// with the shared context parameters and their engine defaults, 32768 tokens of total context and
/// 4 parallel requests, beside the prompt cache, a linux/amd64 NVIDIA and a darwin/arm64 Apple
/// artifact, and the `nvidia` and `apple` hardware classes.
pub fn write_sizing_engine(root: &std::path::Path) {
    write_sizing_engine_with(
        root,
        r#"  schemaId: "fixture-shared"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      visibility: "PARAMETER_VISIBILITY_BASIC"
      unsignedInteger:
        minimum: "1024"
        maximum: "16777216"
        unit: "tokens"
        defaultValue: "32768"
    - parameterId: "context-per-request"
      displayName: "Context per request"
      visibility: "PARAMETER_VISIBILITY_BASIC"
      unsignedInteger:
        minimum: "1024"
        maximum: "16777216"
        unit: "tokens"
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      visibility: "PARAMETER_VISIBILITY_ADVANCED"
      unsignedInteger:
        minimum: "1"
        maximum: "64"
        unit: "requests"
        defaultValue: "4"
    - parameterId: "prompt-cache-memory-mib"
      displayName: "Prompt cache memory"
      visibility: "PARAMETER_VISIBILITY_HIDDEN"
      unsignedInteger:
        minimum: "0"
        maximum: "65536"
        unit: "MiB"
"#,
    );
}

/// Writes the sizing engine release with the given parameter schema body (two-space indent), and
/// the hardware classes it runs on.
fn write_sizing_engine_with(root: &std::path::Path, parameter_schema: &str) {
    write_hardware_classes(root);
    fs::write(
        root.join("engines/fixture.yaml"),
        format!(
            r#"engineReleaseId: "fixture-engine"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "fixture"
parameterSchema:
{parameter_schema}artifacts:
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
  - artifactId: "darwin-arm64"
    nix:
      flake: "self"
      package: "engines.llama-cpp.native"
      executable: "llama-server"
    selectors:
      - platform: {{ operatingSystem: "darwin", architecture: "arm64" }}
        accelerators: ["apple"]
"#
        ),
    )
    .expect("sizing engine fixture");
}

/// Writes the sizing fixture model with the configuration-level `sizing` block (four-space
/// indent, or empty for an unsized configuration), whose `nvidia` support entry sizes both
/// bindings without constants of their own.
pub fn write_sizing_model(root: &std::path::Path, sizing: &str) {
    let support_sizing = if sizing.is_empty() {
        ""
    } else {
        UNPARAMETERIZED_BINDING_SIZING
    };
    write_sizing_model_with(
        root,
        sizing,
        support_sizing,
        &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
    );
}

/// Adds the sizing engine's darwin/arm64 Apple artifact to the sizing fixture model's approved
/// artifacts, for fixtures whose configuration also supports the `apple` class.
pub fn approve_sizing_apple_artifact(root: &std::path::Path) {
    let path = root.join("models/fixture.yaml");
    let model = fs::read_to_string(&path).expect("sizing model fixture");
    fs::write(
        &path,
        model.replace(
            "        engineArtifactId: \"linux-amd64\"\n",
            "        engineArtifactId: \"linux-amd64\"\n      - engineReleaseId: \"fixture-engine\"\n        engineArtifactId: \"darwin-arm64\"\n",
        ),
    )
    .expect("sizing model fixture with the apple artifact");
}

/// Support entry sizing that lists both fixture bindings without binding constants.
pub const UNPARAMETERIZED_BINDING_SIZING: &str = r#"        bindingSizing:
          - bindingId: "sized"
          - bindingId: "second"
"#;

/// Writes the sizing fixture model: one llama.cpp configuration `local` with bindings `sized` and
/// `second` that runs on the `nvidia` class. `sizing` is the configuration-level sizing block,
/// `support_sizing` the constants of the `nvidia` support entry (eight-space indent), and
/// `profile_defaults` the `balanced` profile's engine parameter defaults.
pub fn write_sizing_model_with(
    root: &std::path::Path,
    sizing: &str,
    support_sizing: &str,
    profile_defaults: &str,
) {
    let model = format!(
        r#"modelId: "fixture-model"
displayName: "Fixture model"
version: "fixture"
maximumContextTokens: 262144
artifacts:
  - artifactId: "sized-weights"
    weightQuantization: "SIZED"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "sized.gguf"
        provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222"
  - artifactId: "second-weights"
    weightQuantization: "SECOND"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "second.gguf"
        provenanceDigest: "3333333333333333333333333333333333333333333333333333333333333333"
configurations:
  - configurationId: "local"
    engineArtifacts:
      - engineReleaseId: "fixture-engine"
        engineArtifactId: "linux-amd64"
    artifactBindings:
      - bindingId: "sized"
        modelArtifactId: "sized-weights"
      - bindingId: "second"
        modelArtifactId: "second-weights"
{sizing}    hardwareSupport:
      - hardwareClassId: "nvidia"
{support_sizing}    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp:
        fixedSettings:
          gpuLayers: 1
          flashAttention: true
    executionCapabilities:
      supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"]
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
profiles:
  - profileId: "balanced"
    configurationId: "local"
    defaultModelArtifactBindingId: "sized"
    engineParameterDefaults:
{profile_defaults}"#
    );

    fs::write(root.join("models/fixture.yaml"), model).expect("sizing model fixture");
}

/// Option value sets the linearity guard evaluates for one profile, each with a label naming its
/// choice: the profile's resolved defaults first, unlabeled, then each distinct resolution of every
/// combination of declared boolean and string-enum values. Combinations the configuration's
/// compatibility rule rejects can't be deployed and are left out; any other resolution failure is
/// kept as an error.
fn option_variants(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    profile: &ModelProfile,
) -> Vec<(String, anyhow::Result<Vec<ModelOptionValues>>)> {
    let defaults =
        options::resolve_model_option_values(&model.options, configuration, profile, &[]);
    let mut resolved = defaults.iter().cloned().collect::<Vec<_>>();
    let mut variants = vec![(String::new(), defaults)];
    for combination in choice_combinations(configuration) {
        let mut overrides = BTreeMap::<&str, Vec<ModelOptionParameterValue>>::new();
        for (option_id, value) in &combination {
            overrides.entry(option_id).or_default().push(value.clone());
        }
        let overrides = overrides
            .into_iter()
            .map(|(option_id, values)| ModelOptionValues {
                option_id: option_id.to_owned(),
                values,
            })
            .collect::<Vec<_>>();
        let values = options::overlay_model_option_values(
            &model.options,
            configuration,
            profile,
            &overrides,
        )
        .and_then(|values| {
            let compatible =
                crate::sizing::compatible(configuration, &values).map_err(anyhow::Error::new)?;
            Ok(compatible.then_some(values))
        });
        let values = match values {
            Ok(Some(values)) => values,
            Ok(None) => continue,
            Err(error) => {
                variants.push((format!(" with {}", choice_label(&combination)), Err(error)));
                continue;
            }
        };
        if resolved.contains(&values) {
            continue;
        }
        resolved.push(values.clone());
        variants.push((format!(" with {}", choice_label(&combination)), Ok(values)));
    }
    variants
}

/// Names one combination of option choices, such as `kv-cache.precision = "q8_0"`.
fn choice_label(combination: &[(String, ModelOptionParameterValue)]) -> String {
    combination
        .iter()
        .map(|(option_id, value)| {
            let shown = match value.value.as_ref() {
                Some(model_option_parameter_value::Value::Boolean(value)) => value.to_string(),
                Some(model_option_parameter_value::Value::String(value)) => format!("{value:?}"),
                _ => String::new(),
            };
            format!("{option_id}.{} = {shown}", value.parameter_id)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every combination of one value for each declared boolean and string-enum option parameter of
/// the configuration, keyed by option id; unsigned parameters keep their resolved value.
fn choice_combinations(
    configuration: &ModelEngineConfiguration,
) -> Vec<Vec<(String, ModelOptionParameterValue)>> {
    let mut combinations = vec![Vec::new()];
    for option in &configuration.options {
        for parameter in &option.parameters {
            let choices = match parameter.parameter_type.as_ref() {
                Some(model_option_parameter_definition::ParameterType::Boolean(_)) => vec![
                    model_option_parameter_value::Value::Boolean(false),
                    model_option_parameter_value::Value::Boolean(true),
                ],
                Some(model_option_parameter_definition::ParameterType::StringEnum(schema)) => {
                    schema
                        .values
                        .iter()
                        .cloned()
                        .map(model_option_parameter_value::Value::String)
                        .collect()
                }
                _ => continue,
            };
            combinations = combinations
                .into_iter()
                .flat_map(|combination| {
                    choices.iter().map(move |choice| {
                        let mut next = combination.clone();
                        next.push((
                            option.option_id.clone(),
                            ModelOptionParameterValue {
                                parameter_id: parameter.parameter_id.clone(),
                                value: Some(choice.clone()),
                            },
                        ));
                        next
                    })
                })
                .collect();
        }
    }
    combinations
}

fn unsigned_value(parameter_id: &str, value: u64) -> ParameterValue {
    ParameterValue {
        parameter_id: parameter_id.to_owned(),
        value: Some(parameter_value::Value::UnsignedInteger(value)),
    }
}
