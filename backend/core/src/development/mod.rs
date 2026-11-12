//! Development-only facade that lets a developer launch, inspect, log, and
//! tear down one transient local model workload outside any site.
//!
//! It reuses the same production path as the inference controller: Catalog
//! resolution, artifact-manager preparation, portable launch-plan translation,
//! and the Docker/NVIDIA runtime, but creates no product deployment state and
//! never touches the running backend. Every operation is fenced by ownership
//! custody so it acts only on the exact workload the developer launched.

mod handle;
mod telemetry;
#[cfg(test)]
mod tests;

use std::{
    future::Future,
    num::NonZeroU16,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use dotcore_artifact::{
    ArtifactManager, DockerOciClient, NixSources, OciImagePreparer, OciPreparer,
};
use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_domain::{
    clock::SystemClock,
    deployment::launch_pin::{LaunchPinResolver as _, RetainedArtifact, pin_source},
    hardware_observation::HardwareSnapshotSource,
    names_every_cdi_device, real_time,
    timing::{NativeTiming, Timing},
};
use dotcore_node_hardware::hardware::SystemHardwareSnapshotCollector;
use dotcore_proto::dotcore::{
    catalog::v1::{EngineParameterValues, engine_artifact::Kind as EngineArtifactKind},
    engine::v1::EngineKind,
    model::v1::{
        LocalModelDeploymentConfiguration, PortableModelLaunchPlan,
        ResolvedLocalModelDeploymentSnapshot, ResolvedModelOption,
    },
    site::v1::{
        ExecutionTarget as SiteExecutionTarget, ModelRuntimeBinding,
        execution_target::RuntimeBinding, model_runtime_binding,
    },
    telemetry::v1::{WorkloadResourceInputs, workload_resource_observation::EngineFacts},
};
use dotcore_runtime::{
    ExecutionTarget, RuntimeBackend, RuntimeBackendError, RuntimeBackendId, RuntimeFuture,
    RuntimeLogFollowQuery, RuntimeLogHistory, RuntimeLogQuery, RuntimeLogSource, RuntimeLogStream,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadArtifactBinding,
    WorkloadCorrelation, WorkloadId, WorkloadLaunch, WorkloadObservation,
};
use dotcore_runtime_backends::{docker::ContainerTokens, registry::DOCKER_RUNTIME_BACKEND_ID};
use dotcore_site::node::target_inventory::{
    NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityInventory,
    TargetCapabilityReconciler,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::model::{
    engine::{self, EngineIntegrationRegistry},
    resolution::{
        apply_profile_defaults, resolve_local_launch, resolve_local_values_on_target,
        select_engine_artifact,
    },
    resolved::ResolvedModelLaunch,
};
use crate::storage::deployment_storage_keys;
pub use handle::{DevelopmentOwnershipHandle, DevelopmentOwnershipState, OwnershipHandleError};
use handle::{OwnershipHandleFile, ScopeLock};
pub use telemetry::measurement_record;

const LAUNCH_FACTS_FORMAT_VERSION: u32 = 2;
const LOOPBACK_HOST: &str = "127.0.0.1";
const DEVELOPMENT_RUNTIME_SCOPE_DOMAIN: &[u8] = b"dotcore-development-runtime-scope-v1\0";
const DEVELOPMENT_RUNTIME_OWNER_DOMAIN: &[u8] = b"dotcore-development-runtime-owner-v1\0";

/// Composes shared launch capabilities without creating product state or controller authority.
pub struct DirectWorkloadFacade {
    catalog: Catalog,
    artifacts: ArtifactManager,
    runtime: Arc<dyn DevelopmentRuntime>,
    namespace: DevelopmentRuntimeNamespace,

    /// Timing of this process, read once when the facade connects.
    timing: Timing,
    #[cfg(test)]
    test_target_capabilities: Option<(
        dotcore_domain::ExecutionPlatform,
        dotcore_domain::NvidiaDriverVersion,
    )>,
}

impl DirectWorkloadFacade {
    /// Connects the private facade to catalog inputs and one local runtime: Docker with NVIDIA
    /// devices, or native host processes, whose workloads outlive the command that launched them.
    pub async fn connect(
        config: DirectWorkloadFacadeConfig,
    ) -> Result<Self, DirectWorkloadLaunchError> {
        let timing = Timing::for_process();
        let namespace = DevelopmentRuntimeNamespace::derive(&config.runtime_scope)?;
        validate_operation_path(&config.master_catalog_root)?;
        if let Some(development_catalog_root) = &config.development_catalog_root {
            validate_operation_path(development_catalog_root)?;
        }
        validate_operation_path(&config.cache_root)?;
        let catalog = match &config.development_catalog_root {
            Some(development_catalog_root) => Catalog::load_development_sources(
                &config.master_catalog_root,
                development_catalog_root,
                crate::model::engine::declaration_validator(),
            ),
            None => Catalog::load_with_policy(
                &config.master_catalog_root,
                dotcore_catalog::OciArtifactPolicy::Development,
                crate::model::engine::declaration_validator(),
            ),
        }
        .map_err(DirectWorkloadLaunchError::catalog)?;
        let model_artifacts = dotcore_domain::CacheLayout(&config.cache_root).model_artifacts();
        // Only a Docker runtime prepares images, so a native facade needs no Docker daemon.
        let oci_preparer = match &config.runtime {
            DevelopmentRuntimeChoice::Docker { .. } => {
                let oci_client = DockerOciClient::connect()
                    .await
                    .map_err(DirectWorkloadLaunchError::oci_client)?;
                Some(Arc::new(OciPreparer::new(Arc::new(oci_client))) as Arc<dyn OciImagePreparer>)
            }
            DevelopmentRuntimeChoice::Native { .. } => None,
        };
        let artifacts = ArtifactManager::new(model_artifacts.clone(), oci_preparer)
            .map_err(DirectWorkloadLaunchError::model_artifacts)?
            .with_nix_sources(NixSources::new(dotcore_domain::nix::DotcoreSources {
                development: config
                    .development_source
                    .map(|source| source.trim().to_owned())
                    .filter(|source| !source.is_empty()),
                packaged: None,
            }));
        let runtime: Arc<dyn DevelopmentRuntime> = match config.runtime {
            DevelopmentRuntimeChoice::Docker {
                accelerator_transport,
            } => {
                let transport = match accelerator_transport {
                    DevelopmentAcceleratorTransport::Cdi => {
                        dotcore_runtime_backends::docker::DockerAcceleratorTransport::Cdi
                    }
                    DevelopmentAcceleratorTransport::LegacyNvidia => {
                        dotcore_runtime_backends::docker::DockerAcceleratorTransport::LegacyNvidia
                    }
                };
                Arc::new(
                    dotcore_runtime_backends::docker::DockerRuntimeBackend::connect_development(
                        namespace.runtime_scope.clone(),
                        &config.cache_root,
                        transport,
                    )
                    .await?,
                )
            }
            DevelopmentRuntimeChoice::Native { launcher } => Arc::new(native_development_runtime(
                &namespace,
                &config.cache_root,
                model_artifacts,
                launcher.as_deref(),
                timing.native,
            )?),
        };
        Ok(Self {
            catalog,
            artifacts,
            runtime,
            namespace,
            timing,
            #[cfg(test)]
            test_target_capabilities: None,
        })
    }

    #[cfg(test)]
    fn new(
        catalog: Catalog,
        artifacts: ArtifactManager,
        runtime: Arc<dyn DevelopmentRuntime>,
        namespace: DevelopmentRuntimeNamespace,
    ) -> Self {
        Self {
            catalog,
            artifacts,
            runtime,
            namespace,
            timing: Timing::test(),
            test_target_capabilities: Some((
                dotcore_domain::ExecutionPlatform::linux_amd64(),
                "580".parse().expect("valid test NVIDIA driver"),
            )),
        }
    }

    /// Resolves the selected profile's catalog defaults for a direct launch. Engine parameters
    /// the profile omits take their engine defaults; resolution fills or derives the rest.
    pub fn local_launch_defaults(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
    ) -> Result<LocalModelDeploymentConfiguration, DirectWorkloadLaunchError> {
        let configuration = LocalModelDeploymentConfiguration {
            option_overrides: self
                .catalog
                .local_option_defaults_for(model_id, profile_id)
                .unwrap_or_default(),
            ..Default::default()
        };
        apply_profile_defaults(&self.catalog, model_id, profile_id, &configuration)
            .map_err(DirectWorkloadLaunchError::resolution)
    }

    /// Validates one complete local launch request on its execution target without preparing
    /// artifacts or calling runtime; the target's observed facts pick the hardware class.
    /// Development callers use this preflight after applying typed overrides and before launch.
    pub async fn validate_local_launch_configuration(
        &self,
        model_id: &CatalogModelId,
        profile_id: &str,
        configuration: &LocalModelDeploymentConfiguration,
        execution_target: &ExecutionTarget,
    ) -> Result<(), DirectWorkloadLaunchError> {
        let (configured_target, target_inventory) = self
            .observe_direct_target_capabilities(execution_target)
            .await?;
        resolve_local_values_on_target(
            &self.catalog,
            model_id,
            profile_id,
            configuration,
            &configured_target,
            &target_inventory,
        )
        .map(|_| ())
        .map_err(DirectWorkloadLaunchError::resolution)
    }

    /// Observes the direct launch target's capabilities, read at the system wall time.
    async fn observe_direct_target_capabilities(
        &self,
        target: &ExecutionTarget,
    ) -> Result<(SiteExecutionTarget, TargetCapabilityInventory), DirectWorkloadLaunchError> {
        // Direct launches run outside a composed backend: the real hardware collection and its
        // stamps follow real wall time.
        #[cfg(test)]
        if let Some((platform, driver)) = &self.test_target_capabilities {
            let accelerator = NodeAcceleratorObservation::nvidia(
                target.managed_node_id.to_string(),
                driver.major().to_string(),
            );
            return Ok(direct_target_capabilities(
                target,
                self.runtime_adapter(),
                platform.clone(),
                accelerator,
                self.timing.node_control.capability_max_age,
                real_time::wall_now(),
            ));
        }

        let platform = self
            .runtime
            .execution_platform()
            .await
            .map_err(DirectWorkloadLaunchError::resolution)?;
        let snapshot = tokio::task::spawn_blocking(|| {
            SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock))
                .collect_snapshot()
        })
        .await
        .map_err(DirectWorkloadLaunchError::resolution)?;
        Ok(direct_target_capabilities(
            target,
            self.runtime_adapter(),
            platform,
            NodeAcceleratorObservation::from_hardware_snapshot(
                target.managed_node_id.to_string(),
                &snapshot,
                false,
            ),
            self.timing.node_control.capability_max_age,
            real_time::wall_now(),
        ))
    }

    /// Pins a Nix engine artifact the snapshot leaves to its hosting node to the store path of this
    /// node's own Nix system, as a SiteNode pins it for a site deployment: the catalog store path
    /// of a locked artifact, or the evaluated path of a development-source package.
    async fn pin_nix_engine(
        &self,
        snapshot: &mut ResolvedLocalModelDeploymentSnapshot,
        cancellation: &CancellationToken,
    ) -> Result<(), DirectWorkloadLaunchError> {
        let Some(engine) = snapshot.engine_artifact.as_mut() else {
            return Ok(());
        };
        let Some(EngineArtifactKind::Nix(nix)) = engine
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.kind.as_ref())
        else {
            return Ok(());
        };
        let Some(source) = pin_source(
            RetainedArtifact::Nix(nix),
            engine.preparation_policy,
            &engine.launch_identity,
        ) else {
            return Ok(());
        };
        let platform = engine.platform.clone().ok_or_else(|| {
            DirectWorkloadLaunchError::Resolution(anyhow::anyhow!(
                "resolved Nix engine artifact has no platform"
            ))
        })?;
        engine.launch_identity = self
            .artifacts
            .resolve_pin(&source, &platform, cancellation.child_token())
            .await
            .map_err(DirectWorkloadLaunchError::EngineArtifact)?;
        Ok(())
    }

    /// Resolves and prepares one workload, then establishes durable ownership before runtime mutation.
    pub async fn launch(
        &self,
        request: DirectWorkloadLaunchRequest,
        cancellation: CancellationToken,
        progress: DevelopmentLaunchProgress,
    ) -> Result<DevelopmentLaunchFacts, DirectWorkloadLaunchError> {
        let runtime_owner_id = self.namespace.owner_id(&request.owner_id)?;
        self.validate_runtime()?;
        if request
            .execution_target
            .accelerator_device_ids
            .iter()
            .any(|id| names_every_cdi_device(id))
        {
            return Err(DirectWorkloadLaunchError::EveryAcceleratorDevice);
        }
        let (configured_target, target_inventory) = self
            .observe_direct_target_capabilities(&request.execution_target)
            .await?;
        let selection = select_engine_artifact(
            &self.catalog,
            &request.model_id,
            &request.profile_id,
            &request.configuration,
            &configured_target,
            &target_inventory,
        )
        .map_err(DirectWorkloadLaunchError::resolution)?;
        let engine_identity = selection
            .resolve_launch_identity(&self.artifacts)
            .await
            .map_err(DirectWorkloadLaunchError::engine_artifact)?;
        let mut snapshot = resolve_local_launch(
            &self.catalog,
            &request.model_id,
            &request.profile_id,
            &request.configuration,
            &configured_target,
            &target_inventory,
            engine_identity.as_ref(),
        )
        .map_err(DirectWorkloadLaunchError::resolution)?;
        self.pin_nix_engine(&mut snapshot, &cancellation).await?;
        let resolved = ResolvedModelLaunch::from_local_snapshot(&snapshot)
            .map_err(DirectWorkloadLaunchError::catalog)?;
        let prepared = self
            .artifacts
            .prepare_model(
                &resolved.artifacts,
                &cancellation,
                None,
                None,
                &dotcore_artifact::PreparationOutput::discard(),
            )
            .await
            .map_err(DirectWorkloadLaunchError::model_artifacts)?;
        self.artifacts
            .prepare_engine(
                &resolved.engine.preparation(),
                cancellation,
                &dotcore_artifact::PreparationOutput::discard(),
            )
            .await
            .map_err(DirectWorkloadLaunchError::engine_artifact)?;
        let spec = EngineIntegrationRegistry::production()
            .materialize_workload(
                &resolved,
                &request.execution_target,
                &prepared.artifacts,
                &runtime_owner_id,
            )
            .map_err(DirectWorkloadLaunchError::materialization)?;
        let starts_on_create = self.runtime.starts_on_create();
        // A container command is known before creation; a runtime that starts on create reports
        // the command it resolved with its own host ports.
        let container_command = if starts_on_create {
            None
        } else {
            let command = spec
                .resolve(&ContainerTokens)
                .map_err(DirectWorkloadLaunchError::materialization)?;
            Some(
                std::iter::once(command.executable)
                    .chain(command.arguments)
                    .collect::<Vec<_>>(),
            )
        };
        let launch_inputs = resolved.telemetry_inputs();
        let launch = WorkloadLaunch::new(runtime_owner_id, request.execution_target, spec);
        // Scope custody is always acquired before handle custody so lifecycle operations cannot deadlock.
        let _scope = ScopeLock::acquire(&self.namespace.runtime_scope).await?;
        let namespace_evidence = self.runtime.namespace_evidence_count().await?;
        let existing = self.runtime.discover_workloads().await?;
        if namespace_evidence != 0 || !existing.is_empty() {
            return Err(DirectWorkloadLaunchError::NamespaceNotEmpty);
        }
        let mut handle = OwnershipHandleFile::create(
            &request.handle_path,
            DevelopmentOwnershipHandle::prepared(
                self.namespace.runtime_scope.clone(),
                self.runtime.backend_id().0,
                launch.correlation.owner_id.clone(),
                launch.correlation.reconcile_generation,
                launch.exact_intent_fingerprint(),
                launch_inputs,
            ),
        )?;
        progress.mark_ownership_established();
        transition(
            &mut handle,
            DevelopmentOwnershipState::Creating,
            None,
            None,
            None,
        )?;
        let created = match self.runtime.create(&launch).await {
            Ok(WorkloadObservation::Stopped {
                workload: observed, ..
            }) if !starts_on_create => observed,
            Ok(WorkloadObservation::Running(observed)) if starts_on_create => observed,
            Ok(_) => {
                mark_dirty(
                    &mut handle,
                    "Runtime did not report the workload in its created state",
                );
                return Err(DirectWorkloadLaunchError::IdentityMismatch);
            }
            Err(error) => {
                mark_dirty(&mut handle, "Runtime create did not complete conclusively");
                return Err(error.into());
            }
        };
        if !matches_handle_identity(handle.handle(), &created) {
            mark_dirty(
                &mut handle,
                "Runtime launch identity differs from the ownership handle",
            );
            return Err(DirectWorkloadLaunchError::IdentityMismatch);
        }
        transition(
            &mut handle,
            DevelopmentOwnershipState::Created,
            Some(created.workload_id.0.clone()),
            None,
            None,
        )?;
        let created_process = match self.runtime.inspect_process(&created.workload_id).await {
            Ok(process)
                if !process.runtime_process_id.is_empty()
                    && process.process_id.is_none() != starts_on_create
                    && process.launch_fingerprint == handle.handle().launch_fingerprint =>
            {
                process
            }
            Ok(_) => {
                mark_dirty(
                    &mut handle,
                    "Runtime created process facts differ from the created workload",
                );
                return Err(DirectWorkloadLaunchError::IdentityMismatch);
            }
            Err(error) => {
                mark_dirty(
                    &mut handle,
                    "Runtime created process inspection was inconclusive",
                );
                return Err(error.into());
            }
        };
        transition(
            &mut handle,
            DevelopmentOwnershipState::Created,
            None,
            Some(created_process.runtime_process_id),
            None,
        )?;
        transition(
            &mut handle,
            DevelopmentOwnershipState::Starting,
            None,
            None,
            None,
        )?;
        let started = if starts_on_create {
            Ok(WorkloadObservation::Running(created.clone()))
        } else {
            self.runtime.start_workload(&created.workload_id).await
        };
        let observed = match started {
            Ok(WorkloadObservation::Running(observed)) => observed,
            Ok(_) => {
                mark_dirty(
                    &mut handle,
                    "Runtime did not report the workload running after start",
                );
                return Err(DirectWorkloadLaunchError::NotRunning);
            }
            Err(error) => {
                mark_dirty(&mut handle, "Runtime start did not complete conclusively");
                return Err(error.into());
            }
        };
        if !matches_handle_identity(handle.handle(), &observed)
            || observed.workload_id != created.workload_id
        {
            mark_dirty(&mut handle, "Runtime start changed the workload identity");
            return Err(DirectWorkloadLaunchError::IdentityMismatch);
        }
        transition(
            &mut handle,
            DevelopmentOwnershipState::Started,
            None,
            None,
            None,
        )?;
        let process = match self.runtime.inspect_process(&observed.workload_id).await {
            Ok(process) => process,
            Err(error) => {
                mark_dirty(
                    &mut handle,
                    "Runtime process inspection did not complete conclusively",
                );
                return Err(error.into());
            }
        };
        let observed_endpoints = development_endpoints(&observed);
        let process_id = process.process_id.ok_or_else(|| {
            mark_dirty(&mut handle, "Runtime reported no running process identity");
            DirectWorkloadLaunchError::IdentityMismatch
        })?;
        if process.runtime_process_id.is_empty()
            || process_id == 0
            || process.launch_fingerprint != handle.handle().launch_fingerprint
            || process.loopback_endpoints != observed_endpoints
        {
            mark_dirty(
                &mut handle,
                "Runtime process facts differ from the launched workload",
            );
            return Err(DirectWorkloadLaunchError::IdentityMismatch);
        }
        transition(
            &mut handle,
            DevelopmentOwnershipState::Running,
            Some(observed.workload_id.0.clone()),
            Some(process.runtime_process_id.clone()),
            Some(process_id),
        )?;
        let engine = snapshot
            .engine_artifact
            .as_ref()
            .expect("resolved snapshot has an engine artifact");
        let engine_artifact = engine
            .artifact
            .as_ref()
            .expect("resolved snapshot has an engine artifact value");
        let model_artifact = snapshot
            .model_artifact
            .as_ref()
            .expect("resolved snapshot has a model artifact");

        Ok(DevelopmentLaunchFacts {
            format_version: LAUNCH_FACTS_FORMAT_VERSION,
            model_id: request.model_id,
            profile_id: snapshot.profile_id,
            configuration_id: snapshot.configuration_id,
            model_artifact_id: model_artifact.artifact_id.clone(),
            auxiliary_artifact_ids: snapshot
                .auxiliary_artifacts
                .iter()
                .map(|artifact| artifact.auxiliary_artifact_id.clone())
                .collect(),
            resolved_options: snapshot.resolved_options.clone(),
            engine_parameter_values: snapshot.engine_parameter_values.clone().unwrap_or_default(),
            engine_release_id: engine.engine_release_id.clone(),
            engine_artifact_id: engine_artifact.artifact_id.clone(),
            engine_version: engine.engine_version.clone(),
            desired_launch_plan: snapshot
                .desired_launch_plan
                .expect("resolved snapshot has a desired launch plan"),
            runtime_backend_id: observed.backend_id,
            workload_id: observed.workload_id,
            correlation: observed.correlation,
            runtime_process_id: process.runtime_process_id,
            process_id,
            materialized_argv: match (container_command, process.command) {
                (Some(command), _) | (None, Some(command)) => command,
                (None, None) => {
                    mark_dirty(&mut handle, "Runtime reported no resolved workload command");
                    return Err(DirectWorkloadLaunchError::IdentityMismatch);
                }
            },
            materialized_mounts: launch.spec.artifacts,
            loopback_endpoints: process.loopback_endpoints,
        })
    }

    /// Re-proves one persisted handle against exact current runtime evidence without mutation.
    pub async fn inspect_handle(
        &self,
        handle_path: &Path,
    ) -> Result<DevelopmentWorkloadInspection, DirectWorkloadTeardownError> {
        let (_scope, transaction, observation) = self.prove_inspection_custody(handle_path).await?;
        let state = match &observation {
            Some((WorkloadObservation::Running(_), _))
                if transaction.handle().state == DevelopmentOwnershipState::Running =>
            {
                DevelopmentInspectionState::Running
            }
            Some((WorkloadObservation::Stopped { .. }, _))
                if transaction.handle().state != DevelopmentOwnershipState::Dirty =>
            {
                DevelopmentInspectionState::Stopped
            }
            None if transaction.handle().state == DevelopmentOwnershipState::Removed => {
                DevelopmentInspectionState::Removed
            }
            _ => DevelopmentInspectionState::Dirty,
        };
        let (workload_id, runtime_process_id, process_id) = observation
            .map(|(_, process)| {
                (
                    Some(process.workload_id),
                    Some(process.runtime_process_id),
                    process.process_id,
                )
            })
            .unwrap_or_default();
        Ok(DevelopmentWorkloadInspection {
            handle: transaction.handle().clone(),
            state,
            workload_id,
            runtime_process_id,
            process_id,
        })
    }

    /// Runs one read-only operation while retaining custody of an exact live workload process.
    /// Runtime and process identity are re-proven after the operation before its result is returned.
    pub async fn with_live_process<T, F, Fut>(
        &self,
        handle_path: &Path,
        cancellation: CancellationToken,
        operation: F,
    ) -> Result<(DevelopmentWorkloadProcessIdentity, T), DirectWorkloadTeardownError>
    where
        F: FnOnce(DevelopmentWorkloadProcessIdentity) -> Fut,
        Fut: Future<Output = T>,
    {
        let (_scope, transaction, observation) = self
            .prove_inspection_custody_cancellable(handle_path, cancellation)
            .await?;
        let identity = live_process_identity(transaction.handle(), observation)?;
        let output = operation(identity.clone()).await;
        let final_observation = self.prove_runtime_identity(transaction.handle()).await?;
        let final_identity = live_process_identity(transaction.handle(), final_observation)?;
        if final_identity != identity {
            return Err(DirectWorkloadTeardownError::Dirty(
                "Runtime workload or process identity changed during the read-only operation"
                    .to_owned(),
            ));
        }
        Ok((identity, output))
    }

    /// Reads bounded diagnostics only after reproving the exact owned runtime identity.
    pub async fn read_logs(
        &self,
        handle_path: &Path,
        maximum_entries: NonZeroU16,
    ) -> Result<RuntimeLogHistory, DirectWorkloadTeardownError> {
        let (scope, transaction, workload_id) = self.prove_log_custody(handle_path).await?;
        let query = RuntimeWorkloadLogQuery::new(
            RuntimeLogSource {
                name: "workload".to_owned(),
                label: "workload".to_owned(),
            },
            RuntimeLogQuery::new(maximum_entries, None)
                .map_err(|error| DirectWorkloadTeardownError::Dirty(error.to_string()))?,
        );
        let history = self
            .runtime
            .read_workload_logs(&workload_id, &query)
            .await
            .map_err(DirectWorkloadTeardownError::Runtime)?;
        drop(transaction);
        drop(scope);
        Ok(history)
    }

    /// Reads engine-reported startup memory facts from the owned workload's log history.
    /// Returns none when the workload's engine has no startup fact extractor.
    pub async fn read_engine_startup_facts(
        &self,
        handle_path: &Path,
    ) -> Result<Option<EngineFacts>, DirectWorkloadTeardownError> {
        let (scope, transaction, workload_id) = self.prove_log_custody(handle_path).await?;
        let engine_release_id = &transaction.handle().launch_inputs.engine_release_id;
        let Some(kind) = self
            .catalog
            .list_engine_releases()
            .into_iter()
            .find(|release| &release.engine_release_id == engine_release_id)
            .and_then(|release| EngineKind::try_from(release.engine_kind).ok())
        else {
            return Ok(None);
        };
        let registry = EngineIntegrationRegistry::production();
        let Some(extractor) = registry
            .integration(kind)
            .ok()
            .and_then(|integration| integration.startup_fact_extractor())
        else {
            return Ok(None);
        };
        let runtime = &self.runtime;
        let workload_id = &workload_id;
        let facts = engine::startup_facts::read_startup_facts(extractor, |history| async move {
            let query = RuntimeWorkloadLogQuery::new(
                RuntimeLogSource {
                    name: "workload".to_owned(),
                    label: "workload".to_owned(),
                },
                history,
            );
            runtime.read_workload_logs(workload_id, &query).await
        })
        .await
        .map_err(DirectWorkloadTeardownError::Runtime)?;
        drop(transaction);
        drop(scope);
        Ok(facts.map(|read| read.extracted.facts))
    }

    /// Opens a cancellable diagnostic stream only after reproving exact runtime custody.
    pub async fn follow_logs(
        &self,
        handle_path: &Path,
        cancellation: CancellationToken,
    ) -> Result<RuntimeLogStream, DirectWorkloadTeardownError> {
        let (scope, transaction, workload_id) = self.prove_log_custody(handle_path).await?;
        let query = RuntimeWorkloadLogFollowQuery::new(
            RuntimeLogSource {
                name: "workload".to_owned(),
                label: "workload".to_owned(),
            },
            RuntimeLogFollowQuery::new(None, cancellation),
        );
        let stream = self
            .runtime
            .follow_workload_logs(&workload_id, query)
            .await
            .map_err(DirectWorkloadTeardownError::Runtime)?;
        let guarded = stream.map(move |event| {
            let _custody = (&scope, &transaction);
            event
        });
        Ok(Box::pin(guarded))
    }

    /// Recovers and removes only the workload proven by the complete ownership handle.
    pub async fn teardown(
        &self,
        handle_path: &Path,
    ) -> Result<DevelopmentOwnershipHandle, DirectWorkloadTeardownError> {
        self.validate_runtime()
            .map_err(|_| DirectWorkloadTeardownError::InvalidRuntime)?;
        let _scope = ScopeLock::acquire(&self.namespace.runtime_scope).await?;
        let mut transaction = OwnershipHandleFile::open(handle_path)?;
        if transaction.handle().runtime_scope != self.namespace.runtime_scope
            || transaction.handle().runtime_backend_id != self.runtime.backend_id().0
        {
            return Err(DirectWorkloadTeardownError::HandleNamespaceMismatch);
        }
        let namespace_evidence = self.runtime.namespace_evidence_count().await?;
        let observations = self.runtime.discover_workloads().await?;
        if namespace_evidence != observations.len() {
            return dirty_teardown(
                &mut transaction,
                "Runtime scope contains foreign or incomplete ownership evidence",
            );
        }
        let observed = match observations.as_slice() {
            [] => {
                // A retry after an interrupted teardown still owes the runner's storage removal.
                if transaction.handle().state != DevelopmentOwnershipState::Removed {
                    self.remove_runner_storage(transaction.handle()).await?;
                }
                transition(
                    &mut transaction,
                    DevelopmentOwnershipState::Removed,
                    None,
                    None,
                    None,
                )?;
                return Ok(transaction.handle().clone());
            }
            [observation] => match exact_observed(observation) {
                Some(observed) => observed,
                None => {
                    return dirty_teardown(
                        &mut transaction,
                        "Runtime scope contains inconclusive ownership evidence",
                    );
                }
            },
            _ => {
                return dirty_teardown(
                    &mut transaction,
                    "Runtime scope contains ambiguous workload evidence",
                );
            }
        };
        if matches!(
            transaction.handle().state,
            DevelopmentOwnershipState::Prepared | DevelopmentOwnershipState::Removed
        ) {
            return dirty_teardown(
                &mut transaction,
                "Runtime evidence is impossible for the persisted ownership state",
            );
        }
        if !matches_handle_identity(transaction.handle(), observed)
            || transaction
                .handle()
                .workload_id
                .as_deref()
                .is_some_and(|id| id != observed.workload_id.0)
        {
            return dirty_teardown(
                &mut transaction,
                "Runtime workload identity differs from the ownership handle",
            );
        }
        let process = match self.runtime.inspect_process(&observed.workload_id).await {
            Ok(process) => process,
            Err(error) => {
                mark_dirty(
                    &mut transaction,
                    "Runtime process identity inspection did not complete conclusively",
                );
                return Err(error.into());
            }
        };
        let active = matches!(
            observations[0],
            WorkloadObservation::Starting(_) | WorkloadObservation::Running(_)
        );
        if process.runtime_process_id.is_empty()
            || process.launch_fingerprint != transaction.handle().launch_fingerprint
            || active && process.process_id.is_none()
            || transaction
                .handle()
                .runtime_process_id
                .as_deref()
                .is_some_and(|id| id != process.runtime_process_id)
            || transaction.handle().process_id.is_some()
                && process.process_id.is_some()
                && transaction.handle().process_id != process.process_id
        {
            return dirty_teardown(
                &mut transaction,
                "Runtime process identity differs from the ownership handle",
            );
        }
        let process_id = transaction.handle().process_id.or(process.process_id);
        transition(
            &mut transaction,
            DevelopmentOwnershipState::Stopping,
            Some(observed.workload_id.0.clone()),
            Some(process.runtime_process_id.clone()),
            process_id,
        )?;
        if !matches!(
            observations[0],
            WorkloadObservation::Stopped { .. } | WorkloadObservation::Stopping(_)
        ) && let Err(error) = self.runtime.stop_workload(&observed.workload_id).await
        {
            mark_dirty(
                &mut transaction,
                "Runtime stop did not complete conclusively",
            );
            return Err(error.into());
        }
        let stopped = match self.runtime.inspect_workload(&observed.workload_id).await {
            Ok(stopped) => stopped,
            Err(error) => {
                mark_dirty(
                    &mut transaction,
                    "Runtime stop inspection did not complete conclusively",
                );
                return Err(error.into());
            }
        };
        let Some(stopped_observed) = exact_observed(&stopped)
            .filter(|candidate| matches_handle_identity(transaction.handle(), candidate))
        else {
            return dirty_teardown(
                &mut transaction,
                "Runtime workload identity changed during stop inspection",
            );
        };
        if !matches!(stopped, WorkloadObservation::Stopped { .. }) {
            return dirty_teardown(
                &mut transaction,
                "Runtime did not prove the exact workload stopped",
            );
        }
        let stopped_process = match self
            .runtime
            .inspect_process(&stopped_observed.workload_id)
            .await
        {
            Ok(process) => process,
            Err(error) => {
                mark_dirty(
                    &mut transaction,
                    "Runtime stopped-process inspection did not complete conclusively",
                );
                return Err(error.into());
            }
        };
        if stopped_process.runtime_process_id != process.runtime_process_id
            || stopped_process.launch_fingerprint != transaction.handle().launch_fingerprint
        {
            return dirty_teardown(
                &mut transaction,
                "Runtime container identity or launch intent changed after stop",
            );
        }
        transition(
            &mut transaction,
            DevelopmentOwnershipState::Removing,
            Some(observed.workload_id.0.clone()),
            Some(process.runtime_process_id),
            process_id,
        )?;
        if let Err(error) = self.runtime.remove_workload(&observed.workload_id).await {
            mark_dirty(
                &mut transaction,
                "Runtime removal did not complete conclusively",
            );
            return Err(error.into());
        }
        let remaining_evidence = self.runtime.namespace_evidence_count().await?;
        let remaining = self.runtime.discover_workloads().await?;
        if remaining_evidence != 0 || !remaining.is_empty() {
            return dirty_teardown(
                &mut transaction,
                "Runtime did not prove the owned workload absent after removal",
            );
        }
        self.remove_runner_storage(transaction.handle()).await?;
        transition(
            &mut transaction,
            DevelopmentOwnershipState::Removed,
            Some(observed.workload_id.0.clone()),
            Some(stopped_process.runtime_process_id),
            process_id,
        )?;
        Ok(transaction.handle().clone())
    }

    /// Removes the runner's deployment-scoped storage, keyed by its runtime owner id exactly as
    /// launch attached it from its engine release's storage bindings. Engine-artifact storage is
    /// shared across runs and stays.
    async fn remove_runner_storage(
        &self,
        handle: &DevelopmentOwnershipHandle,
    ) -> Result<(), RuntimeBackendError> {
        let release_id = &handle.launch_inputs.engine_release_id;
        let Some(release) = self
            .catalog
            .list_engine_releases()
            .into_iter()
            .find(|release| &release.engine_release_id == release_id)
        else {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "engine release {release_id:?} of the runner is not in the catalog, so its storage can't be named"
            )));
        };
        let storage = deployment_storage_keys(&release.storage_bindings, &handle.owner_id)
            .map_err(|error| RuntimeBackendError::InvalidLaunch(anyhow::Error::new(error)))?;
        for key in &storage {
            self.runtime.remove_storage(key).await?;
        }
        Ok(())
    }

    async fn prove_log_custody(
        &self,
        handle_path: &Path,
    ) -> Result<(ScopeLock, OwnershipHandleFile, WorkloadId), DirectWorkloadTeardownError> {
        let (scope, transaction, observation) = self.prove_inspection_custody(handle_path).await?;
        let workload_id = observation
            .map(|(_, process)| process.workload_id)
            .ok_or_else(|| {
                DirectWorkloadTeardownError::Dirty(
                    "ownership handle has no live runtime workload".to_owned(),
                )
            })?;
        Ok((scope, transaction, workload_id))
    }

    async fn prove_inspection_custody_cancellable(
        &self,
        handle_path: &Path,
        cancellation: CancellationToken,
    ) -> Result<
        (
            ScopeLock,
            OwnershipHandleFile,
            Option<(WorkloadObservation, ProvenDevelopmentProcess)>,
        ),
        DirectWorkloadTeardownError,
    > {
        self.validate_runtime()
            .map_err(|_| DirectWorkloadTeardownError::InvalidRuntime)?;
        let scope =
            ScopeLock::acquire_cancellable(&self.namespace.runtime_scope, cancellation).await?;
        let transaction = OwnershipHandleFile::open(handle_path)?;
        let observation = self.prove_runtime_identity(transaction.handle()).await?;
        Ok((scope, transaction, observation))
    }

    async fn prove_inspection_custody(
        &self,
        handle_path: &Path,
    ) -> Result<
        (
            ScopeLock,
            OwnershipHandleFile,
            Option<(WorkloadObservation, ProvenDevelopmentProcess)>,
        ),
        DirectWorkloadTeardownError,
    > {
        self.validate_runtime()
            .map_err(|_| DirectWorkloadTeardownError::InvalidRuntime)?;
        let scope = ScopeLock::acquire(&self.namespace.runtime_scope).await?;
        let transaction = OwnershipHandleFile::open(handle_path)?;
        let observation = self.prove_runtime_identity(transaction.handle()).await?;
        Ok((scope, transaction, observation))
    }

    async fn prove_runtime_identity(
        &self,
        handle: &DevelopmentOwnershipHandle,
    ) -> Result<Option<(WorkloadObservation, ProvenDevelopmentProcess)>, DirectWorkloadTeardownError>
    {
        if handle.runtime_scope != self.namespace.runtime_scope
            || handle.runtime_backend_id != self.runtime.backend_id().0
        {
            return Err(DirectWorkloadTeardownError::HandleNamespaceMismatch);
        }
        let namespace_evidence = self.runtime.namespace_evidence_count().await?;
        let observations = self.runtime.discover_workloads().await?;
        if namespace_evidence == 0 && observations.is_empty() {
            if matches!(
                handle.state,
                DevelopmentOwnershipState::Removed | DevelopmentOwnershipState::Dirty
            ) {
                return Ok(None);
            }
            return Err(DirectWorkloadTeardownError::Dirty(
                "Runtime workload is missing for the ownership handle".to_owned(),
            ));
        }
        let Some((observation, observed)) = (namespace_evidence == 1)
            .then_some(observations.as_slice())
            .and_then(|items| items.try_into().ok())
            .and_then(|[observation]: &[WorkloadObservation; 1]| {
                exact_observed(observation).map(|observed| (observation, observed))
            })
        else {
            return Err(DirectWorkloadTeardownError::Dirty(
                "Runtime scope contains missing, foreign, or ambiguous workload evidence"
                    .to_owned(),
            ));
        };
        if !matches_handle_identity(handle, observed)
            || handle.workload_id.as_deref() != Some(&observed.workload_id.0)
        {
            return Err(DirectWorkloadTeardownError::Dirty(
                "Runtime workload identity differs from the ownership handle".to_owned(),
            ));
        }
        let process = self.runtime.inspect_process(&observed.workload_id).await?;
        let active = matches!(
            observation,
            WorkloadObservation::Starting(_) | WorkloadObservation::Running(_)
        );
        if process.runtime_process_id.is_empty()
            || process.launch_fingerprint != handle.launch_fingerprint
            || handle.runtime_process_id.as_deref() != Some(&process.runtime_process_id)
            || active && process.process_id.is_none()
            || handle.process_id.is_some()
                && process.process_id.is_some()
                && handle.process_id != process.process_id
        {
            return Err(DirectWorkloadTeardownError::Dirty(
                "Runtime process identity differs from the ownership handle".to_owned(),
            ));
        }
        Ok(Some((
            observation.clone(),
            ProvenDevelopmentProcess {
                workload_id: observed.workload_id.clone(),
                runtime_process_id: process.runtime_process_id,
                process_id: process.process_id,
            },
        )))
    }

    /// Model runtime binding of the development target, which decides the artifact kinds it can
    /// launch: container images through Docker, or native Nix packages.
    fn runtime_adapter(&self) -> model_runtime_binding::Adapter {
        if self.runtime.backend_id().0 == dotcore_runtime_backends::native::BACKEND_ID {
            model_runtime_binding::Adapter::NativeRuntime(
                dotcore_proto::dotcore::site::v1::NativeRuntimeAdapter {
                    workload_lifetime:
                        dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime::Persistent as i32,
                },
            )
        } else {
            model_runtime_binding::Adapter::DockerRuntime(Default::default())
        }
    }

    fn validate_runtime(&self) -> Result<(), DirectWorkloadLaunchError> {
        let backend = self.runtime.backend_id().0;
        if backend != DOCKER_RUNTIME_BACKEND_ID
            && backend != dotcore_runtime_backends::native::BACKEND_ID
        {
            return Err(DirectWorkloadLaunchError::InvalidRuntime);
        }
        Ok(())
    }
}

