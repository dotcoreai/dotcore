//! Environment-gated process and Docker boundary coverage for site reset.

use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FIXTURE_IMAGE: &str =
    "busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";
static DOCKER_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_reset_removes_residue_before_fresh_simulated_initialization() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create();
    let images_before = docker_sorted_lines(&["image", "ls", "--no-trunc", "--quiet"]);
    let cache_before = docker_system_df_row("Build Cache");

    let unconfirmed = fixture.reset_output(false);

    assert_eq!(unconfirmed.status.code(), Some(1));
    assert!(unconfirmed.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&unconfirmed.stderr);
    assert!(
        stderr.contains("pass --yes to reset"),
        "unexpected stderr: {stderr}"
    );
    for name in [&fixture.target_model, &fixture.target_application] {
        docker(&["container", "inspect", name]);
    }
    docker(&["network", "inspect", &fixture.target_network]);

    fixture.reset();

    for name in [&fixture.target_model, &fixture.target_application] {
        assert!(!docker_succeeds(&["container", "inspect", name]));
    }
    assert!(!docker_succeeds(&[
        "network",
        "inspect",
        &fixture.target_network
    ]));
    for name in [&fixture.foreign_model, &fixture.foreign_application] {
        docker(&["container", "inspect", name]);
    }
    docker(&["network", "inspect", &fixture.foreign_network]);
    docker(&["image", "inspect", FIXTURE_IMAGE]);
    assert_eq!(
        docker_sorted_lines(&["image", "ls", "--no-trunc", "--quiet"]),
        images_before
    );
    assert_eq!(docker_system_df_row("Build Cache"), cache_before);

    fixture.reset();

    for name in [&fixture.foreign_model, &fixture.foreign_application] {
        docker(&["container", "inspect", name]);
    }
    docker(&["network", "inspect", &fixture.foreign_network]);
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_init_replace_force_removes_residue_like_reset() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create();
    let command = ["site", "init", "--replace", "--force"];

    let unconfirmed = fixture.fresh_site_output(&command, false);

    assert_eq!(unconfirmed.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&unconfirmed.stderr);
    assert!(
        stderr.contains("pass --yes to reset"),
        "unexpected stderr: {stderr}"
    );
    docker(&["container", "inspect", &fixture.target_model]);

    let output = fixture.fresh_site_output(&command, true);

    assert_success("dotc site init --replace --force", &output);
    for name in [&fixture.target_model, &fixture.target_application] {
        assert!(!docker_succeeds(&["container", "inspect", name]));
    }
    assert!(!docker_succeeds(&[
        "network",
        "inspect",
        &fixture.target_network
    ]));
    for name in [&fixture.foreign_model, &fixture.foreign_application] {
        docker(&["container", "inspect", name]);
    }
    docker(&["network", "inspect", &fixture.foreign_network]);
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_delete_removes_residue_and_the_site_directory_but_not_foreign_resources() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create();

    let output = fixture.delete_output();

    assert_success("dotc site delete", &output);
    assert!(!fixture.root.exists());
    for name in [&fixture.target_model, &fixture.target_application] {
        assert!(!docker_succeeds(&["container", "inspect", name]));
    }
    assert!(!docker_succeeds(&[
        "network",
        "inspect",
        &fixture.target_network
    ]));
    for name in [&fixture.foreign_model, &fixture.foreign_application] {
        docker(&["container", "inspect", name]);
    }
    docker(&["network", "inspect", &fixture.foreign_network]);
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_delete_removes_root_owned_application_data_links_without_following_them() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create();
    let outside = fixture.root.with_extension("outside");
    std::fs::write(&outside, "outside").expect("write outside link target");
    let mount = format!("{}:/site", fixture.root.display());
    // A container writes as root, as an application caching Hugging Face snapshots does.
    let script = format!(
        "set -e; cache=/site/applications-data/app/cache; \
         mkdir -p $cache/blobs $cache/snapshots/revision; echo blob > $cache/blobs/hash; \
         ln -s ../../blobs/hash $cache/snapshots/revision/config.json; \
         ln -s {} $cache/outside-link",
        outside.display()
    );
    docker(&[
        "run",
        "--rm",
        "-v",
        &mount,
        FIXTURE_IMAGE,
        "sh",
        "-c",
        &script,
    ]);

    let output = fixture.delete_output();

    let outside_after = std::fs::read_to_string(&outside);
    let _ = std::fs::remove_file(&outside);
    assert_success("dotc site delete", &output);
    assert!(!fixture.root.exists());
    assert_eq!(outside_after.expect("read outside link target"), "outside");
    for name in [&fixture.target_model, &fixture.target_application] {
        assert!(!docker_succeeds(&["container", "inspect", name]));
    }
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_reset_removes_root_owned_workload_caches_of_only_its_site() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create();
    let cache = fixture.cache_dir();
    std::fs::create_dir_all(&cache).expect("create cache root");
    let foreign = fixture.foreign_site_id().to_owned();
    let sites = [fixture.site_id.as_str(), foreign.as_str()];
    // The runtime creates the folders as the host user, and an engine then fills them as root.
    for site in sites {
        for version in ["v2", "v1"] {
            std::fs::create_dir_all(cache.join(format!(
                "workload-cache/{version}/{site}/engine-artifact/key/engine-cache"
            )))
            .expect("create cache folder");
        }
    }
    let script = sites
        .iter()
        .flat_map(|site| ["v2", "v1"].map(|version| (site, version)))
        .map(|(site, version)| {
            let folder = format!(
                "/cache/workload-cache/{version}/{site}/engine-artifact/key/engine-cache"
            );
            format!(
                "mkdir {folder}/private && echo {site} > {folder}/private/entry && chmod 755 {folder}/private"
            )
        })
        .collect::<Vec<_>>()
        .join(" && ");
    docker(&[
        "run",
        "--rm",
        "-v",
        &format!("{}:/cache", cache.display()),
        FIXTURE_IMAGE,
        "sh",
        "-c",
        &script,
    ]);
    let entry = |site: &str, version: &str| {
        cache.join(format!(
            "workload-cache/{version}/{site}/engine-artifact/key/engine-cache/private/entry"
        ))
    };

    fixture.reset();

    for version in ["v2", "v1"] {
        assert!(
            !cache
                .join("workload-cache")
                .join(version)
                .join(&fixture.site_id)
                .exists(),
            "{version} cache of the reset site was removed"
        );
        assert_eq!(
            std::fs::read_to_string(entry(&foreign, version)).expect("foreign cache entry"),
            format!("{foreign}\n")
        );
    }

    // The kept root-owned entries are beyond the host user, so a container clears them.
    docker(&[
        "run",
        "--rm",
        "-v",
        &format!("{}:/cache", cache.display()),
        FIXTURE_IMAGE,
        "sh",
        "-c",
        "rm -rf /cache/*",
    ]);
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_site_reset_rejects_target_site_resources_without_managed_ownership_before_mutation() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let fixture = DockerResetFixture::new(prepare_docker_fixture_image());
    fixture.create_with_target_model_managed("false");

    let output = fixture.reset_output(true);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed managed ownership"));
    for name in [
        &fixture.target_model,
        &fixture.target_application,
        &fixture.foreign_model,
        &fixture.foreign_application,
    ] {
        docker(&["container", "inspect", name]);
    }
    for network in [&fixture.target_network, &fixture.foreign_network] {
        docker(&["network", "inspect", network]);
    }
}

