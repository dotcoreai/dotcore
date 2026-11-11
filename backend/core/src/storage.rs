//! Turns catalog storage bindings into the storage attachments and environment of one workload.
//!
//! Engines and applications declare writable storage the same way, and each owner only says which
//! scopes it can resolve and to which key. This module picks the namespace of each scope, so keys
//! of different scopes never share a folder, and fails instead of overriding an environment
//! variable the workload already sets.

use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StorageScope};
use dotcore_runtime::{WorkloadStorageAttachment, WorkloadStorageKey, WorkloadStorageKind};

/// The runtime namespace of storage scoped to one deployment.
pub(crate) const DEPLOYMENT_NAMESPACE: &str = "deployment";

/// Returns the storage one deployment-scoped binding of the given kind attaches for one owner key.
///
/// Launch attaches deployment-scoped bindings under this key and teardown removes it, so both
/// name the same folder.
pub(crate) fn deployment_storage(kind: WorkloadStorageKind, key: &str) -> WorkloadStorageKey {
    WorkloadStorageKey {
        kind,
        namespace: DEPLOYMENT_NAMESPACE.to_owned(),
        key: key.to_owned(),
    }
}

/// Returns the storage keys launch attaches for the deployment-scoped bindings among `bindings`
/// under one owner key, one per storage kind, so teardown removes exactly those folders.
pub(crate) fn deployment_storage_keys<'a>(
    bindings: impl IntoIterator<Item = &'a StorageBinding>,
    key: &str,
) -> Result<Vec<WorkloadStorageKey>, StorageBindingError> {
    Ok(bindings
        .into_iter()
        .filter(|binding| binding.scope == StorageScope::Deployment as i32)
        .map(storage_kind)
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?
        .into_iter()
        .map(|kind| deployment_storage(kind, key))
        .collect())
}

/// Storage attachments and the environment variables that point a workload into them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkloadStorage {
    pub(crate) attachments: Vec<WorkloadStorageAttachment>,
    pub(crate) environment: Vec<(String, String)>,
}

/// Why storage bindings cannot become attachments for one workload.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StorageBindingError {
    #[error("storage binding {binding:?} has no usable kind")]
    InvalidKind { binding: String },

    #[error("storage binding {binding:?} uses a scope its owner cannot resolve")]
    UnsupportedScope { binding: String },

    #[error(
        "storage binding {binding:?} sets environment variable {variable:?}, which the workload already sets"
    )]
    EnvironmentAlreadySet { binding: String, variable: String },
}

/// Maps each `(attachment name, binding)` pair to a storage attachment and its environment.
///
/// `scope_key` returns the owner's key for a scope, or `None` for a scope it cannot resolve;
/// `is_set` tells whether the workload already sets an environment variable.
pub(crate) fn workload_storage<'a>(
    bindings: impl IntoIterator<Item = (String, &'a StorageBinding)>,
    scope_key: impl Fn(StorageScope) -> Option<String>,
    is_set: impl Fn(&str) -> bool,
) -> Result<WorkloadStorage, StorageBindingError> {
    let mut storage = WorkloadStorage::default();
    for (name, binding) in bindings {
        let kind = storage_kind(binding)?;
        let unsupported = || StorageBindingError::UnsupportedScope {
            binding: binding.binding_name.clone(),
        };
        let scope = StorageScope::try_from(binding.scope).map_err(|_| unsupported())?;
        let namespace = scope_namespace(scope).ok_or_else(unsupported)?;
        let key = scope_key(scope).ok_or_else(unsupported)?;

        for variable in &binding.environment {
            if is_set(&variable.name) {
                return Err(StorageBindingError::EnvironmentAlreadySet {
                    binding: binding.binding_name.clone(),
                    variable: variable.name.clone(),
                });
            }
            let value = if variable.relative_path.is_empty() {
                binding.mount_path.clone()
            } else {
                format!("{}/{}", binding.mount_path, variable.relative_path)
            };
            storage.environment.push((variable.name.clone(), value));
        }

        storage.attachments.push(WorkloadStorageAttachment {
            name,
            destination: binding.mount_path.clone(),
            kind,
            namespace: namespace.to_owned(),
            key,
            read_only: false,
            expires_when_unused: scope != StorageScope::Deployment,
        });
    }
    Ok(storage)
}

