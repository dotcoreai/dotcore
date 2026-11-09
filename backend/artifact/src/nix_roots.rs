//! Keeps the Nix store paths a node pins alive for as long as it pins them, and releases them
//! once it no longer does, through the garbage-collector root links in the node's root folder.
//!
//! Preparation creates one root link per pinned store path; this keeper removes links whose path
//! no pin names anymore, re-reading the pins under each root's lock so a path pinned meanwhile
//! keeps its root. At startup it also re-registers every pinned path's link with Nix, since a
//! copied, restored, or moved folder keeps links Nix no longer counts as roots.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use dotcore_domain::{clock::Clock, timing::ArtifactTiming};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{
    nix::{EXPERIMENTAL_FEATURES, NixCommand},
    nix_build::{NixPreparationError, lock_root, store_entry},
};

/// Directory of a node's garbage-collector roots, beside its node store.
pub const NIX_ROOTS_DIRECTORY: &str = "nix-roots";

/// The Nix store paths a node pins, read from the node's durable records.
pub trait PinnedStorePaths: Send + Sync {
    /// Returns every Nix store path the node pins now.
    fn pinned_store_paths(&self) -> anyhow::Result<BTreeSet<String>>;

    /// Returns a receiver whose value changes after each change of the pins.
    fn subscribe(&self) -> watch::Receiver<u64>;
}

/// Reconciles the root links of one node's root folder with the store paths it pins.
pub struct NixRootKeeper {
    roots: PathBuf,
    pins: Arc<dyn PinnedStorePaths>,
    command: NixCommand,
    clock: Arc<dyn Clock>,
    timing: ArtifactTiming,
    passes: watch::Sender<u64>,
}

impl NixRootKeeper {
    /// Keeps the roots in `roots` for the store paths `pins` names, waiting through `clock`, the
    /// backend's time source. A failed pass runs again after the retry interval of `timing` when
    /// no pin change triggers one sooner.
    pub fn new(
        roots: PathBuf,
        pins: Arc<dyn PinnedStorePaths>,
        timing: &ArtifactTiming,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            roots,
            pins,
            command: NixCommand::Discover,
            clock,
            timing: *timing,
            passes: watch::channel(0).0,
        }
    }

    /// Runs a startup pass, then a release pass after each pin change, until the task is aborted.
    /// A failed pass is logged and runs again after the retry delay or the next pin change.
    pub async fn run(self) {
        // Aborting the task stops it; nothing cancels its lock waits and Nix runs separately.
        let shutdown = CancellationToken::new();
        let mut changes = self.pins.subscribe();
        changes.borrow_and_update();
        let mut startup = true;
        loop {
            let retry = match self.pass(startup, &shutdown).await {
                Ok(()) => {
                    startup = false;
                    None
                }
                Err(error) => {
                    warn!(
                        event_kind = "nix_roots_pass_failed",
                        roots = %self.roots.display(),
                        error = %format!("{error:#}"),
                        "Nix root pass failed; retrying"
                    );
                    Some(self.clock.now() + self.timing.nix_root_retry_interval)
                }
            };
            self.passes
                .send_modify(|count| *count = count.wrapping_add(1));
            let retry_due = async {
                match retry {
                    Some(deadline) => self.clock.sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                changed = changes.changed() => if changed.is_err() {
                    return;
                },
                () = retry_due => {},
            }
            changes.borrow_and_update();
        }
    }

    /// Re-registers every pinned path's root when `startup`, then removes the roots of paths no
    /// pin names.
    async fn pass(&self, startup: bool, shutdown: &CancellationToken) -> anyhow::Result<()> {
        let pinned = self.pins.pinned_store_paths()?;
        let registered = if startup {
            self.register(&pinned, shutdown).await?
        } else {
            0
        };
        let released = self.release(&pinned, shutdown).await?;
        if registered > 0 || released > 0 {
            info!(
                event_kind = "nix_roots_reconciled",
                roots = %self.roots.display(),
                registered,
                released,
                "Nix roots reconciled with the node's pins"
            );
        }
        Ok(())
    }

    /// Links and registers with Nix each pinned path present in the store, returning how many.
    /// Absent paths are left to preparation, which builds them.
    async fn register(
        &self,
        pinned: &BTreeSet<String>,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<usize> {
        let mut registered = 0;
        for store_path in pinned {
            let Some(entry) = store_entry(store_path) else {
                continue;
            };
            let _lock = lock_root(&self.roots, entry, shutdown, None).await?;
            if !self.run_nix(&["path-info", store_path], shutdown).await? {
                continue;
            }
            let link = self.roots.join(entry);
            let link = link.to_string_lossy();
            if !self
                .run_nix(&["build", "--out-link", &link, store_path], shutdown)
                .await?
            {
                anyhow::bail!("nix could not root the pinned store path {store_path}");
            }
            registered += 1;
        }
        Ok(registered)
    }

    /// Removes every root whose store path no pin names, returning how many.
    async fn release(
        &self,
        pinned: &BTreeSet<String>,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<usize> {
        let mut released = 0;
        for (name, target) in links(&self.roots).await? {
            if pinned.contains(&target) {
                continue;
            }
            let _lock = lock_root(&self.roots, &name, shutdown, None).await?;
            // A deployment may have pinned the path since the pass read the pins.
            if self.pins.pinned_store_paths()?.contains(&target) {
                continue;
            }
            let link = self.roots.join(&name);
            if link_target(&link).await.as_deref() != Some(target.as_str()) {
                continue;
            }
            match tokio::fs::remove_file(&link).await {
                Ok(()) => released += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::Error::new(error).context("remove an unpinned Nix root"));
                }
            }
        }
        Ok(released)
    }

    /// Runs `nix` with `arguments` and returns whether it succeeded.
    async fn run_nix(
        &self,
        arguments: &[&str],
        shutdown: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let nix = self
            .command
            .executable()
            .ok_or(NixPreparationError::NixMissing)?;
        let status = tokio::process::Command::new(nix)
            .args(EXPERIMENTAL_FEATURES)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        tokio::select! {
            biased;
            () = shutdown.cancelled() => Err(NixPreparationError::Cancelled.into()),
            status = status => Ok(status
                .map_err(|error| anyhow::Error::new(error).context("run nix for a Nix root"))?
                .success()),
        }
    }

    /// Runs `nix` at `executable` instead of discovering it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_executable(mut self, executable: PathBuf) -> Self {
        self.command = NixCommand::Executable(executable);
        self
    }

    /// Returns a receiver whose value counts completed passes, failed ones included.
    #[cfg(any(test, feature = "test-support"))]
    pub fn passes(&self) -> watch::Receiver<u64> {
        self.passes.subscribe()
    }
}

