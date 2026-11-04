//! Site store: the owner `state.pb` of one desired site, durable or in memory.
//!
//! The site manager's owner records live here: accepted deployment status and lifecycle
//! generation high-water marks. Opening the store at startup also raises those marks to the
//! generations declared in `site.yaml`.

mod memory;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use dotcore_domain::deployment::DeploymentOwner;
use dotcore_proto::dotcore::site::{
    store::v1::{ManagerLifecycleRecord, SiteCurrentState},
    v1::Site,
};

use crate::{
    atomic::{AtomicError, FileSystem, StdFileSystem, persist},
    store::{
        CurrentStateStore, StoreError, lifecycle_records, lifecycle_records_mut, new_envelope,
        reserve_lifecycle_generation,
        storage::{StateFile, acquire_lock, read_stored},
        validate_encoded_size, validate_envelope,
    },
    validation::{
        SiteDeclarationRules, decode_site, reject_insecure_credential_file, validate_site,
    },
};
pub use memory::InMemoryCurrentState;

const STATE_FILE: &str = "state.pb";

/// File-backed owner current-state implementation that writes `state.pb` beside `site.yaml`.
#[derive(Clone)]
pub struct FileCurrentState {
    file: StateFile<SiteCurrentState>,
}

impl FileCurrentState {
    pub fn open(
        site_yaml: impl AsRef<Path>,
        site_id: impl Into<String>,
    ) -> Result<Self, StoreError> {
        Self::open_with_filesystem(site_yaml.as_ref(), site_id.into(), Arc::new(StdFileSystem))
    }

    /// Opens the owner state for a site start under exclusive custody. Validates `site.yaml`, then
    /// raises lifecycle high-water marks to the generations it declares, so a generation accepted
    /// before a lost state write is never reserved again. Never writes `site.yaml`.
    pub fn open_at_startup(
        site_yaml: impl AsRef<Path>,
        rules: &SiteDeclarationRules,
    ) -> Result<Self, StoreError> {
        let site_yaml = site_yaml.as_ref();
        let parent = site_yaml.parent().ok_or_else(|| {
            StoreError::Corrupt(anyhow::anyhow!("site.yaml has no parent directory"))
        })?;
        let path = parent.join(STATE_FILE);
        let lock = acquire_lock(&path)?;
        let site = decode_site(site_yaml).map_err(StoreError::SiteInput)?;
        validate_site(&site, rules).map_err(StoreError::SiteInput)?;
        reject_insecure_credential_file(site_yaml, &site).map_err(StoreError::SiteInput)?;
        let mut state = match read_state(&path, &site.site_id)? {
            Some(state) => state,
            None => new_envelope(site.site_id.clone())?,
        };

        if synchronize_lifecycle_records(&site, &mut state)? {
            persist_synchronized_state(&path, &state, &StdFileSystem)?;
        }
        Ok(Self {
            file: StateFile::new(state, path, Arc::new(StdFileSystem), lock),
        })
    }

    fn open_with_filesystem(
        site_yaml: &Path,
        site_id: String,
        filesystem: Arc<dyn FileSystem>,
    ) -> Result<Self, StoreError> {
        let parent = site_yaml.parent().ok_or_else(|| {
            StoreError::Storage(anyhow::anyhow!(
                "current-state site.yaml path has no parent: {}",
                site_yaml.display()
            ))
        })?;
        let state = new_envelope(site_id)?;
        let path = parent.join(STATE_FILE);
        let lock = acquire_lock(&path)?;
        let state = read_state(&path, &state.site_id)?.unwrap_or(state);
        Ok(Self {
            file: StateFile::new(state, path, filesystem, lock),
        })
    }

    pub fn path(&self) -> Result<PathBuf, StoreError> {
        self.file.path()
    }

    /// Reserves the next never-reused lifecycle generation for one owner's deployment before
    /// accepted site intent is exposed.
    pub fn reserve_lifecycle_generation(
        &self,
        owner: DeploymentOwner,
        deployment_id: &str,
    ) -> Result<ManagerLifecycleRecord, StoreError> {
        self.file
            .transact(|state| reserve_lifecycle_generation(state, owner, deployment_id))
    }

    /// Returns the highest lifecycle generation ever reserved for one owner's deployment, if
    /// recorded.
    pub fn lifecycle_high_water(
        &self,
        owner: DeploymentOwner,
        deployment_id: &str,
    ) -> Result<Option<u64>, StoreError> {
        self.file.read(|state| {
            lifecycle_records(state, owner)
                .iter()
                .find(|record| record.deployment_id == deployment_id)
                .map(|record| record.generation_high_water)
        })
    }

