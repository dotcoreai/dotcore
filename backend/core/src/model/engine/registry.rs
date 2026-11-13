//! Selects inference-engine integrations from immutable retained engine identity.
//!
//! Integrations translate catalog snapshots, materialize portable plans, and
//! interpret readiness evidence. Runtime implementation selection remains a
//! separate concern.

use std::{collections::BTreeMap, sync::Arc};

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::StoragePlacement,
    engine::v1::EngineKind,
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{
    ExecutionTarget, LoopbackEndpoint, WorkloadServiceAccess, WorkloadServiceBinding, WorkloadSpec,
    WorkloadValue,
};
use sha2::{Digest, Sha256};

use super::{
    definition::{ENGINES, EngineDefinition},
    startup_facts::StartupFactExtractor,
    startup_refusal::StartupRefusal,
};
use crate::{
    inference::{
        EngineBinding, EngineBindingError, EngineBindingRequest, EngineBindingResolver,
        EngineServiceAccess, EngineServingIntegration, EngineTransportFactory,
        NativeInferenceProvider,
    },
    model::resolved::ResolvedModelLaunch,
    storage::{StorageBindingError, engine_placement, workload_storage},
};

/// Engine-specific transport factories keyed without cross-engine fallback.
#[derive(Clone, Default)]
pub(crate) struct EngineTransportFactories {
    factories: BTreeMap<i32, Arc<dyn EngineTransportFactory>>,
}

impl EngineTransportFactories {
    /// Returns the loopback transport factory of every engine that runs as a local workload.
    pub(crate) fn local_engines() -> Self {
        ENGINES.iter().fold(Self::default(), |factories, engine| {
            factories.with(engine.kind(), engine.transport_factory())
        })
    }

    pub(crate) fn with(
        mut self,
        kind: EngineKind,
        factory: Arc<dyn EngineTransportFactory>,
    ) -> Self {
        self.factories.insert(kind as i32, factory);
        self
    }

    fn get(&self, kind: EngineKind) -> Option<Arc<dyn EngineTransportFactory>> {
        self.factories.get(&(kind as i32)).cloned()
    }
}

/// Registry of supported engine integrations keyed independently of runtime.
#[derive(Clone)]
pub(crate) struct EngineIntegrationRegistry {
    integrations: BTreeMap<i32, EngineIntegration>,
    resolver: EngineBindingResolver,
    native_provider: Option<Arc<dyn NativeInferenceProvider>>,
}

impl EngineIntegrationRegistry {
    pub(crate) fn production() -> Self {
        Self::production_with_transports(
            EngineBindingResolver::default(),
            EngineTransportFactories::default(),
            None,
        )
    }

    pub(crate) fn with_native_provider_and_transports(
        provider: Arc<dyn NativeInferenceProvider>,
        transports: EngineTransportFactories,
    ) -> Self {
        Self::production_with_transports(
            EngineBindingResolver::default(),
            transports,
            Some(provider),
        )
    }

    fn production_with_transports(
        resolver: EngineBindingResolver,
        transports: EngineTransportFactories,
        native_provider: Option<Arc<dyn NativeInferenceProvider>>,
    ) -> Self {
        let integrations = ENGINES
            .iter()
            .map(|&definition| {
                let integration = EngineIntegration {
                    definition,
                    transport: transports.get(definition.kind()),
                };
                (definition.kind() as i32, integration)
            })
            .collect();
        Self {
            integrations,
            resolver,
            native_provider,
        }
    }

    pub(crate) fn integration(
        &self,
        kind: EngineKind,
    ) -> Result<&EngineIntegration, EngineIntegrationError> {
        self.integrations
            .get(&(kind as i32))
            .ok_or(EngineIntegrationError::UnsupportedKind(kind as i32))
    }

