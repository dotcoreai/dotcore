//! Canonical private syntax for Nix engine artifacts: flake references, package and executable
//! names, store paths, the Nix system each execution platform builds for, and a Docker image
//! derivation's loader program and launch reference, both derived from its store path. It also
//! discovers the host's `nix` executable, in the system or default profile before `PATH`.
//!
//! A catalog Nix artifact names either an exact GitHub revision of the Dotcore repository flake,
//! `github:dotcoreai/dotcore/<40 hex>`, or `self`, the development source of the backend that
//! evaluates it. Branches, local paths, query parameters, unlocked repositories, and locked
//! revisions of any other repository are rejected, so an accepted reference always names exact
//! Dotcore sources or an explicit development source.

use std::path::{Path, PathBuf};

use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineArtifact, NixArtifact, engine_artifact::Kind as EngineArtifactKind,
        nix_artifact::Provides,
    },
    deployment::v1::ArtifactPreparationPolicy,
};

pub(crate) const STORE_DIRECTORY: &str = "/nix/store/";
pub(crate) const STORE_HASH_LENGTH: usize = 32;
pub(crate) const STORE_HASH_ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
const GITHUB_SCHEME: &str = "github:";
const DEVELOPMENT_SOURCE: &str = "self";
const TRUSTED_OWNER: &str = "dotcoreai";
const TRUSTED_REPOSITORY: &str = "dotcore";

// Mirrors the loader in `nix/docker-images/loader.sh`, which applies the same tag; change both
// together with the shared store path in its tests.
pub(crate) const DOCKER_IMAGE_PACKAGE_PREFIX: &str = "docker-image-";
const DOCKER_LAUNCH_REPOSITORY: &str = "dotcore-nix";

const PACKAGE_MAX_LENGTH: usize = 128;
const PACKAGE_MAX_SEGMENTS: usize = 8;

/// Profile directories searched for `nix` before `PATH`, in order: the system profile that NixOS
/// and nix-darwin manage, then the default profile of a Nix installer.
const PROFILE_BINS: [&str; 2] = [
    "/run/current-system/sw/bin",
    "/nix/var/nix/profiles/default/bin",
];

/// One validated flake reference of a catalog Nix artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NixFlakeReference {
    /// An exact GitHub revision that every node fetches and builds to the same store path.
    Locked {
        owner: String,
        repository: String,
        revision: String,
    },

    /// The development source of the backend that evaluates or builds the artifact.
    DevelopmentSource,
}

impl NixFlakeReference {
    /// Parses a locked `github:dotcoreai/dotcore/<revision>` reference or a `self`
    /// development-source reference; locked references to other repositories are untrusted.
    pub fn parse(value: &str) -> Result<Self, NixFlakeReferenceError> {
        if value.contains(['?', '#']) {
            return Err(NixFlakeReferenceError::UnsupportedQuery);
        }
        if value == DEVELOPMENT_SOURCE {
            return Ok(Self::DevelopmentSource);
        }
        let path = value
            .strip_prefix(GITHUB_SCHEME)
            .ok_or(NixFlakeReferenceError::UnsupportedReference)?;
        let segments = path.split('/').collect::<Vec<_>>();
        let (owner, repository, revision) = match segments.as_slice() {
            [owner, repository] if valid_github_name(owner) && valid_github_name(repository) => {
                return Err(NixFlakeReferenceError::UnlockedReference);
            }
            [owner, repository, revision] => (*owner, *repository, *revision),
            _ => return Err(NixFlakeReferenceError::InvalidRepository),
        };
        if !valid_github_name(owner) || !valid_github_name(repository) {
            return Err(NixFlakeReferenceError::InvalidRepository);
        }
        if !is_lower_hex(revision, 40) {
            return Err(NixFlakeReferenceError::InvalidRevision);
        }
        if owner != TRUSTED_OWNER || repository != TRUSTED_REPOSITORY {
            return Err(NixFlakeReferenceError::UntrustedRepository);
        }
        Ok(Self::Locked {
            owner: owner.to_owned(),
            repository: repository.to_owned(),
            revision: revision.to_owned(),
        })
    }

