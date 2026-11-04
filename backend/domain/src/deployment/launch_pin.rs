//! Resolves node-specific launch identities once on the SiteNode that hosts a deployment, for
//! example the tag `registry.test/app:dev` pinned to the image ID `sha256:<64 hex>`, package
//! `engines.llama-cpp.native` of the node's development source pinned to its `/nix/store/...` path, or a
//! locked Nix artifact pinned to the catalog store path of the node's own Nix system. A target
//! whose runtime launches from artifact references alone, such as a simulated one, pins without
//! evaluating or inspecting anything: a synthetic identity of the same shape, or one of a locked
//! artifact's catalog store paths.
//!
//! A selection that already has a pin, retained by this node or projected back by the owning
//! controller, is never resolved again, and a projected pin must equal the node's own. Only a
//! selection without any pin is resolved, on the hosting node, so every later launch, pause,
//! resume, or restart uses the same immutable artifact. The owning controller records a pin
//! reported by the node once and never replaces it. Model and application owners share these rules.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
};

use dotcore_proto::dotcore::{
    catalog::v1::{
        ApplicationServiceArtifact, NixArtifact, OciImageReference, OciPlatform,
        application_service_artifact::Kind as ServiceArtifactKind,
    },
    deployment::v1::{ArtifactPreparationPolicy, NodeResolvedLaunchIdentity},
};
use tokio_util::sync::CancellationToken;

/// One development artifact selection that has no launch identity in its snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnpinnedSelection {
    /// Owner-scoped selection key, such as an application service name.
    pub selection_key: String,

    /// What the node resolves for the selection.
    pub source: PinSource,
}

/// Artifact a hosting SiteNode resolves to a node-specific launch identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PinSource {
    /// Rendered tag-only image reference, for example `registry.test/app:dev`, pinned to the
    /// node-local image ID.
    LocalImage { reference: String },

    /// Package of the node's own development source (`self`), for example `engines.llama-cpp.native`,
    /// pinned to the store path it evaluates to for the node's own Nix system.
    NixDevelopmentSource {
        package: String,

        /// Whether the package is a Docker image derivation, whose store entry name carries the
        /// image prefix its launch reference is derived from.
        docker_image: bool,
    },

    /// Locked Nix artifact's catalog store paths as (Nix system, store path) pairs, pinned to the
    /// path of the node's own Nix system.
    NixLocked { store_paths: Vec<(String, String)> },
}

impl PinSource {
    /// Kind of launch identity this source pins to.
    pub fn kind(&self) -> PinKind {
        match self {
            Self::LocalImage { .. } => PinKind::LocalImage,
            Self::NixDevelopmentSource { .. } | Self::NixLocked { .. } => PinKind::NixStorePath,
        }
    }

    /// Returns the store path a locked Nix source pins for one Nix system; `None` for another
    /// source or a system it has no path for.
    pub fn locked_store_path(&self, system: &str) -> Option<&str> {
        let Self::NixLocked { store_paths } = self else {
            return None;
        };
        store_paths
            .iter()
            .find(|(candidate, _)| candidate == system)
            .map(|(_, store_path)| store_path.as_str())
    }
}

