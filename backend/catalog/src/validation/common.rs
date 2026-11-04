//! Common catalog and site-compatible scalar validation.

use std::collections::BTreeSet;

use dotcore_domain::oci::OciReferenceKind;
use dotcore_proto::dotcore::{
    catalog::v1::{ApplicationServiceArtifact, OciImageReference, OciPlatform, Selector},
    observation::v1::AcceleratorMemoryArchitecture,
};

use crate::OciArtifactPolicy;

/// Validates an OCI application service artifact: its reference against the catalog's digest
/// policy and the grammar of its supported platforms.
pub(super) fn validate_oci_artifact(
    reference: &OciImageReference,
    supported_platforms: &[OciPlatform],
    policy: OciArtifactPolicy,
) -> Result<(), &'static str> {
    validate_oci_reference(reference, policy)?;
    if supported_platforms
        .iter()
        .any(|platform| !valid_platform(platform))
    {
        return Err("OCI artifact has an invalid supported platform");
    }
    Ok(())
}

/// Validates a structured OCI reference against the catalog's digest policy.
pub(super) fn validate_oci_reference(
    reference: &OciImageReference,
    policy: OciArtifactPolicy,
) -> Result<(), &'static str> {
    let kind = dotcore_domain::oci::validate_structured_reference(reference)
        .map_err(dotcore_domain::oci::OciReferenceError::message)?;
    if policy == OciArtifactPolicy::DigestRequired && kind == OciReferenceKind::TagOnly {
        return Err("OCI artifact policy requires a digest");
    }
    Ok(())
}

pub(super) fn validate_application_artifact_platforms(
    artifact: &ApplicationServiceArtifact,
) -> Result<(), &'static str> {
    if artifact.supported_platforms.is_empty() {
        return Err("Application service artifact requires at least one supported platform");
    }
    let mut platforms = BTreeSet::new();
    for platform in &artifact.supported_platforms {
        if platform.operating_system != "linux"
            || !matches!(platform.architecture.as_str(), "amd64" | "arm64")
            || platform.variant.is_some()
        {
            return Err(
                "Application service artifact requires concrete linux/amd64 or linux/arm64 platforms without variants",
            );
        }
        if !platforms.insert((
            platform.operating_system.as_str(),
            platform.architecture.as_str(),
        )) {
            return Err("Application service artifact has a duplicate supported platform");
        }
    }
    Ok(())
}

fn valid_platform(platform: &OciPlatform) -> bool {
    valid_platform_component(&platform.operating_system)
        && valid_platform_component(&platform.architecture)
        && platform
            .variant
            .as_ref()
            .is_none_or(|variant| valid_platform_component(variant))
}

fn valid_platform_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

/// Whether a selector's memory architecture is a known value: unified, dedicated, or unspecified
/// to match either.
pub(super) fn valid_selector_memory_architecture(selector: &Selector) -> bool {
    AcceleratorMemoryArchitecture::try_from(selector.memory_architecture).is_ok()
}

