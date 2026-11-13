//! Storage binding validation shared by application services and engine releases.
//!
//! A storage binding names writable storage an owner attaches to its workloads: a unique name, a
//! unique absolute mount path, a kind and the placements the owner supports, and environment
//! variables that point at paths inside the mount. An application binding lists the placements a
//! deployment may choose, the first being the default; an engine binding has exactly one. Mount paths may nest, so one binding can sit inside
//! another; Docker binds the inner mount over the outer one's folder.

use std::collections::BTreeSet;

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{StorageBinding, StorageKind, StoragePlacement};

use super::application::valid_environment_variable;

/// The owner a set of storage bindings belongs to, which decides the kinds and placements it may
/// use.
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

    fn allows_placement(self, placement: StoragePlacement) -> bool {
        match self {
            Self::ApplicationService => matches!(
                placement,
                StoragePlacement::Deployment | StoragePlacement::Application
            ),
            Self::EngineRelease => matches!(
                placement,
                StoragePlacement::Deployment | StoragePlacement::EngineArtifact
            ),
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
        validate_placements(binding, owner, subject, errors);
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

/// Checks that a binding lists at least one placement, each specified, allowed by its owner and
/// listed once, and that an engine binding lists exactly one.
fn validate_placements(
    binding: &StorageBinding,
    owner: StorageOwner,
    subject: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    let name = &binding.binding_name;
    if binding.placements.is_empty() {
        errors.push(anyhow::anyhow!(
            "{subject} storage binding {name:?} requires a placement"
        ));
    }
    if owner == StorageOwner::EngineRelease && binding.placements.len() > 1 {
        errors.push(anyhow::anyhow!(
            "{subject} storage binding {name:?} must declare exactly one placement"
        ));
    }
    let mut seen = BTreeSet::new();
    for &placement in &binding.placements {
        match StoragePlacement::try_from(placement) {
            Ok(StoragePlacement::Unspecified) | Err(_) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} has an unspecified placement"
            )),
            Ok(placement) if !owner.allows_placement(placement) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} cannot use placement {}",
                placement.as_str_name()
            )),
            Ok(placement) if !seen.insert(placement) => errors.push(anyhow::anyhow!(
                "{subject} storage binding {name:?} lists placement {} twice",
                placement.as_str_name()
            )),
            Ok(_) => {}
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
            binding("data", "/data", StoragePlacement::Deployment),
            StorageBinding {
                environment: vec![variable("MODELS_DIR", ""), variable("CHECKPOINTS", "a/b")],
                placements: vec![
                    StoragePlacement::Application as i32,
                    StoragePlacement::Deployment as i32,
                ],
                ..binding("models", "/data/models", StoragePlacement::Application)
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
            binding("data", "/data", StoragePlacement::Deployment),
            binding("data", "/other", StoragePlacement::Deployment),
            binding("logs", "/data", StoragePlacement::Deployment),
            binding("Bad_Name", "/bad", StoragePlacement::Deployment),
            binding("relative", "data", StoragePlacement::Deployment),
            binding("root", "/", StoragePlacement::Deployment),
            binding("escape", "/data/../etc", StoragePlacement::Deployment),
            binding("taken", "/taken", StoragePlacement::Deployment),
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
    fn rejects_missing_kinds_and_placements() {
        let bindings = [StorageBinding {
            kind: 0,
            placements: Vec::new(),
            ..binding("data", "/data", StoragePlacement::Deployment)
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
                "service storage binding \"data\" requires a placement",
            ]
        );
    }

    #[test]
    fn rejects_kinds_and_placements_the_owner_cannot_use() {
        let application = [
            binding("cache", "/cache", StoragePlacement::EngineArtifact),
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("scratch", "/scratch", StoragePlacement::Deployment)
            },
        ];
        let engine = [
            binding("data", "/data", StoragePlacement::Deployment),
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("shared", "/shared", StoragePlacement::Application)
            },
            StorageBinding {
                kind: StorageKind::Cache as i32,
                ..binding("compile", "/compile", StoragePlacement::EngineArtifact)
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
                "service storage binding \"cache\" cannot use placement STORAGE_PLACEMENT_ENGINE_ARTIFACT",
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
                "service storage binding \"shared\" cannot use placement STORAGE_PLACEMENT_APPLICATION",
            ]
        );
    }

    #[test]
    fn rejects_unspecified_or_repeated_placements_and_several_engine_placements() {
        let placements = |placements: &[StoragePlacement]| {
            placements
                .iter()
                .map(|&placement| placement as i32)
                .collect()
        };
        let application = [
            StorageBinding {
                placements: placements(&[
                    StoragePlacement::Deployment,
                    StoragePlacement::Unspecified,
                ]),
                ..binding("unspecified", "/unspecified", StoragePlacement::Deployment)
            },
            StorageBinding {
                placements: placements(&[
                    StoragePlacement::Application,
                    StoragePlacement::Deployment,
                    StoragePlacement::Application,
                ]),
                ..binding("repeated", "/repeated", StoragePlacement::Deployment)
            },
        ];
        let engine = [StorageBinding {
            kind: StorageKind::Cache as i32,
            placements: placements(&[
                StoragePlacement::Deployment,
                StoragePlacement::EngineArtifact,
            ]),
            ..binding("cache", "/cache", StoragePlacement::Deployment)
        }];

        assert_eq!(
            validate(
                &application,
                StorageOwner::ApplicationService,
                &mut BTreeSet::new(),
                &mut BTreeSet::new()
            ),
            [
                "service storage binding \"unspecified\" has an unspecified placement",
                "service storage binding \"repeated\" lists placement STORAGE_PLACEMENT_APPLICATION twice",
            ]
        );
        assert_eq!(
            validate(
                &engine,
                StorageOwner::EngineRelease,
                &mut BTreeSet::new(),
                &mut BTreeSet::new()
            ),
            ["service storage binding \"cache\" must declare exactly one placement"]
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
                ..binding("data", "/data", StoragePlacement::Deployment)
            },
            StorageBinding {
                environment: vec![variable("CACHE_HOME", "other")],
                ..binding("other", "/other", StoragePlacement::Deployment)
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

    fn binding(name: &str, mount_path: &str, placement: StoragePlacement) -> StorageBinding {
        StorageBinding {
            binding_name: name.to_owned(),
            mount_path: mount_path.to_owned(),
            kind: StorageKind::Data as i32,
            placements: vec![placement as i32],
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