    /// Deletes one owner's lifecycle records of deployments whose teardown removal is durable in
    /// `site.yaml`. Callers name only finalized deployments: deployment identities are never
    /// issued again, so no later declaration can need the pruned high-water and no lifecycle
    /// generation can be reused.
    pub fn prune_lifecycle_records(
        &self,
        owner: DeploymentOwner,
        finalized: &BTreeSet<&str>,
    ) -> Result<(), StoreError> {
        let pruned =
            |record: &ManagerLifecycleRecord| finalized.contains(record.deployment_id.as_str());
        if !self
            .file
            .read(|state| lifecycle_records(state, owner).iter().any(pruned))?
        {
            return Ok(());
        }
        self.file.transact(|state| {
            lifecycle_records_mut(state, owner).retain(|record| !pruned(record));
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(())
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn state_pointer(&self) -> usize {
        self.file.state_pointer()
    }
}

impl CurrentStateStore for FileCurrentState {
    type State = SiteCurrentState;

    fn read<T>(&self, read: impl FnOnce(&SiteCurrentState) -> T) -> Result<T, StoreError> {
        self.file.read(read)
    }

    fn transact<T, E: From<StoreError>>(
        &self,
        operation: impl FnOnce(&mut SiteCurrentState) -> Result<T, E>,
    ) -> Result<T, E> {
        self.file.transact(operation)
    }
}

/// Raises each owner's durable lifecycle high-water marks to at least the generation declared in
/// `site.yaml`, so a generation accepted before a lost state write is never reserved again.
fn synchronize_lifecycle_records(
    site: &Site,
    state: &mut SiteCurrentState,
) -> Result<bool, StoreError> {
    let declared = [
        (
            DeploymentOwner::Model,
            site.model_deployments
                .iter()
                .map(|deployment| {
                    (
                        deployment.model_deployment_id.as_str(),
                        deployment.lifecycle_generation,
                    )
                })
                .collect::<Vec<_>>(),
        ),
        (
            DeploymentOwner::Application,
            site.application_deployments
                .iter()
                .map(|deployment| {
                    (
                        deployment.application_deployment_id.as_str(),
                        deployment.lifecycle_generation,
                    )
                })
                .collect::<Vec<_>>(),
        ),
    ];
    let mut changed = false;
    for (owner, deployments) in declared {
        let records = lifecycle_records_mut(state, owner);
        let before = records.clone();
        for (deployment_id, lifecycle_generation) in deployments {
            if lifecycle_generation == 0 {
                continue;
            }
            if let Some(record) = records
                .iter_mut()
                .find(|record| record.deployment_id == deployment_id)
            {
                record.generation_high_water =
                    record.generation_high_water.max(lifecycle_generation);
                continue;
            }
            records.push(ManagerLifecycleRecord {
                deployment_id: deployment_id.to_owned(),
                generation_high_water: lifecycle_generation,
            });
        }
        records.sort_by(|left, right| left.deployment_id.cmp(&right.deployment_id));
        changed |= *records != before;
    }
    if changed {
        state.storage_revision = StoreError::next_revision(state.storage_revision)?;
    }
    Ok(changed)
}

fn persist_synchronized_state(
    state_path: &Path,
    state: &SiteCurrentState,
    filesystem: &dyn FileSystem,
) -> Result<(), StoreError> {
    validate_encoded_size(state)?;
    persist(filesystem, state_path, state).map_err(|error| match error {
        AtomicError::Storage(source) => StoreError::Storage(source),
        AtomicError::OutcomeUncertain(source) => StoreError::OutcomeUncertain(source),
    })
}

fn read_state(path: &Path, expected_site_id: &str) -> Result<Option<SiteCurrentState>, StoreError> {
    let Some(state) = read_stored::<SiteCurrentState>(path)? else {
        return Ok(None);
    };
    validate_envelope(&state, expected_site_id)?;
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        error::Error,
        fs, io,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    #[cfg(target_os = "linux")]
    use std::{
        io::{BufRead, Read, Write},
        os::fd::{AsRawFd, RawFd},
        process::{Command, Stdio},
    };

    use dotcore_proto::dotcore::site::v1::{Site, SiteManagerRole, SiteNode};
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentState, ApplicationLifecycleState, ApplicationReadinessState,
            CurrentApplicationDeployment,
        },
        model::v1::{
            CurrentModelDeployment, ModelDeploymentDesiredState, ModelDeploymentState,
            ModelDeploymentStatus, ObservedPlacement,
        },
        site::store::v1::SiteCurrentState,
    };
    use prost::Message;

    use super::{FileCurrentState, FileSystem, StdFileSystem};
    use crate::store::StoreError;
    use crate::test_support::{
        stored, write_application_record, write_model_record, write_model_records,
    };

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn current_state_clones_retain_custody_until_the_final_owner_drops() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let owner = FileCurrentState::open(&site_yaml, "site-one").expect("initial owner");
        let final_owner = owner.clone();

        drop(owner);
        assert_lock_contended(FileCurrentState::open(&site_yaml, "site-one"));

        drop(final_owner);
        let replacement =
            FileCurrentState::open(&site_yaml, "site-one").expect("replacement owner");
        drop(replacement);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_parent_owner_unlocks_inherited_descriptor_without_releasing_fresh_owner() {
        const OUTER_PROCESS: &str = "DOTCORE_CURRENT_STATE_LOCK_OUTER_PROCESS";
        const INHERITED_CHILD: &str = "DOTCORE_CURRENT_STATE_LOCK_INHERITED_CHILD";
        const TEST_NAME: &str = "store::site::tests::final_parent_owner_unlocks_inherited_descriptor_without_releasing_fresh_owner";

        if std::env::var_os(INHERITED_CHILD).is_some() {
            inherited_lock_child();
            return;
        }
        if std::env::var_os(OUTER_PROCESS).is_none() {
            let output = Command::new(std::env::current_exe().expect("current test executable"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(OUTER_PROCESS, "1")
                .output()
                .expect("isolated lock custody test");
            dotcore_domain::test_support::child::assert_success(
                &output,
                "isolated lock custody test failed",
            );
            return;
        }

        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let owner = FileCurrentState::open(&site_yaml, "site-one").expect("initial owner");
        let inherited_fd = owner.file.lock_file().file.as_raw_fd();
        let lock_path = owner.file.lock_file().path.clone();
        set_close_on_exec(inherited_fd, false).expect("make lock descriptor inheritable");
        let child_result = dotcore_domain::test_support::child::current_test_command(TEST_NAME)
            .env(INHERITED_CHILD, "1")
            .env("DOTCORE_CURRENT_STATE_LOCK_FD", inherited_fd.to_string())
            .env("DOTCORE_CURRENT_STATE_LOCK_PATH", &lock_path)
            .stdin(Stdio::piped())
            .spawn();
        set_close_on_exec(inherited_fd, true).expect("restore close-on-exec");
        let mut child = child_result.expect("inherited lock child");
        let mut child_stdin = child.stdin.take().expect("child stdin");
        let child_stdout = child.stdout.take().expect("child stdout");
        let mut child_stderr = child.stderr.take().expect("child stderr");
        let mut child_stdout = io::BufReader::new(child_stdout);
        let mut output = String::new();
        loop {
            let mut line = String::new();
            let bytes = child_stdout.read_line(&mut line).expect("child output");
            if bytes == 0 {
                let mut stderr = String::new();
                child_stderr
                    .read_to_string(&mut stderr)
                    .expect("child stderr");
                panic!(
                    "child exited before retaining inherited lock descriptor: {output}\nstderr:\n{stderr}"
                );
            }
            output.push_str(&line);
            if line.contains("CURRENT_STATE_LOCK_READY ") {
                break;
            }
        }

        drop(owner);
        let replacement = FileCurrentState::open(&site_yaml, "site-one")
            .expect("fresh owner after explicit unlock");
        assert_lock_contended(FileCurrentState::open(&site_yaml, "site-one"));

        child_stdin
            .write_all(b"release\n")
            .expect("release inherited child");
        drop(child_stdin);
        let status = child.wait().expect("inherited child status");
        let mut stderr = String::new();
        child_stderr
            .read_to_string(&mut stderr)
            .expect("child stderr");
        assert!(
            status.success(),
            "inherited lock child failed: {output}\nstderr:\n{stderr}"
        );
        assert_lock_contended(FileCurrentState::open(&site_yaml, "site-one"));

        drop(replacement);
        let successor = FileCurrentState::open(&site_yaml, "site-one")
            .expect("successor after fresh owner drop");
        drop(successor);
    }

    #[test]
    fn file_current_state_persists_beside_site_yaml_and_reopens_its_state() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let file = FileCurrentState::open(&site_yaml, "site-one").expect("file state");
        assert_eq!(
            file.path().expect("state path"),
            directory.path.join("state.pb")
        );
        write_model_record(&file, deployment("deployment-one")).expect("record write");

        let bytes = fs::read(directory.path.join("state.pb")).expect("state bytes");
        let persisted = SiteCurrentState::decode(bytes.as_slice()).expect("typed current state");
        assert_eq!(persisted.site_id, "site-one");
        assert_eq!(persisted.storage_revision, 1);
        assert_eq!(persisted.model_deployments, [deployment("deployment-one")]);
        drop(file);
        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("reopen state");
        assert_eq!(stored(&reopened).expect("reopened state"), persisted);
    }

    #[test]
    fn model_observed_placement_round_trips_exact_values_after_reopen() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let file = FileCurrentState::open(&site_yaml, "site-one").expect("file state");
        let mut model = deployment("model-one");
        model.state.as_mut().expect("model state").status = ModelDeploymentStatus::Ready as i32;
        model
            .state
            .as_mut()
            .expect("model state")
            .observed_placements = vec![ObservedPlacement {
            managed_node_id: "worker-two".to_owned(),
            accelerator_device_ids: vec!["GPU-abc".to_owned(), "GPU-def".to_owned()],
        }];

        write_model_record(&file, model).expect("Model state write");
        drop(file);

        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("reopen state");
        let deployment = &stored(&reopened)
            .expect("reopened model snapshot")
            .model_deployments[0];
        assert_eq!(
            deployment
                .state
                .as_ref()
                .expect("reopened model state")
                .observed_placements,
            vec![ObservedPlacement {
                managed_node_id: "worker-two".to_owned(),
                accelerator_device_ids: vec!["GPU-abc".to_owned(), "GPU-def".to_owned()],
            }]
        );
    }

    #[test]
    fn interleaved_model_and_application_writes_preserve_both_sections_after_reopen() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let file = FileCurrentState::open(&site_yaml, "site-one").expect("file state");

        write_model_record(&file, deployment("model-one")).expect("Model state write");
        write_application_record(&file, application_deployment("application-one"))
            .expect("Application state write");
        write_model_record(&file, deployment("model-two")).expect("interleaved model state write");

        assert_eq!(
            stored(&file)
                .expect("model snapshot")
                .model_deployments
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["model-one", "model-two"]
        );
        assert_eq!(
            stored(&file)
                .expect("Application snapshot")
                .application_deployments[0]
                .application_deployment_id,
            "application-one"
        );
        drop(file);

        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("reopened state");
        assert_eq!(
            stored(&reopened)
                .expect("reopened model snapshot")
                .model_deployments
                .len(),
            2
        );
        assert_eq!(
            stored(&reopened)
                .expect("reopened application snapshot")
                .application_deployments[0]
                .application_deployment_id,
            "application-one"
        );
    }

