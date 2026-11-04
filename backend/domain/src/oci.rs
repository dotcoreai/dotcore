//! Canonical private OCI image-reference syntax and rendering.

use std::net::{Ipv4Addr, Ipv6Addr};

use dotcore_proto::dotcore::catalog::v1::{
    ApplicationServiceArtifact, EngineArtifact, OciImageReference,
    application_service_artifact::Kind as ServiceArtifactKind,
    engine_artifact::Kind as EngineArtifactKind,
};

const SHA256_PREFIX: &str = "sha256:";

/// Finite validation and canonicalization outcomes for OCI image references.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OciReferenceError {
    #[error("OCI reference requires a repository")]
    MissingRepository,

    #[error("OCI reference has an invalid repository name")]
    InvalidRepositoryName,

    #[error("OCI reference has an invalid registry host")]
    InvalidRegistryHost,

    #[error("OCI reference has an invalid registry port")]
    InvalidRegistryPort,

    #[error("OCI reference registry is ambiguous under Docker semantics")]
    AmbiguousRegistry,

    #[error("OCI repository name implies a Docker registry; use registry location")]
    ImpliedRegistry,

    #[error("OCI reference has an invalid tag")]
    InvalidTag,

    #[error("OCI reference has an invalid digest")]
    InvalidDigest,

    #[error("OCI reference did not round trip canonically")]
    NonCanonicalRoundTrip,

    #[error("invalid OCI image reference")]
    InvalidRenderedReference,

    #[error("invalid OCI image digest")]
    InvalidRenderedDigest,

    #[error("invalid OCI image tag")]
    InvalidRenderedTag,

    #[error("invalid OCI registry IPv6 address")]
    InvalidRenderedRegistryIpv6,

    #[error("invalid OCI registry port")]
    InvalidRenderedRegistryPort,

    #[error("OCI registry requires a repository")]
    MissingRenderedRepository,

    #[error("OCI reference requires an explicit tag or digest")]
    MissingTagOrDigest,
}

impl OciReferenceError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingRepository => "OCI reference requires a repository",
            Self::InvalidRepositoryName => "OCI reference has an invalid repository name",
            Self::InvalidRegistryHost => "OCI reference has an invalid registry host",
            Self::InvalidRegistryPort => "OCI reference has an invalid registry port",
            Self::AmbiguousRegistry => "OCI reference registry is ambiguous under Docker semantics",
            Self::ImpliedRegistry => {
                "OCI repository name implies a Docker registry; use registry location"
            }
            Self::InvalidTag => "OCI reference has an invalid tag",
            Self::InvalidDigest => "OCI reference has an invalid digest",
            Self::NonCanonicalRoundTrip => "OCI reference did not round trip canonically",
            Self::InvalidRenderedReference => "invalid OCI image reference",
            Self::InvalidRenderedDigest => "invalid OCI image digest",
            Self::InvalidRenderedTag => "invalid OCI image tag",
            Self::InvalidRenderedRegistryIpv6 => "invalid OCI registry IPv6 address",
            Self::InvalidRenderedRegistryPort => "invalid OCI registry port",
            Self::MissingRenderedRepository => "OCI registry requires a repository",
            Self::MissingTagOrDigest => "OCI reference requires an explicit tag or digest",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OciReferenceKind {
    TagOnly,
    DigestOnly,
    TagAndDigest,
}

pub fn validate_structured_reference(
    reference: &OciImageReference,
) -> Result<OciReferenceKind, OciReferenceError> {
    let repository = reference
        .repository
        .as_ref()
        .ok_or(OciReferenceError::MissingRepository)?;
    validate_repository_name(&repository.name)?;
    if let Some(registry) = &repository.registry {
        validate_registry_host(&registry.host)?;
        if registry
            .port
            .is_some_and(|port| port == 0 || port > u16::MAX as u32)
        {
            return Err(OciReferenceError::InvalidRegistryPort);
        }
        if !docker_interprets_registry(&registry.host, registry.port) {
            return Err(OciReferenceError::AmbiguousRegistry);
        }
    } else if repository_name_implies_docker_registry(&repository.name) {
        return Err(OciReferenceError::ImpliedRegistry);
    }
    if reference.tag.as_ref().is_some_and(|tag| !valid_tag(tag)) {
        return Err(OciReferenceError::InvalidTag);
    }
    if reference
        .digest
        .as_ref()
        .is_some_and(|digest| digest.algorithm != "sha256" || !valid_lower_hex(&digest.encoded, 64))
    {
        return Err(OciReferenceError::InvalidDigest);
    }
    classify(reference.tag.is_some(), reference.digest.is_some())
}