    pub(crate) fn translate(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> Result<PortableModelLaunchPlan, EngineIntegrationError> {
        self.snapshot_integration(snapshot)?
            .definition
            .translate(snapshot)
            .map_err(EngineIntegrationError::Operation)
    }

    /// Returns the startup fact parser of the snapshot's engine, when it has one.
    pub(crate) fn startup_fact_extractor(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> Option<&dyn StartupFactExtractor> {
        self.snapshot_integration(snapshot)
            .ok()?
            .startup_fact_extractor()
    }

    /// Returns the reason the snapshot's engine gave for stopping during startup, when its output
    /// holds one the engine recognizes.
    pub(crate) fn startup_refusal(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
        output: &str,
    ) -> Option<StartupRefusal> {
        self.snapshot_integration(snapshot)
            .ok()?
            .startup_refusal(output)
    }

    pub(crate) fn enables_host_kv_offload(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> Result<bool, EngineIntegrationError> {
        self.snapshot_integration(snapshot)?
            .definition
            .enables_host_kv_offload(snapshot)
            .map_err(EngineIntegrationError::Operation)
    }

    fn snapshot_integration(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> Result<&EngineIntegration, EngineIntegrationError> {
        let kind = EngineKind::try_from(
            snapshot
                .engine_artifact
                .as_ref()
                .ok_or(EngineIntegrationError::MissingEngineIdentity)?
                .engine_kind,
        )
        .map_err(|_| EngineIntegrationError::InvalidEngineKind)?;
        self.integration(kind)
    }

    /// Builds the workload of one model launch: the engine's own spec plus the storage its
    /// engine release declares, keyed by `deployment_key` for deployment-placed storage.
    pub(crate) fn materialize_workload(
        &self,
        model: &ResolvedModelLaunch,
        target: &ExecutionTarget,
        artifacts: &PreparedModelArtifacts,
        deployment_key: &str,
    ) -> Result<WorkloadSpec, EngineIntegrationError> {
        let kind = EngineKind::try_from(model.engine.kind)
            .map_err(|_| EngineIntegrationError::InvalidEngineKind)?;
        let definition = self.integration(kind)?.definition;
        let mut spec = definition
            .workload(model, target, artifacts)
            .map_err(EngineIntegrationError::Operation)?;
        let artifact_kind = spec.executable.artifact.kind();
        if !definition.artifact_kinds().contains(&artifact_kind) {
            return Err(EngineIntegrationError::UnsupportedArtifactKind(
                artifact_kind,
            ));
        }

        let bindings = model
            .engine_storage_bindings()
            .iter()
            .map(|binding| {
                engine_placement(binding)
                    .map(|placement| (binding.binding_name.clone(), binding, placement))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let storage = workload_storage(
            bindings,
            |placement| match placement {
                StoragePlacement::Deployment => Some(deployment_key.to_owned()),
                StoragePlacement::EngineArtifact => {
                    Some(engine_artifact_storage_key(&model.engine.launch_identity))
                }
                StoragePlacement::Application | StoragePlacement::Unspecified => None,
            },
            |name| spec.environment.contains_key(name),
        )?;
        spec.environment.extend(
            storage
                .environment
                .into_iter()
                .map(|(name, value)| (name, WorkloadValue::literal(value))),
        );
        spec.storage.extend(storage.attachments);
        Ok(spec)
    }

    pub(crate) fn bind(
        &self,
        kind: EngineKind,
        request: EngineBindingRequest,
    ) -> Result<EngineBinding, EngineBindingError> {
        let integration = self
            .integration(kind)
            .map_err(|error| EngineBindingError::Invalid(error.to_string()))?;
        let EngineBindingRequest {
            identity,
            served_model_id,
            resolved_engine_state,
            access,
        } = request;
        self.resolver.bind(
            integration,
            identity,
            served_model_id,
            access,
            resolved_engine_state,
        )
    }

    pub(crate) fn bind_workload(
        &self,
        model: &ResolvedModelLaunch,
        deployment_id: &str,
        service: &WorkloadServiceBinding,
    ) -> Result<EngineBinding, EngineBindingError> {
        let kind = EngineKind::try_from(model.engine.kind)
            .map_err(|_| EngineBindingError::Invalid("invalid engine kind".to_owned()))?;
        let resolved_engine_state =
            model
                .snapshot
                .resolved_engine_state
                .clone()
                .ok_or_else(|| {
                    EngineBindingError::Invalid("missing resolved engine state".to_owned())
                })?;
        let identity = EngineBindingResolver::issue_identity(
            deployment_id,
            service.reference.realization_id.clone(),
            service.reference.service_name.clone(),
            service.reference.revision,
        )
        .map_err(|error| EngineBindingError::Invalid(error.to_string()))?;
        let access = match &service.access {
            WorkloadServiceAccess::Loopback(endpoint) => EngineServiceAccess::Loopback {
                host: std::net::Ipv4Addr::LOCALHOST,
                port: endpoint.host_port,
            },
            WorkloadServiceAccess::Native => {
                EngineServiceAccess::Native(self.native_provider.clone().ok_or_else(|| {
                    EngineBindingError::Unavailable(
                        "native inference provider is not configured".to_owned(),
                    )
                })?)
            }
        };
        self.bind(
            kind,
            EngineBindingRequest {
                identity,
                // Launch plans serve the catalog model id (vLLM `--served-model-name`, llama.cpp
                // `--alias`); readiness verifies it and requests are sent under it.
                served_model_id: model.model_id.0.clone(),
                resolved_engine_state,
                access,
            },
        )
    }

    pub(crate) fn retire_inactive_binding(&self, binding: &EngineBinding) -> bool {
        self.resolver.retire_inactive(binding.identity())
    }

    pub(crate) fn retire_inactive_deployment(&self, deployment_id: &str) -> usize {
        self.resolver.retire_inactive_deployment(deployment_id)
    }
}

/// Engine integration selection or execution failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineIntegrationError {
    #[error("local model snapshot has no retained engine identity")]
    MissingEngineIdentity,

    #[error("local model snapshot has an invalid engine kind")]
    InvalidEngineKind,

    #[error("Engine kind {0} has no integration")]
    UnsupportedKind(i32),

    #[error("Engine cannot launch from a {0:?} executable artifact")]
    UnsupportedArtifactKind(WorkloadArtifactKind),

    #[error("Engine integration failed: {0}")]
    Operation(#[source] anyhow::Error),

    #[error("engine storage cannot be attached")]
    Storage(#[from] StorageBindingError),
}

/// One engine's definition bound to the transport this process uses to reach it.
#[derive(Clone)]
pub(crate) struct EngineIntegration {
    definition: &'static dyn EngineDefinition,
    transport: Option<Arc<dyn EngineTransportFactory>>,
}

impl EngineIntegration {
    /// Returns the engine's startup fact parser, when it has one.
    pub(crate) fn startup_fact_extractor(&self) -> Option<&'static dyn StartupFactExtractor> {
        self.definition.startup_fact_extractor()
    }

    /// Returns the engine's reason for stopping during startup, when its output holds one.
    pub(crate) fn startup_refusal(&self, output: &str) -> Option<StartupRefusal> {
        self.definition.startup_refusal(output)
    }
}

/// Returns the storage key shared by every launch of one engine artifact: a short digest of its
/// launch identity, which names exactly one engine build and is not itself a valid folder name.
fn engine_artifact_storage_key(launch_identity: &str) -> String {
    let digest = Sha256::digest(launch_identity.as_bytes());
    let hex = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256-{hex}")
}

impl EngineServingIntegration for EngineIntegration {
    fn bind(&self, request: EngineBindingRequest) -> Result<EngineBinding, EngineBindingError> {
        let EngineBindingRequest {
            identity,
            served_model_id,
            resolved_engine_state,
            access,
        } = request;
        let (host, port) = match access {
            EngineServiceAccess::Loopback { host, port } => (host, port),
            EngineServiceAccess::Native(provider) => {
                return provider.bind(identity, served_model_id);
            }
        };
        let name = self.definition.name();
        if host != std::net::Ipv4Addr::LOCALHOST {
            return Err(EngineBindingError::Invalid(format!(
                "{name} transport must use the loopback host"
            )));
        }
        self.definition
            .validate_binding_state(&resolved_engine_state)?;
        let transport = self.transport.as_ref().ok_or_else(|| {
            EngineBindingError::Unavailable(format!("{name} transport factory is not registered"))
        })?;
        let invoker = transport.bind_loopback(port, served_model_id.clone());
        let probe = self.definition.readiness_probe(
            LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: port,
            },
            served_model_id,
            &resolved_engine_state,
        );
        Ok(EngineBinding::new(identity, probe, invoker))
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::StorageBinding;
    use dotcore_runtime::{WorkloadStorageAttachment, WorkloadStorageKind};

    use super::*;

    #[test]
    fn transport_factories_are_isolated_and_missing_kind_does_not_fallback() {
        let factories = EngineTransportFactories::default()
            .with(EngineKind::LlamaCpp, ENGINES[0].transport_factory());
        assert!(factories.get(EngineKind::LlamaCpp).is_some());
        assert!(factories.get(EngineKind::Vllm).is_none());
        assert!(factories.get(EngineKind::Unspecified).is_none());
    }

    #[test]
    fn local_engine_transports_cover_every_local_engine_kind() {
        let factories = EngineTransportFactories::local_engines();

        for kind in [
            EngineKind::LlamaCpp,
            EngineKind::Vllm,
            EngineKind::Ninfer,
            EngineKind::Strata,
        ] {
            assert!(factories.get(kind).is_some(), "{kind:?}");
        }
        assert!(factories.get(EngineKind::Unspecified).is_none());
    }

    #[test]
    fn production_registry_selects_each_retained_engine_kind() {
        let registry = EngineIntegrationRegistry::production();

        for kind in [
            EngineKind::LlamaCpp,
            EngineKind::Vllm,
            EngineKind::Ninfer,
            EngineKind::Strata,
        ] {
            let integration = registry.integration(kind).expect("integration");
            assert_eq!(integration.definition.kind(), kind);
        }
        assert!(matches!(
            registry.integration(EngineKind::Unspecified),
            Err(EngineIntegrationError::UnsupportedKind(_))
        ));
    }

    #[test]
    fn only_vllm_extracts_startup_facts() {
        let registry = EngineIntegrationRegistry::production();
        let has_extractor = |kind| {
            registry
                .integration(kind)
                .expect("integration")
                .startup_fact_extractor()
                .is_some()
        };

        assert!(has_extractor(EngineKind::Vllm));
        assert!(!has_extractor(EngineKind::LlamaCpp));
        assert!(!has_extractor(EngineKind::Ninfer));
        assert!(!has_extractor(EngineKind::Strata));
    }

    #[test]
    fn workload_rejects_an_unknown_engine_before_materialization() {
        let mut resolved = resolved_launch(Vec::new());
        resolved.engine.kind = 0;

        let error = EngineIntegrationRegistry::production()
            .materialize_workload(&resolved, &target(), &model_artifacts(), "deployment-one")
            .expect_err("unsupported engine");

        assert!(
            matches!(error, EngineIntegrationError::UnsupportedKind(0)),
            "{error:?}"
        );
    }

    #[test]
    fn workload_image_is_the_retained_launch_identity_not_the_catalog_reference() {
        const IMAGE_ID: &str =
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let mut resolved = resolved_launch(Vec::new());
        resolved.engine.source = crate::model::resolved::ResolvedEngineSource::OciImage {
            reference: "registry.dotcore.test/fixture:dev".to_owned(),
        };
        resolved.engine.launch_identity = IMAGE_ID.to_owned();

        let workload = EngineIntegrationRegistry::production()
            .materialize_workload(&resolved, &target(), &model_artifacts(), "deployment-one")
            .expect("materialized workload");

        assert_eq!(workload.executable.artifact.reference(), IMAGE_ID);
        assert!(workload.storage.is_empty());
    }

    #[test]
    fn workload_attaches_the_engine_release_storage_keyed_by_placement() {
        let resolved = resolved_launch(vec![
            cache_binding(
                "compile-cache",
                "/var/cache/compile",
                StoragePlacement::Deployment,
                "COMPILE_CACHE",
            ),
            cache_binding(
                "kernel-cache",
                "/var/cache/kernels",
                StoragePlacement::EngineArtifact,
                "KERNEL_CACHE",
            ),
        ]);
        let engine_key = engine_artifact_storage_key(&resolved.engine.launch_identity);

        let workload = EngineIntegrationRegistry::production()
            .materialize_workload(&resolved, &target(), &model_artifacts(), "deployment-one")
            .expect("materialized workload");

        assert_eq!(
            workload.storage,
            [
                WorkloadStorageAttachment {
                    name: "compile-cache".to_owned(),
                    destination: "/var/cache/compile".to_owned(),
                    kind: WorkloadStorageKind::Cache,
                    namespace: "deployment".to_owned(),
                    key: "deployment-one".to_owned(),
                    read_only: false,
                    expires_when_unused: false,
                },
                WorkloadStorageAttachment {
                    name: "kernel-cache".to_owned(),
                    destination: "/var/cache/kernels".to_owned(),
                    kind: WorkloadStorageKind::Cache,
                    namespace: "engine-artifact".to_owned(),
                    key: engine_key,
                    read_only: false,
                    expires_when_unused: true,
                },
            ]
        );
        assert_eq!(
            workload.environment.get("COMPILE_CACHE"),
            Some(&WorkloadValue::literal("/var/cache/compile"))
        );
        assert_eq!(
            workload.environment.get("KERNEL_CACHE"),
            Some(&WorkloadValue::literal("/var/cache/kernels"))
        );
    }

    #[test]
    fn engine_artifact_storage_key_is_stable_per_launch_identity() {
        let digest = format!("sha256:{}", "d".repeat(64));
        let store_path = "/nix/store/0000000000000000000000000000000a-vllm-image";

        let key = engine_artifact_storage_key(&digest);

        assert_eq!(key, engine_artifact_storage_key(&digest));
        assert_ne!(key, engine_artifact_storage_key(store_path));
        assert!(dotcore_domain::valid_id(&key), "{key}");
        assert_eq!(key.len(), "sha256-".len() + 32);
    }

    fn resolved_launch(storage_bindings: Vec<StorageBinding>) -> ResolvedModelLaunch {
        let mut snapshot = crate::test_support::local_model_snapshot("fixture");
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            local,
        )) = snapshot.source.as_mut()
        else {
            panic!("local fixture snapshot");
        };
        local
            .engine_artifact
            .as_mut()
            .expect("engine artifact")
            .storage_bindings = storage_bindings;
        ResolvedModelLaunch::from_snapshot(&snapshot).expect("resolved launch")
    }

    fn cache_binding(
        name: &str,
        mount_path: &str,
        placement: StoragePlacement,
        variable: &str,
    ) -> StorageBinding {
        StorageBinding {
            binding_name: name.to_owned(),
            mount_path: mount_path.to_owned(),
            kind: dotcore_proto::dotcore::catalog::v1::StorageKind::Cache as i32,
            placements: vec![placement as i32],
            environment: vec![
                dotcore_proto::dotcore::catalog::v1::StorageEnvironmentVariable {
                    name: variable.to_owned(),
                    relative_path: String::new(),
                },
            ],
        }
    }

    fn target() -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::for_test("node"),
            accelerator_device_ids: vec!["GPU-0".to_owned()],
        }
    }

    fn model_artifacts() -> PreparedModelArtifacts {
        PreparedModelArtifacts {
            artifacts: BTreeMap::from([(
                "model".to_owned(),
                std::path::PathBuf::from("/cache/model.gguf"),
            )]),
        }
    }
}
