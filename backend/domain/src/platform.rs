//! Reports the effective operating system and CPU architecture of execution environments, and the
//! artifact packaging formats, OCI image or Nix store path, that each configured execution target
//! can launch.
//!
//! Runtime backends supply platform observations from their destination infrastructure;
//! normalization never substitutes the controller process architecture.

use std::fmt;

use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    site::v1::{
        ExecutionTarget as SiteExecutionTarget, application_runtime_binding,
        execution_target::RuntimeBinding, model_runtime_binding,
    },
};

/// Canonical Linux or darwin/arm64 platform reported for one effective execution environment.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExecutionPlatform {
    operating_system: &'static str,
    architecture: &'static str,
}

impl ExecutionPlatform {
    /// Normalizes a runtime-reported operating system and CPU architecture.
    pub fn normalize(
        operating_system: &str,
        architecture: &str,
    ) -> Result<Self, ExecutionPlatformError> {
        let normalized = architecture.to_ascii_lowercase();
        if operating_system.eq_ignore_ascii_case("darwin") {
            return match normalized.as_str() {
                "arm64" | "aarch64" => Ok(Self::darwin_arm64()),
                _ => Err(ExecutionPlatformError::UnsupportedDarwinArchitecture {
                    observed: architecture.to_owned(),
                }),
            };
        }
        if !operating_system.eq_ignore_ascii_case("linux") {
            return Err(ExecutionPlatformError::UnsupportedOperatingSystem {
                observed: operating_system.to_owned(),
            });
        }
        match normalized.as_str() {
            "amd64" | "x86_64" => Ok(Self::linux_amd64()),
            "arm64" | "aarch64" => Ok(Self::linux_arm64()),
            _ => Err(ExecutionPlatformError::UnsupportedArchitecture {
                observed: architecture.to_owned(),
            }),
        }
    }

    /// Returns the canonical Linux amd64 execution platform.
    pub const fn linux_amd64() -> Self {
        Self {
            operating_system: "linux",
            architecture: "amd64",
        }
    }

    /// Returns the canonical Linux arm64 execution platform.
    pub const fn linux_arm64() -> Self {
        Self {
            operating_system: "linux",
            architecture: "arm64",
        }
    }

    /// Returns the canonical darwin arm64 execution platform.
    pub const fn darwin_arm64() -> Self {
        Self {
            operating_system: "darwin",
            architecture: "arm64",
        }
    }

    /// Returns whether this platform runs Linux workloads, as Docker-bound targets require.
    pub fn is_linux(&self) -> bool {
        self.operating_system == "linux"
    }

    /// Returns the canonical OCI operating-system component.
    pub const fn operating_system(&self) -> &'static str {
        self.operating_system
    }

    /// Returns the canonical OCI architecture component.
    pub const fn architecture(&self) -> &'static str {
        self.architecture
    }

    /// Projects this validated value into the shared OCI platform contract.
    pub fn to_oci_platform(&self) -> OciPlatform {
        OciPlatform {
            operating_system: self.operating_system.to_owned(),
            architecture: self.architecture.to_owned(),
            variant: None,
        }
    }
}

impl std::fmt::Display for ExecutionPlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.operating_system, self.architecture)
    }
}

impl TryFrom<&OciPlatform> for ExecutionPlatform {
    type Error = ExecutionPlatformError;

    fn try_from(platform: &OciPlatform) -> Result<Self, Self::Error> {
        if let Some(variant) = platform
            .variant
            .as_deref()
            .filter(|variant| !variant.is_empty())
        {
            return Err(ExecutionPlatformError::UnsupportedVariant {
                observed: variant.to_owned(),
            });
        }
        Self::normalize(&platform.operating_system, &platform.architecture)
    }
}

/// A runtime execution environment reported an unsupported platform component.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionPlatformError {
    #[error(
        "Runtime execution environment reported unsupported operating system {observed:?}; expected linux or darwin"
    )]
    UnsupportedOperatingSystem { observed: String },

    #[error(
        "Runtime execution environment reported unsupported CPU architecture {observed:?}; expected amd64, x86_64, arm64, or aarch64"
    )]
    UnsupportedArchitecture { observed: String },

    #[error(
        "Runtime execution environment reported unsupported darwin CPU architecture {observed:?}; expected arm64 or aarch64"
    )]
    UnsupportedDarwinArchitecture { observed: String },

    #[error(
        "Runtime execution environment reported unsupported CPU variant {observed:?}; expected no variant"
    )]
    UnsupportedVariant { observed: String },
}

/// Packaging format of an executable artifact, which decides the runtimes able to launch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadArtifactKind {
    /// Container image launched by a container runtime: an OCI image, or the image a
    /// Docker-delivered Nix artifact loads.
    Oci,

    /// Nix store path launched as a host process by a native runtime.
    Nix,
}

impl WorkloadArtifactKind {
    /// Every artifact kind the backend can launch.
    pub const ALL: &'static [Self] = &[Self::Oci, Self::Nix];

