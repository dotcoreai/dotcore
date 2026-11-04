//! Test support for the site unit tests and, through the `test-support` feature, for the
//! unit tests of the crates above: owner-neutral fakes and fixtures that keep tests independent of
//! the model and application owners.
//!
//! The owner rules accept every model deployment declaration, so site tests exercise site
//! structure only; owner rule rejections are tested by their owners. Store tests write records
//! through the store's own transaction rather than an owner repository.

#[cfg(test)]
use dotcore_proto::dotcore::{
    application::v1::CurrentApplicationDeployment, model::v1::CurrentModelDeployment,
};

pub mod private_tls;

#[cfg(test)]
use crate::store::{CurrentStateStore, StoreError, StoredCurrentState};
use crate::validation::SiteDeclarationRules;

/// Owner rules that accept every local model deployment declaration.
pub fn site_declaration_rules() -> SiteDeclarationRules {
    SiteDeclarationRules {
        validate_local_model_declaration: |_| Ok(()),
    }
}

/// The backends' site cleanup set, for site tests that reset, delete or leave a site. Its cache
/// root holds no caches.
#[cfg(test)]
pub fn site_runtime_cleanups() -> std::sync::Arc<dyn dotcore_runtime::SiteRuntimeCleanups> {
    site_runtime_cleanups_with_cache_root(std::path::Path::new("/nonexistent/dotcore-cache"))
}

/// The backends' site cleanup set over the recreatable caches below `cache_root`.
#[cfg(test)]
pub fn site_runtime_cleanups_with_cache_root(
    cache_root: &std::path::Path,
) -> std::sync::Arc<dyn dotcore_runtime::SiteRuntimeCleanups> {
    std::sync::Arc::new(
        dotcore_runtime_backends::registry::BackendSiteRuntimeCleanups::new(cache_root),
    )
}

/// Appends model records through the store's own transaction and advances its storage revision,
/// so store tests write records without an owner repository.
#[cfg(test)]
pub fn write_model_records<S: CurrentStateStore>(
    store: &S,
    records: Vec<CurrentModelDeployment>,
) -> Result<(), StoreError> {
    store.transact(|state| {
        state.model_deployments_mut().extend(records);
        *state.storage_revision_mut() = StoreError::next_revision(state.storage_revision())?;
        Ok(())
    })
}

/// Appends one model record through the store's own transaction.
#[cfg(test)]
pub fn write_model_record<S: CurrentStateStore>(
    store: &S,
    record: CurrentModelDeployment,
) -> Result<(), StoreError> {
    write_model_records(store, vec![record])
}

/// Appends one application record through the store's own transaction.
#[cfg(test)]
pub fn write_application_record<S: CurrentStateStore>(
    store: &S,
    record: CurrentApplicationDeployment,
) -> Result<(), StoreError> {
    store.transact(|state| {
        state.application_deployments_mut().push(record);
        *state.storage_revision_mut() = StoreError::next_revision(state.storage_revision())?;
        Ok(())
    })
}

/// Returns a copy of the store's committed state.
#[cfg(test)]
pub fn stored<S: CurrentStateStore>(store: &S) -> Result<S::State, StoreError> {
    store.read(Clone::clone)
}

/// Model deployment identity allocator for sites built by tests; every valid site declares one.
pub fn identity_allocator() -> dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
    dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
        namespace: "0123456789abcdef".to_owned(),
        next_sequence: 1,
    }
}

/// Initial site inputs for a simulated inference and application site.
#[cfg(test)]
pub fn initial_site_inputs() -> crate::initial::inputs::InitialSiteInputs {
    initial_site_inputs_for(&crate::initial::request::InitialSiteRequest::new(
        [crate::initial::request::InitialInferenceRuntime::Simulated],
        [crate::initial::request::InitialApplicationRuntime::Simulated],
    ))
}

/// Composes the initial site inputs `request` asks for, with fixed fixture identities.
#[cfg(test)]
pub fn initial_site_inputs_for(
    request: &crate::initial::request::InitialSiteRequest,
) -> crate::initial::inputs::InitialSiteInputs {
    let identities = request
        .resolve_identities(&mut InitialSiteIdentityGenerator::default())
        .expect("valid fixture identities");
    crate::initial::inputs::InitialSiteInputs::compose(
        request,
        identities,
        &crate::initial::probe::InitialSiteProbeReport::default(),
    )
    .expect("valid initial site inputs")
}

#[cfg(test)]
#[derive(Default)]
struct InitialSiteIdentityGenerator {
    next: u64,
}

#[cfg(test)]
impl crate::initial::request::InitialSiteIdentityGenerator for InitialSiteIdentityGenerator {
    fn generate(
        &mut self,
        _kind: crate::initial::request::InitialSiteIdentityKind,
    ) -> anyhow::Result<u64> {
        self.next += 1;
        Ok(self.next)
    }
}