/// Artifact a deployment snapshot retains for one selection, as engines and application services
/// declare it.
#[derive(Clone, Copy, Debug)]
pub enum RetainedArtifact<'a> {
    Oci(&'a OciImageReference),

    Nix(&'a NixArtifact),
}

impl<'a> RetainedArtifact<'a> {
    /// Returns the artifact an application service declares, or `None` when it declares none.
    pub fn of_service(artifact: &'a ApplicationServiceArtifact) -> Option<Self> {
        Some(match artifact.kind.as_ref()? {
            ServiceArtifactKind::Oci(reference) => Self::Oci(reference),
            ServiceArtifactKind::Nix(nix) => Self::Nix(nix),
        })
    }

    /// Returns the Docker image reference a container launch runs for `launch_identity`: the
    /// identity itself for an OCI image, or the reference derived from a Docker-delivered Nix
    /// artifact's loader store path. `None` for a native Nix package or an unusable store path.
    pub fn container_image(self, launch_identity: &str) -> Option<String> {
        match self {
            Self::Oci(_) => Some(launch_identity.to_owned()),
            Self::Nix(nix) if crate::nix::provides_docker_image(nix) => {
                crate::nix::docker_launch_reference(launch_identity)
            }
            Self::Nix(_) => None,
        }
    }
}

/// Returns what the hosting SiteNode resolves for a selection whose snapshot leaves its launch
/// identity to that node: the tag of a local-development image, a package of the node's
/// development source for a `self` Nix artifact, or a locked Nix artifact's catalog store paths.
/// `None` when the snapshot retains the identity or the policy does not fit the artifact.
pub fn pin_source(
    artifact: RetainedArtifact<'_>,
    policy: i32,
    launch_identity: &str,
) -> Option<PinSource> {
    match artifact {
        RetainedArtifact::Oci(reference) => {
            if !super::artifact::node_resolved_selection(policy, reference, launch_identity) {
                return None;
            }
            Some(PinSource::LocalImage {
                reference: crate::oci::render_reference(reference).ok()?,
            })
        }
        RetainedArtifact::Nix(nix) => {
            if !launch_identity.is_empty() {
                return None;
            }
            let expected = crate::nix::NixFlakeReference::parse(&nix.flake)
                .ok()?
                .preparation_policy();
            if ArtifactPreparationPolicy::try_from(policy).ok()? != expected {
                return None;
            }
            match expected {
                ArtifactPreparationPolicy::DevelopmentSource => {
                    Some(PinSource::NixDevelopmentSource {
                        package: nix.package.clone()?,
                        docker_image: crate::nix::provides_docker_image(nix),
                    })
                }
                ArtifactPreparationPolicy::ImmutableRemote => Some(PinSource::NixLocked {
                    store_paths: nix
                        .store_paths
                        .iter()
                        .map(|entry| (entry.system.clone(), entry.store_path.clone()))
                        .collect(),
                }),
                _ => None,
            }
        }
    }
}

/// Kind of launch identity a node pin holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinKind {
    /// Docker image ID, `sha256:` followed by 64 lowercase hex digits.
    LocalImage,

    /// Top-level `/nix/store` path.
    NixStorePath,
}

impl PinKind {
    /// Whether `launch_identity` has this kind's shape.
    pub fn admits(self, launch_identity: &str) -> bool {
        match self {
            Self::LocalImage => crate::oci::valid_local_image_id(launch_identity),
            Self::NixStorePath => crate::nix::valid_store_path(launch_identity),
        }
    }
}

/// Node-local capability that resolves a development source to a launch identity of the given
/// platform.
pub trait LaunchPinResolver: Send + Sync {
    fn resolve_pin<'a>(
        &'a self,
        source: &'a PinSource,
        platform: &'a OciPlatform,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<String, anyhow::Error>> + Send + 'a>>;
}

/// Resolver for targets whose runtime launches from artifact references alone, such as a
/// simulated one: it never evaluates, builds, pulls or inspects an artifact. A local image or a
/// development-source package pins a synthetic identity of its kind's shape, derived from the
/// source and platform, whose store entry name is marked as simulated; a locked Nix artifact pins
/// one of its catalog store paths, which every launch of it must name.
pub struct SyntheticLaunchPins;

impl SyntheticLaunchPins {
    /// Returns the identity `source` pins to on `platform`; the same inputs give the same one.
    /// `None` for a locked artifact without catalog store paths.
    pub fn identity(source: &PinSource, platform: &OciPlatform) -> Option<String> {
        let seed = format!("{source:?} {platform:?}");
        match source {
            PinSource::LocalImage { .. } => {
                let digest: String = seeded_words(&seed, 4)
                    .iter()
                    .map(|word| format!("{word:016x}"))
                    .collect();
                Some(format!("sha256:{digest}"))
            }
            PinSource::NixDevelopmentSource {
                package,
                docker_image,
            } => Some(synthetic_store_path(&seed, package, *docker_image)),
            PinSource::NixLocked { store_paths } => {
                let (operating_system, architecture) =
                    (&platform.operating_system, &platform.architecture);
                let systems: Vec<&str> = crate::nix::system_for(operating_system, architecture)
                    .into_iter()
                    .chain(
                        crate::nix::image_systems_for(operating_system, architecture)
                            .iter()
                            .copied(),
                    )
                    .collect();
                store_paths
                    .iter()
                    .find(|(system, _)| systems.contains(&system.as_str()))
                    .or_else(|| store_paths.first())
                    .map(|(_, store_path)| store_path.clone())
            }
        }
    }
}

