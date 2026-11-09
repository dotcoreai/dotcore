//! Offline deletion of a stopped site.
//!
//! The command locks the site, lists the runtime resources it owns, asks the operator to confirm,
//! removes those resources, and then removes the site directory. It acts on nothing a running
//! backend holds, and it touches only resources whose ownership evidence names the site, or, for
//! native workloads, places them in the site directory.

use std::{io::Write, path::Path, sync::Arc};

use anyhow::Context;
use clap::Args;
use dotcore_runtime::SiteRuntimeCleanup;
use dotcore_site::reset::{
    persistence::SitePersistenceReset,
    transaction::{
        SiteDeleteTransaction, SiteResetLockError, SiteResetSession, SiteResetTransactionError,
    },
};

use crate::{
    RootPathArgs,
    runtime_confirmation::{
        RuntimeRemovalApproval, RuntimeRemovalArgs, approve_runtime_removal,
        native_cleanup_failure, site_runtime_cleanup, site_runtime_cleanups,
    },
    terminal::{Terminal, open_terminal},
};

/// Arguments of `dotc site delete`.
#[derive(Debug, Args)]
pub struct SiteDeleteArgs {
    /// Never prompt; a site that owns runtime resources then requires --yes.
    #[arg(long)]
    pub non_interactive: bool,

    #[command(flatten)]
    pub removal: RuntimeRemovalArgs,
}

/// Deletes the site using the process terminal and the runtimes its configuration binds.
pub(crate) async fn delete(arguments: SiteDeleteArgs, paths: &RootPathArgs) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    let cache_root = paths.configured_cache_root()?;
    let mut terminal = open_terminal();
    delete_with(
        arguments,
        &site_dir,
        cache_root.as_deref(),
        terminal.as_mut(),
        site_runtime_cleanup,
    )
    .await
}

async fn delete_with(
    arguments: SiteDeleteArgs,
    site_dir: &Path,
    cache_root: Option<&Path>,
    terminal: &mut dyn Terminal,
    select_runtime: impl FnOnce(&SiteResetSession) -> anyhow::Result<Box<dyn SiteRuntimeCleanup>>,
) -> anyhow::Result<()> {
    let cleanups = site_runtime_cleanups(cache_root);
    let session = if arguments.removal.skip_runtime_cleanup {
        SiteResetSession::begin_unidentified(site_dir, Arc::clone(&cleanups))
    } else {
        SiteResetSession::begin(site_dir, Arc::clone(&cleanups))
    }
    .map_err(|error| delete_failure(error, site_dir))?;
    let transaction = SiteDeleteTransaction::new(
        select_runtime(&session)?,
        Box::new(SitePersistenceReset::new(cleanups)),
    );

    let inventory = transaction
        .inventory(&session)
        .await
        .map_err(|error| delete_failure(error, site_dir))?;
    approve_runtime_removal(
        terminal,
        site_dir,
        &inventory,
        &RuntimeRemovalApproval {
            subject: format!("Site at {}", site_dir.display()),
            verb: "delete",
            question: "Remove them and delete the site?",
            yes: arguments.removal.yes,
            non_interactive: arguments.non_interactive,
            skip_runtime_cleanup: arguments.removal.skip_runtime_cleanup,
        },
    )?;

    transaction
        .delete(session)
        .await
        .map_err(|error| delete_failure(error, site_dir))?;
    writeln!(
        std::io::stdout().lock(),
        "Site deleted\nDOTCORE_SITE_DIR={}",
        site_dir.display()
    )
    .context("write site delete result")
}