    /// Whether the reference pins exact sources rather than naming the development source.
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked { .. })
    }

    /// Returns how a node prepares an artifact of this reference: from the store paths a locked
    /// revision pins, or from its own development source.
    pub fn preparation_policy(&self) -> ArtifactPreparationPolicy {
        match self {
            Self::Locked { .. } => ArtifactPreparationPolicy::ImmutableRemote,
            Self::DevelopmentSource => ArtifactPreparationPolicy::DevelopmentSource,
        }
    }

    /// Returns the revision a locked reference pins, or `None` for the development source.
    pub fn revision(&self) -> Option<&str> {
        match self {
            Self::Locked { revision, .. } => Some(revision),
            Self::DevelopmentSource => None,
        }
    }

    /// Returns the flake URL to evaluate or build, with `self` replaced by `development_source`;
    /// `None` when the reference needs a development source and none is supplied.
    pub fn source_url(&self, development_source: Option<&str>) -> Option<String> {
        match self {
            Self::Locked {
                owner,
                repository,
                revision,
            } => Some(format!("{GITHUB_SCHEME}{owner}/{repository}/{revision}")),
            Self::DevelopmentSource => development_source.map(str::to_owned),
        }
    }
}

/// Reasons a Nix flake reference is not accepted for an engine artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NixFlakeReferenceError {
    #[error(
        "Nix flake reference must be a locked github:dotcoreai/dotcore/<revision> or self reference"
    )]
    UnsupportedReference,

    #[error(
        "Nix GitHub flake reference must name the dotcoreai/dotcore repository; other repositories are not allowed"
    )]
    UntrustedRepository,

    #[error("Nix GitHub flake reference must pin a revision")]
    UnlockedReference,

    #[error("Nix GitHub flake reference revision must be 40 lowercase hexadecimal characters")]
    InvalidRevision,

    #[error("Nix GitHub flake reference has an invalid owner or repository")]
    InvalidRepository,

    #[error(
        "Nix flake reference must not carry query parameters or a fragment; name the package separately"
    )]
    UnsupportedQuery,
}

/// The Dotcore sources a node holds locally. A development checkout serves `self` artifacts and
/// any locked revision it holds; the source a packaged runtime was built from serves locked
/// references at its own revision. Other locked revisions are fetched from GitHub.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DotcoreSources {
    /// Development checkout flake URL, for example `git+file:///src/dotcore`.
    pub development: Option<String>,

    /// Source the packaged runtime was built from.
    pub packaged: Option<LocalDotcoreSource>,
}

/// A Dotcore flake source in the local Nix store with the revision it holds, such as the source a
/// packaged runtime was built from. A node builds locked references at that revision from it
/// instead of fetching them; the built store path is still checked against the retained one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDotcoreSource {
    url: String,
    revision: String,
}

impl LocalDotcoreSource {
    /// Parses a `path:` flake URL of a top-level store path that pins its revision, for example
    /// `path:/nix/store/<hash>-source?rev=<40 hex>&narHash=sha256-<hash>`; `None` otherwise.
    pub fn parse(url: &str) -> Option<Self> {
        let (path, query) = url.strip_prefix("path:")?.split_once('?')?;
        let revision = query
            .split('&')
            .find_map(|parameter| parameter.strip_prefix("rev="))?;
        (valid_store_path(path) && is_lower_hex(revision, 40)).then(|| Self {
            url: url.to_owned(),
            revision: revision.to_owned(),
        })
    }

    /// Flake URL to build from.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Revision the source holds.
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// Returns the Nix payload of an engine artifact, or `None` for another artifact kind.
pub fn engine_artifact_nix(artifact: &EngineArtifact) -> Option<&NixArtifact> {
    match artifact.kind.as_ref()? {
        EngineArtifactKind::Nix(nix) => Some(nix),
        EngineArtifactKind::Oci(_) => None,
    }
}

/// Returns the program a native Nix artifact runs, or `None` when its package provides a Docker
/// image or names nothing.
pub fn nix_executable(nix: &NixArtifact) -> Option<&str> {
    match nix.provides.as_ref()? {
        Provides::Executable(executable) => Some(executable),
        Provides::DockerImage(_) => None,
    }
}

/// Whether a Nix artifact's package provides a Docker image rather than a native executable.
pub fn provides_docker_image(nix: &NixArtifact) -> bool {
    matches!(nix.provides, Some(Provides::DockerImage(_)))
}

/// Whether a Nix artifact declares what its package provides: a valid executable for a native
/// package, or a Docker image, whose loader program follows from its store path.
pub fn valid_provides(nix: &NixArtifact) -> bool {
    match nix.provides.as_ref() {
        Some(Provides::Executable(executable)) => valid_executable(executable),
        Some(Provides::DockerImage(_)) => true,
        None => false,
    }
}

/// Returns the Nix system that builds native packages for a canonical platform, for example
/// `aarch64-darwin` for darwin/arm64, or `None` for a platform Nix artifacts cannot target. Catalog
/// declarations use it to know which store paths a native artifact needs; hosting nodes build for
/// their own system instead. The engine library (`nix/engines.nix`) mirrors this map for master
/// catalog materialization; change both together.
pub fn system_for(operating_system: &str, architecture: &str) -> Option<&'static str> {
    match (operating_system, architecture) {
        ("linux", "amd64") => Some("x86_64-linux"),
        ("linux", "arm64") => Some("aarch64-linux"),
        ("darwin", "arm64") => Some("aarch64-darwin"),
        _ => None,
    }
}

