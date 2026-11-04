//! Docker runtime configuration validation and backend construction.

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use dotcore_domain::{AcceleratorDeviceSelector, AcceleratorSelectionError, SiteNodeId};
use dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter;
use dotcore_runtime::{RuntimeBackend, RuntimeBackendError, WorkloadGroupBackend};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::adapter::DockerRuntimeBackend;

/// Site-local Docker settings that do not enter workload owner contracts.
#[derive(Clone, Debug)]
pub struct DockerRuntimeConfig {
    pub site_id: String,
    pub execution_target_id: String,
    pub approved_mount_root: PathBuf,
    pub application_data_root: PathBuf,
    pub workload_cache_root: PathBuf,
    pub accelerator_transport: DockerAcceleratorTransport,
    #[allow(
        dead_code,
        reason = "bounded Docker log acquisition consumes this shared state in its dedicated adapter task"
    )]
    pub source_stream_permits: DockerLogSourcePermits,
}

/// Composition-shared admission state for active direct-Docker log sources.
#[derive(Clone, Debug)]
#[allow(
    dead_code,
    reason = "bounded Docker log acquisition uses the shared semaphore in its dedicated adapter task"
)]
pub struct DockerLogSourcePermits(Arc<Semaphore>);

impl DockerLogSourcePermits {
    pub fn new() -> Self {
        Self(Arc::new(Semaphore::new(32)))
    }

    /// Acquires one site-wide Docker source-stream slot without waiting.
    #[allow(
        dead_code,
        reason = "bounded Docker log acquisition calls this admission operation in its dedicated adapter task"
    )]
    pub fn try_acquire(&self) -> Result<OwnedSemaphorePermit, RuntimeBackendError> {
        self.0
            .clone()
            .try_acquire_owned()
            .map_err(|_| RuntimeBackendError::LogSourceLimitReached)
    }
}

/// Docker accelerator request transports supported by the site configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockerAcceleratorTransport {
    Cdi,

    LegacyNvidia,
}

/// Invalid adapter-specific Docker site configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DockerRuntimeConfigurationError {
    #[error("requires CDI or Legacy NVIDIA transport")]
    InvalidAcceleratorTransport,

    #[error("has accelerator IDs invalid for its selected transport")]
    InvalidAcceleratorIdentifiers,
}

pub type DockerBackendFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RuntimeBackendError>> + Send + 'a>>;

/// Construction-only port for the Docker backends selected by composition.
pub trait DockerBackendFactory: Send + Sync {
    fn model_backend(
        &self,
        accelerator_transport: DockerAcceleratorTransport,
        managed_node_id: SiteNodeId,
        execution_target_id: String,
    ) -> DockerBackendFuture<'_, Box<dyn RuntimeBackend>>;

    fn application_backend(
        &self,
        accelerator_transport: DockerAcceleratorTransport,
        execution_target_id: String,
    ) -> DockerBackendFuture<'_, Box<dyn WorkloadGroupBackend>>;
}

/// Creates local Docker backends from already selected and validated configuration.
pub struct DockerRuntimeFactory {
    site_id: String,
    approved_mount_root: PathBuf,
    application_data_root: PathBuf,
    workload_cache_root: PathBuf,
    source_stream_permits: DockerLogSourcePermits,
}

impl DockerRuntimeFactory {
    pub fn new(
        site_id: impl Into<String>,
        approved_mount_root: PathBuf,
        application_data_root: PathBuf,
        workload_cache_root: PathBuf,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            approved_mount_root,
            application_data_root,
            workload_cache_root,
            source_stream_permits: DockerLogSourcePermits::new(),
        }
    }

    /// Returns the site-wide direct-Docker source-stream admission state.
    pub fn source_stream_permits(&self) -> DockerLogSourcePermits {
        self.source_stream_permits.clone()
    }

    async fn connect(
        &self,
        accelerator_transport: DockerAcceleratorTransport,
        managed_node_id: Option<SiteNodeId>,
        execution_target_id: String,
    ) -> Result<DockerRuntimeBackend, RuntimeBackendError> {
        std::fs::create_dir_all(&self.application_data_root).map_err(|error| {
            RuntimeBackendError::docker_with_source(
                "Application site-data root creation failed",
                error,
            )
        })?;
        std::fs::create_dir_all(&self.workload_cache_root).map_err(|error| {
            RuntimeBackendError::docker_with_source("Workload cache root creation failed", error)
        })?;
        let config = DockerRuntimeConfig {
            site_id: self.site_id.clone(),
            execution_target_id,
            approved_mount_root: self.approved_mount_root.clone(),
            application_data_root: self.application_data_root.clone(),
            workload_cache_root: self.workload_cache_root.clone(),
            accelerator_transport,
            source_stream_permits: self.source_stream_permits(),
        };
        match managed_node_id {
            Some(managed_node_id) => {
                DockerRuntimeBackend::connect_managed(config, managed_node_id).await
            }
            None => DockerRuntimeBackend::connect(config).await,
        }
    }
}