    #[test]
    fn parentless_site_path_is_a_source_less_storage_diagnostic() {
        let error = match FileCurrentState::open(Path::new("/"), "site-one") {
            Ok(_) => panic!("filesystem root must have no parent"),
            Err(error) => error,
        };
        let StoreError::Storage(source) = error else {
            panic!("expected storage diagnostic");
        };

        assert_eq!(
            source.to_string(),
            "current-state site.yaml path has no parent: /"
        );
        assert!(source.source().is_none());
    }

    #[test]
    fn file_current_state_reopen_fences_corrupt_oversized_and_mismatched_state() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let state_path = directory.path.join("state.pb");
        fs::write(&state_path, [1_u8, 2, 3]).expect("corrupt bytes");
        assert!(matches!(
            FileCurrentState::open(&site_yaml, "site-one"),
            Err(StoreError::Decode(_))
        ));

        let valid = SiteCurrentState {
            format_version: crate::store::FORMAT_VERSION,
            site_id: "site-two".to_owned(),
            storage_revision: 0,
            model_deployments: Vec::new(),
            application_deployments: Vec::new(),
            manager_model_lifecycle_records: Vec::new(),
            manager_application_lifecycle_records: Vec::new(),
        };
        fs::write(&state_path, valid.encode_to_vec()).expect("mismatched state");
        assert!(matches!(
            FileCurrentState::open(&site_yaml, "site-one"),
            Err(StoreError::SiteMismatch)
        ));

