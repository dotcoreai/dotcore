//! Read-only host capability probes a node runs before it uses a runtime: whether the Docker
//! daemon is reachable on which platform with which CDI devices, and whether this host can run
//! native workloads.
//!
//! The probes never change host state. The backends implement them, and the node supplies one
//! set of them to initial site creation and to its observation reports, so neither names a
//! backend. Simulated targets need no host prerequisite, so the set has no simulated probe.

use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

use dotcore_domain::ExecutionPlatform;

/// Typed capability probes of the runtimes with host prerequisites.
#[derive(Clone)]
pub struct RuntimeCapabilityProbes {
    pub docker: Arc<dyn DockerCapabilityProbe>,
    pub native: Arc<dyn NativeCapabilityProbe>,
}

/// Read-only probe of the local Docker daemon and its NVIDIA CDI devices.
pub trait DockerCapabilityProbe: Send + Sync {
    /// Verifies daemon reachability and returns the runtime capability facts it reports.
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>>;

    /// Discovers NVIDIA CDI selectors through the local Docker toolchain.
    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>>;
}

/// Docker Engine facts retained by a read-only capability report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerCapabilityReport {
    execution_platform: ExecutionPlatform,
    cdi_spec_dirs: Vec<String>,
}

impl DockerCapabilityReport {
    /// Builds a report from an explicit effective platform and the daemon's CDI directories.
    pub fn new(
        execution_platform: ExecutionPlatform,
        cdi_spec_dirs: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            execution_platform,
            cdi_spec_dirs: cdi_spec_dirs.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns Docker's effective execution-environment platform.
    pub fn execution_platform(&self) -> &ExecutionPlatform {
        &self.execution_platform
    }

    /// Returns whether Docker reports CDI device injection as enabled.
    pub fn cdi_enabled(&self) -> bool {
        !self.cdi_spec_dirs.is_empty()
    }

    /// Returns Docker's configured CDI specification directories.
    pub fn cdi_spec_dirs(&self) -> &[String] {
        &self.cdi_spec_dirs
    }
}

/// Host-wide checks for native workloads, shared by every native target of a node.
pub trait NativeCapabilityProbe: Send + Sync {
    /// Runs the checks once. They block on the file system, so callers run them off the async
    /// executor.
    fn probe(&self) -> NativeCapabilityReport;
}

/// Outcome of the host-wide native checks; each failed check holds an actionable detail.
#[derive(Debug)]
pub struct NativeCapabilityReport {
    /// Platform native workloads run on, or none on an unsupported host.
    pub host_platform: Option<ExecutionPlatform>,

    /// Whether the launcher that holds each native workload is installed.
    pub launcher: Result<(), String>,

    /// Whether the native workload state directory is writable.
    pub state_root: Result<(), String>,

    /// Whether `nix` is available for workloads built from Nix packages.
    pub nix: Result<(), String>,
}

impl NativeCapabilityReport {
    /// Returns the first failure that keeps a target running on `required` from using native
    /// workloads here, checked in the order platform, launcher, state root, Nix.
    pub fn qualify(&self, required: &ExecutionPlatform) -> Result<(), NativeCapabilityFailure> {
        if self.host_platform.as_ref() != Some(required) {
            let host = self
                .host_platform
                .as_ref()
                .map_or_else(|| "an unsupported platform".to_owned(), ToString::to_string);
            return Err(NativeCapabilityFailure {
                code: "PLATFORM_UNSUPPORTED",
                platform_mismatch: true,
                message: format!(
                    "Native workloads run on this node's {host} platform, but the execution target requires {required}."
                ),
            });
        }

        for (code, result, subject) in [
            (
                "LAUNCHER_UNAVAILABLE",
                &self.launcher,
                "The native launcher",
            ),
            (
                "STATE_ROOT_UNWRITABLE",
                &self.state_root,
                "The native workload state directory",
            ),
            ("NIX_UNAVAILABLE", &self.nix, "Nix"),
        ] {
            if let Err(detail) = result {
                return Err(NativeCapabilityFailure {
                    code,
                    platform_mismatch: false,
                    message: format!("{subject} is unavailable on this node: {detail}."),
                });
            }
        }

        Ok(())
    }
}

/// Reason a native execution target cannot be used on this node.
#[derive(Debug, PartialEq, Eq)]
pub struct NativeCapabilityFailure {
    pub code: &'static str,

    /// Whether the target's platform differs from the host's, which no node repair fixes.
    pub platform_mismatch: bool,
    pub message: String,
}
