//! Registers compiled runtime adapter factories for process composition and selects them by exact configuration keys.
//!
//! Model and application capabilities are independent, duplicate registrations fail, and a missing configured adapter never falls back to another implementation.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use dotcore_proto::dotcore::site::v1::ExecutionTarget;
use dotcore_runtime::{RuntimeBackend, WorkloadGroupBackend};

use super::CompositionError;

/// Process-local registry of compiled model and application runtime adapter factories.
#[derive(Default)]
pub(super) struct RuntimeAdapterRegistry {
    model_factories: BTreeMap<RuntimeAdapterKey, Arc<dyn ModelRuntimeAdapterFactory>>,
    application_factories: BTreeMap<RuntimeAdapterKey, Arc<dyn ApplicationRuntimeAdapterFactory>>,
}

impl RuntimeAdapterRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn register_model(
        &mut self,
        key: RuntimeAdapterKey,
        factory: Arc<dyn ModelRuntimeAdapterFactory>,
    ) -> Result<(), RuntimeAdapterRegistryError> {
        match self.model_factories.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(factory);
                Ok(())
            }
            Entry::Occupied(_) => Err(RuntimeAdapterRegistryError::DuplicateModel {
                adapter_key: key.as_str(),
            }),
        }
    }

    pub(super) fn register_application(
        &mut self,
        key: RuntimeAdapterKey,
        factory: Arc<dyn ApplicationRuntimeAdapterFactory>,
    ) -> Result<(), RuntimeAdapterRegistryError> {
        match self.application_factories.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(factory);
                Ok(())
            }
            Entry::Occupied(_) => Err(RuntimeAdapterRegistryError::DuplicateApplication {
                adapter_key: key.as_str(),
            }),
        }
    }

    pub(super) async fn model_backend(
        &self,
        key: RuntimeAdapterKey,
        target: &ExecutionTarget,
    ) -> Result<Box<dyn RuntimeBackend>, CompositionError> {
        let factory = self.model_factories.get(&key).ok_or_else(|| {
            CompositionError::composition_with_source(
                "Runtime adapter selection failed",
                RuntimeAdapterRegistryError::MissingModel {
                    execution_target_id: target.execution_target_id.clone(),
                    adapter_key: key.as_str(),
                },
            )
        })?;
        factory.build(target).await
    }

    pub(super) async fn application_backend(
        &self,
        key: RuntimeAdapterKey,
        target: &ExecutionTarget,
    ) -> Result<Box<dyn WorkloadGroupBackend>, CompositionError> {
        let factory = self.application_factories.get(&key).ok_or_else(|| {
            CompositionError::composition_with_source(
                "Runtime adapter selection failed",
                RuntimeAdapterRegistryError::MissingApplication {
                    execution_target_id: target.execution_target_id.clone(),
                    adapter_key: key.as_str(),
                },
            )
        })?;
        factory.build(target).await
    }
}

/// Stable process-local key for one compiled runtime adapter implementation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct RuntimeAdapterKey(&'static str);

impl RuntimeAdapterKey {
    pub(super) const fn new(value: &'static str) -> Self {
        Self(value)
    }

    pub(super) const fn as_str(self) -> &'static str {
        self.0
    }
}

pub(super) type ModelRuntimeAdapterFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn RuntimeBackend>, CompositionError>> + Send + 'a>>;

pub(super) trait ModelRuntimeAdapterFactory: Send + Sync {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a>;
}

pub(super) type ApplicationRuntimeAdapterFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Box<dyn WorkloadGroupBackend>, CompositionError>> + Send + 'a>,
>;

pub(super) trait ApplicationRuntimeAdapterFactory: Send + Sync {
    fn build<'a>(&'a self, target: &'a ExecutionTarget) -> ApplicationRuntimeAdapterFuture<'a>;
}

#[derive(Debug, thiserror::Error)]
pub(super) enum RuntimeAdapterRegistryError {
    #[error("duplicate model runtime adapter registration for {adapter_key:?}")]
    DuplicateModel { adapter_key: &'static str },

    #[error("duplicate application runtime adapter registration for {adapter_key:?}")]
    DuplicateApplication { adapter_key: &'static str },

    #[error(
        "execution target {execution_target_id:?} selects unregistered model runtime adapter {adapter_key:?}"
    )]
    MissingModel {
        execution_target_id: String,
        adapter_key: &'static str,
    },