/// Returns the Nix system of the running host, for example `aarch64-darwin` on an Apple Silicon
/// Mac, or `None` on a host Nix artifacts cannot target. A hosting node builds every Nix artifact,
/// native or Docker-delivered, for this system.
pub fn host_system() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-linux"),
        ("aarch64", "linux") => Some("aarch64-linux"),
        ("aarch64", "macos") => Some("aarch64-darwin"),
        _ => None,
    }
}

/// Returns the Nix systems whose Docker image derivations load images of a Docker platform: each
/// Linux system for its own architecture, and Apple Silicon Macs, whose Docker Desktop runs
/// linux/arm64 images. Empty for a platform no Nix system loads. The image library
/// (`nix/docker-images/default.nix`) owns the same map; change both together.
pub fn image_systems_for(operating_system: &str, architecture: &str) -> &'static [&'static str] {
    match (operating_system, architecture) {
        ("linux", "amd64") => &["x86_64-linux"],
        ("linux", "arm64") => &["aarch64-linux", "aarch64-darwin"],
        _ => &[],
    }
}

/// Returns the Docker reference a Docker image derivation's loader tags its image with, derived
/// from the loader's store path alone: the store entry name as repository and its hash as tag. Any
/// node computes it from a pinned store path without running the loader, so neither the catalog
/// nor a deployment records a Docker reference. Only the last path component is read, so store
/// roots other than the system store resolve alike; `None` when that component is not a Docker
/// image derivation's store entry or its name is not a valid Docker repository component.
pub fn docker_launch_reference(store_path: &str) -> Option<String> {
    let (hash, name) = docker_image_entry(store_path)?;
    Some(format!("{DOCKER_LAUNCH_REPOSITORY}/{name}:{hash}"))
}

/// Returns the loader program of a Docker image derivation, derived from its store path alone: the
/// store entry name without its hash, run from the package's `bin` directory. The image library
/// sets the loader's program name to its derivation name, so the catalog names only the package.
/// `None` under the same conditions as the launch reference.
pub fn docker_image_loader(store_path: &str) -> Option<&str> {
    docker_image_entry(store_path).map(|(_, name)| name)
}

/// Returns the installable that evaluates or builds one package of a flake for one system. The
/// package is an attribute path under the flake's nested package tree.
pub fn installable(source_url: &str, system: &str, package: &str) -> String {
    format!("{source_url}#legacyPackages.{system}.{package}")
}

/// Whether a package is a dot-separated path of plain Nix attribute names under the flake's nested
/// package tree, for example `engines.llama-cpp.native`. A path starting at a flake output root
/// (`packages` or `legacyPackages`) is rejected, since the system is chosen by the hosting node.
pub fn valid_package(value: &str) -> bool {
    let segments: Vec<&str> = value.split('.').collect();
    value.len() <= PACKAGE_MAX_LENGTH
        && segments.len() <= PACKAGE_MAX_SEGMENTS
        && !matches!(segments[0], "packages" | "legacyPackages")
        && segments.into_iter().all(valid_attribute_name)
}

/// Whether an executable is one program name in a package's `bin` directory, for example
/// `llama-server`.
pub fn valid_executable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