impl LaunchPinResolver for SyntheticLaunchPins {
    fn resolve_pin<'a>(
        &'a self,
        source: &'a PinSource,
        platform: &'a OciPlatform,
        _cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<String, anyhow::Error>> + Send + 'a>> {
        let identity = Self::identity(source, platform)
            .ok_or_else(|| anyhow::anyhow!("locked Nix artifact has no catalog store path"));
        Box::pin(std::future::ready(identity))
    }
}

/// Pins every selection, resolving only selections without a retained or projected pin.
///
/// Returns the complete pin set ordered by selection key; it differs from `retained` exactly
/// when this call resolved a new pin, which the caller must persist before any launch uses it.
pub async fn pin_launch_identities(
    selections: &[UnpinnedSelection],
    projected: &[NodeResolvedLaunchIdentity],
    retained: &[NodeResolvedLaunchIdentity],
    platform: &OciPlatform,
    resolver: Option<&dyn LaunchPinResolver>,
    cancellation: &CancellationToken,
) -> Result<Vec<NodeResolvedLaunchIdentity>, LaunchPinError> {
    let projected = by_key(projected);
    let retained = by_key(retained);
    let mut pins = BTreeMap::new();
    for selection in selections {
        let key = selection.selection_key.as_str();
        let pinned = match (projected.get(key), retained.get(key)) {
            (Some(projected), Some(retained)) if projected != retained => {
                return Err(LaunchPinError::ProjectedMismatch {
                    selection_key: selection.selection_key.clone(),
                });
            }
            (Some(identity), _) | (None, Some(identity)) => {
                if !selection.source.kind().admits(identity) {
                    return Err(LaunchPinError::KindMismatch {
                        selection_key: selection.selection_key.clone(),
                    });
                }
                (*identity).to_owned()
            }
            (None, None) => {
                let resolver = resolver.ok_or_else(|| LaunchPinError::Unresolved {
                    selection_key: selection.selection_key.clone(),
                    source: anyhow::anyhow!("launch identity resolution is not configured"),
                })?;
                resolver
                    .resolve_pin(&selection.source, platform, cancellation.child_token())
                    .await
                    .map_err(|source| LaunchPinError::Unresolved {
                        selection_key: selection.selection_key.clone(),
                        source,
                    })?
            }
        };
        pins.insert(key, pinned);
    }
    Ok(pins
        .into_iter()
        .map(
            |(selection_key, launch_identity)| NodeResolvedLaunchIdentity {
                selection_key: selection_key.to_owned(),
                launch_identity,
            },
        )
        .collect())
}

/// Records the pins a node reported over the owner's recorded pins. A recorded pin is never
/// replaced: a report that contradicts it is returned separately and otherwise ignored.
pub fn record_reported_pins<'a>(
    recorded: &'a [NodeResolvedLaunchIdentity],
    reported: &'a [NodeResolvedLaunchIdentity],
) -> ReportedPins<'a> {
    let mut pins = recorded.to_vec();
    let mut contradicted = Vec::new();
    let mut added = false;
    for pin in reported {
        match recorded
            .iter()
            .find(|existing| existing.selection_key == pin.selection_key)
        {
            Some(existing) if existing == pin => {}
            Some(existing) => contradicted.push(Contradiction {
                recorded: existing,
                reported: pin,
            }),
            None if pins
                .iter()
                .any(|added| added.selection_key == pin.selection_key) => {}
            None => {
                pins.push(pin.clone());
                added = true;
            }
        }
    }
    pins.sort_by(|left, right| left.selection_key.cmp(&right.selection_key));
    ReportedPins {
        recorded: added.then_some(pins),
        contradicted,
    }
}

/// Whether owner-recorded pins each name a distinct selection the node resolves, with an
/// identity of the kind `expected_kind` returns for its key; `None` means the key takes no pin.
pub fn valid_node_pins(
    pins: &[NodeResolvedLaunchIdentity],
    expected_kind: impl Fn(&str) -> Option<PinKind>,
) -> bool {
    let mut keys = BTreeSet::new();
    pins.iter().all(|pin| {
        keys.insert(pin.selection_key.as_str())
            && expected_kind(&pin.selection_key)
                .is_some_and(|kind| kind.admits(&pin.launch_identity))
    })
}

