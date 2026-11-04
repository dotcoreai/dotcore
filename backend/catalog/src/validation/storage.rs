//! Storage binding validation shared by application services and engine releases.
//!
//! A storage binding names writable storage an owner attaches to its workloads: a unique name, a
//! unique absolute mount path, a kind and a scope the owner supports, and environment variables
//! that point at paths inside the mount. Mount paths may nest, so one binding can sit inside
//! another; Docker binds the inner mount over the outer one's folder.

use std::collections::BTreeSet;

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StorageScope};

use super::application::valid_environment_variable;

/// The owner a set of storage bindings belongs to, which decides the kinds and scopes it may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StorageOwner {
    ApplicationService,
    EngineRelease,
}

impl StorageOwner {
    fn allows_kind(self, kind: StorageKind) -> bool {
        match self {
            Self::ApplicationService => kind == StorageKind::Data,
            Self::EngineRelease => kind == StorageKind::Cache,
        }
    }

    fn allows_scope(self, scope: StorageScope) -> bool {
        match self {
            Self::ApplicationService => {
                matches!(scope, StorageScope::Deployment | StorageScope::Application)
            }
            Self::EngineRelease => {
                matches!(
                    scope,
                    StorageScope::Deployment | StorageScope::EngineArtifact
                )
            }
        }
    }
}

/// Validates one owner's storage bindings and records their names and environment variables.
///
/// `binding_names` and `environment_variables` hold the names the owner already uses, so storage
/// bindings cannot collide with its other bindings; `subject` names the owner in each error.
pub(super) fn validate_storage_bindings(
    bindings: &[StorageBinding],
    owner: StorageOwner,
    subject: &str,
    binding_names: &mut BTreeSet<String>,
    environment_variables: &mut BTreeSet<String>,
    errors: &mut Vec<anyhow::Error>,
) {
    let mut mount_paths = BTreeSet::new();
    for binding in bindings {
        let name = &binding.binding_name;
        if !valid_id(name) {
            errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} must match [a-z][a-z0-9-]{{0,62}}"
            ));
        } else if !binding_names.insert(name.clone()) {
            errors.push(anyhow::anyhow!(
                "{subject} has duplicate binding name {name:?}"
            ));
        }
        if !valid_container_path(&binding.mount_path) {
            errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} requires an absolute container mount path"
            ));
        } else if !mount_paths.insert(binding.mount_path.as_str()) {
            errors.push(anyhow::anyhow!(
                "{subject} has duplicate storage mount path {:?}",
                binding.mount_path
            ));
        }
        match StorageKind::try_from(binding.kind) {
            Ok(StorageKind::Unspecified) | Err(_) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} requires a kind"
            )),
            Ok(kind) if !owner.allows_kind(kind) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} cannot use kind {}",
                kind.as_str_name()
            )),
            Ok(_) => {}
        }
        match StorageScope::try_from(binding.scope) {
            Ok(StorageScope::Unspecified) | Err(_) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} requires a scope"
            )),
            Ok(scope) if !owner.allows_scope(scope) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} cannot use scope {}",
                scope.as_str_name()
            )),
            Ok(_) => {}
        }
        for variable in &binding.environment {
            if !valid_environment_variable(&variable.name) {
                errors.push(anyhow::anyhow!(
                    "{subject} storage binding {name:?} has an invalid environment variable name {:?}",
                    variable.name
                ));
            } else if !environment_variables.insert(variable.name.clone()) {
                errors.push(anyhow::anyhow!(
                    "{subject} has duplicate environment variable {:?}",
                    variable.name
                ));
            }
            if !valid_relative_path(&variable.relative_path) {
                errors.push(anyhow::anyhow!(
                    "{subject} storage binding {name:?} environment variable {:?} requires a path relative to the mount",
                    variable.name
                ));
            }
        }
    }
}

/// Whether a value is an absolute container path other than `/`, without empty, `.` or `..`
/// components.
pub(super) fn valid_container_path(value: &str) -> bool {
    value.starts_with('/')
        && value != "/"
        && value
            .split('/')
            .skip(1)
            .all(|part| !matches!(part, "" | "." | ".."))
}

