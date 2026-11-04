//! Classifies native workload directories by their ownership evidence without a live adapter, so
//! discovery and site cleanup decide the same way which workloads they may adopt, replace, stop,
//! or leave alone; site cleanup, which may not know the site identity, classifies by location.
//!
//! A workload under an execution target's directory belongs to this runtime only when its owner
//! record names this site, SiteNode, execution target, and canonical state root. A different state
//! root means the directory was copied or moved from another site directory, so the workload is
//! foreign and never touched; any other contradiction, or launcher files that fail their checksum,
//! is malformed and blocks mutation. An owned workload whose identity no longer recomputes the
//! same way, for example after an upgrade, is stale: replaced, never adopted, but still removable.

use std::collections::BTreeMap;

use dotcore_domain::{SiteNodeId, valid_id};
use dotcore_launcher::{supervisor::Inconsistency, workload::LaunchSpec};
use dotcore_runtime::WorkloadLaunch;

use super::{
    NativeRuntimeConfig,
    launch::{NativeLaunchError, resolve_command, workload_id},
    record::{IDENTITY_VERSION, NativeOwnerRecord, NativeRecordError},
    state_root::OpenedStateRoot,
};

/// Ownership evidence that every workload of one native execution target must carry.
#[derive(Clone, Copy, Debug)]
pub(super) struct OwnershipEvidence<'a> {
    config: &'a NativeRuntimeConfig,
    state_root: &'a OpenedStateRoot,
}

impl<'a> OwnershipEvidence<'a> {
    pub(super) fn new(config: &'a NativeRuntimeConfig, state_root: &'a OpenedStateRoot) -> Self {
        Self { config, state_root }
    }

    /// Classifies every workload of a scan, failing closed when two owned workloads carry the
    /// same owner correlation.
    pub(super) fn classify_all(
        &self,
        workloads: impl IntoIterator<Item = (String, Result<LaunchSpec, Inconsistency>)>,
    ) -> Result<Vec<ClassifiedWorkload>, NativeOwnershipError> {
        let mut owners = BTreeMap::<String, String>::new();
        let mut classified = Vec::new();
        for (id, payload) in workloads {
            let class = match payload {
                Ok(spec) => self.classify(&id, Ok(&spec)),
                Err(problem) => self.classify(&id, Err(problem)),
            };
            if let Some(record) = class.owned_record()
                && let Some(first) =
                    owners.insert(record.launch.correlation.owner_id.clone(), id.clone())
            {
                return Err(NativeOwnershipError::DuplicateCorrelation {
                    owner_id: record.launch.correlation.owner_id.clone(),
                    first,
                    second: id,
                });
            }
            classified.push(ClassifiedWorkload { id, class });
        }
        Ok(classified)
    }

    /// Classifies one workload from its directory name and verified launch payload, or the
    /// reason its launcher files cannot be trusted. Never reads the store or artifact paths.
    pub(super) fn classify(
        &self,
        workload_id: &str,
        payload: Result<&LaunchSpec, Inconsistency>,
    ) -> NativeWorkloadClass {
        let spec = match payload {
            Ok(spec) => spec,
            Err(problem) => {
                return NativeWorkloadClass::Malformed(NativeOwnershipError::Untrusted(problem));
            }
        };
        let record = match NativeOwnerRecord::decode(&spec.owner) {
            Ok(record) => record,
            Err(error) => {
                return NativeWorkloadClass::Malformed(NativeOwnershipError::Record(error));
            }
        };
        if record.state_root != self.state_root.canonical_root {
            return NativeWorkloadClass::Foreign {
                state_root: record.state_root,
            };
        }
        if let Err(error) = self.check_evidence(workload_id, &record) {
            return NativeWorkloadClass::Malformed(error);
        }
        match self.stale_reason(spec, &record) {
            Some(reason) => NativeWorkloadClass::OwnedStale { record, reason },
            None => NativeWorkloadClass::Owned(record),
        }
    }