/// Whether a value is a top-level store path, for example `/nix/store/<32 chars>-llama-cpp-b11417`.
pub fn valid_store_path(value: &str) -> bool {
    value
        .strip_prefix(STORE_DIRECTORY)
        .is_some_and(valid_store_entry)
}

/// Whether a value is one top-level store entry name, `<32 chars>-<name>`, for example
/// `l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417`.
pub fn valid_store_entry(entry: &str) -> bool {
    let Some((hash, name)) = entry.split_at_checked(STORE_HASH_LENGTH) else {
        return false;
    };
    let Some(name) = name.strip_prefix('-') else {
        return false;
    };
    hash.bytes().all(|byte| STORE_HASH_ALPHABET.contains(&byte))
        && !name.is_empty()
        && name.len() <= 211
        && !name.starts_with('.')
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
        })
}

/// Returns the `nix` executable: the first one in the system or default profile, else on `PATH`.
pub fn discover_nix() -> Option<PathBuf> {
    discover_nix_in(
        PROFILE_BINS.map(Path::new),
        std::env::var_os("PATH").as_deref(),
    )
}

/// Returns where `nix` is searched for, in order, for messages about a missing Nix.
pub fn search_locations() -> String {
    format!("{}, or PATH", PROFILE_BINS.join(", "))
}

// Splits a Docker image derivation's store entry into its hash and name; `None` when the last
// path component is not such an entry or its name is not a valid Docker repository component.
fn docker_image_entry(store_path: &str) -> Option<(&str, &str)> {
    let entry = store_path.rsplit('/').next()?;
    if !valid_store_entry(entry) {
        return None;
    }
    let (hash, name) = entry.split_at(STORE_HASH_LENGTH);
    let name = &name[1..];
    (name.starts_with(DOCKER_IMAGE_PACKAGE_PREFIX) && valid_docker_repository_component(name))
        .then_some((hash, name))
}