/// Whether a value names the mount itself (empty) or a path below it, without empty, `.` or `..`
/// components.
fn valid_relative_path(value: &str) -> bool {
    value.is_empty()
        || value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::StorageEnvironmentVariable;

    use super::*;

    #[test]
    fn valid_bindings_may_nest_and_record_their_names() {
        let bindings = [
            binding("data", "/data", StorageScope::Deployment),
            StorageBinding {
                environment: vec![variable("MODELS_DIR", ""), variable("CHECKPOINTS", "a/b")],
                ..binding("models", "/data/models", StorageScope::Application)
            },
        ];
        let mut names = BTreeSet::new();
        let mut variables = BTreeSet::new();

        assert_eq!(
            validate(
                &bindings,
                StorageOwner::ApplicationService,
                &mut names,
                &mut variables
            ),
            Vec::<String>::new()
        );
        assert_eq!(names, BTreeSet::from(["data".into(), "models".into()]));
        assert_eq!(
            variables,
            BTreeSet::from(["CHECKPOINTS".into(), "MODELS_DIR".into()])
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_names_and_mount_paths() {
        let bindings = [
            binding("data", "/data", StorageScope::Deployment),
            binding("data", "/other", StorageScope::Deployment),
            binding("logs", "/data", StorageScope::Deployment),
            binding("Bad_Name", "/bad", StorageScope::Deployment),
            binding("relative", "data", StorageScope::Deployment),
            binding("root", "/", StorageScope::Deployment),
            binding("escape", "/data/../etc", StorageScope::Deployment),
            binding("taken", "/taken", StorageScope::Deployment),
        ];
        let mut names = BTreeSet::from(["taken".to_owned()]);

        assert_eq!(
            validate(
                &bindings,
                StorageOwner::ApplicationService,
                &mut names,
                &mut BTreeSet::new()
            ),
            [
                "service has duplicate binding name \"data\"",
                "service has duplicate storage mount path \"/data\"",
                "service storage binding \"Bad_Name\" must match [a-z][a-z0-9-]{0,62}",
                "service storage binding \"relative\" requires an absolute container mount path",
                "service storage binding \"root\" requires an absolute container mount path",
                "service storage binding \"escape\" requires an absolute container mount path",
                "service has duplicate binding name \"taken\"",
            ]
        );
    }

    #[test]
    fn rejects_missing_kinds_and_scopes() {
        let bindings = [StorageBinding {
            kind: 0,
            scope: 0,
            ..binding("data", "/data", StorageScope::Deployment)
        }];

        assert_eq!(
            validate(
                &bindings,
                StorageOwner::ApplicationService,
                &mut BTreeSet::new(),
                &mut BTreeSet::new()
            ),
            [
                "service storage binding \"data\" requires a kind",
                "service storage binding \"data\" requires a scope",
            ]
        );
    }

    #[test]
    fn rejects_kinds_and_scopes_the_owner_cannot_use() {
        let application = [
            binding("cache", "/cache", StorageScope::EngineArtifact),
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("scratch", "/scratch", StorageScope::Deployment)
            },
        ];
        let engine = [
            binding("data", "/data", StorageScope::Deployment),
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("shared", "/shared", StorageScope::Application)
            },
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("compile", "/compile", StorageScope::EngineArtifact)
            },
        ];

        assert_eq!(
            validate(
                &application,
                StorageOwner::ApplicationService,
                &mut BTreeSet::new(),
                &mut BTreeSet::new()
            ),
            [
                "service storage binding \"cache\" cannot use scope STORAGE_SCOPE_ENGINE_ARTIFACT",
                "service storage binding \"scratch\" cannot use kind STORAGE_KIND_CACHE",
            ]
        );
        assert_eq!(
            validate(
                &engine,
                StorageOwner::EngineRelease,
                &mut BTreeSet::new(),
                &mut BTreeSet::new()
            ),
            [
                "service storage binding \"data\" cannot use kind STORAGE_KIND_DATA",
                "service storage binding \"shared\" cannot use scope STORAGE_SCOPE_APPLICATION",
            ]
        );
    }

    #[test]
    fn rejects_invalid_or_colliding_environment_variables_and_unsafe_relative_paths() {
        let bindings = [
            StorageBinding {
                environment: vec![
                    variable("CACHE_HOME", ""),
                    variable("lower", ""),
                    variable("CACHE_HOME", "again"),
                    variable("TAKEN", ""),
                    variable("ABSOLUTE", "/etc"),
                    variable("PARENT", "a/../b"),
                    variable("TRAILING", "a/"),
                ],
                ..binding("data", "/data", StorageScope::Deployment)
            },
            StorageBinding {
                environment: vec![variable("CACHE_HOME", "other")],
                ..binding("other", "/other", StorageScope::Deployment)
            },
        ];
        let mut variables = BTreeSet::from(["TAKEN".to_owned()]);

        assert_eq!(
            validate(
                &bindings,
                StorageOwner::ApplicationService,
                &mut BTreeSet::new(),
                &mut variables
            ),
            [
                "service storage binding \"data\" has an invalid environment variable name \"lower\"",
                "service has duplicate environment variable \"CACHE_HOME\"",
                "service has duplicate environment variable \"TAKEN\"",
                "service storage binding \"data\" environment variable \"ABSOLUTE\" requires a path relative to the mount",
                "service storage binding \"data\" environment variable \"PARENT\" requires a path relative to the mount",
                "service storage binding \"data\" environment variable \"TRAILING\" requires a path relative to the mount",
                "service has duplicate environment variable \"CACHE_HOME\"",
            ]
        );
    }

    fn validate(
        bindings: &[StorageBinding],
        owner: StorageOwner,
        names: &mut BTreeSet<String>,
        variables: &mut BTreeSet<String>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        validate_storage_bindings(bindings, owner, "service", names, variables, &mut errors);
        errors.iter().map(ToString::to_string).collect()
    }

    fn binding(name: &str, mount_path: &str, scope: StorageScope) -> StorageBinding {
        StorageBinding {
            binding_name: name.to_owned(),
            mount_path: mount_path.to_owned(),
            kind: StorageKind::Data as i32,
            scope: scope as i32,
            environment: Vec::new(),
        }
    }

    fn variable(name: &str, relative_path: &str) -> StorageEnvironmentVariable {
        StorageEnvironmentVariable {
            name: name.to_owned(),
            relative_path: relative_path.to_owned(),
        }
    }
}