/// Outcome of recording reported pins over the owner's recorded pins.
#[derive(Debug, PartialEq)]
pub struct ReportedPins<'a> {
    /// The complete pin set ordered by selection key when the report added a pin, else `None`.
    pub recorded: Option<Vec<NodeResolvedLaunchIdentity>>,

    /// Reported pins that contradict a recorded pin; the recorded pin is kept.
    pub contradicted: Vec<Contradiction<'a>>,
}

/// One reported pin that differs from the pin recorded for the same selection.
#[derive(Debug, PartialEq)]
pub struct Contradiction<'a> {
    pub recorded: &'a NodeResolvedLaunchIdentity,

    pub reported: &'a NodeResolvedLaunchIdentity,
}

/// Reasons a local-development selection cannot be pinned.
#[derive(Debug, thiserror::Error)]
pub enum LaunchPinError {
    #[error(
        "projected launch identity of selection {selection_key:?} differs from this node's pin"
    )]
    ProjectedMismatch { selection_key: String },

    #[error(
        "recorded launch identity of selection {selection_key:?} is not of the kind its source pins"
    )]
    KindMismatch { selection_key: String },

    #[error(
        "development selection {selection_key:?} did not resolve to a launch identity for the retained platform"
    )]
    Unresolved {
        selection_key: String,
        #[source]
        source: anyhow::Error,
    },
}

fn by_key(pins: &[NodeResolvedLaunchIdentity]) -> BTreeMap<&str, &str> {
    pins.iter()
        .map(|pin| (pin.selection_key.as_str(), pin.launch_identity.as_str()))
        .collect()
}

/// Builds a store path under the system store for `name`, marked as simulated after the Docker
/// image prefix when `docker_image`, with a hash derived from `seed`.
fn synthetic_store_path(seed: &str, name: &str, docker_image: bool) -> String {
    let alphabet = crate::nix::STORE_HASH_ALPHABET;
    let mut bits = seeded_words(seed, 3).into_iter().flat_map(u64::to_be_bytes);
    let mut hash = String::with_capacity(crate::nix::STORE_HASH_LENGTH);
    let (mut buffer, mut buffered) = (0_u32, 0_u32);
    while hash.len() < crate::nix::STORE_HASH_LENGTH {
        if buffered < 5 {
            buffer = (buffer << 8) | u32::from(bits.next().unwrap_or_default());
            buffered += 8;
        }
        buffered -= 5;
        hash.push(char::from(alphabet[((buffer >> buffered) & 0x1f) as usize]));
    }
    // Docker repository components take only lowercase letters, digits and separators.
    let name: String = name
        .chars()
        .map(|character| match character.to_ascii_lowercase() {
            lower @ ('a'..='z' | '0'..='9') => lower,
            _ => '-',
        })
        .collect();
    let name = name.trim_matches('-');
    let prefix = if docker_image {
        crate::nix::DOCKER_IMAGE_PACKAGE_PREFIX
    } else {
        ""
    };
    format!(
        "{}{}-{prefix}simulated-{name}",
        crate::nix::STORE_DIRECTORY,
        hash
    )
}

