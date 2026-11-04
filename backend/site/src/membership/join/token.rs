//! The one-time join token an operator copies from the manager machine to a new machine. It
//! names the manager's join endpoint, pins the site and manager identity the new machine must
//! see, and carries a random secret that authorizes one join.
//!
//! The token is a single opaque string with its own version, for example `dcjoin1.eyJ...`. It
//! is never part of the wire contract, so the two evolve separately.

use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use url::Url;

use crate::node::folder::{CertificateFingerprint, NodeFolderContractError};

/// Length in bytes of the random join secret (256 bits).
pub const JOIN_SECRET_BYTES: usize = 32;

const TOKEN_FAMILY: &str = "dcjoin";
const TOKEN_VERSION: u32 = 1;

/// Everything a new machine needs to find, authenticate, and join one site.
#[derive(Clone, Debug)]
pub struct JoinToken {
    /// Address of the manager's one-time join listener.
    pub endpoint: Url,

    /// Site the new node will join.
    pub site_id: String,

    /// Site manager node hosting the join; its identity is checked in the TLS handshake.
    pub manager_node_id: String,

    /// SHA-256 of the manager's leaf certificate, which the join listener must present.
    pub manager_certificate_sha256: CertificateFingerprint,

    /// SHA-256 of the site CA certificate that must anchor the presented chain.
    pub site_ca_sha256: CertificateFingerprint,

    /// Random secret authorizing one join.
    pub secret: JoinSecret,
}

impl JoinToken {
    /// Creates a token with a fresh 256-bit secret from the operating system's CSPRNG.
    pub fn generate(
        endpoint: Url,
        site_id: impl Into<String>,
        manager_node_id: impl Into<String>,
        manager_certificate_sha256: CertificateFingerprint,
        site_ca_sha256: CertificateFingerprint,
    ) -> Result<Self, JoinTokenError> {
        let token = Self {
            endpoint,
            site_id: site_id.into(),
            manager_node_id: manager_node_id.into(),
            manager_certificate_sha256,
            site_ca_sha256,
            secret: JoinSecret::generate()?,
        };
        token.validate()?;
        Ok(token)
    }

    /// Renders the token as one line of text safe to paste into a prompt or file.
    pub fn encode(&self) -> String {
        let wire = WireToken {
            endpoint: self.endpoint.as_str().to_owned(),
            site_id: self.site_id.clone(),
            manager_node_id: self.manager_node_id.clone(),
            manager_certificate_sha256: self.manager_certificate_sha256.as_str().to_owned(),
            site_ca_sha256: self.site_ca_sha256.as_str().to_owned(),
            secret: URL_SAFE_NO_PAD.encode(self.secret.0),
        };
        let json = serde_json::to_vec(&wire).expect("token fields always serialize");
        format!(
            "{TOKEN_FAMILY}{TOKEN_VERSION}.{}",
            URL_SAFE_NO_PAD.encode(json)
        )
    }

    /// Parses a token produced by [`JoinToken::encode`], ignoring surrounding whitespace.
    pub fn parse(text: &str) -> Result<Self, JoinTokenError> {
        let text = text.trim();
        let (label, payload) = text.split_once('.').ok_or(JoinTokenError::NotAToken)?;
        let version = label
            .strip_prefix(TOKEN_FAMILY)
            .ok_or(JoinTokenError::NotAToken)?;
        if version != TOKEN_VERSION.to_string() {
            return Err(JoinTokenError::UnsupportedVersion {
                found: version.to_owned(),
                supported: TOKEN_VERSION,
            });
        }

        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|source| JoinTokenError::BadEncoding(source.to_string()))?;
        let wire: WireToken = serde_json::from_slice(&json)
            .map_err(|source| JoinTokenError::BadPayload(source.to_string()))?;
        let endpoint =
            Url::parse(&wire.endpoint).map_err(|source| JoinTokenError::InvalidField {
                field: "endpoint",
                reason: source.to_string(),
            })?;
        let secret = URL_SAFE_NO_PAD
            .decode(&wire.secret)
            .ok()
            .and_then(|bytes| <[u8; JOIN_SECRET_BYTES]>::try_from(bytes).ok())
            .ok_or_else(|| JoinTokenError::InvalidField {
                field: "secret",
                reason: format!("must be {JOIN_SECRET_BYTES} bytes"),
            })?;

