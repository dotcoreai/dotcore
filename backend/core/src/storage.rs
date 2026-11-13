//! Turns catalog storage bindings into the storage attachments and environment of one workload.
//!
//! Engines and applications declare writable storage the same way. An application deployment
//! chooses each binding's placement at creation; an engine binding has one fixed placement. Each
//! owner only says which placements it can resolve and to which key. This module picks the
//! namespace of each placement, so keys of different placements never share a folder, and fails
//! instead of overriding an environment variable the workload already sets.

use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StoragePlacement};
use dotcore_runtime::{WorkloadStorageAttachment, WorkloadStorageKey, WorkloadStorageKind};

/// The runtime namespace of storage placed with one deployment.
pub(crate) const DEPLOYMENT_NAMESPACE: &str = "deployment";

/// Returns the storage one deployment-placed binding of the given kind attaches for one owner key.
///
/// Launch attaches deployment-placed bindings under this key and teardown removes it, so both
/// name the same folder.
pub(crate) fn deployment_storage(kind: WorkloadStorageKind, key: &str) -> WorkloadStorageKey {
    WorkloadStorageKey {
        kind,
        namespace: DEPLOYMENT_NAMESPACE.to_owned(),
        key: key.to_owned(),
    }
}

/// Returns the storage keys launch attaches for the deployment-placed engine bindings among
/// `bindings` under one owner key, one per storage kind, so teardown removes exactly those folders.
pub(crate) fn deployment_storage_keys<'a>(
    bindings: impl IntoIterator<Item = &'a StorageBinding>,
    key: &str,
) -> Result<Vec<WorkloadStorageKey>, StorageBindingError> {
    let mut kinds = std::collections::BTreeSet::new();
    for binding in bindings {
        if engine_placement(binding)? == StoragePlacement::Deployment {
            kinds.insert(storage_kind(binding)?);
        }
    }
    Ok(kinds
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

    #[error("storage binding {binding:?} uses a placement its owner cannot resolve")]
    UnsupportedPlacement { binding: String },

    #[error("engine storage binding {binding:?} does not declare exactly one placement")]
    EnginePlacement { binding: String },

    #[error(
        "storage binding {binding:?} sets environment variable {variable:?}, which the workload already sets"
    )]
    EnvironmentAlreadySet { binding: String, variable: String },
}

/// Maps each `(attachment name, binding, placement)` triple to a storage attachment and its
/// environment.
///
/// `placement_key` returns the owner's key for a placement, or `None` for a placement it cannot
/// resolve; `is_set` tells whether the workload already sets an environment variable.
pub(crate) fn workload_storage<'a>(
    bindings: impl IntoIterator<Item = (String, &'a StorageBinding, StoragePlacement)>,
    placement_key: impl Fn(StoragePlacement) -> Option<String>,
    is_set: impl Fn(&str) -> bool,
) -> Result<WorkloadStorage, StorageBindingError> {
    let mut storage = WorkloadStorage::default();
    for (name, binding, placement) in bindings {
        let kind = storage_kind(binding)?;
        let unsupported = || StorageBindingError::UnsupportedPlacement {
            binding: binding.binding_name.clone(),
        };
        let namespace = placement_namespace(placement).ok_or_else(unsupported)?;
        let key = placement_key(placement).ok_or_else(unsupported)?;

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
            expires_when_unused: placement != StoragePlacement::Deployment,
        });
    }
    Ok(storage)
}