fn direct_target_capabilities(
    target: &ExecutionTarget,
    adapter: model_runtime_binding::Adapter,
    platform: dotcore_domain::ExecutionPlatform,
    accelerator: NodeAcceleratorObservation,
    capability_max_age: Duration,
    observed_at: SystemTime,
) -> (SiteExecutionTarget, TargetCapabilityInventory) {
    let execution_target_id = "development-target";
    let development_target = SiteExecutionTarget {
        execution_target_id: execution_target_id.to_owned(),
        display_name: "Development target".to_owned(),
        site_node_id: target.managed_node_id.to_string(),
        platform: Some(platform.to_oci_platform()),
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
            adapter: Some(adapter),
        })),
    };
    // The target's accelerator selectors pick the observed devices whose models and memory the
    // hardware class and capacity read.
    let inventory = TargetCapabilityReconciler::reconcile(
        [&development_target],
        [RuntimePlatformObservation::available_at(
            execution_target_id,
            platform,
            observed_at,
        )],
        [accelerator],
        capability_max_age,
        observed_at,
    );
    (development_target, inventory)
}

/// Invocation-local proof that the current launch established new handle custody.
#[derive(Clone, Debug, Default)]
pub struct DevelopmentLaunchProgress {
    ownership_established: Arc<AtomicBool>,
}