/// Returns the OCI image of an engine artifact, or `None` for another artifact kind.
pub fn engine_artifact_reference(artifact: &EngineArtifact) -> Option<&OciImageReference> {
    match artifact.kind.as_ref()? {
        EngineArtifactKind::Oci(reference) => Some(reference),
        EngineArtifactKind::Nix(_) => None,
    }
}

/// Returns the OCI image of an application service artifact, or `None` for another artifact kind.
pub fn service_artifact_reference(
    artifact: &ApplicationServiceArtifact,
) -> Option<&OciImageReference> {
    match artifact.kind.as_ref()? {
        ServiceArtifactKind::Oci(reference) => Some(reference),
        ServiceArtifactKind::Nix(_) => None,
    }
}

pub fn render_reference(reference: &OciImageReference) -> Result<String, OciReferenceError> {
    let expected_kind = validate_structured_reference(reference)?;
    let repository = reference
        .repository
        .as_ref()
        .expect("validated OCI reference has a repository");
    let mut rendered = String::new();
    if let Some(registry) = &repository.registry {
        if registry.host.contains(':') {
            rendered.push('[');
            rendered.push_str(&registry.host);
            rendered.push(']');
        } else {
            rendered.push_str(&registry.host);
        }
        if let Some(port) = registry.port {
            rendered.push(':');
            rendered.push_str(&port.to_string());
        }
        rendered.push('/');
    }
    rendered.push_str(&repository.name);
    if let Some(tag) = &reference.tag {
        rendered.push(':');
        rendered.push_str(tag);
    }
    if let Some(digest) = &reference.digest {
        rendered.push('@');
        rendered.push_str(&digest.algorithm);
        rendered.push(':');
        rendered.push_str(&digest.encoded);
    }
    ensure_canonical_round_trip(&rendered, expected_kind)?;
    Ok(rendered)
}

pub fn validate_rendered_reference(reference: &str) -> Result<OciReferenceKind, OciReferenceError> {
    if reference.is_empty()
        || reference.contains("://")
        || reference
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || reference.matches('@').count() > 1
    {
        return Err(OciReferenceError::InvalidRenderedReference);
    }
    let (name_and_tag, has_digest) = match reference.split_once('@') {
        Some((name, digest)) if valid_rendered_digest(digest) => (name, true),
        Some(_) => return Err(OciReferenceError::InvalidRenderedDigest),
        None => (reference, false),
    };
    let last_slash = name_and_tag.rfind('/');
    let tag_separator = name_and_tag
        .rfind(':')
        .filter(|colon| last_slash.is_none_or(|slash| *colon > slash));
    let (repository, has_tag) = match tag_separator {
        Some(colon) if valid_tag(&name_and_tag[colon + 1..]) => (&name_and_tag[..colon], true),
        Some(_) => return Err(OciReferenceError::InvalidRenderedTag),
        None => (name_and_tag, false),
    };
    validate_rendered_repository(repository)?;
    classify(has_tag, has_digest)
}

pub fn canonical_digest_reference(reference: &str) -> Result<Option<String>, OciReferenceError> {
    let kind = validate_rendered_reference(reference)?;
    if kind == OciReferenceKind::TagOnly {
        return Ok(None);
    }
    let (name_and_tag, digest) = reference
        .split_once('@')
        .expect("validated digest-bearing OCI reference has a digest");
    let last_slash = name_and_tag.rfind('/');
    let repository = name_and_tag
        .rfind(':')
        .filter(|colon| last_slash.is_none_or(|slash| *colon > slash))
        .map_or(name_and_tag, |colon| &name_and_tag[..colon]);
    Ok(Some(format!("{repository}@{digest}")))
}