    /// Returns the artifact kinds the runtime bound to a configured execution target can realize,
    /// derived from the binding type alone so a manager knows them for worker-hosted targets.
    /// Docker realizes OCI images and the native runtime realizes Nix store paths. Simulated never launches an artifact and stands in for the
    /// production runtime of its platform: OCI on Linux and Nix on darwin. An unbound target
    /// realizes none.
    pub fn realizable_by(target: &SiteExecutionTarget) -> &'static [Self] {
        let simulated = || match target
            .platform
            .as_ref()
            .map(|platform| platform.operating_system.as_str())
        {
            Some("linux") => &[Self::Oci][..],
            Some("darwin") => &[Self::Nix][..],
            _ => &[],
        };
        match target.runtime_binding.as_ref() {
            Some(RuntimeBinding::ModelRuntime(binding)) => match binding.adapter {
                Some(model_runtime_binding::Adapter::DockerRuntime(_)) => &[Self::Oci],
                Some(model_runtime_binding::Adapter::NativeRuntime(_)) => &[Self::Nix],
                Some(model_runtime_binding::Adapter::SimulatedRuntime(_)) => simulated(),
                None => &[],
            },
            Some(RuntimeBinding::ApplicationRuntime(binding)) => match binding.adapter {
                Some(application_runtime_binding::Adapter::DockerRuntime(_)) => &[Self::Oci],
                Some(application_runtime_binding::Adapter::SimulatedRuntime(_)) => simulated(),
                None => &[],
            },
            None => &[],
        }
    }
}

impl fmt::Display for WorkloadArtifactKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Oci => "OCI image",
            Self::Nix => "Nix store path",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionPlatform, ExecutionPlatformError, RuntimeBinding, SiteExecutionTarget,
        WorkloadArtifactKind, model_runtime_binding,
    };

    #[test]
    fn normalizes_only_supported_execution_platforms() {
        for (operating_system, architecture, expected) in [
            ("linux", "x86_64", ExecutionPlatform::linux_amd64()),
            ("Linux", "amd64", ExecutionPlatform::linux_amd64()),
            ("linux", "aarch64", ExecutionPlatform::linux_arm64()),
            ("LINUX", "arm64", ExecutionPlatform::linux_arm64()),
            ("darwin", "arm64", ExecutionPlatform::darwin_arm64()),
            ("Darwin", "aarch64", ExecutionPlatform::darwin_arm64()),
        ] {
            assert_eq!(
                ExecutionPlatform::normalize(operating_system, architecture),
                Ok(expected)
            );
        }
    }

    #[test]
    fn unknown_operating_system_or_architecture_is_typed_and_actionable() {
        assert_eq!(
            ExecutionPlatform::normalize("windows", "arm64"),
            Err(ExecutionPlatformError::UnsupportedOperatingSystem {
                observed: "windows".to_owned(),
            })
        );
        assert_eq!(
            ExecutionPlatform::normalize("darwin", "amd64"),
            Err(ExecutionPlatformError::UnsupportedDarwinArchitecture {
                observed: "amd64".to_owned(),
            })
        );
        assert_eq!(
            ExecutionPlatform::normalize("darwin", "x86_64")
                .expect_err("darwin x86_64")
                .to_string(),
            "Runtime execution environment reported unsupported darwin CPU architecture \"x86_64\"; expected arm64 or aarch64"
        );
        assert_eq!(
            ExecutionPlatform::normalize("linux", "riscv64"),
            Err(ExecutionPlatformError::UnsupportedArchitecture {
                observed: "riscv64".to_owned(),
            })
        );
        assert_eq!(
            ExecutionPlatform::normalize("linux", "riscv64")
                .expect_err("unsupported architecture")
                .to_string(),
            "Runtime execution environment reported unsupported CPU architecture \"riscv64\"; expected amd64, x86_64, arm64, or aarch64"
        );
    }

    #[test]
    fn only_linux_platforms_support_docker() {
        assert!(ExecutionPlatform::linux_amd64().is_linux());
        assert!(ExecutionPlatform::linux_arm64().is_linux());
        assert!(!ExecutionPlatform::darwin_arm64().is_linux());
    }

    #[test]
    fn native_model_bindings_realize_nix_artifacts() {
        use dotcore_proto::dotcore::site::v1::{ModelRuntimeBinding, NativeRuntimeAdapter};

        let target = SiteExecutionTarget {
            execution_target_id: "native-target".to_owned(),
            site_node_id: "mac-node".to_owned(),
            platform: Some(ExecutionPlatform::darwin_arm64().to_oci_platform()),
            runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                    NativeRuntimeAdapter::default(),
                )),
            })),
            ..Default::default()
        };

        assert_eq!(
            WorkloadArtifactKind::realizable_by(&target),
            &[WorkloadArtifactKind::Nix]
        );
    }
}