#[test]
#[ignore = "hardware: just backend::test-docker"]
fn hw_docker_node_leave_removes_only_the_joined_nodes_resources_on_a_shared_daemon() {
    let _serial = DOCKER_TEST_LOCK.lock().expect("Docker test lock");
    let mut fixture = NodeLeaveFixture::new(prepare_docker_fixture_image());
    fixture.create();

    let output = fixture.leave_output();

    assert_success("dotc node leave", &output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "Node left\nDOTCORE_SITE_ID={}\nDOTCORE_NODE_ID={}\n",
            fixture.site_id, fixture.worker_node_id
        )
    );
    assert!(!fixture.node_dir.exists());
    assert!(!docker_succeeds(&[
        "container",
        "inspect",
        &fixture.worker_application
    ]));
    assert!(!docker_succeeds(&[
        "network",
        "inspect",
        &fixture.worker_network
    ]));
    // The manager shares the daemon and the site label, but not the execution target.
    docker(&["container", "inspect", &fixture.manager_application]);
    docker(&["network", "inspect", &fixture.manager_network]);
    assert!(fixture.site_dir.join("site.yaml").is_file());
}

/// A manager site with one joined worker that has a Docker application target, plus runtime
/// resources of the worker's and of the manager's application targets under the same site label.
struct NodeLeaveFixture {
    root: PathBuf,
    site_dir: PathBuf,
    node_dir: PathBuf,
    site_id: String,
    worker_node_id: String,
    worker_network: String,
    worker_application: String,
    manager_network: String,
    manager_application: String,
    image_was_present: bool,
    /// Private port the worker listens on while it joins, kept from other test processes.
    worker_private_port: dotcore::testing::ReservedPort,
}