/// Returns `count` 64-bit FNV-1a hashes of `seed`, each salted by its index, so a synthetic
/// identity stays the same across processes.
fn seeded_words(seed: &str, count: u8) -> Vec<u64> {
    (0..count)
        .map(|salt| {
            std::iter::once(salt)
                .chain(seed.bytes())
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const RESOLVED: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const RETAINED: &str =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const STORE_PATH: &str = "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417";

    #[tokio::test]
    async fn only_selections_without_a_pin_are_resolved_and_retained_pins_are_reused() {
        let resolver = RecordingResolver::new(Ok(RESOLVED));
        let pins = pin_launch_identities(
            &[selection("api"), selection("web")],
            &[],
            &[pin("web", RETAINED)],
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect("pins");

        assert_eq!(pins, [pin("api", RESOLVED), pin("web", RETAINED)]);
        assert_eq!(resolver.calls(), [image("api")]);

        let repeated = pin_launch_identities(
            &[selection("api"), selection("web")],
            &[],
            &pins,
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect("repeated pins");
        assert_eq!(repeated, pins);
        assert_eq!(
            resolver.calls().len(),
            1,
            "a retained pin is never re-resolved"
        );
    }

    #[tokio::test]
    async fn a_projected_pin_is_used_and_must_equal_the_retained_one() {
        let resolver = RecordingResolver::new(Ok(RESOLVED));
        let adopted = pin_launch_identities(
            &[selection("web")],
            &[pin("web", RETAINED)],
            &[],
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect("projected pin");
        assert_eq!(adopted, [pin("web", RETAINED)]);
        assert!(resolver.calls().is_empty());

        let mismatch = pin_launch_identities(
            &[selection("web")],
            &[pin("web", RESOLVED)],
            &[pin("web", RETAINED)],
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect_err("mismatched projection");
        assert!(matches!(
            mismatch,
            LaunchPinError::ProjectedMismatch { selection_key } if selection_key == "web"
        ));
    }

    #[tokio::test]
    async fn unresolvable_tags_and_missing_resolvers_fail_the_selection() {
        let failing = RecordingResolver::new(Err("platform mismatch"));
        for resolver in [Some(&failing as &dyn LaunchPinResolver), None] {
            let error = pin_launch_identities(
                &[selection("web")],
                &[],
                &[],
                &platform(),
                resolver,
                &CancellationToken::new(),
            )
            .await
            .expect_err("unresolved");
            assert!(matches!(
                error,
                LaunchPinError::Unresolved { selection_key, .. } if selection_key == "web"
            ));
        }
    }

    #[test]
    fn reported_pins_are_recorded_once_and_never_replace_a_recorded_pin() {
        let (recorded, reported) = ([pin("web", RETAINED)], [pin("api", RESOLVED)]);
        let added = record_reported_pins(&recorded, &reported);
        assert_eq!(
            added.recorded,
            Some(vec![pin("api", RESOLVED), pin("web", RETAINED)])
        );
        assert!(added.contradicted.is_empty());

        let recorded = [pin("web", RETAINED)];
        let reported = [pin("web", RETAINED)];
        assert_eq!(
            record_reported_pins(&recorded, &reported),
            ReportedPins {
                recorded: None,
                contradicted: Vec::new(),
            }
        );

        let reported = [pin("web", RESOLVED)];
        let contradiction = record_reported_pins(&recorded, &reported);
        assert_eq!(contradiction.recorded, None);
        assert_eq!(
            contradiction.contradicted,
            [Contradiction {
                recorded: &recorded[0],
                reported: &reported[0],
            }]
        );
    }

    #[tokio::test]
    async fn a_development_source_package_is_resolved_like_an_image() {
        let resolver = RecordingResolver::new(Ok(STORE_PATH));
        let nix = UnpinnedSelection {
            selection_key: "engine".to_owned(),
            source: PinSource::NixDevelopmentSource {
                package: "engines.llama-cpp.native".to_owned(),
                docker_image: false,
            },
        };

        let pins = pin_launch_identities(
            std::slice::from_ref(&nix),
            &[],
            &[],
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect("pins");
        let repeated = pin_launch_identities(
            std::slice::from_ref(&nix),
            &[],
            &pins,
            &platform(),
            Some(&resolver),
            &CancellationToken::new(),
        )
        .await
        .expect("repeated pins");

        assert_eq!(pins, [pin("engine", STORE_PATH)]);
        assert_eq!(repeated, pins);
        assert_eq!(resolver.calls(), [nix.source]);
    }

    #[tokio::test]
    async fn synthetic_pins_take_their_kinds_shape_and_repeat_for_the_same_source() {
        let image = PinSource::LocalImage {
            reference: "registry.test/app:dev".to_owned(),
        };
        let native = PinSource::NixDevelopmentSource {
            package: "engines.llama-cpp.native".to_owned(),
            docker_image: false,
        };
        let docker = PinSource::NixDevelopmentSource {
            package: "apps.open-webui.docker".to_owned(),
            docker_image: true,
        };
        let locked = PinSource::NixLocked {
            store_paths: vec![
                (
                    "x86_64-linux".to_owned(),
                    "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-open-webui"
                        .to_owned(),
                ),
                (
                    "aarch64-darwin".to_owned(),
                    "/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-docker-image-open-webui"
                        .to_owned(),
                ),
            ],
        };
        let arm = OciPlatform {
            architecture: "arm64".to_owned(),
            ..platform()
        };

        let identity = |source: &PinSource| {
            SyntheticLaunchPins::identity(source, &platform()).expect("a synthetic pin")
        };
        let resolved = SyntheticLaunchPins
            .resolve_pin(&native, &platform(), CancellationToken::new())
            .await
            .expect("a synthetic pin");

        for source in [&image, &native, &docker, &locked] {
            assert!(source.kind().admits(&identity(source)), "{source:?}");
            assert_eq!(identity(source), identity(source), "{source:?}");
        }
        assert_eq!(resolved, identity(&native));
        assert_ne!(
            SyntheticLaunchPins::identity(&native, &arm),
            Some(identity(&native))
        );
        assert!(
            identity(&native).ends_with("-simulated-engines-llama-cpp-native"),
            "{}",
            identity(&native)
        );
        let reference =
            crate::nix::docker_launch_reference(&identity(&docker)).expect("a launch reference");
        assert!(
            reference.starts_with("dotcore-nix/docker-image-simulated-apps-open-webui-docker:"),
            "{reference}"
        );

        // A locked artifact pins the catalog path of the platform's Nix system, else any.
        assert_eq!(
            identity(&locked),
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-open-webui"
        );
        assert_eq!(
            SyntheticLaunchPins::identity(&locked, &arm).as_deref(),
            Some("/nix/store/zyxwvsrqpnmlkjihgfdcba9876543210-docker-image-open-webui")
        );
        let riscv = OciPlatform {
            architecture: "riscv64".to_owned(),
            ..platform()
        };
        assert_eq!(
            SyntheticLaunchPins::identity(&locked, &riscv),
            Some(identity(&locked))
        );
        let unlocked = PinSource::NixLocked {
            store_paths: Vec::new(),
        };
        assert!(
            SyntheticLaunchPins
                .resolve_pin(&unlocked, &platform(), CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[test]
    fn recorded_pins_name_distinct_node_resolved_selections_of_their_expected_kind() {
        let expected_kind = |key: &str| match key {
            "web" => Some(PinKind::LocalImage),
            "engine" => Some(PinKind::NixStorePath),
            _ => None,
        };

        assert!(valid_node_pins(&[], expected_kind));
        assert!(valid_node_pins(
            &[pin("engine", STORE_PATH), pin("web", RESOLVED)],
            expected_kind
        ));
        for invalid in [
            vec![pin("api", RESOLVED)],
            vec![pin("web", "registry.test/web:dev")],
            vec![pin("web", RESOLVED), pin("web", RESOLVED)],
            vec![pin("web", STORE_PATH)],
            vec![pin("engine", RESOLVED)],
            vec![pin("engine", "/tmp/llama-server")],
        ] {
            assert!(!valid_node_pins(&invalid, expected_kind), "{invalid:?}");
        }
    }

    fn selection(key: &str) -> UnpinnedSelection {
        UnpinnedSelection {
            selection_key: key.to_owned(),
            source: image(key),
        }
    }

    fn image(key: &str) -> PinSource {
        PinSource::LocalImage {
            reference: format!("registry.test/{key}:dev"),
        }
    }

    fn pin(key: &str, identity: &str) -> NodeResolvedLaunchIdentity {
        NodeResolvedLaunchIdentity {
            selection_key: key.to_owned(),
            launch_identity: identity.to_owned(),
        }
    }

    fn platform() -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    }

    struct RecordingResolver {
        outcome: Result<&'static str, &'static str>,
        calls: Mutex<Vec<PinSource>>,
    }

    impl RecordingResolver {
        fn new(outcome: Result<&'static str, &'static str>) -> Self {
            Self {
                outcome,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<PinSource> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl LaunchPinResolver for RecordingResolver {
        fn resolve_pin<'a>(
            &'a self,
            source: &'a PinSource,
            _: &'a OciPlatform,
            _: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<String, anyhow::Error>> + Send + 'a>> {
            self.calls.lock().expect("calls").push(source.clone());
            let outcome = self
                .outcome
                .map(str::to_owned)
                .map_err(|message| anyhow::anyhow!(message));
            Box::pin(std::future::ready(outcome))
        }
    }
}