impl DevelopmentLaunchProgress {
    pub fn ownership_established(&self) -> bool {
        self.ownership_established.load(Ordering::Acquire)
    }

    fn mark_ownership_established(&self) {
        self.ownership_established.store(true, Ordering::Release);
    }
}

/// Exact current inspection state after runtime identity proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevelopmentInspectionState {
    Running,

    Stopped,

    Removed,

    Dirty,
}

/// Persisted handle and exact current state for one inspected workload.
#[derive(Clone, Debug, PartialEq)]
pub struct DevelopmentWorkloadInspection {
    pub handle: DevelopmentOwnershipHandle,
    pub state: DevelopmentInspectionState,
    pub workload_id: Option<WorkloadId>,
    pub runtime_process_id: Option<String>,
    pub process_id: Option<u32>,
}

/// Exact live workload and process identity held under development custody.
/// These facts are stable across one successful read-only operation.
#[derive(Clone, Debug, PartialEq)]
pub struct DevelopmentWorkloadProcessIdentity {
    pub runtime_scope: String,
    pub runtime_backend_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub launch_fingerprint: String,
    pub workload_id: WorkloadId,
    pub runtime_process_id: String,
    pub process_id: u32,
    pub launch_inputs: WorkloadResourceInputs,
}

/// Exact caller-selected inputs for one private development launch.
pub struct DirectWorkloadLaunchRequest {
    pub handle_path: PathBuf,
    pub owner_id: String,
    pub model_id: CatalogModelId,
    pub profile_id: String,
    pub configuration: LocalModelDeploymentConfiguration,
    pub execution_target: ExecutionTarget,
}

