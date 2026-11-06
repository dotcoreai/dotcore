//! Engine release and typed parameter schema validation.
//!
//! Each engine artifact is an OCI image or a Nix flake package, providing a native executable or a
//! Docker image, with selectors naming the targets it runs on. Each selector names one concrete
//! platform and the accelerator vendors the artifact uses there, for example linux/arm64 with nvidia
//! and a minimum driver major of 580, or darwin/arm64 with apple for a native Nix package.

use std::collections::BTreeSet;

use dotcore_domain::{Accelerator, valid_id};
use dotcore_proto::dotcore::catalog::v1::{
    EngineArtifact, EngineParameterDefinition, EngineParameterSchema, EngineRelease, Selector,
    engine_artifact::Kind as EngineArtifactKind, engine_parameter_definition::ParameterType,
};

use super::common::{valid_selector_memory_architecture, validate_oci_reference};
use super::nix::{AcceptedProvides, nix_systems, validate_nix_artifact};
use super::storage::{StorageOwner, validate_storage_bindings};
use crate::{OciArtifactPolicy, shared_parameters::validate_shared_parameters};

pub fn validate_engine_release_with_policy(
    engine: &EngineRelease,
    oci_artifact_policy: OciArtifactPolicy,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if !valid_id(&engine.engine_release_id) {
        errors.push(anyhow::anyhow!(
            "engine release id {:?} must match [a-z][a-z0-9-]{{0,62}}",
            engine.engine_release_id
        ));
    }
    if engine.version.is_empty() {
        errors.push(anyhow::anyhow!("engine release version must not be empty"));
    }
    if engine.artifacts.is_empty() {
        errors.push(anyhow::anyhow!(
            "engine release must contain at least one artifact"
        ));
    }
    let mut ids = BTreeSet::new();
    for artifact in &engine.artifacts {
        validate_engine_artifact(artifact, oci_artifact_policy, &mut ids, &mut errors);
    }
    validate_parameter_schema(engine.parameter_schema.as_ref(), &mut errors);
    validate_engine_storage(engine, &mut errors);
    errors
}

/// Validates an engine release's storage bindings. The runtime attaches storage only to container
/// workloads, so a release with bindings holds only OCI images or Docker-delivered Nix packages.
fn validate_engine_storage(engine: &EngineRelease, errors: &mut Vec<anyhow::Error>) {
    if engine.storage_bindings.is_empty() {
        return;
    }
    validate_storage_bindings(
        &engine.storage_bindings,
        StorageOwner::EngineRelease,
        &format!("engine release {:?}", engine.engine_release_id),
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
        errors,
    );
    for artifact in &engine.artifacts {
        let container = match artifact.kind.as_ref() {
            Some(EngineArtifactKind::Oci(_)) => true,
            Some(EngineArtifactKind::Nix(nix)) => dotcore_domain::nix::provides_docker_image(nix),
            None => true,
        };
        if !container {
            errors.push(anyhow::anyhow!(
                "engine release {:?} declares storage bindings, so engine artifact {:?} must be a container image",
                engine.engine_release_id,
                artifact.artifact_id
            ));
        }
    }
}

fn validate_engine_artifact(
    artifact: &EngineArtifact,
    policy: OciArtifactPolicy,
    ids: &mut BTreeSet<String>,
    errors: &mut Vec<anyhow::Error>,
) {
    if !ids.insert(artifact.artifact_id.clone()) || !valid_id(&artifact.artifact_id) {
        errors.push(anyhow::anyhow!(
            "engine artifact id {:?} must be unique and valid",
            artifact.artifact_id
        ));
    }
    let Some(kind) = artifact.kind.as_ref() else {
        errors.push(anyhow::anyhow!(
            "engine artifact {:?} requires an OCI or Nix artifact",
            artifact.artifact_id
        ));
        return;
    };
    if artifact.selectors.is_empty() {
        errors.push(anyhow::anyhow!(
            "engine artifact {:?} requires at least one selector",
            artifact.artifact_id
        ));
    }
    let mut platforms = BTreeSet::new();
    for selector in &artifact.selectors {
        if let Err(error) = validate_selector(selector, kind, &mut platforms) {
            errors.push(anyhow::anyhow!(
                "engine artifact {:?} has an invalid selector: {error}",
                artifact.artifact_id
            ));
        }
    }
    match kind {
        EngineArtifactKind::Oci(oci) => {
            if let Err(error) = validate_oci_reference(oci, policy) {
                errors.push(anyhow::anyhow!(
                    "engine artifact {:?} has an invalid OCI artifact: {error}",
                    artifact.artifact_id
                ));
            }
        }
        EngineArtifactKind::Nix(nix) => {
            let systems = nix_systems(nix, platforms.iter().copied());
            if let Err(error) = validate_nix_artifact(nix, &systems, AcceptedProvides::Any) {
                errors.push(anyhow::anyhow!(
                    "engine artifact {:?} has an invalid Nix artifact: {error}",
                    artifact.artifact_id
                ));
            }
        }
    }
}