impl DockerBackendFactory for DockerRuntimeFactory {
    fn model_backend(
        &self,
        accelerator_transport: DockerAcceleratorTransport,
        managed_node_id: SiteNodeId,
        execution_target_id: String,
    ) -> DockerBackendFuture<'_, Box<dyn RuntimeBackend>> {
        Box::pin(async move {
            Ok(Box::new(
                self.connect(
                    accelerator_transport,
                    Some(managed_node_id),
                    execution_target_id,
                )
                .await?,
            ) as Box<dyn RuntimeBackend>)
        })
    }

    fn application_backend(
        &self,
        accelerator_transport: DockerAcceleratorTransport,
        execution_target_id: String,
    ) -> DockerBackendFuture<'_, Box<dyn WorkloadGroupBackend>> {
        Box::pin(async move {
            Ok(Box::new(
                self.connect(accelerator_transport, None, execution_target_id)
                    .await?,
            ) as Box<dyn WorkloadGroupBackend>)
        })
    }
}

/// Validates and converts one Docker execution-target transport and its accelerator identifiers.
pub fn validated_accelerator_transport(
    settings: &DockerRuntimeAdapter,
    accelerator_device_ids: &[String],
) -> Result<DockerAcceleratorTransport, DockerRuntimeConfigurationError> {
    match AcceleratorDeviceSelector::for_docker_transport(
        settings.accelerator_transport,
        accelerator_device_ids,
    ) {
        Ok(AcceleratorDeviceSelector::Cdi) => Ok(DockerAcceleratorTransport::Cdi),
        Ok(AcceleratorDeviceSelector::NvidiaIndex) => Ok(DockerAcceleratorTransport::LegacyNvidia),
        Err(AcceleratorSelectionError::InvalidTransport) => {
            Err(DockerRuntimeConfigurationError::InvalidAcceleratorTransport)
        }
        Err(AcceleratorSelectionError::InvalidIdentifiers) => {
            Err(DockerRuntimeConfigurationError::InvalidAcceleratorIdentifiers)
        }
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::site::v1::{
        DockerAcceleratorTransport as ProtoDockerAcceleratorTransport, DockerRuntimeAdapter,
    };

    use super::*;

    #[test]
    fn validates_transport_specific_identifiers_with_actionable_errors() {
        for (transport, identifier, expected) in [
            (
                ProtoDockerAcceleratorTransport::Cdi,
                "0",
                DockerRuntimeConfigurationError::InvalidAcceleratorIdentifiers,
            ),
            (
                ProtoDockerAcceleratorTransport::LegacyNvidia,
                "nvidia.com/gpu=0",
                DockerRuntimeConfigurationError::InvalidAcceleratorIdentifiers,
            ),
            (
                ProtoDockerAcceleratorTransport::Cdi,
                "nvidia.com/gpu",
                DockerRuntimeConfigurationError::InvalidAcceleratorIdentifiers,
            ),
            (
                ProtoDockerAcceleratorTransport::Unspecified,
                "nvidia.com/gpu=0",
                DockerRuntimeConfigurationError::InvalidAcceleratorTransport,
            ),
        ] {
            let error = validated_accelerator_transport(
                &DockerRuntimeAdapter {
                    accelerator_transport: transport as i32,
                },
                &[identifier.to_owned()],
            )
            .expect_err("invalid Docker configuration");
            assert_eq!(error, expected);
            assert_eq!(error.to_string(), expected.to_string());
        }
    }

    #[test]
    fn source_stream_permits_are_shared_across_model_and_application_backends() {
        let factory = DockerRuntimeFactory::new(
            "site-one",
            PathBuf::from("/artifacts"),
            PathBuf::from("/applications"),
            PathBuf::from("/workload-cache"),
        );
        let permits = factory.source_stream_permits();
        let mut held = Vec::new();

        for _ in 0..32 {
            held.push(permits.try_acquire().expect("available source permit"));
        }

        assert!(permits.try_acquire().is_err());
        drop(held.pop());
        assert!(permits.try_acquire().is_ok());
    }

    #[test]
    fn construction_boundary_contains_no_capability_or_selection_policy() {
        let source = include_str!("configuration.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");
        for forbidden in [
            ["capability", "probe"].concat(),
            ["nvidia", "-ctk"].concat(),
            ["fallback", "selection"].concat(),
            ["placement", "choice"].concat(),
        ] {
            assert!(!source.contains(&forbidden), "factory contains {forbidden}");
        }
    }

    #[test]
    fn factory_boundary_accepts_exact_target_and_typed_managed_node_scope() {
        type ModelBackendFactoryMethod =
            for<'a> fn(
                &'a DockerRuntimeFactory,
                DockerAcceleratorTransport,
                SiteNodeId,
                String,
            ) -> DockerBackendFuture<'a, Box<dyn RuntimeBackend>>;

        let _: ModelBackendFactoryMethod =
            <DockerRuntimeFactory as DockerBackendFactory>::model_backend;
    }
}