/// Private construction inputs for a local development facade.
pub struct DirectWorkloadFacadeConfig {
    pub master_catalog_root: PathBuf,
    pub development_catalog_root: Option<PathBuf>,
    /// Flake `self` Nix engine artifacts are evaluated and built from, when any.
    pub development_source: Option<String>,
    pub cache_root: PathBuf,
    pub runtime_scope: String,
    pub runtime: DevelopmentRuntimeChoice,
}

/// Runtime a development facade launches its workload through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevelopmentRuntimeChoice {
    /// Docker containers, with NVIDIA devices passed through `accelerator_transport`.
    Docker {
        accelerator_transport: DevelopmentAcceleratorTransport,
    },

    /// Native host processes held by the launcher at `launcher`, or the one beside this
    /// executable.
    Native { launcher: Option<PathBuf> },
}

/// SiteNode id native development workloads record; a native development launch targets it.
pub const DEVELOPMENT_NODE_ID: &str = "local-node";

/// Opens the native runtime of a development scope: its own state directory under the cache,
/// its workloads recording the scope as site and execution target, and artifacts only from the
/// model cache. Workloads are persistent, so they outlive the command that launched them.
fn native_development_runtime(
    namespace: &DevelopmentRuntimeNamespace,
    cache_root: &Path,
    model_artifacts: PathBuf,
    launcher: Option<&Path>,
    timing: NativeTiming,
) -> Result<dotcore_runtime_backends::native::NativeRuntimeBackend, DirectWorkloadLaunchError> {
    let launcher =
        dotcore_runtime_backends::native::resolve_launcher(launcher).map_err(|error| {
            DirectWorkloadLaunchError::Runtime(RuntimeBackendError::native_with_source(
                "resolve the native workload launcher",
                error,
            ))
        })?;
    let config = dotcore_runtime_backends::native::NativeRuntimeConfig {
        state_root: dotcore_runtime_backends::native::NativeStateRoot::for_site(
            cache_root.join("development-native"),
        ),
        store_root: PathBuf::from(dotcore_runtime_backends::native::DEFAULT_STORE_ROOT),
        approved_artifact_root: model_artifacts,
        site_id: namespace.runtime_scope.clone(),
        site_node_id: dotcore_domain::SiteNodeId::parse(DEVELOPMENT_NODE_ID)
            .expect("valid development node id"),
        execution_target_id: namespace.runtime_scope.clone(),
        lifetime: dotcore_runtime_backends::native::NativeWorkloadLifetime::Persistent,
    };
    Ok(dotcore_runtime_backends::native::NativeRuntimeBackend::new(
        config, launcher, timing,
    )?)
}