fn valid_attribute_name(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_github_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

// Docker's repository path component rule: lowercase alphanumeric runs joined by `.`, `_`, `__`, or
// one or more `-`.
fn valid_docker_repository_component(value: &str) -> bool {
    let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = value.as_bytes();
    bytes.first().is_some_and(|byte| alphanumeric(*byte))
        && bytes.last().is_some_and(|byte| alphanumeric(*byte))
        && value
            .split(|character: char| character.is_ascii_lowercase() || character.is_ascii_digit())
            .filter(|separator| !separator.is_empty())
            .all(|separator| {
                matches!(separator, "." | "_" | "__") || separator.bytes().all(|byte| byte == b'-')
            })
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn discover_nix_in<'a>(
    profile_bins: impl IntoIterator<Item = &'a Path>,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    profile_bins
        .into_iter()
        .map(Path::to_path_buf)
        .chain(path.into_iter().flat_map(std::env::split_paths))
        .map(|directory| directory.join("nix"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::ExecutionPlatform;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn locked_github_and_development_source_references_parse() {
        assert_eq!(
            NixFlakeReference::parse(&format!("github:dotcoreai/dotcore/{REVISION}")),
            Ok(NixFlakeReference::Locked {
                owner: "dotcoreai".to_owned(),
                repository: "dotcore".to_owned(),
                revision: REVISION.to_owned(),
            })
        );
        assert_eq!(
            NixFlakeReference::parse("self"),
            Ok(NixFlakeReference::DevelopmentSource)
        );
    }

    #[test]
    fn unlocked_path_and_malformed_references_are_rejected_with_their_reason() {
        let locked_with_directory = format!("github:dotcoreai/dotcore/{REVISION}?dir=engines/a");
        let cases = [
            ("", NixFlakeReferenceError::UnsupportedReference),
            (
                "path:/src/dotcore",
                NixFlakeReferenceError::UnsupportedReference,
            ),
            ("/src/dotcore", NixFlakeReferenceError::UnsupportedReference),
            (
                "./engines/llama-cpp",
                NixFlakeReferenceError::UnsupportedReference,
            ),
            (
                "git+file:///src/dotcore",
                NixFlakeReferenceError::UnsupportedReference,
            ),
            (
                "github:dotcoreai/dotcore",
                NixFlakeReferenceError::UnlockedReference,
            ),
            (
                "github:dotcoreai/dotcore/main",
                NixFlakeReferenceError::InvalidRevision,
            ),
            (
                "github:dotcoreai/dotcore/0123456789ABCDEF0123456789abcdef01234567",
                NixFlakeReferenceError::InvalidRevision,
            ),
            (
                "github:dotcoreai/dotcore/0123456789abcdef",
                NixFlakeReferenceError::InvalidRevision,
            ),
            (
                "github:example/engines/0123456789abcdef0123456789abcdef01234567",
                NixFlakeReferenceError::UntrustedRepository,
            ),
            (
                "github:dotcoreai/other/0123456789abcdef0123456789abcdef01234567",
                NixFlakeReferenceError::UntrustedRepository,
            ),
            (
                "github:other/dotcore/0123456789abcdef0123456789abcdef01234567",
                NixFlakeReferenceError::UntrustedRepository,
            ),
            (
                "github:dotcoreai",
                NixFlakeReferenceError::InvalidRepository,
            ),
            (
                "github:dotcoreai/../0123456789abcdef0123456789abcdef01234567",
                NixFlakeReferenceError::InvalidRepository,
            ),
            ("self?ref=main", NixFlakeReferenceError::UnsupportedQuery),
            (
                "self?dir=engines/llama-cpp",
                NixFlakeReferenceError::UnsupportedQuery,
            ),
            (
                locked_with_directory.as_str(),
                NixFlakeReferenceError::UnsupportedQuery,
            ),
            (
                "self#engines.llama-cpp.native",
                NixFlakeReferenceError::UnsupportedQuery,
            ),
        ];
        for (reference, expected) in cases {
            assert_eq!(
                NixFlakeReference::parse(reference),
                Err(expected),
                "{reference:?}"
            );
        }
    }

    #[test]
    fn source_urls_keep_locked_sources_and_replace_self_with_the_development_source() {
        let locked = NixFlakeReference::parse(&format!("github:dotcoreai/dotcore/{REVISION}"))
            .expect("locked");
        assert_eq!(
            locked.source_url(None).as_deref(),
            Some(format!("github:dotcoreai/dotcore/{REVISION}").as_str())
        );

        let development = NixFlakeReference::parse("self").expect("self");
        assert_eq!(development.source_url(None), None);
        assert_eq!(
            development.source_url(Some("git+file:///src/dotcore?ref=main")),
            Some("git+file:///src/dotcore?ref=main".to_owned())
        );
        assert_eq!(
            installable(
                "git+file:///src/dotcore",
                "aarch64-darwin",
                "engines.llama-cpp.native"
            ),
            "git+file:///src/dotcore#legacyPackages.aarch64-darwin.engines.llama-cpp.native"
        );
    }

    #[test]
    fn platforms_map_to_their_nix_systems() {
        for (platform, system) in [
            (ExecutionPlatform::darwin_arm64(), "aarch64-darwin"),
            (ExecutionPlatform::linux_amd64(), "x86_64-linux"),
            (ExecutionPlatform::linux_arm64(), "aarch64-linux"),
        ] {
            assert_eq!(
                system_for(platform.operating_system(), platform.architecture()),
                Some(system)
            );
        }
        assert_eq!(system_for("darwin", "amd64"), None);
        assert_eq!(system_for("linux", "aarch64"), None);
    }

    #[test]
    fn image_platforms_map_to_the_systems_loading_them() {
        assert_eq!(image_systems_for("linux", "amd64"), ["x86_64-linux"]);
        assert_eq!(
            image_systems_for("linux", "arm64"),
            ["aarch64-linux", "aarch64-darwin"]
        );
        assert!(image_systems_for("darwin", "arm64").is_empty());
    }

    #[test]
    fn package_executable_and_store_path_syntax_is_checked() {
        assert!(valid_package("engines.llama-cpp.docker"));
        assert!(valid_package("llama-cpp_metal"));
        let too_long = format!("engines.{}", "a".repeat(PACKAGE_MAX_LENGTH));
        let too_deep = ["a"; PACKAGE_MAX_SEGMENTS + 1].join(".");
        for invalid in [
            "",
            "engines..docker",
            "engines.llama-cpp.",
            "engines.1package",
            "packages.default",
            "legacyPackages.engines.vllm.docker",
            "a/b",
            "a b",
            &too_long,
            &too_deep,
        ] {
            assert!(!valid_package(invalid), "{invalid:?}");
        }

        assert!(valid_executable("llama-server"));
        assert!(valid_executable("g++"));
        for invalid in ["", ".", "..", "bin/llama-server", "a b"] {
            assert!(!valid_executable(invalid), "{invalid:?}");
        }

        assert!(valid_store_path(
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417"
        ));
        for invalid in [
            "",
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9",
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-",
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama/bin",
            "/nix/store/e0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama",
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh-llama",
            "/opt/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama",
        ] {
            assert!(!valid_store_path(invalid), "{invalid:?}");
        }
    }

    // The loader tests in `nix/docker-images/tests/loader-test.sh` assert the same reference for the
    // same store path.
    #[test]
    fn docker_launch_references_derive_from_the_loader_store_path() {
        assert_eq!(
            docker_launch_reference(
                "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-fixture-busybox"
            )
            .as_deref(),
            Some("dotcore-nix/docker-image-fixture-busybox:0123456789abcdfghijklmnpqrsvwxyz")
        );
        assert_eq!(
            docker_launch_reference(
                "/tmp/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-open-webui"
            )
            .as_deref(),
            Some("dotcore-nix/docker-image-open-webui:0123456789abcdfghijklmnpqrsvwxyz")
        );
        for invalid in [
            "",
            "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-Fixture",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-fixture.",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-a..b",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-a+b",
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-n8n/bin",
            "/nix/store/e123456789abcdfghijklmnpqrsvwxyz-docker-image-n8n",
        ] {
            assert_eq!(docker_launch_reference(invalid), None, "{invalid:?}");
        }
        assert_eq!(
            docker_launch_reference(
                "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-a__b-c"
            )
            .as_deref(),
            Some("dotcore-nix/docker-image-a__b-c:0123456789abcdfghijklmnpqrsvwxyz")
        );
    }

    #[test]
    fn local_dotcore_sources_are_store_paths_pinning_their_revision() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let url = format!(
            "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source?rev={revision}&narHash=sha256-x"
        );
        let source = LocalDotcoreSource::parse(&url).expect("local source");
        assert_eq!((source.url(), source.revision()), (url.as_str(), revision));
        for invalid in [
            format!("git+file:///src/dotcore?rev={revision}"),
            "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source".to_owned(),
            "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source?rev=main".to_owned(),
            format!("path:/src/dotcore?rev={revision}"),
        ] {
            assert_eq!(LocalDotcoreSource::parse(&invalid), None, "{invalid:?}");
        }
        assert_eq!(
            NixFlakeReference::parse(&format!("github:dotcoreai/dotcore/{revision}"))
                .expect("locked")
                .revision(),
            Some(revision)
        );
        assert_eq!(NixFlakeReference::DevelopmentSource.revision(), None);
    }

    #[test]
    fn docker_image_loaders_derive_from_the_store_path() {
        let store_path = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-vllm";
        assert_eq!(docker_image_loader(store_path), Some("docker-image-vllm"));
        assert_eq!(
            docker_launch_reference(store_path).as_deref(),
            Some("dotcore-nix/docker-image-vllm:0123456789abcdfghijklmnpqrsvwxyz")
        );
        assert_eq!(
            docker_image_loader("/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417"),
            None
        );
    }

    #[test]
    fn discovery_prefers_the_profiles_in_order_then_path_order() {
        let root = tempfile::tempdir().expect("temporary directory");
        let system = root.path().join("system");
        let profile = root.path().join("profile");
        let first = root.path().join("first");
        let second = root.path().join("second");
        for directory in [&system, &profile, &first, &second] {
            std::fs::create_dir(directory).expect("directory");
        }
        let path = std::env::join_paths([&first, &second]).expect("PATH");
        let install = |directory: &Path, mode: u32| {
            let nix = directory.join("nix");
            std::fs::write(&nix, "#!/bin/sh\n").expect("nix");
            std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(mode))
                .expect("permissions");
            nix
        };

        let discover = || discover_nix_in([system.as_path(), profile.as_path()], Some(&path));

        assert_eq!(discover(), None);
        install(&first, 0o644);
        let second_nix = install(&second, 0o755);
        assert_eq!(discover(), Some(second_nix));
        let profile_nix = install(&profile, 0o755);
        assert_eq!(discover(), Some(profile_nix));
        let system_nix = install(&system, 0o755);
        assert_eq!(discover(), Some(system_nix));
    }
}