pub(super) fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        OciDigest, OciImageReference, OciPlatform, OciRegistryLocation, OciRepository,
    };

    use super::validate_oci_artifact;
    use crate::OciArtifactPolicy;

    #[test]
    fn development_accepts_tag_digest_and_combined_references_while_strict_requires_digest() {
        for (tag, digest) in [
            (Some("dev"), None),
            (None, Some(valid_digest())),
            (Some("release"), Some(valid_digest())),
        ] {
            let artifact = artifact(tag, digest);
            assert_eq!(
                validate_oci_artifact(
                    &artifact.reference,
                    &artifact.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Ok(())
            );
            assert_eq!(
                validate_oci_artifact(
                    &artifact.reference,
                    &artifact.supported_platforms,
                    OciArtifactPolicy::DigestRequired
                ),
                if digest.is_some() {
                    Ok(())
                } else {
                    Err("OCI artifact policy requires a digest")
                }
            );
        }
    }

    #[test]
    fn structured_oci_validation_rejects_each_unsafe_or_incomplete_field() {
        let cases = [
            (
                artifact(None, None),
                "OCI reference requires an explicit tag or digest",
            ),
            (artifact(Some(""), None), "OCI reference has an invalid tag"),
            (
                artifact(Some("bad:tag"), None),
                "OCI reference has an invalid tag",
            ),
            (
                artifact(Some(&"a".repeat(129)), None),
                "OCI reference has an invalid tag",
            ),
            (
                artifact(Some("dev"), Some(("sha512", &"a".repeat(64)))),
                "OCI reference has an invalid digest",
            ),
            (
                artifact(Some("dev"), Some(("sha256", &"A".repeat(64)))),
                "OCI reference has an invalid digest",
            ),
        ];
        for (artifact, expected) in cases {
            assert_eq!(
                validate_oci_artifact(
                    &artifact.reference,
                    &artifact.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Err(expected),
            );
        }

        let mut invalid = artifact(Some("dev"), None);
        invalid.reference.repository = None;
        assert_eq!(
            validate_oci_artifact(
                &invalid.reference,
                &invalid.supported_platforms,
                OciArtifactPolicy::Development
            ),
            Err("OCI reference requires a repository"),
        );

        for name in [
            "",
            "/image",
            "image/",
            "Image",
            "image:dev",
            "image@sha256",
            "image..part",
            "image._part",
        ] {
            let mut invalid = artifact(Some("dev"), None);
            invalid
                .reference
                .repository
                .as_mut()
                .expect("repository")
                .name = name.to_owned();
            assert_eq!(
                validate_oci_artifact(
                    &invalid.reference,
                    &invalid.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Err("OCI reference has an invalid repository name"),
            );
        }

        for host in [
            "",
            "https://registry.test",
            "Registry.test",
            ".registry.test",
        ] {
            let mut invalid = artifact(Some("dev"), None);
            invalid
                .reference
                .repository
                .as_mut()
                .expect("repository")
                .registry = Some(OciRegistryLocation {
                host: host.to_owned(),
                port: Some(5_000),
            });
            assert_eq!(
                validate_oci_artifact(
                    &invalid.reference,
                    &invalid.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Err("OCI reference has an invalid registry host"),
            );
        }

        for port in [0, 65_536] {
            let mut invalid = artifact(Some("dev"), None);
            invalid
                .reference
                .repository
                .as_mut()
                .expect("repository")
                .registry
                .as_mut()
                .expect("registry")
                .port = Some(port);
            assert_eq!(
                validate_oci_artifact(
                    &invalid.reference,
                    &invalid.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Err("OCI reference has an invalid registry port"),
            );
        }
    }

    #[test]
    fn structured_oci_validation_accepts_registry_platform_and_canonical_name_grammar() {
        let mut artifact = artifact(Some("v1.2-rc_1"), Some(valid_digest()));
        let repository = artifact.reference.repository.as_mut().expect("repository");
        repository.name = "dotcore/hermes__app.test".to_owned();
        repository.registry.as_mut().expect("registry").host = "::1".to_owned();
        artifact.supported_platforms = vec![OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: Some("v8".to_owned()),
        }];
        assert_eq!(
            validate_oci_artifact(
                &artifact.reference,
                &artifact.supported_platforms,
                OciArtifactPolicy::Development
            ),
            Ok(()),
        );

        for (operating_system, architecture, variant) in [
            ("", "amd64", None),
            ("linux", "", None),
            ("Linux", "amd64", None),
            ("linux", "amd64", Some("")),
            ("linux", "arm64", Some("v8/unsafe")),
        ] {
            let mut invalid = artifact.clone();
            invalid.supported_platforms = vec![OciPlatform {
                operating_system: operating_system.to_owned(),
                architecture: architecture.to_owned(),
                variant: variant.map(str::to_owned),
            }];
            assert_eq!(
                validate_oci_artifact(
                    &invalid.reference,
                    &invalid.supported_platforms,
                    OciArtifactPolicy::Development
                ),
                Err("OCI artifact has an invalid supported platform"),
            );
        }
    }

    /// OCI reference and supported platforms of one service artifact under test.
    #[derive(Clone)]
    struct Artifact {
        reference: OciImageReference,
        supported_platforms: Vec<OciPlatform>,
    }

    fn artifact(tag: Option<&str>, digest: Option<(&str, &str)>) -> Artifact {
        Artifact {
            reference: OciImageReference {
                repository: Some(OciRepository {
                    registry: Some(OciRegistryLocation {
                        host: "127.0.0.1".to_owned(),
                        port: Some(5_000),
                    }),
                    name: "dotcore/hermes_app.test".to_owned(),
                }),
                tag: tag.map(str::to_owned),
                digest: digest.map(|(algorithm, encoded)| OciDigest {
                    algorithm: algorithm.to_owned(),
                    encoded: encoded.to_owned(),
                }),
            },
            supported_platforms: Vec::new(),
        }
    }

    fn valid_digest() -> (&'static str, &'static str) {
        (
            "sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
    }
}