/// Validates one engine artifact selector: a unique concrete platform the artifact kind can
/// target, known accelerator vendors that each run there, a driver minimum exactly when nvidia
/// is listed, and no device models, which only hardware classes match on. OCI images and Docker-delivered Nix packages target Linux only; native Nix packages
/// target Linux or darwin/arm64.
fn validate_selector<'a>(
    selector: &'a Selector,
    kind: &EngineArtifactKind,
    platforms: &mut BTreeSet<(&'a str, &'a str)>,
) -> Result<(), &'static str> {
    let platform = selector
        .platform
        .as_ref()
        .ok_or("selector requires a platform")?;
    let concrete = (
        platform.operating_system.as_str(),
        platform.architecture.as_str(),
    );
    let linux = matches!(concrete, ("linux", "amd64" | "arm64"));
    let darwin = concrete == ("darwin", "arm64");
    match kind {
        EngineArtifactKind::Oci(_) if !linux || platform.variant.is_some() => {
            return Err(
                "OCI artifacts support only linux/amd64 or linux/arm64 platforms without variants",
            );
        }
        EngineArtifactKind::Nix(nix)
            if dotcore_domain::nix::provides_docker_image(nix)
                && (!linux || platform.variant.is_some()) =>
        {
            return Err(
                "Docker-delivered Nix artifacts support only linux/amd64 or linux/arm64 platforms without variants",
            );
        }
        EngineArtifactKind::Nix(_) if !(linux || darwin) || platform.variant.is_some() => {
            return Err(
                "Nix artifacts support only linux/amd64, linux/arm64, or darwin/arm64 platforms without variants",
            );
        }
        EngineArtifactKind::Oci(_) | EngineArtifactKind::Nix(_) => {}
    }
    if !platforms.insert(concrete) {
        return Err("each platform may appear in only one selector");
    }
    if !selector.device_models.is_empty() {
        return Err("device models belong to hardware classes, not engine artifact selectors");
    }
    if selector.accelerators.is_empty() {
        return Err("selector requires at least one accelerator");
    }
    let mut names = BTreeSet::new();
    for name in &selector.accelerators {
        if !names.insert(name.as_str()) {
            return Err("each accelerator may appear only once in a selector");
        }
        match Accelerator::from_catalog_name(name) {
            Some(Accelerator::Nvidia) if !linux => return Err("nvidia requires a Linux platform"),
            Some(Accelerator::Apple) if !darwin => {
                return Err("apple requires the darwin/arm64 platform");
            }
            Some(_) => {}
            None => return Err("accelerators must be nvidia or apple"),
        }
    }
    if !valid_selector_memory_architecture(selector) {
        return Err("memory architecture must be unified, dedicated or unspecified");
    }
    let nvidia = names.contains(Accelerator::Nvidia.catalog_name());
    match (nvidia, selector.minimum_nvidia_driver_major_version) {
        (true, 0) => Err("nvidia requires a minimum NVIDIA driver major version"),
        (false, minimum) if minimum != 0 => {
            Err("selectors without nvidia take no minimum NVIDIA driver major version")
        }
        _ => Ok(()),
    }
}

fn validate_parameter_schema(
    schema: Option<&EngineParameterSchema>,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(schema) = schema else {
        errors.push(anyhow::anyhow!(
            "engine release requires a typed parameter schema"
        ));
        return;
    };
    if !valid_id(&schema.schema_id) || schema.parameters.is_empty() {
        errors.push(anyhow::anyhow!(
            "engine release requires a valid non-empty typed parameter schema"
        ));
    }
    let mut ids = BTreeSet::new();
    for definition in &schema.parameters {
        validate_parameter_definition(definition, &mut ids, errors);
    }
    errors.extend(validate_shared_parameters(schema));
}