impl NodeLeaveFixture {
    fn new(image_was_present: bool) -> Self {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        );
        let site_id = format!("leave-{unique}");
        let root = canonical_temp_dir().join(format!("dotcore-leave-docker-{unique}"));
        Self {
            site_dir: root.join("site"),
            node_dir: root.join("worker"),
            root,
            worker_network: format!("dotcore_{site_id}_group_worker"),
            worker_application: format!("dotcore_{site_id}_group_worker_unit_web"),
            manager_network: format!("dotcore_{site_id}_group_manager"),
            manager_application: format!("dotcore_{site_id}_group_manager_unit_web"),
            site_id,
            worker_node_id: String::new(),
            image_was_present,
            worker_private_port: dotcore::testing::reserve_port(),
        }
    }

    fn create(&mut self) {
        std::fs::create_dir_all(&self.root).expect("create fixture root");
        let initialized = self
            .dotc()
            .args(["site", "init", "--site-dir"])
            .arg(&self.site_dir)
            .args([
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                "--site-id",
                &self.site_id,
                "--node-id",
                "leave-manager",
                "--host",
                "127.0.0.2",
                "--non-interactive",
            ])
            .output()
            .expect("run dotc site init");
        assert_success("dotc site init", &initialized);
        self.join_worker();

        let site: dotcore_proto::dotcore::site::v1::Site =
            dotcore_proto::yaml::from_file(&self.site_dir.join("site.yaml")).expect("read site");
        self.worker_node_id = site
            .nodes
            .iter()
            .find(|node| node.site_manager.is_none())
            .expect("joined worker")
            .node_id
            .clone();
        let application_target = |node_id: &str| {
            site.execution_targets
                .iter()
                .find(|target| {
                    target.site_node_id == node_id
                        && matches!(
                            target.runtime_binding,
                            Some(
                                dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(_)
                            )
                        )
                })
                .expect("application target")
                .execution_target_id
                .clone()
        };
        let worker_target = application_target(&self.worker_node_id);
        let manager_target = application_target("leave-manager");

        create_group_network_for_target(
            &self.worker_network,
            &self.site_id,
            &worker_target,
            "worker",
        );
        create_application_for_target(
            &self.worker_application,
            &self.worker_network,
            &self.site_id,
            &worker_target,
            "worker",
        );
        create_group_network_for_target(
            &self.manager_network,
            &self.site_id,
            &manager_target,
            "manager",
        );
        create_application_for_target(
            &self.manager_application,
            &self.manager_network,
            &self.site_id,
            &manager_target,
            "manager",
        );
    }

    /// Joins the worker through the real `node add` and `node join` commands.
    fn join_worker(&self) {
        let token_file = self.root.join("join.token");
        let mut add = self
            .dotc()
            .args(["node", "add", "--yes", "--site-dir"])
            .arg(&self.site_dir)
            .args([
                "--listen",
                "127.0.0.2:0",
                "--manager-private-port",
                "9443",
                "--timeout",
                "120s",
                "--token-file",
            ])
            .arg(&token_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dotc node add");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !token_file.exists() {
            assert!(
                add.try_wait().expect("poll node add").is_none(),
                "node add exited early"
            );
            assert!(
                Instant::now() < deadline,
                "node add never published its token"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let joined = self
            .dotc()
            .args(["node", "join", "--yes", "--token-file"])
            .arg(&token_file)
            .arg("--site-dir")
            .arg(&self.node_dir)
            .args([
                "--name",
                "Docker worker",
                "--host",
                "127.0.0.3",
                "--inference",
                "simulated",
                "--applications",
                "docker",
                "--private-port",
                &self.worker_private_port.port().to_string(),
            ])
            .output()
            .expect("run dotc node join");
        assert_success("dotc node join", &joined);
        let added = add.wait_with_output().expect("wait for node add");
        assert_success("dotc node add", &added);
    }

    fn leave_output(&self) -> Output {
        self.dotc()
            .args(["node", "leave", "--yes", "--site-dir"])
            .arg(&self.node_dir)
            .output()
            .expect("run dotc node leave")
    }

    fn dotc(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotc"));
        // Workspace listener variables would pin the SiteNode host and refuse the fixture's hosts.
        command
            .env_remove("DOTCORE_MANAGEMENT_ORIGIN")
            .env_remove("DOTCORE_SITE_DIR")
            .env_remove("DOTCORE_MANAGEMENT_ADDR")
            .env_remove("DOTCORE_INFERENCE_ADDR")
            .env_remove("DOTCORE_PRIVATE_ADDR");
        command
    }
}

impl Drop for NodeLeaveFixture {
    fn drop(&mut self) {
        for container in [&self.worker_application, &self.manager_application] {
            let _ = docker_output(&["container", "rm", "--force", container]);
        }
        for network in [&self.worker_network, &self.manager_network] {
            let _ = docker_output(&["network", "rm", network]);
        }
        if !self.image_was_present {
            let _ = docker_output(&["image", "rm", FIXTURE_IMAGE]);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct DockerResetFixture {
    root: PathBuf,
    site_id: String,
    target_model: String,
    target_application: String,
    target_network: String,
    foreign_model: String,
    foreign_application: String,
    foreign_network: String,
    image_was_present: bool,
}

impl DockerResetFixture {
    fn new(image_was_present: bool) -> Self {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        );
        let site_id = format!("reset-{unique}");
        let foreign_site_id = format!("foreign-{unique}");
        let target_network = format!("dotcore_{site_id}_group_app");
        let foreign_network = format!("dotcore_{foreign_site_id}_group_app");

        Self {
            root: canonical_temp_dir().join(format!("dotcore-reset-docker-{unique}")),
            target_model: format!("dotcore_{site_id}_model_1"),
            target_application: format!("{target_network}_unit_web"),
            target_network,
            foreign_model: format!("dotcore_{foreign_site_id}_model_1"),
            foreign_application: format!("{foreign_network}_unit_web"),
            foreign_network,
            site_id,
            image_was_present,
        }
    }

    fn create(&self) {
        self.create_with_target_model_managed("true");
    }

    fn create_with_target_model_managed(&self, managed: &str) {
        std::fs::create_dir_all(&self.root).expect("create site directory");
        std::fs::write(
            self.root.join("site.yaml"),
            format!("siteId: {}\n", self.site_id),
        )
        .expect("write retained site identity");
        create_group_network(&self.target_network, &self.site_id);
        create_group_network(&self.foreign_network, self.foreign_site_id());
        create_model(&self.target_model, &self.site_id, managed);
        create_model(&self.foreign_model, self.foreign_site_id(), "true");
        create_application(
            &self.target_application,
            &self.target_network,
            &self.site_id,
        );
        create_application(
            &self.foreign_application,
            &self.foreign_network,
            self.foreign_site_id(),
        );
    }

    fn reset(&self) {
        let output = self.reset_output(true);
        assert_success("dotc site reset", &output);
    }

    fn reset_output(&self, yes: bool) -> Output {
        self.fresh_site_output(&["site", "reset"], yes)
    }

    /// Runs `subcommand` (`site reset` or `site init --replace --force`) non-interactively.
    fn fresh_site_output(&self, subcommand: &[&str], yes: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotc"));
        command
            .args(subcommand)
            .arg("--site-dir")
            .arg(&self.root)
            .arg("--cache-dir")
            .arg(self.cache_dir())
            .args([
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                "--site-id",
                &self.site_id,
                "--node-id",
                "reset-node",
                "--host",
                "127.0.0.1",
                "--non-interactive",
            ])
            .env_remove("DOTCORE_MANAGEMENT_ORIGIN")
            .env_remove("DOTCORE_MANAGEMENT_ADDR")
            .env_remove("DOTCORE_INFERENCE_ADDR")
            .env_remove("DOTCORE_PRIVATE_ADDR");
        if yes {
            command.arg("--yes");
        }
        command.output().expect("run dotc site reset")
    }

    fn delete_output(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dotc"))
            .args(["site", "delete", "--yes", "--site-dir"])
            .arg(&self.root)
            .arg("--cache-dir")
            .arg(self.cache_dir())
            .env_remove("DOTCORE_MANAGEMENT_ORIGIN")
            .output()
            .expect("run dotc site delete")
    }

    /// Cache root of the commands, kept apart from the user's real one.
    fn cache_dir(&self) -> PathBuf {
        self.root.with_extension("cache")
    }

    fn foreign_site_id(&self) -> &str {
        self.foreign_network
            .strip_prefix("dotcore_")
            .and_then(|value| value.strip_suffix("_group_app"))
            .expect("foreign network naming")
    }
}

impl Drop for DockerResetFixture {
    fn drop(&mut self) {
        for container in [
            &self.target_model,
            &self.target_application,
            &self.foreign_model,
            &self.foreign_application,
        ] {
            let _ = docker_output(&["container", "rm", "--force", container]);
        }
        for network in [&self.target_network, &self.foreign_network] {
            let _ = docker_output(&["network", "rm", network]);
        }
        if !self.image_was_present {
            let _ = docker_output(&["image", "rm", FIXTURE_IMAGE]);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(self.cache_dir());
    }
}

fn create_model(name: &str, site_id: &str, managed: &str) {
    create_model_for_target(name, site_id, managed, "fixture-target");
}

fn create_model_for_target(name: &str, site_id: &str, managed: &str, target: &str) {
    docker(&[
        "container",
        "create",
        "--name",
        name,
        "--label",
        &format!("com.dotcore.managed={managed}"),
        "--label",
        &format!("com.dotcore.site={site_id}"),
        "--label",
        &format!("com.dotcore.execution-target={target}"),
        "--label",
        "com.dotcore.runtime.schema=6",
        "--label",
        "com.dotcore.workload.correlation=model",
        "--label",
        "com.dotcore.reconcile.generation=1",
        "--label",
        &format!("com.dotcore.runtime.workload={name}"),
        "--label",
        "com.dotcore.workload.ports=fixture=1",
        "--label",
        &format!("com.dotcore.workload.launch-fingerprint={}", "0".repeat(64)),
        FIXTURE_IMAGE,
        "true",
    ]);
}

fn create_group_network(name: &str, site_id: &str) {
    create_group_network_for_target(name, site_id, "fixture-target", "app");
}

fn create_group_network_for_target(name: &str, site_id: &str, target: &str, group: &str) {
    docker(&[
        "network",
        "create",
        "--driver",
        "bridge",
        "--label",
        "com.dotcore.managed=true",
        "--label",
        &format!("com.dotcore.site={site_id}"),
        "--label",
        &format!("com.dotcore.execution-target={target}"),
        "--label",
        "com.dotcore.runtime.schema=5",
        "--label",
        "com.dotcore.workload.correlation=application",
        "--label",
        "com.dotcore.reconcile.generation=1",
        "--label",
        &format!("com.dotcore.runtime.group={group}"),
        "--label",
        "com.dotcore.runtime.group-network=true",
        name,
    ]);
}

fn create_application(name: &str, network: &str, site_id: &str) {
    create_application_for_target(name, network, site_id, "fixture-target", "app");
}

fn create_application_for_target(
    name: &str,
    network: &str,
    site_id: &str,
    target: &str,
    group: &str,
) {
    docker(&[
        "container",
        "create",
        "--name",
        name,
        "--network",
        network,
        "--network-alias",
        "web",
        "--label",
        "com.dotcore.managed=true",
        "--label",
        &format!("com.dotcore.site={site_id}"),
        "--label",
        &format!("com.dotcore.execution-target={target}"),
        "--label",
        "com.dotcore.runtime.schema=5",
        "--label",
        "com.dotcore.workload.correlation=application",
        "--label",
        "com.dotcore.reconcile.generation=1",
        "--label",
        &format!("com.dotcore.runtime.group={group}"),
        "--label",
        "com.dotcore.runtime.unit=web",
        FIXTURE_IMAGE,
        // A shell PID 1 that traps SIGTERM stops at once; bare `sleep` ignores it until the
        // stop grace ends.
        "sh",
        "-c",
        "trap 'exit 0' TERM; sleep 300 & wait $!",
    ]);
    docker(&["container", "start", name]);
}

fn prepare_docker_fixture_image() -> bool {
    docker(&["info"]);
    let image_was_present = docker_succeeds(&["image", "inspect", FIXTURE_IMAGE]);
    if !image_was_present {
        docker(&["pull", FIXTURE_IMAGE]);
    }
    image_was_present
}

fn docker(args: &[&str]) -> Output {
    let output = docker_output(args);
    assert_success(&format!("docker {}", args.join(" ")), &output);
    output
}

fn docker_stdout(args: &[&str]) -> String {
    String::from_utf8(docker(args).stdout).expect("Docker stdout is UTF-8")
}

fn docker_sorted_lines(args: &[&str]) -> Vec<String> {
    let mut lines = docker_stdout(args)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    lines
}

fn docker_succeeds(args: &[&str]) -> bool {
    docker_output(args).status.success()
}

fn docker_output(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("run Docker CLI")
}

fn docker_system_df_row(resource_type: &str) -> String {
    docker_stdout(&["system", "df", "--format", "{{json .}}"])
        .lines()
        .find(|line| line.contains(&format!("\"Type\":\"{resource_type}\"")))
        .unwrap_or_else(|| panic!("Docker system df omitted {resource_type}"))
        .to_owned()
}

fn assert_success(operation: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink.
fn canonical_temp_dir() -> std::path::PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}