    #[error(
        "execution target {execution_target_id:?} selects unregistered application runtime adapter {adapter_key:?}"
    )]
    MissingApplication {
        execution_target_id: String,
        adapter_key: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use dotcore_runtime::{
        RuntimeBackendId, RuntimeFuture, WorkloadGroupLaunch, WorkloadGroupObservation, WorkloadId,
        WorkloadLaunch, WorkloadObservation, WorkloadStorageKey,
    };

    use super::*;

    const THIRD: RuntimeAdapterKey = RuntimeAdapterKey::new("test/third-runtime");
    const OTHER: RuntimeAdapterKey = RuntimeAdapterKey::new("test/other-runtime");

    #[tokio::test]
    async fn duplicate_registrations_fail_without_replacing_the_first_factory() {
        let mut registry = RuntimeAdapterRegistry::new();
        registry
            .register_model(THIRD, Arc::new(TestModelFactory))
            .expect("first model registration");
        registry
            .register_application(THIRD, Arc::new(TestApplicationFactory))
            .expect("first application registration");

        assert!(matches!(
            registry.register_model(THIRD, Arc::new(PanicModelFactory)),
            Err(RuntimeAdapterRegistryError::DuplicateModel {
                adapter_key: "test/third-runtime"
            })
        ));
        assert!(matches!(
            registry.register_application(THIRD, Arc::new(PanicApplicationFactory)),
            Err(RuntimeAdapterRegistryError::DuplicateApplication {
                adapter_key: "test/third-runtime"
            })
        ));

        let model = registry
            .model_backend(
                THIRD,
                &ExecutionTarget {
                    execution_target_id: "third-target".to_owned(),
                    site_node_id: "test-node".to_owned(),
                    ..Default::default()
                },
            )
            .await
            .expect("original model registration");
        assert_eq!(model.backend_id().0, "test/third-model");
        let application = registry
            .application_backend(THIRD, &target("third-target"))
            .await
            .expect("original application registration");
        assert_eq!(application.backend_id().0, "test/third-application");
    }

    #[tokio::test]
    async fn exact_model_and_application_registrations_select_independently() {
        let mut registry = RuntimeAdapterRegistry::new();
        registry
            .register_model(THIRD, Arc::new(TestModelFactory))
            .expect("Model registration");

        let model = registry
            .model_backend(
                THIRD,
                &ExecutionTarget {
                    execution_target_id: "third-target".to_owned(),
                    site_node_id: "test-node".to_owned(),
                    ..Default::default()
                },
            )
            .await
            .expect("registered model adapter");
        assert_eq!(model.backend_id().0, "test/third-model");

        let application_error = match registry
            .application_backend(THIRD, &target("third-target"))
            .await
        {
            Ok(_) => panic!("Model-only registration supplied application capability"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{:#}", anyhow::Error::from(application_error)),
            "Backend composition failed: Runtime adapter selection failed: execution target \"third-target\" selects unregistered application runtime adapter \"test/third-runtime\""
        );

        registry
            .register_application(THIRD, Arc::new(TestApplicationFactory))
            .expect("Application registration");
        let application = registry
            .application_backend(THIRD, &target("third-target"))
            .await
            .expect("registered application adapter");
        assert_eq!(application.backend_id().0, "test/third-application");
    }

    #[tokio::test]
    async fn missing_model_registration_names_the_target_and_adapter_without_fallback() {
        let mut registry = RuntimeAdapterRegistry::new();
        registry
            .register_model(OTHER, Arc::new(PanicModelFactory))
            .expect("unrelated model registration");
        let error = match registry
            .model_backend(
                THIRD,
                &ExecutionTarget {
                    execution_target_id: "third-target".to_owned(),
                    site_node_id: "test-node".to_owned(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => panic!("unregistered model adapter was selected"),
            Err(error) => error,
        };

        assert_eq!(
            format!("{:#}", anyhow::Error::from(error)),
            "Backend composition failed: Runtime adapter selection failed: execution target \"third-target\" selects unregistered model runtime adapter \"test/third-runtime\""
        );
    }

    #[tokio::test]
    async fn application_only_registration_does_not_supply_model_capability() {
        let mut registry = RuntimeAdapterRegistry::new();
        registry
            .register_application(THIRD, Arc::new(TestApplicationFactory))
            .expect("Application registration");

        let application = registry
            .application_backend(THIRD, &target("third-target"))
            .await
            .expect("registered application adapter");
        assert_eq!(application.backend_id().0, "test/third-application");

        let model_error = match registry
            .model_backend(
                THIRD,
                &ExecutionTarget {
                    execution_target_id: "third-target".to_owned(),
                    site_node_id: "test-node".to_owned(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => panic!("Application-only registration supplied model capability"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{:#}", anyhow::Error::from(model_error)),
            "Backend composition failed: Runtime adapter selection failed: execution target \"third-target\" selects unregistered model runtime adapter \"test/third-runtime\""
        );
    }

    fn target(execution_target_id: &str) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: execution_target_id.to_owned(),
            site_node_id: "test-node".to_owned(),
            ..Default::default()
        }
    }

    struct TestModelFactory;

    impl ModelRuntimeAdapterFactory for TestModelFactory {
        fn build<'a>(&'a self, _: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a> {
            Box::pin(async { Ok(Box::new(TestModelBackend) as Box<dyn RuntimeBackend>) })
        }
    }

    struct TestApplicationFactory;

    impl ApplicationRuntimeAdapterFactory for TestApplicationFactory {
        fn build<'a>(&'a self, _: &'a ExecutionTarget) -> ApplicationRuntimeAdapterFuture<'a> {
            Box::pin(async {
                Ok(Box::new(TestApplicationBackend) as Box<dyn WorkloadGroupBackend>)
            })
        }
    }

    struct PanicModelFactory;

    impl ModelRuntimeAdapterFactory for PanicModelFactory {
        fn build<'a>(&'a self, _: &'a ExecutionTarget) -> ModelRuntimeAdapterFuture<'a> {
            Box::pin(async { panic!("duplicate model registration replaced the original") })
        }
    }

    struct PanicApplicationFactory;

    impl ApplicationRuntimeAdapterFactory for PanicApplicationFactory {
        fn build<'a>(&'a self, _: &'a ExecutionTarget) -> ApplicationRuntimeAdapterFuture<'a> {
            Box::pin(async { panic!("duplicate application registration replaced the original") })
        }
    }

    #[derive(Debug)]
    struct TestModelBackend;

    impl RuntimeBackend for TestModelBackend {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("test/third-model".to_owned())
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            unused_runtime_operation()
        }

        fn create_workload<'a>(
            &'a self,
            _: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            unused_runtime_operation()
        }

        fn start_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            unused_runtime_operation()
        }

        fn inspect_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            unused_runtime_operation()
        }

        fn stop_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            unused_runtime_operation()
        }

        fn remove_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            unused_runtime_operation()
        }

        fn remove_storage<'a>(
            &'a self,
            _: &'a dotcore_runtime::WorkloadStorageKey,
        ) -> RuntimeFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    #[derive(Debug)]
    struct TestApplicationBackend;

    impl WorkloadGroupBackend for TestApplicationBackend {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("test/third-application".to_owned())
        }

        fn execution_platform<'a>(
            &'a self,
            _: &'a str,
        ) -> RuntimeFuture<'a, dotcore_domain::ExecutionPlatform> {
            Box::pin(async { Ok(dotcore_domain::ExecutionPlatform::linux_amd64()) })
        }

        fn discover_workload_groups<'a>(
            &'a self,
        ) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
            unused_runtime_operation()
        }

        fn storage_exists<'a>(&'a self, _: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
            unused_runtime_operation()
        }

        fn create_workload_group<'a>(
            &'a self,
            _: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
            unused_runtime_operation()
        }

        fn inspect_workload_group<'a>(
            &'a self,
            _: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
            unused_runtime_operation()
        }

        fn remove_workload_group<'a>(
            &'a self,
            _: &'a WorkloadGroupLaunch,
        ) -> RuntimeFuture<'a, ()> {
            unused_runtime_operation()
        }
    }

    fn unused_runtime_operation<'a, T>() -> RuntimeFuture<'a, T> {
        Box::pin(async { panic!("registry selection tests do not execute runtime operations") })
    }
}