    /// Checks that the record names this site, SiteNode, and target, and the directory name.
    fn check_evidence(
        &self,
        id: &str,
        record: &NativeOwnerRecord,
    ) -> Result<(), NativeOwnershipError> {
        ExpectedOwner {
            site_id: Some(&self.config.site_id),
            site_node_id: Some(&self.config.site_node_id),
            execution_target_id: &self.config.execution_target_id,
        }
        .check(id, record)
    }

    /// Recomputes the identity of an owned workload from its record alone and reports why it
    /// cannot be adopted, if it cannot.
    fn stale_reason(&self, spec: &LaunchSpec, record: &NativeOwnerRecord) -> Option<StaleReason> {
        if record.identity_version != IDENTITY_VERSION {
            return Some(StaleReason::IdentityVersion {
                recorded: record.identity_version,
            });
        }
        let recorded = &record.launch;
        let recomputed = WorkloadLaunch::new(
            recorded.correlation.owner_id.clone(),
            recorded.execution_target.clone(),
            recorded.spec.clone(),
        );
        if recomputed.correlation != recorded.correlation {
            return Some(StaleReason::Generation {
                recomputed: recomputed.correlation.reconcile_generation,
            });
        }
        if recomputed.exact_intent_fingerprint() != spec.fingerprint {
            return Some(StaleReason::Fingerprint);
        }
        let command = match resolve_command(&recorded.spec, self.config, &record.ports) {
            Ok(command) => command,
            Err(error) => return Some(StaleReason::Unresolvable(error)),
        };
        let same_command = spec.executable.to_str() == Some(command.executable.as_str())
            && spec.args == command.arguments
            && spec.environment == command.environment;
        (!same_command).then_some(StaleReason::Command)
    }
}

/// Classifies one workload for site removal by its location: a record naming another state root
/// is foreign, and a record under `canonical_root` must name the `expected` owner. Owned workloads
/// are never reported stale, because removal does not depend on their launch identity.
pub(super) fn classify_for_removal(
    canonical_root: &str,
    expected: ExpectedOwner<'_>,
    workload_id: &str,
    payload: Result<&LaunchSpec, Inconsistency>,
) -> NativeWorkloadClass {
    let record = match payload
        .map_err(NativeOwnershipError::Untrusted)
        .and_then(|spec| {
            NativeOwnerRecord::decode(&spec.owner).map_err(NativeOwnershipError::Record)
        }) {
        Ok(record) => record,
        Err(error) => return NativeWorkloadClass::Malformed(error),
    };
    if record.state_root != canonical_root {
        return NativeWorkloadClass::Foreign {
            state_root: record.state_root,
        };
    }
    match expected.check(workload_id, &record) {
        Ok(()) => NativeWorkloadClass::Owned(record),
        Err(error) => NativeWorkloadClass::Malformed(error),
    }
}

/// Owner identities a workload record under its own state root must name. An unknown site or
/// SiteNode, as during removal of a site directory whose configuration is unreadable, is not
/// checked here.
#[derive(Clone, Copy, Debug)]
pub(super) struct ExpectedOwner<'a> {
    pub(super) site_id: Option<&'a str>,
    pub(super) site_node_id: Option<&'a SiteNodeId>,
    pub(super) execution_target_id: &'a str,
}

impl ExpectedOwner<'_> {
    /// Checks the recorded site, SiteNode, and target, and that the directory name `id` is the
    /// workload id the recorded correlation derives.
    pub(super) fn check(
        &self,
        id: &str,
        record: &NativeOwnerRecord,
    ) -> Result<(), NativeOwnershipError> {
        let recorded_node = &record.launch.execution_target.managed_node_id;
        if self
            .site_id
            .is_some_and(|site_id| record.site_id != site_id)
        {
            return Err(NativeOwnershipError::SiteMismatch {
                recorded: record.site_id.clone(),
            });
        }
        if self
            .site_node_id
            .is_some_and(|site_node_id| recorded_node != site_node_id)
        {
            return Err(NativeOwnershipError::SiteNodeMismatch {
                recorded: recorded_node.to_string(),
            });
        }
        if record.execution_target_id != self.execution_target_id {
            return Err(NativeOwnershipError::ExecutionTargetMismatch {
                recorded: record.execution_target_id.clone(),
            });
        }
        let correlation = &record.launch.correlation;
        let expected = workload_id(correlation);
        if !valid_id(&correlation.owner_id)
            || correlation.reconcile_generation == 0
            || id != expected
        {
            return Err(NativeOwnershipError::WorkloadIdMismatch { expected });
        }
        Ok(())
    }
}