/// Returns the name and target of every root link in `roots`, skipping its own dot-named
/// internals. A missing folder holds no roots.
async fn links(roots: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let mut entries = match tokio::fs::read_dir(roots).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(anyhow::Error::new(error).context("read the Nix root folder")),
    };
    let mut links = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| anyhow::Error::new(error).context("read the Nix root folder"))?
    {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if let Some(target) = link_target(&entry.path()).await {
            links.push((name, target));
        }
    }
    Ok(links)
}

/// Returns the target of the symbolic link at `link`, when it is one with a UTF-8 target.
async fn link_target(link: &Path) -> Option<String> {
    tokio::fs::read_link(link)
        .await
        .ok()
        .and_then(|target| target.to_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque, os::unix::fs::PermissionsExt as _, sync::Mutex, time::Duration,
    };

    use dotcore_domain::clock::{ManualClock, SystemClock};

    use super::*;
    use crate::nix_build::LOCK_DIRECTORY;

    const PINNED: &str = "/nix/store/00000000000000000000000000000001-pinned";
    const UNPINNED: &str = "/nix/store/00000000000000000000000000000002-unpinned";
    const ABSENT: &str = "/nix/store/00000000000000000000000000000003-absent";

    #[tokio::test]
    async fn startup_registers_present_pinned_paths_and_releases_unpinned_roots() {
        let fixture = Fixture::new();
        fixture.link(PINNED);
        fixture.link(UNPINNED);
        let pins = FakePins::new([[PINNED, ABSENT]]);

        let (keeper, _) = fixture.start(pins);
        keeper.wait_passes(1).await;

        assert_eq!(fixture.links(), [PINNED]);
        assert_eq!(
            fixture.calls(),
            [
                format!("path-info {PINNED}"),
                format!(
                    "build --out-link {} {PINNED}",
                    fixture.roots().join(entry(PINNED)).display()
                ),
                format!("path-info {ABSENT}"),
            ]
        );
        assert!(
            fixture.roots().join(LOCK_DIRECTORY).is_dir(),
            "dot-named internals stay"
        );
    }

    #[tokio::test]
    async fn a_root_is_released_by_the_pass_after_its_pin_goes() {
        let fixture = Fixture::new();
        fixture.link(PINNED);
        let pins = FakePins::new([[PINNED]]);
        let (keeper, pins) = fixture.start(pins);
        keeper.wait_passes(1).await;
        let startup_calls = fixture.calls().len();

        pins.set([]);
        keeper.wait_passes(2).await;

        assert!(fixture.links().is_empty());
        assert_eq!(
            fixture.calls().len(),
            startup_calls,
            "a release pass runs no nix"
        );
    }

    #[tokio::test]
    async fn a_path_pinned_during_a_pass_keeps_its_root() {
        let fixture = Fixture::new();
        fixture.link(PINNED);
        // The pass reads no pin, then the re-read under the root's lock finds the new pin.
        let pins = FakePins::new([vec![], vec![PINNED]]);

        let (keeper, _) = fixture.start(pins);
        keeper.wait_passes(1).await;

        assert_eq!(fixture.links(), [PINNED]);
    }

    #[tokio::test]
    async fn a_failed_pass_is_retried_after_the_retry_delay() {
        let fixture = Fixture::new();
        fixture.link(UNPINNED);
        fixture.fail_nix(true);
        let pins = FakePins::new([[PINNED]]);
        let (keeper, _) = fixture.start(pins);
        keeper.wait_passes(1).await;
        assert_eq!(
            fixture.links(),
            [UNPINNED],
            "a failed startup releases nothing"
        );

        fixture.fail_nix(false);
        keeper.clock.advance(RETRY);
        keeper.wait_passes(2).await;

        assert_eq!(fixture.links(), [PINNED]);
    }

    /// Adds a fresh path to the host store and checks which roots Nix reports for it while a
    /// keeper pins it, also after the root folder moves elsewhere, and once unpinned. Skipped
    /// without a host Nix.
    #[tokio::test]
    async fn host_nix_keeps_a_pinned_path_rooted_through_a_moved_root_folder() {
        let Some(nix) = dotcore_domain::nix::discover_nix() else {
            eprintln!("skipping: requires Nix");
            return;
        };
        let nix_store = nix.with_file_name("nix-store");
        let fixture = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("fixture root");
        let content = fixture.path().join("dotcore-nix-roots-test");
        std::fs::write(&content, format!("{:?}", std::time::SystemTime::now())).expect("content");
        let added = std::process::Command::new(&nix_store)
            .arg("--add")
            .arg(&content)
            .output()
            .expect("nix-store --add");
        assert!(added.status.success(), "nix-store --add: {added:?}");
        let store_path: &'static str = String::from_utf8(added.stdout)
            .expect("store path")
            .trim()
            .to_owned()
            .leak();
        // Lists the root links Nix counts for the path.
        let roots = || {
            let output = std::process::Command::new(&nix_store)
                .args(["--query", "--roots", store_path])
                .output()
                .expect("nix-store --query --roots");
            assert!(
                output.status.success(),
                "nix-store --query --roots: {output:?}"
            );
            String::from_utf8(output.stdout)
                .expect("roots")
                .lines()
                .filter_map(|line| line.split(" -> ").next().map(PathBuf::from))
                .collect::<Vec<_>>()
        };
        let start = |roots: PathBuf, pins: Arc<FakePins>| {
            let keeper = NixRootKeeper::new(
                roots,
                pins,
                &dotcore_domain::timing::Timing::production().artifact,
                Arc::new(SystemClock),
            )
            .with_executable(nix.clone());
            Running {
                passes: keeper.passes(),
                clock: ManualClock::new(),
                task: tokio::spawn(keeper.run()),
            }
        };
        let pins = FakePins::new([[store_path]]);
        let link = |folder: &Path| folder.join(entry(store_path));

        let original = fixture.path().join("site/nix-roots");
        start(original.clone(), pins.clone()).wait_passes(1).await;
        assert_eq!(roots(), [link(&original)]);

        let moved = fixture.path().join("moved-site/nix-roots");
        std::fs::create_dir_all(moved.parent().expect("moved parent")).expect("moved parent");
        std::fs::rename(&original, &moved).expect("move the root folder");
        assert!(roots().is_empty(), "Nix does not count a moved link");
        let keeper = start(moved.clone(), pins.clone());
        keeper.wait_passes(1).await;
        assert_eq!(roots(), [link(&moved)], "startup registers the moved link");

        pins.set([]);
        keeper.wait_passes(2).await;
        assert!(roots().is_empty(), "an unpinned path has no root");
    }

    const RETRY: Duration = Duration::from_secs(5);

    fn entry(store_path: &str) -> &str {
        store_path.trim_start_matches("/nix/store/")
    }

    /// Running keeper with its completed-pass counter and clock, aborted when dropped.
    struct Running {
        passes: watch::Receiver<u64>,
        clock: ManualClock,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Running {
        async fn wait_passes(&self, count: u64) {
            let mut passes = self.passes.clone();
            tokio::time::timeout(
                Duration::from_secs(30),
                passes.wait_for(|passes| *passes >= count),
            )
            .await
            .unwrap_or_else(|_| panic!("pass {count} deadline, saw {}", *self.passes.borrow()))
            .expect("keeper running");
        }
    }

    /// Pins answering a scripted sequence of reads, repeating the last one.
    struct FakePins {
        reads: Mutex<VecDeque<BTreeSet<String>>>,
        changes: watch::Sender<u64>,
    }

    impl FakePins {
        fn new<R: IntoIterator<Item = I>, I: IntoIterator<Item = &'static str>>(
            reads: R,
        ) -> Arc<Self> {
            Arc::new(Self {
                reads: Mutex::new(
                    reads
                        .into_iter()
                        .map(|read| read.into_iter().map(str::to_owned).collect())
                        .collect(),
                ),
                changes: watch::channel(0).0,
            })
        }

        fn set<const N: usize>(&self, pins: [&str; N]) {
            *self.reads.lock().expect("pins") =
                VecDeque::from([pins.into_iter().map(str::to_owned).collect()]);
            self.changes.send_modify(|count| *count += 1);
        }
    }

    impl PinnedStorePaths for FakePins {
        fn pinned_store_paths(&self) -> anyhow::Result<BTreeSet<String>> {
            let mut reads = self.reads.lock().expect("pins");
            Ok(if reads.len() > 1 {
                reads.pop_front().expect("scripted read")
            } else {
                reads.front().cloned().unwrap_or_default()
            })
        }

        fn subscribe(&self) -> watch::Receiver<u64> {
            self.changes.subscribe()
        }
    }

    /// Temporary root folder and fake `nix` that knows only `PINNED`, logs its calls, and fails
    /// builds while the fail flag exists.
    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                root: tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
                    .expect("fixture root"),
            };
            let nix = fixture.root.path().join("nix");
            std::fs::write(
                &nix,
                format!(
                    "#!/bin/sh\nshift 2\nprintf '%s\\n' \"$*\" >> '{calls}'\ncase \"$1\" in\n  path-info) [ \"$2\" = '{PINNED}' ] ;;\n  build) [ ! -e '{fail}' ] && ln -sfn \"$4\" \"$3\" ;;\n  *) exit 2 ;;\nesac\n",
                    calls = fixture.root.path().join("calls").display(),
                    fail = fixture.root.path().join("fail").display(),
                ),
            )
            .expect("fake nix");
            std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755))
                .expect("fake nix permissions");
            std::fs::create_dir_all(fixture.roots().join(LOCK_DIRECTORY)).expect("root folder");
            fixture
        }

        fn roots(&self) -> PathBuf {
            self.root.path().join("roots")
        }

        fn start(&self, pins: Arc<FakePins>) -> (Running, Arc<FakePins>) {
            let clock = ManualClock::new();
            let keeper = NixRootKeeper::new(
                self.roots(),
                pins.clone(),
                &ArtifactTiming {
                    nix_root_retry_interval: RETRY,
                },
                Arc::new(clock.clone()),
            )
            .with_executable(self.root.path().join("nix"));
            let passes = keeper.passes();
            (
                Running {
                    passes,
                    clock,
                    task: tokio::spawn(keeper.run()),
                },
                pins,
            )
        }

        fn link(&self, store_path: &str) {
            std::os::unix::fs::symlink(store_path, self.roots().join(entry(store_path)))
                .expect("fixture root link");
        }

        /// Returns the targets of the root links, sorted.
        fn links(&self) -> Vec<String> {
            let mut targets: Vec<String> = std::fs::read_dir(self.roots())
                .expect("root folder")
                .filter_map(|entry| std::fs::read_link(entry.expect("entry").path()).ok())
                .map(|target| target.display().to_string())
                .collect();
            targets.sort();
            targets
        }

        fn fail_nix(&self, fail: bool) {
            let flag = self.root.path().join("fail");
            if fail {
                std::fs::write(flag, b"").expect("fail flag");
            } else {
                std::fs::remove_file(flag).expect("fail flag");
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.path().join("calls"))
                .map(|calls| calls.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }
    }
}