        let token = Self {
            endpoint,
            site_id: wire.site_id,
            manager_node_id: wire.manager_node_id,
            manager_certificate_sha256: CertificateFingerprint::parse(
                wire.manager_certificate_sha256,
            )
            .map_err(|source| fingerprint_error("manager_certificate_sha256", source))?,
            site_ca_sha256: CertificateFingerprint::parse(wire.site_ca_sha256)
                .map_err(|source| fingerprint_error("site_ca_sha256", source))?,
            secret: JoinSecret(secret),
        };
        token.validate()?;
        Ok(token)
    }

    fn validate(&self) -> Result<(), JoinTokenError> {
        let invalid = |field, reason: &str| JoinTokenError::InvalidField {
            field,
            reason: reason.to_owned(),
        };
        if self.endpoint.scheme() != "https" {
            return Err(invalid("endpoint", "must use https"));
        }
        if self.endpoint.host_str().is_none() || self.endpoint.port().is_none() {
            return Err(invalid("endpoint", "must include a host and a port"));
        }
        if !self.endpoint.username().is_empty()
            || self.endpoint.password().is_some()
            || self.endpoint.query().is_some()
            || self.endpoint.fragment().is_some()
            || self.endpoint.path() != "/"
        {
            return Err(invalid("endpoint", "must be a bare scheme, host, and port"));
        }
        crate::node::folder::certificate_identity_uri(&self.site_id, &self.manager_node_id)
            .map_err(|source| fingerprint_error("site_id or manager_node_id", source))?;
        Ok(())
    }
}

/// The 256-bit random value that authorizes one join. Its debug output is redacted and
/// comparison takes constant time.
#[derive(Clone)]
pub struct JoinSecret([u8; JOIN_SECRET_BYTES]);

impl JoinSecret {
    /// Draws a fresh secret from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, JoinTokenError> {
        let mut bytes = [0_u8; JOIN_SECRET_BYTES];
        getrandom::fill(&mut bytes).map_err(JoinTokenError::Randomness)?;
        Ok(Self(bytes))
    }

    /// Wraps the bytes a joining node presented on the wire.
    pub fn from_bytes(bytes: [u8; JOIN_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    /// Returns the secret bytes for transmission by the joining node.
    pub fn as_bytes(&self) -> &[u8; JOIN_SECRET_BYTES] {
        &self.0
    }

    /// Compares a presented value against this secret in constant time; a value of the wrong
    /// length never matches.
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.0.as_slice().ct_eq(presented).into()
    }
}

impl fmt::Debug for JoinSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JoinSecret(<redacted>)")
    }
}

/// A join token could not be created or does not parse; the message says what to fix.
#[derive(Debug, thiserror::Error)]
pub enum JoinTokenError {
    #[error("this is not a dotcore join token; copy the whole token")]
    NotAToken,