/// NVIDIA device transport used by the private development Docker runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevelopmentAcceleratorTransport {
    Cdi,

    LegacyNvidia,
}

#[derive(Clone, Debug)]
struct ProvenDevelopmentProcess {
    workload_id: WorkloadId,
    runtime_process_id: String,
    process_id: Option<u32>,
}

#[derive(Clone, Debug)]
struct DevelopmentRuntimeNamespace {
    runtime_scope: String,
}

impl DevelopmentRuntimeNamespace {
    fn derive(runtime_scope: &str) -> Result<Self, DirectWorkloadLaunchError> {
        validate_development_identity(runtime_scope)?;
        Ok(Self {
            runtime_scope: format!(
                "development-{}",
                digest_prefix(DEVELOPMENT_RUNTIME_SCOPE_DOMAIN, &[runtime_scope], 48)
            ),
        })
    }

    fn owner_id(&self, owner_id: &str) -> Result<String, DirectWorkloadLaunchError> {
        validate_development_identity(owner_id)?;
        Ok(format!(
            "runner-{}",
            digest_prefix(
                DEVELOPMENT_RUNTIME_OWNER_DOMAIN,
                &[&self.runtime_scope, owner_id],
                56
            )
        ))
    }
}

/// Versioned private facts for the exact launched workload and process.
#[derive(Clone, Debug, PartialEq)]
pub struct DevelopmentLaunchFacts {
    pub format_version: u32,
    pub model_id: CatalogModelId,
    pub profile_id: String,
    pub configuration_id: String,
    pub model_artifact_id: String,
    pub auxiliary_artifact_ids: Vec<String>,
    pub resolved_options: Vec<ResolvedModelOption>,
    pub engine_parameter_values: EngineParameterValues,
    pub engine_release_id: String,
    pub engine_artifact_id: String,
    pub engine_version: String,
    pub desired_launch_plan: PortableModelLaunchPlan,
    pub runtime_backend_id: RuntimeBackendId,
    pub workload_id: WorkloadId,
    pub correlation: WorkloadCorrelation,
    pub runtime_process_id: String,
    pub process_id: u32,
    pub materialized_argv: Vec<String>,
    pub materialized_mounts: Vec<WorkloadArtifactBinding>,
    pub loopback_endpoints: Vec<DevelopmentLoopbackEndpoint>,
}