/// One workload of a scan and its ownership class.
#[derive(Debug)]
pub(super) struct ClassifiedWorkload {
    pub(super) id: String,
    pub(super) class: NativeWorkloadClass,
}

/// What a workload's ownership evidence allows the native runtime to do with it.
#[derive(Debug)]
pub(super) enum NativeWorkloadClass {
    /// Owned and adoptable by its reconcile generation.
    Owned(NativeOwnerRecord),

    /// Owned, but its identity no longer recomputes the same way: replaced, never adopted, and
    /// still removable.
    OwnedStale {
        record: NativeOwnerRecord,
        reason: StaleReason,
    },

    /// Created by another state root, such as the site directory this one was copied from:
    /// never adopted, stopped, or removed.
    Foreign { state_root: String },

    /// Evidence that cannot be trusted; the workload blocks every mutation.
    Malformed(NativeOwnershipError),
}

impl NativeWorkloadClass {
    /// Returns the owner record of an owned workload, stale or not.
    pub(super) fn owned_record(&self) -> Option<&NativeOwnerRecord> {
        match self {
            Self::Owned(record) | Self::OwnedStale { record, .. } => Some(record),
            Self::Foreign { .. } | Self::Malformed(_) => None,
        }
    }
}

/// Why an owned workload cannot be adopted.
#[derive(Debug, thiserror::Error)]
pub(super) enum StaleReason {
    #[error("recorded under identity algorithm version {recorded}")]
    IdentityVersion { recorded: u32 },

    #[error("recorded spec now derives reconcile generation {recomputed}")]
    Generation { recomputed: u64 },

    #[error("recorded launch no longer derives the stored launch fingerprint")]
    Fingerprint,

    #[error("recorded spec no longer resolves under the current configuration")]
    Unresolvable(#[source] NativeLaunchError),

    #[error("recorded spec resolves to a command other than the one the launcher holds")]
    Command,
}

/// Native ownership evidence that cannot be trusted.
#[derive(Debug, thiserror::Error)]
pub enum NativeOwnershipError {
    #[error("native workload launcher files cannot be trusted")]
    Untrusted(#[source] Inconsistency),

    #[error("native workload owner record cannot be trusted")]
    Record(#[source] NativeRecordError),

    #[error("native workload owner record names site {recorded:?}, not this site")]
    SiteMismatch { recorded: String },

    #[error("native workload owner record names node {recorded:?}, not this node")]
    SiteNodeMismatch { recorded: String },

    #[error(
        "native workload owner record names execution target {recorded:?}, not this execution target"
    )]
    ExecutionTargetMismatch { recorded: String },

    #[error("native workload directory name does not match its recorded correlation {expected:?}")]
    WorkloadIdMismatch { expected: String },