    #[error(
        "join token version {found:?} is not supported by this build (supported: {supported}); \
         use matching dotcore versions on both machines"
    )]
    UnsupportedVersion { found: String, supported: u32 },

    #[error("join token is damaged or truncated ({0}); copy the whole token again")]
    BadEncoding(String),

    #[error("join token content is invalid ({0}); request a new token")]
    BadPayload(String),

    #[error("join token field {field} is invalid: {reason}")]
    InvalidField { field: &'static str, reason: String },

    #[error("could not draw a join secret from the system random source")]
    Randomness(#[source] getrandom::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireToken {
    endpoint: String,
    site_id: String,
    manager_node_id: String,
    manager_certificate_sha256: String,
    site_ca_sha256: String,
    secret: String,
}

fn fingerprint_error(field: &'static str, source: NodeFolderContractError) -> JoinTokenError {
    JoinTokenError::InvalidField {
        field,
        reason: source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: u8) -> CertificateFingerprint {
        CertificateFingerprint::from_bytes([byte; 32])
    }

    fn token() -> JoinToken {
        JoinToken::generate(
            "https://10.0.0.1:7444".parse().expect("endpoint"),
            "site-one",
            "node-manager",
            fingerprint(1),
            fingerprint(2),
        )
        .expect("token")
    }

    fn wire_token(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let encoded = token().encode();
        let payload = encoded.split_once('.').expect("separator").1;
        let mut json: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("decode"))
                .expect("json");
        edit(&mut json);
        format!(
            "{TOKEN_FAMILY}{TOKEN_VERSION}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json).expect("serialize"))
        )
    }

    #[test]
    fn token_round_trips_every_field_with_a_fresh_secret() {
        let original = token();
        let parsed = JoinToken::parse(&format!("  {}\n", original.encode())).expect("parse");

        assert_eq!(parsed.endpoint.as_str(), "https://10.0.0.1:7444/");
        assert_eq!(parsed.site_id, "site-one");
        assert_eq!(parsed.manager_node_id, "node-manager");
        assert_eq!(parsed.manager_certificate_sha256, fingerprint(1));
        assert_eq!(parsed.site_ca_sha256, fingerprint(2));
        assert_eq!(parsed.secret.as_bytes(), original.secret.as_bytes());
        assert_ne!(token().secret.as_bytes(), original.secret.as_bytes());
        assert!(original.encode().starts_with("dcjoin1."));
    }

    #[test]
    fn parse_rejects_wrong_version_truncation_and_bad_encoding() {
        let encoded = token().encode();

        assert!(matches!(
            JoinToken::parse(&encoded.replacen("dcjoin1", "dcjoin2", 1)),
            Err(JoinTokenError::UnsupportedVersion { found, supported: 1 }) if found == "2"
        ));
        assert!(matches!(
            JoinToken::parse(&encoded[..encoded.len() - 10]),
            Err(JoinTokenError::BadPayload(_)) | Err(JoinTokenError::BadEncoding(_))
        ));
        assert!(matches!(
            JoinToken::parse("dcjoin1.not base64!"),
            Err(JoinTokenError::BadEncoding(_))
        ));
        assert!(matches!(
            JoinToken::parse("dcjoin1.e30"),
            Err(JoinTokenError::BadPayload(_))
        ));
        assert!(matches!(
            JoinToken::parse("hello"),
            Err(JoinTokenError::NotAToken)
        ));
        assert!(matches!(
            JoinToken::parse(""),
            Err(JoinTokenError::NotAToken)
        ));
    }

    #[test]
    fn parse_rejects_invalid_fields() {
        for (field, value) in [
            ("endpoint", "http://10.0.0.1:7444"),
            ("endpoint", "https://10.0.0.1"),
            ("endpoint", "https://10.0.0.1:7444/path"),
            ("secret", "AAAA"),
            ("manager_certificate_sha256", "abc"),
            ("site_id", "Not Valid"),
        ] {
            let text = wire_token(|json| json[field] = value.into());
            assert!(
                matches!(
                    JoinToken::parse(&text),
                    Err(JoinTokenError::InvalidField { .. })
                ),
                "{field}={value} must be rejected"
            );
        }
        let text = wire_token(|json| json["extra"] = "value".into());
        assert!(matches!(
            JoinToken::parse(&text),
            Err(JoinTokenError::BadPayload(_))
        ));
    }

    #[test]
    fn secret_comparison_matches_only_the_exact_value() {
        let secret = JoinSecret::from_bytes([7; JOIN_SECRET_BYTES]);
        let mut near = [7_u8; JOIN_SECRET_BYTES];
        near[JOIN_SECRET_BYTES - 1] = 8;

        assert!(secret.matches(&[7; JOIN_SECRET_BYTES]));
        assert!(!secret.matches(&near));
        assert!(!secret.matches(&[7; JOIN_SECRET_BYTES - 1]));
        assert!(!secret.matches(&[]));
        assert_eq!(format!("{secret:?}"), "JoinSecret(<redacted>)");
    }
}
