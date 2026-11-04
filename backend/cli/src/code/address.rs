//! The inference API address a coding agent is pointed at, from which the agent gets its
//! OpenAI-compatible base URL.
//!
//! An address is written as `host:port` or `http(s)://host`, with an optional port after a scheme
//! host that otherwise takes the scheme's default. An unspecified bind host, as a
//! server's own listener variable may hold, becomes loopback so the address can be called. Without
//! a given address, this machine with the default inference port is assumed.

use std::{fmt, net::IpAddr, str::FromStr};

use dotcore_backend::composition::DEFAULT_INFERENCE_PORT;
use tonic::{codegen::http::uri::InvalidUri, transport::Uri};

/// A validated inference API address, shown as its origin: scheme, host and port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InferenceAddress {
    secure: bool,
    host: String,
    port: u16,
}

impl InferenceAddress {
    /// The address on this machine at the default inference port.
    pub(super) fn local() -> Self {
        Self {
            secure: false,
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_INFERENCE_PORT,
        }
    }

    /// The OpenAI-compatible base URL under this origin.
    pub(super) fn base_url(&self) -> String {
        format!("{self}/v1")
    }

    /// The URL listing the models this origin serves.
    pub(super) fn models_url(&self) -> String {
        format!("{}/models", self.base_url())
    }
}

impl FromStr for InferenceAddress {
    type Err = InferenceAddressError;

    /// Accepts `host:port`, `http://host[:port]` or `https://host[:port]`, with an optional trailing
    /// slash; a scheme without a port takes the scheme's default. Any other scheme, a scheme-less
    /// host without a port, a path, a query, or user information is refused.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(InferenceAddressError::Empty);
        }
        // A trailing slash is only valid URI syntax after a scheme.
        let uri = if text.contains("://") {
            text
        } else {
            text.strip_suffix('/').unwrap_or(text)
        };
        let parsed = uri
            .parse::<Uri>()
            .map_err(|source| InferenceAddressError::Invalid {
                address: text.to_owned(),
                source,
            })?;
        let secure = match parsed.scheme_str() {
            None | Some("http") => false,
            Some("https") => true,
            Some(scheme) => {
                return Err(InferenceAddressError::UnsupportedScheme {
                    address: text.to_owned(),
                    scheme: scheme.to_owned(),
                });
            }
        };
        let shape = || InferenceAddressError::Shape {
            address: text.to_owned(),
        };
        let authority = parsed
            .authority()
            .filter(|authority| !authority.as_str().contains('@'))
            .ok_or_else(shape)?;
        let port = match (authority.port_u16(), parsed.scheme_str()) {
            (Some(port), _) => port,
            (None, Some("http")) => 80,
            (None, Some("https")) => 443,
            (None, _) => return Err(shape()),
        };
        if parsed
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
        {
            return Err(shape());
        }

        Ok(Self {
            secure,
            host: callable_host(authority.host()),
            port,
        })
    }
}

impl fmt::Display for InferenceAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.secure { "https" } else { "http" };
        write!(formatter, "{scheme}://{}:{}", self.host, self.port)
    }
}

/// An inference API address the operator gave that cannot be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InferenceAddressError {
    #[error("inference API address is empty")]
    Empty,

    #[error("inference API address {address:?} is not host:port or http(s)://host[:port]")]
    Invalid {
        address: String,
        #[source]
        source: InvalidUri,
    },

    #[error(
        "inference API address {address:?} uses {scheme}://; the inference API takes http:// or https://"
    )]
    UnsupportedScheme { address: String, scheme: String },

    #[error("inference API address {address:?} must be host:port or http(s)://host[:port]")]
    Shape { address: String },
}

/// Replaces an unspecified bind host with the loopback host of the same family.
fn callable_host(host: &str) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) if address.is_unspecified() => "127.0.0.1".to_owned(),
        Ok(IpAddr::V6(address)) if address.is_unspecified() => "[::1]".to_owned(),
        _ => host.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(text: &str) -> Result<String, String> {
        text.parse::<InferenceAddress>()
            .map(|address| address.to_string())
            .map_err(|error| error.to_string())
    }

    #[test]
    fn addresses_keep_their_scheme_and_default_to_http() {
        assert_eq!(
            origin("127.0.0.1:50052"),
            Ok("http://127.0.0.1:50052".into())
        );
        assert_eq!(origin("localhost:9000"), Ok("http://localhost:9000".into()));
        assert_eq!(
            origin("http://10.0.0.5:50052"),
            Ok("http://10.0.0.5:50052".into())
        );
        assert_eq!(
            origin("https://dotcore.example:8443"),
            Ok("https://dotcore.example:8443".into())
        );
        assert_eq!(origin("[::1]:50052"), Ok("http://[::1]:50052".into()));
    }

    #[test]
    fn a_trailing_slash_is_dropped() {
        assert_eq!(
            origin("10.0.0.5:50052/"),
            Ok("http://10.0.0.5:50052".into())
        );
        assert_eq!(
            origin("http://10.0.0.5:50052/"),
            Ok("http://10.0.0.5:50052".into())
        );
        assert_eq!(
            origin("https://10.0.0.5:50052/"),
            Ok("https://10.0.0.5:50052".into())
        );
    }

    #[test]
    fn scheme_addresses_without_a_port_take_the_scheme_default() {
        assert_eq!(origin("http://h"), Ok("http://h:80".into()));
        assert_eq!(origin("https://h/"), Ok("https://h:443".into()));
        assert_eq!(origin("https://h:8443"), Ok("https://h:8443".into()));
        assert_eq!(origin("http://0.0.0.0"), Ok("http://127.0.0.1:80".into()));
    }

    #[test]
    fn unspecified_bind_hosts_become_loopback() {
        assert_eq!(origin("0.0.0.0:50052"), Ok("http://127.0.0.1:50052".into()));
        assert_eq!(origin("[::]:50052"), Ok("http://[::1]:50052".into()));
        assert_eq!(
            origin("http://0.0.0.0:7000"),
            Ok("http://127.0.0.1:7000".into())
        );
    }

    #[test]
    fn addresses_without_a_port_or_with_extra_parts_are_refused() {
        assert_eq!(origin(""), Err("inference API address is empty".into()));
        assert_eq!(
            origin("ftp://10.0.0.5:50052"),
            Err(
                "inference API address \"ftp://10.0.0.5:50052\" uses ftp://; the inference API \
                 takes http:// or https://"
                    .into()
            )
        );
        for shape in [
            "10.0.0.5",
            "h",
            "http://10.0.0.5:50052/v1",
            "http://10.0.0.5:50052?x=1",
            "http://user@10.0.0.5:50052",
        ] {
            assert_eq!(
                origin(shape),
                Err(format!(
                    "inference API address {shape:?} must be host:port or http(s)://host[:port]"
                )),
                "{shape}"
            );
        }
        assert_eq!(
            origin("not an address"),
            Err(
                "inference API address \"not an address\" is not host:port or http(s)://host[:port]"
                    .into()
            )
        );
    }

    #[test]
    fn the_local_address_uses_loopback_and_the_default_port() {
        assert_eq!(
            InferenceAddress::local().to_string(),
            "http://127.0.0.1:50052"
        );
    }

    #[test]
    fn the_base_url_is_the_v1_path_of_the_origin() {
        let address = "https://10.0.0.5:8443/"
            .parse::<InferenceAddress>()
            .expect("address");

        assert_eq!(address.base_url(), "https://10.0.0.5:8443/v1");
        assert_eq!(address.models_url(), "https://10.0.0.5:8443/v1/models");
    }
}