/// Reports whether a value is a Docker local image ID: `sha256:` and 64 lowercase hex digits.
pub fn valid_local_image_id(identity: &str) -> bool {
    valid_rendered_digest(identity)
}

fn ensure_canonical_round_trip(
    rendered: &str,
    expected_kind: OciReferenceKind,
) -> Result<(), OciReferenceError> {
    if validate_rendered_reference(rendered)? != expected_kind {
        return Err(OciReferenceError::NonCanonicalRoundTrip);
    }
    Ok(())
}

fn classify(has_tag: bool, has_digest: bool) -> Result<OciReferenceKind, OciReferenceError> {
    match (has_tag, has_digest) {
        (true, false) => Ok(OciReferenceKind::TagOnly),
        (false, true) => Ok(OciReferenceKind::DigestOnly),
        (true, true) => Ok(OciReferenceKind::TagAndDigest),
        (false, false) => Err(OciReferenceError::MissingTagOrDigest),
    }
}

fn repository_name_implies_docker_registry(repository: &str) -> bool {
    repository
        .split_once('/')
        .is_some_and(|(first, _)| docker_interprets_registry(first, None))
}

fn docker_interprets_registry(host: &str, port: Option<u32>) -> bool {
    port.is_some() || host == "localhost" || host.contains(['.', ':'])
}

fn validate_rendered_repository(repository: &str) -> Result<(), OciReferenceError> {
    if let Some(bracketed) = repository.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or(OciReferenceError::InvalidRenderedRegistryIpv6)?;
        let host = &bracketed[..close];
        validate_registry_host(host)?;
        let remainder = &bracketed[close + 1..];
        let name = if let Some(name) = remainder.strip_prefix('/') {
            name
        } else {
            let port_and_name = remainder
                .strip_prefix(':')
                .ok_or(OciReferenceError::InvalidRenderedRegistryIpv6)?;
            let (port, name) = port_and_name
                .split_once('/')
                .ok_or(OciReferenceError::InvalidRenderedRegistryPort)?;
            validate_rendered_port(port)?;
            name
        };
        return validate_repository_name(name);
    }

    let (first, remainder) = repository
        .split_once('/')
        .map_or((repository, None), |(first, rest)| (first, Some(rest)));
    if first.contains(':') {
        let (host, port) = first
            .split_once(':')
            .expect("colon-containing registry has a host-port separator");
        if port.contains(':') {
            return Err(OciReferenceError::InvalidRenderedRegistryIpv6);
        }
        validate_registry_host(host)?;
        validate_rendered_port(port)?;
        validate_repository_name(remainder.ok_or(OciReferenceError::MissingRenderedRepository)?)
    } else if remainder.is_some() && (first.contains('.') || first == "localhost") {
        validate_registry_host(first)?;
        validate_repository_name(remainder.expect("registry repository"))
    } else {
        validate_repository_name(repository)
    }
}

fn validate_repository_name(value: &str) -> Result<(), OciReferenceError> {
    if value.is_empty() || value.len() > 255 || !value.split('/').all(valid_repository_component) {
        return Err(OciReferenceError::InvalidRepositoryName);
    }
    Ok(())
}

fn valid_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = consume_repository_alphanumeric(bytes, 0);
    if index == 0 {
        return false;
    }
    while index < bytes.len() {
        match bytes[index] {
            b'.' => index += 1,
            b'_' if bytes.get(index + 1) == Some(&b'_') => index += 2,
            b'_' => index += 1,
            b'-' => {
                index += 1;
                while bytes.get(index) == Some(&b'-') {
                    index += 1;
                }
            }
            _ => return false,
        }
        let next = consume_repository_alphanumeric(bytes, index);
        if next == index {
            return false;
        }
        index = next;
    }
    true
}

fn consume_repository_alphanumeric(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        index += 1;
    }
    index
}