    #[error("native workloads {first} and {second} carry the same owner correlation {owner_id:?}")]
    DuplicateCorrelation {
        owner_id: String,
        first: String,
        second: String,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dotcore_domain::SiteNodeId;
    use dotcore_launcher::{
        supervisor::{Supervisor, SupervisorError},
        workload::{LAUNCH_CHECKSUM_FILE, LAUNCH_FILE},
    };
    use dotcore_proto::dotcore::runtime::store::v1 as store;
    use dotcore_runtime::WorkloadValue;
    use prost::Message;

    use super::*;
    use crate::native::launch::{PreparedLaunch, tests::Fixture};

    /// Expected class of one classification case.
    #[derive(Debug, PartialEq)]
    enum Expected {
        Owned,
        Stale(&'static str),
        Foreign(String),
        Malformed(&'static str),
    }

    #[test]
    fn classification_follows_the_ownership_evidence() {
        let fixture = Fixture::new();
        let prepared = fixture.prepare(&fixture.launch()).expect("prepare launch");
        let id = prepared.workload_id.clone();
        let cases: Vec<(&str, String, LaunchSpec, Expected)> = vec![
            ("owned", id.clone(), prepared.spec.clone(), Expected::Owned),
            (
                "undecodable record",
                id.clone(),
                with_owner(&prepared, b"\xff".to_vec()),
                Expected::Malformed("record"),
            ),
            (
                "wrong record schema",
                id.clone(),
                with_stored(&prepared, |stored| stored.schema_version = 2),
                Expected::Malformed("record"),
            ),
            (
                "wrong site",
                id.clone(),
                with_record(&prepared, |record| record.site_id = "site-two".to_owned()),
                Expected::Malformed("site"),
            ),
            (
                "wrong node",
                id.clone(),
                with_record(&prepared, |record| {
                    record.launch.execution_target.managed_node_id = SiteNodeId::for_test("node-b");
                }),
                Expected::Malformed("site node"),
            ),
            (
                "wrong execution target",
                id.clone(),
                with_record(&prepared, |record| {
                    record.execution_target_id = "linux-gpu".to_owned();
                }),
                Expected::Malformed("target"),
            ),
            (
                "directory name of another generation",
                format!("deployment-one_{}", u64::MAX),
                prepared.spec.clone(),
                Expected::Malformed("workload id"),
            ),
            (
                "copied site directory",
                id.clone(),
                with_record(&prepared, |record| {
                    record.state_root = COPIED_ROOT.to_owned();
                }),
                Expected::Foreign(COPIED_ROOT.to_owned()),
            ),
            (
                "copied site directory of another site",
                id.clone(),
                with_record(&prepared, |record| {
                    record.state_root = COPIED_ROOT.to_owned();
                    record.site_id = "site-two".to_owned();
                }),
                Expected::Foreign(COPIED_ROOT.to_owned()),
            ),
            (
                "older identity algorithm",
                id.clone(),
                with_record(&prepared, |record| {
                    record.identity_version = IDENTITY_VERSION - 1
                }),
                Expected::Stale("identity version"),
            ),
            (
                "spec that no longer derives its generation",
                id.clone(),
                with_record(&prepared, |record| {
                    record
                        .launch
                        .spec
                        .arguments
                        .push(WorkloadValue::literal("--verbose"));
                }),
                Expected::Stale("generation"),
            ),
            (
                "launch that no longer derives its fingerprint",
                id.clone(),
                LaunchSpec {
                    fingerprint: "0".repeat(64),
                    ..prepared.spec.clone()
                },
                Expected::Stale("fingerprint"),
            ),
            (
                "held command other than the recorded spec resolves to",
                id.clone(),
                LaunchSpec {
                    args: vec!["--other".to_owned()],
                    ..prepared.spec.clone()
                },
                Expected::Stale("command"),
            ),
            (
                "recorded port the spec does not declare",
                id.clone(),
                with_record(&prepared, |record| {
                    record.ports.insert("metrics".to_owned(), 41_235);
                }),
                Expected::Stale("unresolvable"),
            ),
        ];

        let evidence = OwnershipEvidence::new(&fixture.config, &fixture.state_root);
        for (name, id, spec, expected) in cases {
            assert_eq!(
                expected_of(&evidence.classify(&id, Ok(&spec))),
                expected,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn a_launch_payload_that_fails_its_checksum_is_malformed() {
        let fixture = Fixture::new();
        let prepared = fixture.prepare(&fixture.launch()).expect("prepare launch");
        let directory = fixture
            .state_root
            .target_directory
            .join(&prepared.workload_id);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(LAUNCH_FILE), prepared.spec.encode().unwrap()).unwrap();
        fs::write(directory.join(LAUNCH_CHECKSUM_FILE), "0".repeat(64)).unwrap();
        let supervisor = Supervisor::new(&fixture.state_root.target_directory, "/nonexistent")
            .expect("supervisor");

        let problem = match supervisor.payload(&prepared.workload_id).await {
            Err(SupervisorError::Inconsistent(problem)) => problem,
            other => panic!("expected an inconsistent payload, got {other:?}"),
        };
        let class = OwnershipEvidence::new(&fixture.config, &fixture.state_root)
            .classify(&prepared.workload_id, Err(problem));

        assert!(
            matches!(
                class,
                NativeWorkloadClass::Malformed(NativeOwnershipError::Untrusted(
                    Inconsistency::ChecksumMismatch
                ))
            ),
            "{class:?}"
        );
    }

    #[test]
    fn classification_never_reads_the_store_or_artifact_paths() {
        let fixture = Fixture::new();
        let prepared = fixture.prepare(&fixture.launch()).expect("prepare launch");
        fs::remove_dir_all(&fixture.config.store_root).unwrap();
        fs::remove_dir_all(&fixture.config.approved_artifact_root).unwrap();

        let class = OwnershipEvidence::new(&fixture.config, &fixture.state_root)
            .classify(&prepared.workload_id, Ok(&prepared.spec));

        assert!(matches!(class, NativeWorkloadClass::Owned(_)), "{class:?}");
    }

    #[test]
    fn duplicate_owner_correlations_fail_closed_unless_foreign() {
        let fixture = Fixture::new();
        let first = fixture.prepare(&fixture.launch()).expect("prepare launch");
        let mut changed = fixture.launch();
        changed
            .spec
            .arguments
            .push(WorkloadValue::literal("--verbose"));
        let changed = WorkloadLaunch::new(
            changed.correlation.owner_id,
            changed.execution_target,
            changed.spec,
        );
        let second = fixture.prepare(&changed).expect("prepare changed launch");
        let foreign = with_record(&second, |record| {
            record.state_root = COPIED_ROOT.to_owned();
        });
        let evidence = OwnershipEvidence::new(&fixture.config, &fixture.state_root);

        let classified = evidence
            .classify_all([
                (first.workload_id.clone(), Ok(first.spec.clone())),
                (second.workload_id.clone(), Ok(foreign)),
            ])
            .expect("foreign workloads never conflict");
        let duplicate = evidence.classify_all([
            (first.workload_id.clone(), Ok(first.spec.clone())),
            (second.workload_id.clone(), Ok(second.spec.clone())),
        ]);

        assert_eq!(
            classified
                .iter()
                .map(|workload| (workload.id.as_str(), expected_of(&workload.class)))
                .collect::<Vec<_>>(),
            [
                (first.workload_id.as_str(), Expected::Owned),
                (
                    second.workload_id.as_str(),
                    Expected::Foreign(COPIED_ROOT.to_owned())
                ),
            ]
        );
        assert!(
            matches!(
                &duplicate,
                Err(NativeOwnershipError::DuplicateCorrelation { owner_id, first: one, second: two })
                    if owner_id == "deployment-one"
                        && *one == first.workload_id
                        && *two == second.workload_id
            ),
            "{duplicate:?}"
        );
    }

    #[test]
    fn removal_classifies_by_location_and_checks_only_the_known_owner() {
        let fixture = Fixture::new();
        let prepared = fixture.prepare(&fixture.launch()).expect("prepare launch");
        let id = prepared.workload_id.clone();
        let root = fixture.state_root.canonical_root.as_str();
        let node = SiteNodeId::for_test("node-a");
        let unknown = ExpectedOwner {
            site_id: None,
            site_node_id: None,
            execution_target_id: "mac-gpu",
        };
        let known = ExpectedOwner {
            site_id: Some("site-one"),
            site_node_id: Some(&node),
            ..unknown
        };
        let cases: Vec<(&str, ExpectedOwner<'_>, String, LaunchSpec, Expected)> = vec![
            (
                "owned, identity unknown",
                unknown,
                id.clone(),
                prepared.spec.clone(),
                Expected::Owned,
            ),
            (
                "owned, identity known",
                known,
                id.clone(),
                prepared.spec.clone(),
                Expected::Owned,
            ),
            (
                "stale launch identity is still owned",
                known,
                id.clone(),
                LaunchSpec {
                    fingerprint: "0".repeat(64),
                    ..prepared.spec.clone()
                },
                Expected::Owned,
            ),
            (
                "other state root of another site",
                known,
                id.clone(),
                with_record(&prepared, |record| {
                    record.state_root = COPIED_ROOT.to_owned();
                    record.site_id = "site-two".to_owned();
                }),
                Expected::Foreign(COPIED_ROOT.to_owned()),
            ),
            (
                "other site under this root",
                known,
                id.clone(),
                with_record(&prepared, |record| record.site_id = "site-two".to_owned()),
                Expected::Malformed("site"),
            ),
            (
                "other target directory",
                ExpectedOwner {
                    execution_target_id: "linux-gpu",
                    ..unknown
                },
                id.clone(),
                prepared.spec.clone(),
                Expected::Malformed("target"),
            ),
            (
                "directory name of another generation",
                unknown,
                format!("deployment-one_{}", u64::MAX),
                prepared.spec.clone(),
                Expected::Malformed("workload id"),
            ),
            (
                "undecodable record",
                unknown,
                id.clone(),
                with_owner(&prepared, b"\xff".to_vec()),
                Expected::Malformed("record"),
            ),
        ];

        for (name, expected_owner, id, spec, expected) in cases {
            assert_eq!(
                expected_of(&classify_for_removal(root, expected_owner, &id, Ok(&spec))),
                expected,
                "{name}"
            );
        }
        assert_eq!(
            expected_of(&classify_for_removal(
                root,
                unknown,
                &prepared.workload_id,
                Err(Inconsistency::ChecksumMismatch)
            )),
            Expected::Malformed("untrusted")
        );
    }

    /// Recorded state root of a site directory copied from another location.
    const COPIED_ROOT: &str = "/elsewhere/site/runtime/native";

    fn expected_of(class: &NativeWorkloadClass) -> Expected {
        match class {
            NativeWorkloadClass::Owned(_) => Expected::Owned,
            NativeWorkloadClass::OwnedStale { reason, .. } => Expected::Stale(match reason {
                StaleReason::IdentityVersion { .. } => "identity version",
                StaleReason::Generation { .. } => "generation",
                StaleReason::Fingerprint => "fingerprint",
                StaleReason::Unresolvable(_) => "unresolvable",
                StaleReason::Command => "command",
            }),
            NativeWorkloadClass::Foreign { state_root } => Expected::Foreign(state_root.clone()),
            NativeWorkloadClass::Malformed(error) => Expected::Malformed(match error {
                NativeOwnershipError::Untrusted(_) => "untrusted",
                NativeOwnershipError::Record(_) => "record",
                NativeOwnershipError::SiteMismatch { .. } => "site",
                NativeOwnershipError::SiteNodeMismatch { .. } => "site node",
                NativeOwnershipError::ExecutionTargetMismatch { .. } => "target",
                NativeOwnershipError::WorkloadIdMismatch { .. } => "workload id",
                NativeOwnershipError::DuplicateCorrelation { .. } => "duplicate",
            }),
        }
    }

    fn with_owner(prepared: &PreparedLaunch, owner: Vec<u8>) -> LaunchSpec {
        LaunchSpec {
            owner,
            ..prepared.spec.clone()
        }
    }

    fn with_record(
        prepared: &PreparedLaunch,
        change: impl FnOnce(&mut NativeOwnerRecord),
    ) -> LaunchSpec {
        let mut record = NativeOwnerRecord::decode(&prepared.spec.owner).expect("owner record");
        change(&mut record);
        with_owner(prepared, record.encode().expect("encode owner record"))
    }

    fn with_stored(
        prepared: &PreparedLaunch,
        change: impl FnOnce(&mut store::NativeWorkloadRecord),
    ) -> LaunchSpec {
        let mut stored =
            store::NativeWorkloadRecord::decode(prepared.spec.owner.as_slice()).expect("stored");
        change(&mut stored);
        with_owner(prepared, stored.encode_to_vec())
    }
}