fn delete_failure(error: SiteResetTransactionError, site_dir: &Path) -> anyhow::Error {
    let context = match &error {
        SiteResetTransactionError::Lock(SiteResetLockError::SiteRunning { .. }) => format!(
            "cannot delete site at {}; stop the backend and retry",
            site_dir.display()
        ),
        SiteResetTransactionError::Lock(SiteResetLockError::NodeRunning { .. }) => format!(
            "cannot delete the node folder at {}; stop the node server and retry",
            site_dir.display()
        ),
        SiteResetTransactionError::ExistingSiteIdentity(_) => format!(
            "cannot identify the site at {}; repair it, or pass --skip-runtime-cleanup to delete it without runtime cleanup",
            site_dir.display()
        ),
        SiteResetTransactionError::RuntimeCleanup(_) => format!(
            "Runtime cleanup failed for the site at {}; nothing was deleted. Fix the cause, or pass --skip-runtime-cleanup to delete without runtime cleanup",
            site_dir.display()
        ),
        SiteResetTransactionError::NativeCleanup(_) => native_cleanup_failure("delete", site_dir),
        _ => format!("offline site delete failed at {}", site_dir.display()),
    };
    anyhow::Error::new(error).context(context)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use dotcore_domain::timing::Timing;

    use super::*;
    use crate::{
        terminal::{
            Message, PromptError, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal,
            TerminalCondition,
        },
        test_support::FakeSiteRuntime,
    };

    #[test]
    fn empty_inventory_deletes_without_prompting() {
        let fixture = Fixture::new(&[]);
        let mut terminal = ScriptedTerminal::interactive([]);

        fixture
            .run(&mut terminal, false, false, false)
            .expect("delete");

        assert!(terminal.prompts().is_empty());
        assert!(!fixture.site_dir.exists());
        assert_eq!(
            fixture.calls(),
            ["inventory:test-site", "cleanup:test-site"]
        );
    }

    #[test]
    fn confirmed_deletion_lists_resources_cleans_runtime_then_removes_the_site() {
        let fixture = Fixture::new(&["model-one", "app-one"]);
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        fixture
            .run(&mut terminal, false, false, false)
            .expect("delete");

        assert!(!fixture.site_dir.exists());
        assert_eq!(
            fixture.calls(),
            ["inventory:test-site", "cleanup:test-site"]
        );
        assert_eq!(
            terminal.messages(),
            [
                Message::Heading(format!(
                    "Site at {} owns these resources",
                    fixture.site_dir.display()
                )),
                Message::KeyValues(vec![
                    ("Test Runtime".to_owned(), "model-one".to_owned()),
                    ("Test Runtime".to_owned(), "app-one".to_owned()),
                ]),
            ]
        );
        assert!(matches!(terminal.prompts(), [ScriptedPrompt::Confirm(_)]));
    }

    #[test]
    fn declined_deletion_is_a_cancel_and_changes_nothing() {
        let fixture = Fixture::new(&["model-one"]);
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let error = fixture
            .run(&mut terminal, false, false, false)
            .expect_err("declined");

        assert!(matches!(
            error.downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        assert!(fixture.site_dir.join("site.yaml").is_file());
        assert_eq!(fixture.calls(), ["inventory:test-site"]);
    }

    #[test]
    fn missing_terminal_requires_yes_when_resources_exist() {
        let fixture = Fixture::new(&["model-one"]);
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error = fixture
            .run(&mut terminal, false, false, false)
            .expect_err("needs confirmation");

        assert_eq!(
            error.to_string(),
            format!(
                "Site at {} owns 1 runtime resources; pass --yes to delete without confirmation",
                fixture.site_dir.display()
            )
        );
        assert!(fixture.site_dir.join("site.yaml").is_file());

        fixture
            .run(&mut terminal, true, false, false)
            .expect("--yes deletes");

        assert!(!fixture.site_dir.exists());
        assert_eq!(
            fixture.calls(),
            [
                "inventory:test-site",
                "inventory:test-site",
                "cleanup:test-site"
            ]
        );
    }

    #[test]
    fn non_interactive_requires_yes_even_with_a_terminal() {
        let fixture = Fixture::new(&["model-one"]);
        let mut terminal = ScriptedTerminal::interactive([]);

        let error = fixture
            .run(&mut terminal, false, false, true)
            .expect_err("needs --yes");

        assert_eq!(
            error.to_string(),
            format!(
                "Site at {} owns 1 runtime resources; pass --yes to delete without confirmation",
                fixture.site_dir.display()
            )
        );
        assert!(terminal.prompts().is_empty());
        assert!(fixture.site_dir.join("site.yaml").is_file());
        assert_eq!(fixture.calls(), ["inventory:test-site"]);
    }

    #[test]
    fn runtime_failure_keeps_the_site_and_names_the_skip_flag() {
        let fixture = Fixture::new(&["model-one"]);
        fixture.fail_cleanup();
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        let error = fixture
            .run(&mut terminal, false, false, false)
            .expect_err("runtime failure");

        assert_eq!(
            format!("{error:#}"),
            format!(
                "Runtime cleanup failed for the site at {}; nothing was deleted. Fix the cause, or pass --skip-runtime-cleanup to delete without runtime cleanup: site runtime cleanup failed: injected cleanup failure: injected cleanup failure",
                fixture.site_dir.display()
            )
        );
        assert!(fixture.site_dir.join("site.yaml").is_file());
    }

    #[test]
    fn skip_runtime_cleanup_deletes_an_unidentifiable_site_without_touching_the_runtime() {
        let fixture = Fixture::new(&["model-one"]);
        fs::write(fixture.site_dir.join("site.yaml"), "siteId: [\n").expect("broken site");
        let mut terminal = ScriptedTerminal::interactive([]);

        let error = fixture
            .run(&mut terminal, true, false, false)
            .expect_err("unidentifiable without --force");
        assert!(format!("{error:#}").contains("pass --skip-runtime-cleanup"));

        fixture
            .run(&mut terminal, false, true, false)
            .expect("delete without runtime cleanup");

        assert!(!fixture.site_dir.exists());
        assert!(fixture.calls().is_empty());
    }

    #[test]
    fn running_backend_blocks_deletion() {
        let fixture = Fixture::new(&["model-one"]);
        let _held = SiteResetSession::begin(
            &fixture.site_dir,
            crate::runtime_confirmation::site_runtime_cleanups(None),
        )
        .expect("held lock");
        let mut terminal = ScriptedTerminal::interactive([]);

        let error = fixture
            .run(&mut terminal, true, false, false)
            .expect_err("lock held");

        assert_eq!(
            error.to_string(),
            format!(
                "cannot delete site at {}; stop the backend and retry",
                fixture.site_dir.display()
            )
        );
        assert!(fixture.site_dir.join("site.yaml").is_file());
    }

    #[test]
    fn native_workloads_are_listed_and_confirmed_and_never_skipped() {
        let fixture = Fixture::new(&["model-one"]);
        let native = NativeWorkload::start(&fixture.site_dir);
        let mut declining = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let declined = fixture.run(&mut declining, false, true, false);

        assert!(matches!(
            declined
                .expect_err("declined")
                .downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        assert!(native.engine_runs());
        assert!(fixture.site_dir.join("site.yaml").is_file());
        let listed = Message::KeyValues(vec![(
            "Native runtime".to_owned(),
            format!(
                "workload {} on execution target mac-gpu (running)",
                native.workload_id
            ),
        )]);
        assert_eq!(declining.messages()[1], listed);

        let mut confirming = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);
        fixture
            .run(&mut confirming, false, true, false)
            .expect("delete");

        assert_eq!(confirming.messages()[1], listed);
        assert!(!native.engine_runs());
        assert!(!fixture.site_dir.exists());
        assert!(
            fixture.calls().is_empty(),
            "the skipped runtime is never asked"
        );
    }

    #[test]
    fn an_unknown_runtime_binding_names_the_skip_flag() {
        let fixture = Fixture::new(&[]);
        fs::write(
            fixture.site_dir.join("site.yaml"),
            "siteId: test-site\nexecutionTargets:\n- modelRuntime: { futureRuntime: {} }\n",
        )
        .expect("site YAML");
        let session = SiteResetSession::begin(
            &fixture.site_dir,
            crate::runtime_confirmation::site_runtime_cleanups(None),
        )
        .expect("session");

        let error = match site_runtime_cleanup(&session) {
            Ok(_) => panic!("unknown binding selected a cleanup"),
            Err(error) => error,
        };

        assert_eq!(
            format!("{error:#}"),
            format!(
                "cannot select runtime cleanup for site at {}; repair its execution targets, or pass --skip-runtime-cleanup to remove it without Docker runtime cleanup (native workloads are still removed): an execution target binds unknown runtime adapter \"futureRuntime\", whose resources site cleanup cannot classify",
                fixture.site_dir.display()
            )
        );
    }

    /// Native workload started in a site directory through the public native runtime, with a fake
    /// engine that records its process id. It watches the test process, so it stops even when the
    /// test fails first.
    struct NativeWorkload {
        _roots: tempfile::TempDir,
        workload_id: String,
        pid_file: PathBuf,
    }

    impl NativeWorkload {
        fn start(site_dir: &Path) -> Self {
            use std::collections::BTreeMap;

            use dotcore_domain::{ExecutionPlatform, SiteNodeId};
            use dotcore_runtime::{
                ExecutionTarget, RuntimeBackend, WorkloadExecutable, WorkloadExecutableArtifact,
                WorkloadIpcMode, WorkloadLaunch, WorkloadSpec, WorkloadValue,
            };
            use dotcore_runtime_backends::native::{
                NativeRuntimeBackend, NativeRuntimeConfig, NativeStateRoot, NativeWorkloadLifetime,
            };

            let roots = crate::test_support::canonical_tempdir().expect("native roots");
            let store_path = roots
                .path()
                .join("store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fake");
            fs::create_dir_all(store_path.join("bin")).expect("fake store entry");
            let engine = store_path.join("bin/server");
            let path = std::env::var("PATH").expect("PATH");
            crate::test_support::write_executable_script(
                &engine,
                &format!(
                    "#!/bin/sh\nPATH='{path}'\necho $$ > \"$PID_FILE\"\ntrap 'exit 0' TERM\nwhile :; do sleep 0.05; done\n"
                ),
                0o755,
            );
            let pid_file = roots.path().join("engine.pid");
            let node = SiteNodeId::parse("node-a").expect("node");
            let operating_system = if cfg!(target_os = "macos") {
                "darwin"
            } else {
                std::env::consts::OS
            };
            let launch = WorkloadLaunch::new(
                "deployment-one",
                ExecutionTarget {
                    managed_node_id: node.clone(),
                    accelerator_device_ids: Vec::new(),
                },
                WorkloadSpec {
                    executable: WorkloadExecutable {
                        artifact: WorkloadExecutableArtifact::NixStorePath(
                            store_path.to_str().expect("UTF-8").to_owned(),
                        ),
                        path: "bin/server".to_owned(),
                    },
                    required_platform: ExecutionPlatform::normalize(
                        operating_system,
                        std::env::consts::ARCH,
                    )
                    .expect("host platform"),
                    arguments: Vec::new(),
                    environment: BTreeMap::from([(
                        "PID_FILE".to_owned(),
                        WorkloadValue::literal(pid_file.to_str().expect("UTF-8")),
                    )]),
                    artifacts: Vec::new(),
                    storage: Vec::new(),
                    ports: Vec::new(),
                    accelerator_device_ids: Vec::new(),
                    required_accelerator_device_count: 0,
                    ipc_mode: WorkloadIpcMode::Isolated,
                    expected_memory: Default::default(),
                },
            );
            let launcher = std::env::current_exe()
                .expect("test executable")
                .parent()
                .and_then(Path::parent)
                .expect("Cargo profile directory")
                .join("dotcore-launcher");
            assert!(
                launcher.is_file(),
                "build the launcher first: cargo build -p dotcore-launcher"
            );
            let backend = NativeRuntimeBackend::new(
                NativeRuntimeConfig {
                    state_root: NativeStateRoot::for_site(site_dir),
                    store_root: roots.path().join("store"),
                    approved_artifact_root: roots.path().to_owned(),
                    site_id: "test-site".to_owned(),
                    site_node_id: node,
                    execution_target_id: "mac-gpu".to_owned(),
                    lifetime: NativeWorkloadLifetime::Ephemeral,
                },
                launcher,
                Timing::test().native,
            )
            .expect("native backend");
            crate::build_runtime()
                .expect("runtime")
                .block_on(backend.create_workload(&launch))
                .expect("create native workload");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !pid_file.is_file() {
                assert!(std::time::Instant::now() < deadline, "engine never started");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Self {
                workload_id: format!("deployment-one_{}", launch.correlation.reconcile_generation),
                _roots: roots,
                pid_file,
            }
        }

        fn engine_runs(&self) -> bool {
            let pid = fs::read_to_string(&self.pid_file)
                .expect("engine pid")
                .trim()
                .parse::<libc::pid_t>()
                .expect("numeric pid");
            // SAFETY: signal 0 only checks that the process exists.
            unsafe { libc::kill(pid, 0) == 0 }
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        site_dir: PathBuf,
        runtime: FakeSiteRuntime,
    }

    impl Fixture {
        fn new(resources: &[&str]) -> Self {
            let root = crate::test_support::canonical_tempdir().expect("fixture root");
            let site_dir = root.path().join("site");
            fs::create_dir(&site_dir).expect("Site directory");
            fs::write(
                site_dir.join("site.yaml"),
                "siteId: test-site\nmodelDeployments: []\n",
            )
            .expect("site YAML");
            Self {
                _root: root,
                site_dir,
                runtime: FakeSiteRuntime::with_resources(resources),
            }
        }

        fn fail_cleanup(&self) {
            self.runtime.fail_cleanup();
        }

        fn calls(&self) -> Vec<String> {
            self.runtime.calls()
        }

        fn run(
            &self,
            terminal: &mut ScriptedTerminal,
            yes: bool,
            skip_runtime_cleanup: bool,
            non_interactive: bool,
        ) -> anyhow::Result<()> {
            let runtime = self.runtime.clone();
            crate::build_runtime()
                .expect("runtime")
                .block_on(delete_with(
                    SiteDeleteArgs {
                        non_interactive,
                        removal: RuntimeRemovalArgs {
                            yes,
                            skip_runtime_cleanup,
                        },
                    },
                    &self.site_dir,
                    None,
                    terminal,
                    move |_| Ok(Box::new(runtime)),
                ))
        }
    }
}