/// Returns the runtime storage kind one binding attaches.
fn storage_kind(binding: &StorageBinding) -> Result<WorkloadStorageKind, StorageBindingError> {
    match StorageKind::try_from(binding.kind) {
        Ok(StorageKind::Cache) => Ok(WorkloadStorageKind::Cache),
        Ok(StorageKind::Data) => Ok(WorkloadStorageKind::Data),
        Ok(StorageKind::Unspecified) | Err(_) => Err(StorageBindingError::InvalidKind {
            binding: binding.binding_name.clone(),
        }),
    }
}

/// Returns the runtime namespace that keeps one scope's keys apart from every other scope's.
fn scope_namespace(scope: StorageScope) -> Option<&'static str> {
    match scope {
        StorageScope::Deployment => Some(DEPLOYMENT_NAMESPACE),
        StorageScope::Application => Some("application"),
        StorageScope::EngineArtifact => Some("engine-artifact"),
        StorageScope::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::StorageEnvironmentVariable;

    use super::*;

    #[test]
    fn maps_a_binding_to_its_attachment_and_environment() {
        let binding = binding(StorageScope::Deployment);

        let storage = workload_storage(
            [("engine-cache".to_owned(), &binding)],
            |scope| (scope == StorageScope::Deployment).then(|| "deployment-one".to_owned()),
            |_| false,
        )
        .expect("storage");

        assert_eq!(
            storage,
            WorkloadStorage {
                attachments: vec![WorkloadStorageAttachment {
                    name: "engine-cache".to_owned(),
                    destination: "/var/cache/engine".to_owned(),
                    kind: WorkloadStorageKind::Cache,
                    namespace: "deployment".to_owned(),
                    key: "deployment-one".to_owned(),
                    read_only: false,
                    expires_when_unused: false,
                }],
                environment: vec![
                    ("CACHE_HOME".to_owned(), "/var/cache/engine".to_owned()),
                    (
                        "TRITON_CACHE".to_owned(),
                        "/var/cache/engine/triton".to_owned()
                    ),
                ],
            }
        );
    }

    #[test]
    fn rejects_an_environment_variable_the_workload_already_sets() {
        let binding = binding(StorageScope::Deployment);

        let error = workload_storage(
            [("engine-cache".to_owned(), &binding)],
            |_| Some("deployment-one".to_owned()),
            |name| name == "TRITON_CACHE",
        )
        .expect_err("collision");

        assert_eq!(
            error.to_string(),
            "storage binding \"engine-cache\" sets environment variable \"TRITON_CACHE\", which the workload already sets"
        );
    }

    #[test]
    fn rejects_a_scope_the_owner_cannot_resolve() {
        for scope in [StorageScope::Application, StorageScope::Unspecified] {
            let binding = binding(scope);

            let error = workload_storage(
                [("engine-cache".to_owned(), &binding)],
                |scope| (scope == StorageScope::Deployment).then(|| "deployment-one".to_owned()),
                |_| false,
            )
            .expect_err("unsupported scope");

            assert!(
                matches!(error, StorageBindingError::UnsupportedScope { ref binding } if binding == "engine-cache"),
                "{error:?}"
            );
        }
    }

    fn binding(scope: StorageScope) -> StorageBinding {
        StorageBinding {
            binding_name: "engine-cache".to_owned(),
            mount_path: "/var/cache/engine".to_owned(),
            kind: StorageKind::Cache as i32,
            scope: scope as i32,
            environment: vec![
                StorageEnvironmentVariable {
                    name: "CACHE_HOME".to_owned(),
                    relative_path: String::new(),
                },
                StorageEnvironmentVariable {
                    name: "TRITON_CACHE".to_owned(),
                    relative_path: "triton".to_owned(),
                },
            ],
        }
    }
}