/// One exact loopback-only endpoint assigned to the transient workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevelopmentLoopbackEndpoint {
    pub port_name: String,
    pub host: String,
    pub host_port: u16,
}

/// Runtime-specific process facts kept outside the owner-neutral runtime contract: a stable
/// runtime identity (a container ID, or a native workload's id), the host process while it runs,
/// the recorded launch intent, and, for runtimes that resolve it themselves, the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DevelopmentProcessFacts {
    pub(crate) runtime_process_id: String,
    pub(crate) process_id: Option<u32>,
    pub(crate) launch_fingerprint: String,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) loopback_endpoints: Vec<DevelopmentLoopbackEndpoint>,
}

pub(crate) trait DevelopmentRuntime: RuntimeBackend {
    fn namespace_evidence_count<'a>(&'a self) -> RuntimeFuture<'a, usize>;

    /// Whether creating a workload also starts it. A runtime that does not leaves the created
    /// workload stopped until an explicit start, so custody covers the stopped container first.
    fn starts_on_create(&self) -> bool {
        false
    }

    /// Creates the workload: stopped, or running when the runtime starts on create.
    fn create<'a>(&'a self, workload: &'a WorkloadLaunch)
    -> RuntimeFuture<'a, WorkloadObservation>;

    fn inspect_process<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, DevelopmentProcessFacts>;
}