fn validate_parameter_definition(
    definition: &EngineParameterDefinition,
    ids: &mut BTreeSet<String>,
    errors: &mut Vec<anyhow::Error>,
) {
    if !ids.insert(definition.parameter_id.clone())
        || !valid_id(&definition.parameter_id)
        || definition.display_name.trim().is_empty()
    {
        errors.push(anyhow::anyhow!(
            "engine parameter requires a unique valid ID and display name"
        ));
    }
    if definition
        .parameter_type
        .as_ref()
        .is_none_or(|kind| match kind {
            ParameterType::UnsignedInteger(schema) => {
                schema.minimum > schema.maximum
                    || schema.unit.trim().is_empty()
                    || schema.default_value.is_some_and(|default| {
                        !(schema.minimum..=schema.maximum).contains(&default)
                    })
            }
            ParameterType::Boolean(_) => false,
        })
    {
        errors.push(anyhow::anyhow!(
            "engine parameter {:?} requires a valid typed range holding its default",
            definition.parameter_id
        ));
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{
            NixArtifact, NixDockerImage, StorageBinding, StorageKind, StorageScope,
            nix_artifact::Provides,
        },
        observation::v1::AcceleratorMemoryArchitecture,
    };

    use super::*;

    #[test]
    fn selectors_reject_declared_variants_for_oci_and_nix_artifacts() {
        let selector = |variant: Option<&str>| Selector {
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                variant: variant.map(str::to_owned),
            }),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            ..Default::default()
        };
        let oci = EngineArtifactKind::Oci(Default::default());
        let nix = EngineArtifactKind::Nix(nix("self"));

        for kind in [&oci, &nix] {
            assert_eq!(
                validate_selector(&selector(None), kind, &mut BTreeSet::new()),
                Ok(())
            );
        }
        assert_eq!(
            validate_selector(&selector(Some("v8")), &oci, &mut BTreeSet::new()),
            Err("OCI artifacts support only linux/amd64 or linux/arm64 platforms without variants")
        );
        assert_eq!(
            validate_selector(&selector(Some("v8")), &nix, &mut BTreeSet::new()),
            Err(
                "Nix artifacts support only linux/amd64, linux/arm64, or darwin/arm64 platforms without variants"
            )
        );
    }

    #[test]
    fn docker_image_selectors_support_only_linux_platforms() {
        let selector = |operating_system: &str, accelerator: &str| Selector {
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: operating_system.to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            }),
            accelerators: vec![accelerator.to_owned()],
            minimum_nvidia_driver_major_version: if accelerator == "nvidia" { 580 } else { 0 },
            ..Default::default()
        };
        let mut image = nix("self");
        image.provides = Some(Provides::DockerImage(NixDockerImage {}));
        let image = EngineArtifactKind::Nix(image);

        assert_eq!(
            validate_selector(&selector("linux", "nvidia"), &image, &mut BTreeSet::new()),
            Ok(())
        );
        assert_eq!(
            validate_selector(&selector("darwin", "apple"), &image, &mut BTreeSet::new()),
            Err(
                "Docker-delivered Nix artifacts support only linux/amd64 or linux/arm64 platforms without variants"
            )
        );
        assert_eq!(
            validate_selector(
                &selector("darwin", "apple"),
                &EngineArtifactKind::Nix(nix("self")),
                &mut BTreeSet::new()
            ),
            Ok(())
        );
    }

    #[test]
    fn selectors_reject_an_unknown_memory_architecture() {
        let selector = |memory_architecture: i32| Selector {
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            }),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            memory_architecture,
            device_models: Vec::new(),
        };
        let oci = EngineArtifactKind::Oci(Default::default());

        for known in [
            AcceleratorMemoryArchitecture::Unspecified,
            AcceleratorMemoryArchitecture::Unified,
            AcceleratorMemoryArchitecture::Dedicated,
        ] {
            assert_eq!(
                validate_selector(&selector(known as i32), &oci, &mut BTreeSet::new()),
                Ok(()),
                "{known:?}"
            );
        }
        assert_eq!(
            validate_selector(&selector(99), &oci, &mut BTreeSet::new()),
            Err("memory architecture must be unified, dedicated or unspecified")
        );
    }

    #[test]
    fn selectors_reject_device_models() {
        let selector = Selector {
            platform: Some(dotcore_proto::dotcore::catalog::v1::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            device_models: vec!["GPU A".to_owned()],
            ..Default::default()
        };

        assert_eq!(
            validate_selector(
                &selector,
                &EngineArtifactKind::Oci(Default::default()),
                &mut BTreeSet::new()
            ),
            Err("device models belong to hardware classes, not engine artifact selectors")
        );
    }

    #[test]
    fn parameter_schemas_reject_an_out_of_range_default_and_a_missing_shared_parameter() {
        let unsigned = |parameter_id: &str, display_name: &str, unit: &str, default_value| {
            EngineParameterDefinition {
                parameter_id: parameter_id.to_owned(),
                display_name: display_name.to_owned(),
                parameter_type: Some(ParameterType::UnsignedInteger(
                    dotcore_proto::dotcore::catalog::v1::UnsignedIntegerEngineParameterSchema {
                        minimum: 1024,
                        maximum: 65_536,
                        unit: unit.to_owned(),
                        default_value,
                    },
                )),
                exposed: true,
                description: String::new(),
            }
        };
        let schema = EngineParameterSchema {
            schema_id: "fixture-shared".to_owned(),
            parameters: vec![
                unsigned("total-context", "Total context", "tokens", Some(131_072)),
                unsigned("context-per-request", "Context per request", "tokens", None),
            ],
        };
        let mut errors = Vec::new();

        validate_parameter_schema(Some(&schema), &mut errors);

        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "engine parameter \"total-context\" requires a valid typed range holding its default",
                "engine parameter schema \"fixture-shared\" must declare the shared parameter \"parallel-requests\"",
            ]
        );
    }

    #[test]
    fn engine_storage_requires_engine_scopes_cache_kind_and_container_artifacts() {
        let mut image = nix("self");
        image.provides = Some(Provides::DockerImage(NixDockerImage {}));
        let artifact = |artifact_id: &str, kind| EngineArtifact {
            artifact_id: artifact_id.to_owned(),
            kind: Some(kind),
            ..Default::default()
        };
        let binding = |binding_name: &str, kind: StorageKind, scope: StorageScope| StorageBinding {
            binding_name: binding_name.to_owned(),
            mount_path: format!("/cache/{binding_name}"),
            kind: kind as i32,
            scope: scope as i32,
            environment: Vec::new(),
        };
        let mut engine = EngineRelease {
            engine_release_id: "fixture".to_owned(),
            artifacts: vec![
                artifact("oci", EngineArtifactKind::Oci(Default::default())),
                artifact("image", EngineArtifactKind::Nix(image)),
            ],
            storage_bindings: vec![
                binding("compile", StorageKind::Cache, StorageScope::EngineArtifact),
                binding("scratch", StorageKind::Cache, StorageScope::Deployment),
            ],
            ..Default::default()
        };
        let errors = |engine: &EngineRelease| {
            let mut errors = Vec::new();
            validate_engine_storage(engine, &mut errors);
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        };

        assert_eq!(errors(&engine), Vec::<String>::new());

        engine
            .artifacts
            .push(artifact("native", EngineArtifactKind::Nix(nix("self"))));
        engine.storage_bindings.extend([
            binding("data", StorageKind::Data, StorageScope::Deployment),
            binding("shared", StorageKind::Cache, StorageScope::Application),
        ]);

        assert_eq!(
            errors(&engine),
            [
                "engine release \"fixture\" storage binding \"data\" cannot use kind STORAGE_KIND_DATA",
                "engine release \"fixture\" storage binding \"shared\" cannot use scope STORAGE_SCOPE_APPLICATION",
                "engine release \"fixture\" declares storage bindings, so engine artifact \"native\" must be a container image",
            ]
        );
    }

    fn nix(flake: &str) -> NixArtifact {
        NixArtifact {
            flake: flake.to_owned(),
            package: Some("engines.llama-cpp.native".to_owned()),
            provides: Some(Provides::Executable("llama-server".to_owned())),
            ..Default::default()
        }
    }
}