fn validate_registry_host(host: &str) -> Result<(), OciReferenceError> {
    if host.is_empty() || host.len() > 253 || host.contains(['[', ']']) {
        return Err(OciReferenceError::InvalidRegistryHost);
    }
    if host.contains(':') {
        return host
            .parse::<Ipv6Addr>()
            .ok()
            .filter(|address| address.to_string() == host)
            .map(|_| ())
            .ok_or(OciReferenceError::InvalidRegistryHost);
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return host
            .parse::<Ipv4Addr>()
            .ok()
            .filter(|address| address.to_string() == host)
            .map(|_| ())
            .ok_or(OciReferenceError::InvalidRegistryHost);
    }
    if host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    }) {
        Ok(())
    } else {
        Err(OciReferenceError::InvalidRegistryHost)
    }
}

fn validate_rendered_port(port: &str) -> Result<(), OciReferenceError> {
    port.parse::<u16>()
        .ok()
        .filter(|value| *value != 0 && value.to_string() == port)
        .map(|_| ())
        .ok_or(OciReferenceError::InvalidRegistryPort)
}

fn valid_tag(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn valid_rendered_digest(value: &str) -> bool {
    value
        .strip_prefix(SHA256_PREFIX)
        .is_some_and(|encoded| valid_lower_hex(encoded, 64))
}

fn valid_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use dotcore_proto::dotcore::catalog::v1::{
        OciDigest, OciImageReference, OciRegistryLocation, OciRepository,
    };

    use super::*;

    const ENCODED: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn structured_references_render_canonically_and_round_trip() {
        let cases = [
            (
                reference(None, "dotcore/hermes", Some("dev"), None),
                "dotcore/hermes:dev",
                OciReferenceKind::TagOnly,
            ),
            (
                reference(None, "dotcore/hermes", None, Some(("sha256", ENCODED))),
                &format!("dotcore/hermes@sha256:{ENCODED}"),
                OciReferenceKind::DigestOnly,
            ),
            (
                reference(
                    Some(("registry.test", Some(5000))),
                    "dotcore/hermes",
                    Some("release_1"),
                    Some(("sha256", ENCODED)),
                ),
                &format!("registry.test:5000/dotcore/hermes:release_1@sha256:{ENCODED}"),
                OciReferenceKind::TagAndDigest,
            ),
            (
                reference(
                    Some(("::1", Some(5000))),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                "[::1]:5000/dotcore/hermes:dev",
                OciReferenceKind::TagOnly,
            ),
        ];

        for (reference, expected, kind) in cases {
            assert_eq!(validate_structured_reference(&reference), Ok(kind));
            let rendered = render_reference(&reference).expect("canonical reference");
            assert_eq!(rendered, expected);
            assert_eq!(validate_rendered_reference(&rendered), Ok(kind));
        }
    }

    #[test]
    fn structured_registry_rendering_preserves_docker_registry_and_repository_identity() {
        let cases = [
            (
                reference(
                    Some(("registry", None)),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                None,
                "dotcore/hermes",
            ),
            (
                reference(
                    Some(("registry", Some(5000))),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                Some("registry:5000"),
                "dotcore/hermes",
            ),
            (
                reference(
                    Some(("registry.test", None)),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                Some("registry.test"),
                "dotcore/hermes",
            ),
            (
                reference(
                    Some(("localhost", None)),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                Some("localhost"),
                "dotcore/hermes",
            ),
            (
                reference(
                    Some(("127.0.0.1", None)),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                Some("127.0.0.1"),
                "dotcore/hermes",
            ),
            (
                reference(
                    Some(("::1", Some(5000))),
                    "dotcore/hermes",
                    Some("dev"),
                    None,
                ),
                Some("[::1]:5000"),
                "dotcore/hermes",
            ),
            (
                reference(None, "registry.test/dotcore/hermes", Some("dev"), None),
                None,
                "registry.test/dotcore/hermes",
            ),
            (
                reference(None, "localhost/dotcore/hermes", Some("dev"), None),
                None,
                "localhost/dotcore/hermes",
            ),
            (
                reference(None, "127.0.0.1/dotcore/hermes", Some("dev"), None),
                None,
                "127.0.0.1/dotcore/hermes",
            ),
        ];

        for (reference, expected_registry, expected_repository) in cases {
            let rendered = render_reference(&reference);
            match expected_registry {
                Some(expected_registry) => {
                    let rendered = rendered.expect("unambiguous structured reference");
                    assert_eq!(
                        docker_repository_identity(&rendered),
                        (Some(expected_registry), expected_repository),
                        "{rendered}",
                    );
                }
                None => assert!(rendered.is_err(), "accepted {reference:?}"),
            }
        }
    }

    #[test]
    fn digest_identity_canonicalization_removes_only_an_explicit_tag() {
        assert_eq!(
            canonical_digest_reference(&format!("registry.test/repo:release@sha256:{ENCODED}")),
            Ok(Some(format!("registry.test/repo@sha256:{ENCODED}"))),
        );
        assert_eq!(
            canonical_digest_reference(&format!("registry.test/repo@sha256:{ENCODED}")),
            Ok(Some(format!("registry.test/repo@sha256:{ENCODED}"))),
        );
        assert_eq!(
            canonical_digest_reference("registry.test/repo:dev"),
            Ok(None)
        );
    }

    #[test]
    fn every_reference_error_has_a_stable_exact_display() {
        let cases = [
            (
                OciReferenceError::MissingRepository,
                "OCI reference requires a repository",
            ),
            (
                OciReferenceError::InvalidRepositoryName,
                "OCI reference has an invalid repository name",
            ),
            (
                OciReferenceError::InvalidRegistryHost,
                "OCI reference has an invalid registry host",
            ),
            (
                OciReferenceError::InvalidRegistryPort,
                "OCI reference has an invalid registry port",
            ),
            (
                OciReferenceError::AmbiguousRegistry,
                "OCI reference registry is ambiguous under Docker semantics",
            ),
            (
                OciReferenceError::ImpliedRegistry,
                "OCI repository name implies a Docker registry; use registry location",
            ),
            (
                OciReferenceError::InvalidTag,
                "OCI reference has an invalid tag",
            ),
            (
                OciReferenceError::InvalidDigest,
                "OCI reference has an invalid digest",
            ),
            (
                OciReferenceError::NonCanonicalRoundTrip,
                "OCI reference did not round trip canonically",
            ),
            (
                OciReferenceError::InvalidRenderedReference,
                "invalid OCI image reference",
            ),
            (
                OciReferenceError::InvalidRenderedDigest,
                "invalid OCI image digest",
            ),
            (
                OciReferenceError::InvalidRenderedTag,
                "invalid OCI image tag",
            ),
            (
                OciReferenceError::InvalidRenderedRegistryIpv6,
                "invalid OCI registry IPv6 address",
            ),
            (
                OciReferenceError::InvalidRenderedRegistryPort,
                "invalid OCI registry port",
            ),
            (
                OciReferenceError::MissingRenderedRepository,
                "OCI registry requires a repository",
            ),
            (
                OciReferenceError::MissingTagOrDigest,
                "OCI reference requires an explicit tag or digest",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn rendered_validation_reports_every_finite_outcome() {
        let long_tag = format!("repo:{}", "a".repeat(129));
        let uppercase_digest = format!("repo@sha256:{}", "A".repeat(64));
        let short_digest = format!("repo@sha256:{}", "a".repeat(63));
        let cases = [
            ("", OciReferenceError::InvalidRenderedReference),
            ("repo@", OciReferenceError::InvalidRenderedDigest),
            ("repo:", OciReferenceError::InvalidRenderedTag),
            (
                "::1/repo:dev",
                OciReferenceError::InvalidRenderedRegistryIpv6,
            ),
            (
                "[::1]:5000:dev",
                OciReferenceError::InvalidRenderedRegistryPort,
            ),
            (
                "registry.test:0/repo:dev",
                OciReferenceError::InvalidRegistryPort,
            ),
            (
                "registry.test:5000:dev",
                OciReferenceError::MissingRenderedRepository,
            ),
            ("Repo:dev", OciReferenceError::InvalidRepositoryName),
            ("repo", OciReferenceError::MissingTagOrDigest),
            ("repo:bad/tag", OciReferenceError::InvalidRegistryPort),
            ("repo:bad:tag", OciReferenceError::InvalidRegistryPort),
            (
                "repo@sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                OciReferenceError::InvalidRenderedDigest,
            ),
            (
                "repo@@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                OciReferenceError::InvalidRenderedReference,
            ),
            (&uppercase_digest, OciReferenceError::InvalidRenderedDigest),
            (&short_digest, OciReferenceError::InvalidRenderedDigest),
            (&long_tag, OciReferenceError::InvalidRenderedTag),
        ];

        for (reference, expected) in cases {
            let error = validate_rendered_reference(reference).expect_err(reference);
            assert_eq!(error, expected);
            assert_eq!(error.to_string(), expected.message());
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn structured_validation_reports_every_finite_outcome() {
        let cases = [
            (
                OciImageReference::default(),
                OciReferenceError::MissingRepository,
            ),
            (
                reference(None, "", Some("dev"), None),
                OciReferenceError::InvalidRepositoryName,
            ),
            (
                reference(Some(("Registry.test", None)), "repo", Some("dev"), None),
                OciReferenceError::InvalidRegistryHost,
            ),
            (
                reference(Some(("registry.test", Some(0))), "repo", Some("dev"), None),
                OciReferenceError::InvalidRegistryPort,
            ),
            (
                reference(Some(("registry", None)), "repo", Some("dev"), None),
                OciReferenceError::AmbiguousRegistry,
            ),
            (
                reference(None, "registry.test/repo", Some("dev"), None),
                OciReferenceError::ImpliedRegistry,
            ),
            (
                reference(None, "repo", Some("bad/tag"), None),
                OciReferenceError::InvalidTag,
            ),
            (
                reference(None, "repo", None, Some(("sha512", ENCODED))),
                OciReferenceError::InvalidDigest,
            ),
            (
                reference(None, "repo", None, None),
                OciReferenceError::MissingTagOrDigest,
            ),
        ];

        for (reference, expected) in cases {
            let structured =
                validate_structured_reference(&reference).expect_err("invalid reference");
            assert_eq!(structured, expected);
            assert_eq!(structured.to_string(), expected.message());
            assert!(structured.source().is_none());
            let rendered = render_reference(&reference).expect_err("invalid reference");
            assert_eq!(rendered, expected);
            assert_eq!(rendered.to_string(), expected.message());
            assert!(rendered.source().is_none());
        }
    }

    #[test]
    fn canonical_round_trip_invariant_reports_an_exact_error() {
        let round_trip = ensure_canonical_round_trip("repo:dev", OciReferenceKind::DigestOnly)
            .expect_err("mismatched reference kind");
        assert_eq!(round_trip, OciReferenceError::NonCanonicalRoundTrip);
        assert_eq!(
            round_trip.to_string(),
            "OCI reference did not round trip canonically"
        );
        assert!(round_trip.source().is_none());
    }

    fn docker_repository_identity(reference: &str) -> (Option<&str>, &str) {
        let name_and_tag = reference
            .split_once('@')
            .map_or(reference, |(name, _)| name);
        let last_slash = name_and_tag.rfind('/');
        let repository = name_and_tag
            .rfind(':')
            .filter(|colon| last_slash.is_none_or(|slash| *colon > slash))
            .map_or(name_and_tag, |colon| &name_and_tag[..colon]);
        let (first, remainder) = repository
            .split_once('/')
            .expect("test references have a repository path");
        if first == "localhost" || first.contains(['.', ':']) {
            (Some(first), remainder)
        } else {
            (None, repository)
        }
    }

    fn reference(
        registry: Option<(&str, Option<u32>)>,
        name: &str,
        tag: Option<&str>,
        digest: Option<(&str, &str)>,
    ) -> OciImageReference {
        OciImageReference {
            repository: Some(OciRepository {
                registry: registry.map(|(host, port)| OciRegistryLocation {
                    host: host.to_owned(),
                    port,
                }),
                name: name.to_owned(),
            }),
            tag: tag.map(str::to_owned),
            digest: digest.map(|(algorithm, encoded)| OciDigest {
                algorithm: algorithm.to_owned(),
                encoded: encoded.to_owned(),
            }),
        }
    }
}