impl DevelopmentRuntime for dotcore_runtime_backends::docker::DockerRuntimeBackend {
    fn namespace_evidence_count<'a>(&'a self) -> RuntimeFuture<'a, usize> {
        Box::pin(self.development_namespace_evidence_count())
    }

    fn create<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.create_development_workload(workload))
    }

    fn inspect_process<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, DevelopmentProcessFacts> {
        Box::pin(async move {
            let process: dotcore_runtime_backends::docker::DockerDevelopmentProcess =
                self.inspect_development_process(workload_id).await?;
            Ok(DevelopmentProcessFacts {
                runtime_process_id: process.container_id,
                process_id: process.process_id,
                launch_fingerprint: process.launch_fingerprint,
                command: None,
                loopback_endpoints: process
                    .loopback_endpoints
                    .into_iter()
                    .map(|endpoint| DevelopmentLoopbackEndpoint {
                        port_name: endpoint.port_name,
                        host: LOOPBACK_HOST.to_owned(),
                        host_port: endpoint.host_port,
                    })
                    .collect(),
            })
        })
    }
}

impl DevelopmentRuntime for dotcore_runtime_backends::native::NativeRuntimeBackend {
    fn namespace_evidence_count<'a>(&'a self) -> RuntimeFuture<'a, usize> {
        Box::pin(self.workload_directory_count())
    }

    fn starts_on_create(&self) -> bool {
        true
    }

    fn create<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.create_workload(workload)
    }

    fn inspect_process<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, DevelopmentProcessFacts> {
        Box::pin(async move {
            let process = self.workload_process(workload_id).await?;
            Ok(DevelopmentProcessFacts {
                // A native workload's directory is its stable runtime identity while it exists.
                runtime_process_id: workload_id.0.clone(),
                process_id: process.process_id,
                launch_fingerprint: process.launch_fingerprint,
                command: Some(process.command),
                loopback_endpoints: process
                    .loopback_endpoints
                    .into_iter()
                    .map(|endpoint| DevelopmentLoopbackEndpoint {
                        port_name: endpoint.port_name,
                        host: LOOPBACK_HOST.to_owned(),
                        host_port: endpoint.host_port,
                    })
                    .collect(),
            })
        })
    }
}