/// Returns the one placement an engine storage binding declares, which every deployment of the
/// engine uses.
pub(crate) fn engine_placement(
    binding: &StorageBinding,
) -> Result<StoragePlacement, StorageBindingError> {
    match binding.placements.as_slice() {
        [placement] => StoragePlacement::try_from(*placement).map_err(|_| {
            StorageBindingError::UnsupportedPlacement {
                binding: binding.binding_name.clone(),
            }
        }),
        _ => Err(StorageBindingError::EnginePlacement {
            binding: binding.binding_name.clone(),
        }),
    }
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

/// Returns the runtime namespace that keeps one placement's keys apart from every other
/// placement's.
fn placement_namespace(placement: StoragePlacement) -> Option<&'static str> {
    match placement {
        StoragePlacement::Deployment => Some(DEPLOYMENT_NAMESPACE),
        StoragePlacement::Application => Some("application"),
        StoragePlacement::EngineArtifact => Some("engine-artifact"),
        StoragePlacement::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::StorageEnvironmentVariable;

    use super::*;

    #[test]
    fn maps_a_binding_to_its_attachment_and_environment() {
        let binding = binding(StoragePlacement::Deployment);

        let storage = workload_storage(
            [(
                "engine-cache".to_owned(),
                &binding,
                StoragePlacement::Deployment,
            )],
            |placement| {
                (placement == StoragePlacement::Deployment).then(|| "deployment-one".to_owned())
            },
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
        let binding = binding(StoragePlacement::Deployment);

        let error = workload_storage(
            [(
                "engine-cache".to_owned(),
                &binding,
                StoragePlacement::Deployment,
            )],
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
    fn maps_each_application_placement_to_its_namespace_key_and_lifetime() {
        let binding = binding(StoragePlacement::Application);
        let attach = |placement| {
            workload_storage(
                [("web-models".to_owned(), &binding, placement)],
                |placement| match placement {
                    StoragePlacement::Deployment => Some("deployment-one".to_owned()),
                    StoragePlacement::Application => Some("comfyui".to_owned()),
                    _ => None,
                },
                |_| false,
            )
            .expect("storage")
            .attachments
            .into_iter()
            .map(|attachment| {
                (
                    attachment.namespace,
                    attachment.key,
                    attachment.expires_when_unused,
                )
            })
            .collect::<Vec<_>>()
        };

        assert_eq!(
            attach(StoragePlacement::Deployment),
            [("deployment".to_owned(), "deployment-one".to_owned(), false)]
        );
        assert_eq!(
            attach(StoragePlacement::Application),
            [("application".to_owned(), "comfyui".to_owned(), true)]
        );
    }

    #[test]
    fn rejects_a_placement_the_owner_cannot_resolve() {
        let binding = binding(StoragePlacement::Deployment);
        for placement in [StoragePlacement::Application, StoragePlacement::Unspecified] {
            let error = workload_storage(
                [("engine-cache".to_owned(), &binding, placement)],
                |placement| {
                    (placement == StoragePlacement::Deployment).then(|| "deployment-one".to_owned())
                },
                |_| false,
            )
            .expect_err("unsupported placement");

            assert!(
                matches!(error, StorageBindingError::UnsupportedPlacement { ref binding } if binding == "engine-cache"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn deployment_storage_keys_follow_the_single_engine_placement() {
        let deployment = binding(StoragePlacement::Deployment);
        let shared = binding(StoragePlacement::EngineArtifact);

        assert_eq!(
            deployment_storage_keys([&deployment, &shared], "deployment-one").expect("keys"),
            [deployment_storage(
                WorkloadStorageKind::Cache,
                "deployment-one"
            )]
        );

        let several = StorageBinding {
            placements: vec![
                StoragePlacement::Deployment as i32,
                StoragePlacement::EngineArtifact as i32,
            ],
            ..binding(StoragePlacement::Deployment)
        };
        let error = deployment_storage_keys([&several], "deployment-one").expect_err("several");
        assert!(
            matches!(error, StorageBindingError::EnginePlacement { ref binding } if binding == "engine-cache"),
            "{error:?}"
        );
    }

    fn binding(placement: StoragePlacement) -> StorageBinding {
        StorageBinding {
            binding_name: "engine-cache".to_owned(),
            mount_path: "/var/cache/engine".to_owned(),
            kind: StorageKind::Cache as i32,
            placements: vec![placement as i32],
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