        fs::OpenOptions::new()
            .write(true)
            .open(&state_path)
            .expect("open state")
            .set_len(4 * 1024 * 1024 + 1)
            .expect("oversize state");
        assert!(matches!(
            FileCurrentState::open(&site_yaml, "site-one"),
            Err(StoreError::LimitExceeded)
        ));
    }

    #[test]
    fn oversized_model_and_application_updates_preserve_bytes_and_reopen() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let state_path = directory.path.join("state.pb");
        let file = FileCurrentState::open(&site_yaml, "site-one").expect("file state");
        write_model_record(&file, deployment("model-one")).expect("initial model write");
        let before_model = fs::read(&state_path).expect("model bytes");
        let before_snapshot = stored(&file).expect("model snapshot");
        assert!(matches!(
            write_model_record(&file, oversized_deployment("model-two")),
            Err(StoreError::LimitExceeded)
        ));
        assert_eq!(
            fs::read(&state_path).expect("unchanged model bytes"),
            before_model
        );
        assert_eq!(
            stored(&file).expect("unchanged model snapshot"),
            before_snapshot
        );

        write_application_record(&file, application_deployment("application-one"))
            .expect("initial application write");
        let before_application = fs::read(&state_path).expect("application bytes");
        let application_snapshot = stored(&file).expect("application snapshot");
        assert!(matches!(
            write_application_record(&file, oversized_application_deployment("application-two")),
            Err(StoreError::LimitExceeded)
        ));
        assert_eq!(
            fs::read(&state_path).expect("unchanged application bytes"),
            before_application
        );
        assert_eq!(
            stored(&file).expect("unchanged application snapshot"),
            application_snapshot
        );
        drop(file);
        FileCurrentState::open(&site_yaml, "site-one").expect("reopen after rejection");
    }

    #[test]
    fn oversized_model_update_is_rejected_before_temporary_file_creation() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let file = FileCurrentState::open_with_filesystem(
            &site_yaml,
            "site-one".to_owned(),
            Arc::new(FailingFileSystem::new(FailurePoint::Create)),
        )
        .expect("file state");
        assert!(matches!(
            write_model_record(&file, oversized_deployment("model-one")),
            Err(StoreError::LimitExceeded)
        ));
    }

    #[test]
    fn file_current_state_faults_preserve_or_poison_state_at_the_durability_boundary() {
        for failure in [FailurePoint::Write, FailurePoint::Rename] {
            let directory = TestDirectory::new();
            let site_yaml = directory.path.join("site.yaml");
            fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
            let initial = FileCurrentState::open(&site_yaml, "site-one").expect("initial state");
            write_model_record(&initial, deployment("deployment-one")).expect("initial write");
            let before = stored(&initial).expect("initial snapshot");
            let bytes = fs::read(directory.path.join("state.pb")).expect("initial bytes");
            drop(initial);

            let state = FileCurrentState::open_with_filesystem(
                &site_yaml,
                "site-one".to_owned(),
                Arc::new(FailingFileSystem::new(failure)),
            )
            .expect("faulting state");
            let error = write_model_record(&state, deployment("deployment-two"))
                .expect_err("injected storage failure");
            assert!(matches!(error, StoreError::Storage(_)));
            let mut cause = error.source();
            let mut found_io_error = false;
            while let Some(value) = cause {
                found_io_error |= value.downcast_ref::<io::Error>().is_some();
                cause = value.source();
            }
            assert!(
                found_io_error,
                "storage source chain must retain the I/O error"
            );
            assert_eq!(stored(&state).expect("memory rollback"), before);
            assert_eq!(
                fs::read(directory.path.join("state.pb")).expect("disk rollback"),
                bytes
            );
        }

        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let initial = FileCurrentState::open(&site_yaml, "site-one").expect("initial state");
        drop(initial);
        let state = FileCurrentState::open_with_filesystem(
            &site_yaml,
            "site-one".to_owned(),
            Arc::new(FailingFileSystem::new(FailurePoint::DirectorySync)),
        )
        .expect("faulting state");
        assert!(matches!(
            write_model_record(&state, deployment("deployment-one")),
            Err(StoreError::OutcomeUncertain(_))
        ));
        assert!(matches!(stored(&state), Err(StoreError::Poisoned)));
    }

    #[test]
    fn lifecycle_reservation_write_failure_exposes_no_record_in_memory_or_after_restart() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let initial = FileCurrentState::open(&site_yaml, "site-one").expect("initial state");
        initial
            .reserve_lifecycle_generation(
                dotcore_domain::deployment::DeploymentOwner::Model,
                "deployment-one",
            )
            .expect("initial reservation");
        let before = stored(&initial).expect("initial snapshot");
        let bytes = fs::read(directory.path.join("state.pb")).expect("initial bytes");
        drop(initial);

        let faulting = FileCurrentState::open_with_filesystem(
            &site_yaml,
            "site-one".to_owned(),
            Arc::new(FailingFileSystem::new(FailurePoint::Write)),
        )
        .expect("faulting state");
        assert!(matches!(
            faulting.reserve_lifecycle_generation(
                dotcore_domain::deployment::DeploymentOwner::Model,
                "deployment-one"
            ),
            Err(StoreError::Storage(_))
        ));
        assert_eq!(stored(&faulting).expect("memory rollback"), before);
        assert_eq!(
            fs::read(directory.path.join("state.pb")).expect("disk rollback"),
            bytes
        );
        drop(faulting);

        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("reopened state");
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "deployment-one"
                )
                .expect("high-water lookup"),
            Some(1)
        );
    }

    #[test]
    fn durable_high_water_skips_generation_when_matching_site_intent_is_absent_after_restart() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let current = FileCurrentState::open(&site_yaml, "site-one").expect("current state");
        assert_eq!(
            current
                .reserve_lifecycle_generation(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "deployment-one"
                )
                .expect("create reservation")
                .generation_high_water,
            1
        );
        let unused_resume = current
            .reserve_lifecycle_generation(
                dotcore_domain::deployment::DeploymentOwner::Model,
                "deployment-one",
            )
            .expect("resume reserved before site acceptance");
        assert_eq!(unused_resume.generation_high_water, 2);
        drop(current);

        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("restart");
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "deployment-one"
                )
                .expect("persisted high-water"),
            Some(2)
        );
        let next = reopened
            .reserve_lifecycle_generation(
                dotcore_domain::deployment::DeploymentOwner::Model,
                "deployment-one",
            )
            .expect("next reservation");
        assert_eq!(next.generation_high_water, 3);
    }
    #[test]
    fn retired_receipt_and_resume_fields_are_ignored_on_decode() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let lifecycle = dotcore_proto::dotcore::site::store::v1::ManagerLifecycleRecord {
            deployment_id: "model-one".to_owned(),
            generation_high_water: 3,
        };
        let mut old_lifecycle = lifecycle.encode_to_vec();
        // Retired Resume generation (field 3) and authorization identity (field 4).
        old_lifecycle.extend([0x18, 0x03, 0x22, 0x08]);
        old_lifecycle.extend(b"resume-3");
        let mut bytes = SiteCurrentState {
            format_version: crate::store::FORMAT_VERSION,
            site_id: "site-one".to_owned(),
            storage_revision: 7,
            ..Default::default()
        }
        .encode_to_vec();
        prost::encoding::encode_key(9, prost::encoding::WireType::LengthDelimited, &mut bytes);
        prost::encoding::encode_varint(old_lifecycle.len() as u64, &mut bytes);
        bytes.extend(old_lifecycle);
        // Retired manager cleanup receipt (field 8).
        prost::encoding::string::encode(8, &"retired-receipt".to_owned(), &mut bytes);
        // Retired lifecycle upgrade plan (field 6) left by development sites.
        prost::encoding::string::encode(6, &"retired-plan".to_owned(), &mut bytes);
        fs::write(directory.path.join("state.pb"), bytes).expect("old-format state");

        let current = FileCurrentState::open(&site_yaml, "site-one").expect("old-format state");

        assert_eq!(
            current
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-one"
                )
                .expect("high-water"),
            Some(3)
        );
    }

    #[test]
    fn startup_open_raises_high_water_from_site_yaml_without_writing_it() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        let site = site_with_deployment("model-one", 5);
        let site_bytes = dotcore_proto::yaml::to_string(&site).expect("site YAML");
        fs::write(&site_yaml, &site_bytes).expect("Site input");
        let record = CurrentModelDeployment {
            record_revision: 4,
            state: Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Ready as i32,
                status_message: "Ready".to_owned(),
                ..Default::default()
            }),
            ..deployment("model-one")
        };
        let state = SiteCurrentState {
            format_version: crate::store::FORMAT_VERSION,
            site_id: "site-one".to_owned(),
            storage_revision: 7,
            model_deployments: vec![record.clone()],
            manager_model_lifecycle_records: vec![
                dotcore_proto::dotcore::site::store::v1::ManagerLifecycleRecord {
                    deployment_id: "model-one".to_owned(),
                    generation_high_water: 3,
                },
            ],
            ..Default::default()
        };
        fs::write(directory.path.join("state.pb"), state.encode_to_vec()).expect("owner state");

        let current = FileCurrentState::open_at_startup(
            &site_yaml,
            &crate::test_support::site_declaration_rules(),
        )
        .expect("startup open");

        assert_eq!(
            fs::read_to_string(&site_yaml).expect("unchanged site"),
            site_bytes
        );
        assert_eq!(
            current
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-one"
                )
                .expect("high-water"),
            Some(5),
            "high-water resyncs from the site.yaml generation"
        );
        let snapshot = stored(&current).expect("current records");
        assert_eq!(snapshot.storage_revision, 8);
        assert_eq!(snapshot.model_deployments, [record]);
        drop(current);
        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("restart");
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-one"
                )
                .expect("persisted high-water"),
            Some(5)
        );
    }

    #[test]
    fn startup_open_rejects_a_site_without_identity_allocator_and_writes_nothing() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        let site = Site {
            model_deployment_identity_allocator: None,
            ..site_with_deployment("model-one", 5)
        };
        let site_bytes = dotcore_proto::yaml::to_string(&site).expect("site YAML");
        fs::write(&site_yaml, &site_bytes).expect("Site input");

        let error = match FileCurrentState::open_at_startup(
            &site_yaml,
            &crate::test_support::site_declaration_rules(),
        ) {
            Ok(_) => panic!("a site without allocator must not open"),
            Err(error) => error,
        };

        let StoreError::SiteInput(error) = error else {
            panic!("expected a site input error, got {error:?}");
        };
        assert_eq!(
            error.to_string(),
            "Invalid desired site: site.yaml model deployment identity allocator is missing"
        );
        assert_eq!(
            fs::read_to_string(&site_yaml).expect("unchanged site"),
            site_bytes
        );
        assert!(!directory.path.join("state.pb").exists());
    }

    #[test]
    fn pruning_removes_only_named_lifecycle_records_and_skips_unchanged_writes() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let current = FileCurrentState::open(&site_yaml, "site-one").expect("current state");
        for deployment_id in ["model-gone", "model-gone", "model-live"] {
            current
                .reserve_lifecycle_generation(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    deployment_id,
                )
                .expect("reserved generation");
        }
        let reserved = stored(&current).expect("reserved").storage_revision;

        current
            .prune_lifecycle_records(
                dotcore_domain::deployment::DeploymentOwner::Model,
                &BTreeSet::from(["model-gone", "model-unknown"]),
            )
            .expect("pruned finalized record");
        let pruned = stored(&current).expect("pruned").storage_revision;
        current
            .prune_lifecycle_records(
                dotcore_domain::deployment::DeploymentOwner::Model,
                &BTreeSet::from(["model-gone"]),
            )
            .expect("repeated pruning");

        assert_eq!(pruned, reserved + 1);
        assert_eq!(
            stored(&current).expect("unchanged").storage_revision,
            pruned
        );
        drop(current);
        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("restart");
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-gone"
                )
                .expect("pruned high-water"),
            None
        );
        assert_eq!(
            reopened
                .lifecycle_high_water(
                    dotcore_domain::deployment::DeploymentOwner::Model,
                    "model-live"
                )
                .expect("retained high-water"),
            Some(1)
        );
    }

    fn site_with_deployment(deployment_id: &str, lifecycle_generation: u64) -> Site {
        use dotcore_proto::dotcore::{
            catalog::v1::{ModelParallelShape, OciPlatform},
            deployment::v1::ExactPlacement,
            model::v1::{ModelDeploymentDeclaration, ModelDeploymentLifecycleTransition},
            site::v1::{
                ExecutionTarget, ModelDeploymentIdentityAllocator, ModelRuntimeBinding,
                SimulatedRuntimeAdapter, SimulatedRuntimeScenario, execution_target,
                model_runtime_binding,
            },
        };

        Site {
            model_deployment_identity_allocator: Some(ModelDeploymentIdentityAllocator {
                namespace: crate::store::model_deployment_identity_namespace("site-one"),
                next_sequence: 1,
            }),
            execution_targets: vec![ExecutionTarget {
                execution_target_id: "target-one".to_owned(),
                display_name: "target-one".to_owned(),
                site_node_id: "node-one".to_owned(),
                accelerator_device_ids: Vec::new(),
                runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                    ModelRuntimeBinding {
                        adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                            SimulatedRuntimeAdapter {
                                default_scenario: Some(SimulatedRuntimeScenario::default()),
                                deployment_scenarios: Vec::new(),
                            },
                        )),
                    },
                )),
                platform: Some(OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                }),
            }],
            model_deployments: vec![ModelDeploymentDeclaration {
                model_deployment_id: deployment_id.to_owned(),
                model_deployment_name: "Model".to_owned(),
                placement: Some(ExactPlacement {
                    site_node_id: "node-one".to_owned(),
                    execution_target_id: "target-one".to_owned(),
                }),
                requested_parallel_shape: ModelParallelShape::One as i32,
                snapshot: Some(dotcore_domain::test_support::model::local_model_snapshot(
                    deployment_id,
                )),
                desired_state: ModelDeploymentDesiredState::Present as i32,
                upstream_desired_configuration: None,
                lifecycle_generation,
                lifecycle_transition: ModelDeploymentLifecycleTransition::Resume as i32,
                lifecycle_recovery_required: false,
                node_resolved_launch_identities: Vec::new(),
            }],
            ..base_site()
        }
    }

    fn base_site() -> Site {
        Site {
            site_id: "site-one".to_owned(),
            nodes: vec![SiteNode {
                node_id: "node-one".to_owned(),
                host: "127.0.0.1".to_owned(),
                display_name: "Node One".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn opening_never_publishes_a_partial_state() {
        let directory = TestDirectory::new();
        let site_yaml = directory.path.join("site.yaml");
        fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let state = FileCurrentState::open_with_filesystem(
            &site_yaml,
            "site-one".to_owned(),
            Arc::new(FailingFileSystem::new(FailurePoint::Write)),
        )
        .expect("unprepared state");

        assert!(matches!(
            write_model_records(
                &state,
                vec![deployment("deployment-one"), deployment("deployment-two")],
            ),
            Err(StoreError::Storage(_))
        ));
        assert!(
            !directory.path.join("state.pb").exists(),
            "failed open must not publish a partial current state"
        );
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

    #[derive(Clone, Copy)]
    enum FailurePoint {
        Create,

        Write,

        Rename,

        DirectorySync,
    }

    struct FailingFileSystem {
        point: FailurePoint,
        inner: StdFileSystem,
    }

    impl FailingFileSystem {
        fn new(point: FailurePoint) -> Self {
            Self {
                point,
                inner: StdFileSystem,
            }
        }
    }

    impl FileSystem for FailingFileSystem {
        fn create_temp(&self, path: &Path) -> io::Result<(PathBuf, std::fs::File)> {
            if matches!(self.point, FailurePoint::Create) {
                return Err(io::Error::other("injected create failure"));
            }
            self.inner.create_temp(path)
        }

        fn write_all(&self, file: &mut std::fs::File, bytes: &[u8]) -> io::Result<()> {
            if matches!(self.point, FailurePoint::Write) {
                Err(io::Error::other("injected write failure"))
            } else {
                self.inner.write_all(file, bytes)
            }
        }

        fn sync_file(&self, file: &std::fs::File) -> io::Result<()> {
            self.inner.sync_file(file)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if matches!(self.point, FailurePoint::Rename) {
                Err(io::Error::other("injected rename failure"))
            } else {
                self.inner.rename(from, to)
            }
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            if matches!(self.point, FailurePoint::DirectorySync) {
                Err(io::Error::other("injected directory sync failure"))
            } else {
                self.inner.sync_directory(path)
            }
        }
    }

    fn assert_lock_contended(result: Result<FileCurrentState, StoreError>) {
        match result {
            Err(StoreError::Storage(source)) => {
                let source = source
                    .root_cause()
                    .downcast_ref::<io::Error>()
                    .expect("lock contention IO source");
                assert_eq!(source.kind(), io::ErrorKind::WouldBlock);
            }
            Err(error) => panic!("unexpected lock acquisition error: {error:?}"),
            Ok(_) => panic!("concurrent current-state owner acquired custody"),
        }
    }

    #[cfg(target_os = "linux")]
    fn inherited_lock_child() {
        let fd = std::env::var("DOTCORE_CURRENT_STATE_LOCK_FD")
            .expect("inherited lock descriptor")
            .parse::<RawFd>()
            .expect("numeric inherited lock descriptor");
        let expected_path = PathBuf::from(
            std::env::var_os("DOTCORE_CURRENT_STATE_LOCK_PATH").expect("inherited lock path"),
        );
        let inherited_path =
            fs::read_link(format!("/proc/self/fd/{fd}")).expect("inherited lock descriptor target");
        assert_eq!(inherited_path, expected_path);
        println!(
            "CURRENT_STATE_LOCK_READY fd={fd} path={}",
            inherited_path.display()
        );
        io::stdout().flush().expect("flush inherited lock marker");

        let mut release = [0_u8; 1];
        io::stdin()
            .read_exact(&mut release)
            .expect("parent release signal");
    }

    #[cfg(target_os = "linux")]
    fn set_close_on_exec(fd: RawFd, enabled: bool) -> io::Result<()> {
        // SAFETY: fcntl reads or updates flags for a live descriptor without taking ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        let updated = if enabled {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        // SAFETY: the descriptor remains owned by CurrentStateFileLock and updated is a flag set.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let path = dotcore_domain::test_support::canonical_temp_dir().join(format!(
                "dotcore-current-state-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
