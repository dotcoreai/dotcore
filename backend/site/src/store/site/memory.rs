//! In-memory site store for tests and non-durable operation, with the same envelope and size
//! rules as the durable store.

use std::sync::{Arc, Mutex};

use dotcore_proto::dotcore::site::store::v1::SiteCurrentState;

use crate::store::{
    CurrentStateStore, StoreError, new_envelope, validate_encoded_size, validate_envelope,
};

/// Site-local in-memory owner snapshot shared by every clone.
#[derive(Clone, Debug)]
pub struct InMemoryCurrentState {
    state: Arc<Mutex<SiteCurrentState>>,
}

impl InMemoryCurrentState {
    pub fn new(site_id: impl Into<String>) -> Result<Self, StoreError> {
        let state = new_envelope(site_id.into())?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SiteCurrentState>, StoreError> {
        self.state.lock().map_err(|_| StoreError::Unavailable)
    }
}

impl CurrentStateStore for InMemoryCurrentState {
    type State = SiteCurrentState;

    fn read<T>(&self, read: impl FnOnce(&SiteCurrentState) -> T) -> Result<T, StoreError> {
        Ok(read(&*self.lock()?))
    }

    /// Applies one owner's mutation to a candidate copy and keeps it once its envelope and size
    /// are valid.
    fn transact<T, E: From<StoreError>>(
        &self,
        operation: impl FnOnce(&mut SiteCurrentState) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut state = self.lock()?;
        let mut candidate = state.clone();
        let result = operation(&mut candidate)?;
        validate_envelope(&candidate, &candidate.site_id)?;
        validate_encoded_size(&candidate)?;
        *state = candidate;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentState, ApplicationLifecycleState, ApplicationReadinessState,
            CurrentApplicationDeployment,
        },
        model::v1::{CurrentModelDeployment, ModelDeploymentState, ModelDeploymentStatus},
    };

    use super::InMemoryCurrentState;
    use crate::{
        store::StoreError,
        test_support::{stored, write_application_record, write_model_record},
    };

    #[test]
    fn oversized_updates_preserve_in_memory_state_and_revisions() {
        let state = InMemoryCurrentState::new("site-one").expect("valid site identity");
        write_model_record(&state, deployment("model-one")).expect("model write");
        let model_before = stored(&state).expect("model snapshot");
        assert!(matches!(
            write_model_record(&state, oversized_deployment("model-two")),
            Err(StoreError::LimitExceeded)
        ));
        assert_eq!(stored(&state).expect("model unchanged"), model_before);

        write_application_record(&state, application_deployment("application-one"))
            .expect("application write");
        let application_before = stored(&state).expect("application snapshot");
        assert!(matches!(
            write_application_record(&state, oversized_application_deployment("application-two")),
            Err(StoreError::LimitExceeded)
        ));
        assert_eq!(
            stored(&state).expect("application unchanged"),
            application_before
        );
    }

    #[test]
    fn current_state_rejects_a_noncanonical_site_id() {
        assert!(matches!(
            InMemoryCurrentState::new("Site One"),
            Err(StoreError::InvalidState)
        ));
    }

    fn oversized_deployment(id: &str) -> CurrentModelDeployment {
        let mut deployment = deployment(id);
        deployment.state.as_mut().expect("state").status_message = "x".repeat(4 * 1024 * 1024);
        deployment
    }

    fn oversized_application_deployment(id: &str) -> CurrentApplicationDeployment {
        let mut deployment = application_deployment(id);
        deployment.state.as_mut().expect("state").stage = "x".repeat(4 * 1024 * 1024);
        deployment
    }

    fn application_deployment(id: &str) -> CurrentApplicationDeployment {
        CurrentApplicationDeployment {
            application_deployment_id: id.to_owned(),
            record_revision: 0,
            state: Some(ApplicationDeploymentState {
                lifecycle_state: ApplicationLifecycleState::Starting as i32,
                readiness_state: ApplicationReadinessState::NotReady as i32,
                stage: "starting".to_owned(),
                ..Default::default()
            }),
            reconciled_lifecycle_generation: None,
        }
    }

    fn deployment(id: &str) -> CurrentModelDeployment {
        CurrentModelDeployment {
            model_deployment_id: id.to_owned(),
            reconciled_lifecycle_generation: None,
            record_revision: 0,
            recovery_barrier: None,
            state: Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Loading as i32,
                status_message: "Loading".to_owned(),
                ..Default::default()
            }),
        }
    }
}
