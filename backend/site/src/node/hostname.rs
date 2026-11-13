//! Derives a default SiteNode display name from the machine's hostname, for example `studio` from
//! the macOS-style `studio.local`.
//!
//! The name is the hostname as reported, with only a trailing `.local` removed. Nothing is returned
//! when the hostname is unavailable or would not be a valid display name, so callers can fall back
//! to another name such as the node ID.

use sysinfo::System;

use crate::validation::is_valid_display_name;

const LOCAL_SUFFIX: &str = ".local";

/// Returns the default SiteNode display name for this machine, when it has a usable hostname.
pub fn local_node_name() -> Option<String> {
    node_name_from_hostname(&System::host_name().ok()?)
}

/// Normalizes a raw hostname into a SiteNode display name.
///
/// Surrounding whitespace, one trailing `.`, and a trailing `.local` (any case) are removed. Returns
/// nothing when the result is not a valid display name.
pub fn node_name_from_hostname(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.strip_suffix('.').unwrap_or(name);
    let name = strip_local_suffix(name);
    is_valid_display_name(name).then(|| name.to_owned())
}

fn strip_local_suffix(name: &str) -> &str {
    let Some(split) = name.len().checked_sub(LOCAL_SUFFIX.len()) else {
        return name;
    };
    match name.get(split..) {
        Some(suffix) if suffix.eq_ignore_ascii_case(LOCAL_SUFFIX) => &name[..split],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::node_name_from_hostname;

    #[test]
    fn removes_only_a_trailing_local_suffix() {
        for (raw, expected) in [
            ("studio.local", Some("studio")),
            ("Studio.LOCAL", Some("Studio")),
            ("studio.local.", Some("studio")),
            ("  studio.local\n", Some("studio")),
            ("spark-01", Some("spark-01")),
            ("host.example.com", Some("host.example.com")),
            ("local", Some("local")),
            ("my.localhost", Some("my.localhost")),
            ("é.local", Some("é")),
            (".local", None),
            ("", None),
            ("   ", None),
        ] {
            assert_eq!(
                node_name_from_hostname(raw).as_deref(),
                expected,
                "hostname {raw:?}"
            );
        }
    }

    #[test]
    fn rejects_names_longer_than_the_display_name_limit() {
        assert_eq!(node_name_from_hostname(&"a".repeat(129)), None);
        assert_eq!(
            node_name_from_hostname(&"a".repeat(128)),
            Some("a".repeat(128))
        );
    }
}