/// Failures while launching one private transient development workload.
#[derive(Debug, thiserror::Error)]
pub enum DirectWorkloadLaunchError {
    #[error("direct development scope and owner identities must be safe lowercase identifiers")]
    InvalidDevelopmentIdentity,

    #[error(
        "direct development path is not an absolute normal path with only real directory ancestors"
    )]
    UnsafePath,

    #[error("direct development workloads require the Docker or native runtime")]
    InvalidRuntime,

    #[error(
        "direct development workloads must name individual accelerator devices, not every device"
    )]
    EveryAcceleratorDevice,

    #[error("could not load or validate direct development catalog: {0}")]
    Catalog(#[source] anyhow::Error),

    #[error("could not resolve direct development launch: {0}")]
    Resolution(#[source] anyhow::Error),

    #[error("could not connect direct development OCI preparation: {0}")]
    OciClient(#[source] anyhow::Error),

    #[error("could not prepare direct development model artifacts: {0}")]
    ModelArtifacts(#[source] anyhow::Error),

    #[error("could not prepare direct development engine artifact: {0}")]
    EngineArtifact(#[source] anyhow::Error),

    #[error("could not materialize direct development workload: {0}")]
    Materialization(#[source] anyhow::Error),

    #[error("direct development runtime operation failed: {0}")]
    Runtime(#[from] RuntimeBackendError),

    #[error("reserved direct development runtime namespace is not empty")]
    NamespaceNotEmpty,

    #[error("direct development runtime did not report a running workload")]
    NotRunning,

    #[error("direct development runtime facts do not match the launched workload")]
    IdentityMismatch,

    #[error("direct development ownership handle failed: {0}")]
    OwnershipHandle(#[from] OwnershipHandleError),
}

/// Failures while recovering or tearing down one exact development workload.
#[derive(Debug, thiserror::Error)]
pub enum DirectWorkloadTeardownError {
    #[error("direct development workloads require the Docker or native runtime")]
    InvalidRuntime,

    #[error("ownership handle does not belong to this reserved runtime namespace")]
    HandleNamespaceMismatch,

    #[error("Runtime ownership evidence is missing, foreign, duplicate, or ambiguous")]
    AmbiguousEvidence,

    #[error("Runtime ownership evidence changed during exact teardown")]
    AlteredEvidence,

    #[error("cleanup remains dirty and requires inspection: {0}")]
    Dirty(String),

    #[error("direct development runtime operation failed: {0}")]
    Runtime(#[from] RuntimeBackendError),

    #[error("direct development ownership handle failed: {0}")]
    OwnershipHandle(#[from] OwnershipHandleError),
}

fn live_process_identity(
    handle: &DevelopmentOwnershipHandle,
    observation: Option<(WorkloadObservation, ProvenDevelopmentProcess)>,
) -> Result<DevelopmentWorkloadProcessIdentity, DirectWorkloadTeardownError> {
    let Some((WorkloadObservation::Running(_), process)) = observation else {
        return Err(DirectWorkloadTeardownError::Dirty(
            "ownership handle has no live running runtime process".to_owned(),
        ));
    };
    if handle.state != DevelopmentOwnershipState::Running {
        return Err(DirectWorkloadTeardownError::Dirty(
            "ownership handle is not in the running state".to_owned(),
        ));
    }
    let process_id = process.process_id.ok_or_else(|| {
        DirectWorkloadTeardownError::Dirty(
            "owned runtime workload has no live process identity".to_owned(),
        )
    })?;
    Ok(DevelopmentWorkloadProcessIdentity {
        runtime_scope: handle.runtime_scope.clone(),
        runtime_backend_id: handle.runtime_backend_id.clone(),
        owner_id: handle.owner_id.clone(),
        generation: handle.generation,
        launch_fingerprint: handle.launch_fingerprint.clone(),
        workload_id: process.workload_id,
        runtime_process_id: process.runtime_process_id,
        process_id,
        launch_inputs: handle.launch_inputs.clone(),
    })
}

fn transition(
    transaction: &mut OwnershipHandleFile,
    state: DevelopmentOwnershipState,
    workload_id: Option<String>,
    runtime_process_id: Option<String>,
    process_id: Option<u32>,
) -> Result<(), OwnershipHandleError> {
    let mut next = transaction.handle().clone();
    next.state = state;
    next.workload_id = workload_id.or(next.workload_id);
    next.runtime_process_id = runtime_process_id.or(next.runtime_process_id);
    next.process_id = process_id.or(next.process_id);
    next.dirty_reason = None;
    transaction.replace(next)
}

fn mark_dirty(transaction: &mut OwnershipHandleFile, reason: &str) {
    let mut dirty = transaction.handle().clone();
    dirty.state = DevelopmentOwnershipState::Dirty;
    dirty.dirty_reason = Some(reason.to_owned());
    let _ = transaction.replace(dirty);
}

fn dirty_teardown(
    transaction: &mut OwnershipHandleFile,
    reason: &str,
) -> Result<DevelopmentOwnershipHandle, DirectWorkloadTeardownError> {
    mark_dirty(transaction, reason);
    Err(DirectWorkloadTeardownError::Dirty(reason.to_owned()))
}

fn exact_observed(observation: &WorkloadObservation) -> Option<&dotcore_runtime::ObservedWorkload> {
    match observation {
        WorkloadObservation::Starting(observed)
        | WorkloadObservation::Running(observed)
        | WorkloadObservation::Stopping(observed)
        | WorkloadObservation::Unavailable(observed)
        | WorkloadObservation::Absent(observed) => Some(observed),
        WorkloadObservation::Stopped { workload, .. } => Some(workload),
        WorkloadObservation::Unknown { .. } => None,
    }
}

fn matches_handle_identity(
    handle: &DevelopmentOwnershipHandle,
    observed: &dotcore_runtime::ObservedWorkload,
) -> bool {
    observed.backend_id.0 == handle.runtime_backend_id
        && observed.correlation.owner_id == handle.owner_id
        && observed.correlation.reconcile_generation == handle.generation
}

fn development_endpoints(
    observed: &dotcore_runtime::ObservedWorkload,
) -> Vec<DevelopmentLoopbackEndpoint> {
    observed
        .loopback_endpoints
        .iter()
        .map(|endpoint| DevelopmentLoopbackEndpoint {
            port_name: endpoint.port_name.clone(),
            host: LOOPBACK_HOST.to_owned(),
            host_port: endpoint.host_port,
        })
        .collect()
}

fn validate_operation_path(path: &Path) -> Result<(), DirectWorkloadLaunchError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(DirectWorkloadLaunchError::UnsafePath);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(DirectWorkloadLaunchError::UnsafePath);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(DirectWorkloadLaunchError::UnsafePath),
        }
    }
    Ok(())
}

fn validate_development_identity(value: &str) -> Result<(), DirectWorkloadLaunchError> {
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_lowercase()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(DirectWorkloadLaunchError::InvalidDevelopmentIdentity);
    }
    Ok(())
}

fn digest_prefix(domain: &[u8], values: &[&str], length: usize) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            digest.update([0]);
        }
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())[..length].to_owned()
}

impl DirectWorkloadLaunchError {
    fn catalog(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Catalog(anyhow::Error::new(error))
    }

    fn resolution(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Resolution(anyhow::Error::new(error))
    }

    fn oci_client(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::OciClient(anyhow::Error::new(error))
    }

    fn model_artifacts(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::ModelArtifacts(anyhow::Error::new(error))
    }

    fn engine_artifact(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::EngineArtifact(anyhow::Error::new(error))
    }

    fn materialization(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Materialization(anyhow::Error::new(error))
    }
}
