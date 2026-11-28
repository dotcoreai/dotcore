//! Readable deployment identities, such as `qwen3-8-27b-k3f9x` or `open-webui-7tq2m`.
//!
//! An identity is a short readable name of what is deployed followed by a five-character suffix.
//! The suffix alone carries uniqueness: it scrambles the site's next deployment sequence one-to-one
//! with the site's random key, so a site never issues the same identity twice and consecutive
//! identities look unrelated. Every identity fits the shared identifier grammar with room left for
//! the names runtimes derive from it.

use sha2::{Digest, Sha256};

/// Longest deployment identity, in bytes, leaving room for derived names such as `<id>-group`.
const MAX_DEPLOYMENT_ID_BYTES: usize = 40;

/// Number of sequences one site can reserve: one per distinct five-character suffix.
const SEQUENCE_CAPACITY: u64 = 1 << SUFFIX_BITS;

/// Readable part used when no source yields one.
const FALLBACK_SLUG: &str = "deployment";

const SUFFIX_LENGTH: usize = 5;
const MAX_SLUG_BYTES: usize = MAX_DEPLOYMENT_ID_BYTES - SUFFIX_LENGTH - 1;
const SUFFIX_BITS: u32 = 25;

/// Crockford base32 in lowercase: no `i`, `l`, `o` or `u`, so suffixes stay unambiguous to read.
const SUFFIX_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Feistel halves span 26 bits, one more than the suffix, and walk back into the suffix range.
const HALF_BITS: u32 = 13;
const HALF_MASK: u32 = (1 << HALF_BITS) - 1;
const FEISTEL_ROUNDS: u8 = 4;

/// Formats the identity for `sequence` under `suffix_key`, named after the first of
/// `slug_sources` that yields a readable name. Returns `None` once the sequence is beyond the
/// site's capacity.
pub(crate) fn deployment_id(
    slug_sources: &[&str],
    suffix_key: u64,
    sequence: u64,
) -> Option<String> {
    if sequence == 0 || sequence > SEQUENCE_CAPACITY {
        return None;
    }
    let slug = slug_sources
        .iter()
        .find_map(|source| slug(source))
        .unwrap_or_else(|| FALLBACK_SLUG.to_owned());
    let index = u32::try_from(sequence - 1).ok()?;
    Some(format!("{slug}-{}", suffix(scramble(suffix_key, index))))
}

/// Parses a stored suffix key: exactly 16 lowercase hexadecimal digits.
pub(crate) fn parse_suffix_key(value: &str) -> Option<u64> {
    if value.len() != 16
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

/// Formats a suffix key the way the site stores it.
pub(crate) fn format_suffix_key(key: u64) -> String {
    format!("{key:016x}")
}

/// Lowercases `source`, turns every run of other characters than ASCII letters and digits into one
/// hyphen and cuts the result at a word boundary to fit the slug budget. Returns `None` unless the
/// result starts with a letter.
fn slug(source: &str) -> Option<String> {
    let mut slug = String::new();
    for character in source
        .chars()
        .map(|character| character.to_ascii_lowercase())
    {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    if !slug.starts_with(|character: char| character.is_ascii_lowercase()) {
        return None;
    }
    if slug.len() > MAX_SLUG_BYTES {
        // A hyphen right after the budget still ends a whole word, so it is part of the search.
        let cut = slug[..=MAX_SLUG_BYTES].rfind('-').unwrap_or(MAX_SLUG_BYTES);
        slug.truncate(cut);
    }
    Some(slug)
}

/// Maps `index` one-to-one onto the suffix range: a keyed Feistel network over 26 bits, repeated
/// until the value falls inside the 25-bit range (cycle walking keeps the mapping one-to-one).
fn scramble(key: u64, index: u32) -> u32 {
    let mut value = index;
    loop {
        value = feistel(key, value);
        if u64::from(value) < SEQUENCE_CAPACITY {
            return value;
        }
    }
}

fn feistel(key: u64, value: u32) -> u32 {
    let (mut left, mut right) = (value >> HALF_BITS, value & HALF_MASK);
    for round in 0..FEISTEL_ROUNDS {
        (left, right) = (right, left ^ round_value(key, round, right));
    }
    (left << HALF_BITS) | right
}

fn round_value(key: u64, round: u8, half: u32) -> u32 {
    let digest = Sha256::new()
        .chain_update(b"dotcore.deployment-id-suffix.v1\0")
        .chain_update(key.to_be_bytes())
        .chain_update([round])
        .chain_update(half.to_be_bytes())
        .finalize();
    u32::from(u16::from_be_bytes([digest[0], digest[1]])) & HALF_MASK
}

fn suffix(value: u32) -> String {
    (0..SUFFIX_LENGTH)
        .rev()
        .map(|position| char::from(SUFFIX_ALPHABET[(value >> (5 * position)) as usize & 31]))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use dotcore_domain::valid_id;

    use super::*;

    const KEY: u64 = 0x0123_4567_89ab_cdef;

    #[test]
    fn identity_is_the_catalog_slug_and_a_scrambled_sequence_suffix() {
        let ids = (1..=3)
            .map(|sequence| deployment_id(&["qwen3-8-27b"], KEY, sequence).expect("identity"))
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            [
                "qwen3-8-27b-3cfmn",
                "qwen3-8-27b-qfqjk",
                "qwen3-8-27b-65nbt"
            ]
        );
        assert_eq!(
            deployment_id(&["open-webui"], KEY, 1).expect("application identity"),
            "open-webui-3cfmn"
        );
    }

    #[test]
    fn another_key_scrambles_the_same_sequences_differently() {
        let first = deployment_id(&["qwen3-8-27b"], KEY, 1).expect("identity");
        let other = deployment_id(&["qwen3-8-27b"], KEY + 1, 1).expect("identity");

        assert_eq!(first, "qwen3-8-27b-3cfmn");
        assert_eq!(other, "qwen3-8-27b-yd750");
    }

    #[test]
    fn slug_comes_from_the_first_readable_source() {
        assert_eq!(
            deployment_id(&["GPT-5.1 Mini (preview)", "openai-compatible"], KEY, 1)
                .expect("named upstream"),
            "gpt-5-1-mini-preview-3cfmn"
        );
        assert_eq!(
            deployment_id(&["4o-mini", "openai-compatible"], KEY, 1).expect("digit start"),
            "openai-compatible-3cfmn"
        );
        assert_eq!(
            deployment_id(&["--", "openai-compatible"], KEY, 1).expect("empty name"),
            "openai-compatible-3cfmn"
        );
        assert_eq!(
            deployment_id(&["é"], KEY, 1).expect("nothing readable"),
            "deployment-3cfmn"
        );
        assert_eq!(
            slug("model--with-trailing-"),
            Some("model-with-trailing".to_owned())
        );
    }

    #[test]
    fn long_sources_are_cut_at_a_word_boundary_within_the_budget() {
        let longest = "application-with-the-longest-valid-identifier-01234567890123456";
        assert_eq!(longest.len(), 63);
        assert!(valid_id(longest));

        let id = deployment_id(&[longest], KEY, SEQUENCE_CAPACITY).expect("identity");

        assert_eq!(id, "application-with-the-longest-valid-xrd6p");
        assert!(id.len() <= MAX_DEPLOYMENT_ID_BYTES);
        for derived in [
            id.clone(),
            format!("{id}-group"),
            format!("{id}-realization"),
            format!("workload-{id}"),
        ] {
            assert!(valid_id(&derived), "{derived} is a valid identifier");
        }
        assert_eq!(slug(&"a".repeat(63)), Some("a".repeat(MAX_SLUG_BYTES)));
        assert_eq!(
            slug("abcdefghij-abcdefghij-abcdefghijkl-next"),
            Some("abcdefghij-abcdefghij-abcdefghijkl".to_owned()),
            "a hyphen right after the budget keeps the whole last word"
        );
    }

    #[test]
    fn sequences_outside_the_site_capacity_have_no_identity() {
        assert_eq!(deployment_id(&["model"], KEY, 0), None);
        assert_eq!(deployment_id(&["model"], KEY, SEQUENCE_CAPACITY + 1), None);
    }

    #[test]
    fn scrambled_suffixes_never_repeat_within_a_sample() {
        let suffixes = (0..50_000_u32)
            .map(|index| scramble(KEY, index))
            .collect::<BTreeSet<_>>();

        assert_eq!(suffixes.len(), 50_000);
        assert!(
            suffixes
                .iter()
                .all(|value| u64::from(*value) < SEQUENCE_CAPACITY)
        );
    }

    #[test]
    fn suffix_keys_round_trip_and_reject_other_forms() {
        assert_eq!(parse_suffix_key(&format_suffix_key(KEY)), Some(KEY));
        assert_eq!(format_suffix_key(0xab), "00000000000000ab");
        assert_eq!(parse_suffix_key("0123456789ABCDEF"), None);
        assert_eq!(parse_suffix_key("0123456789abcde"), None);
        assert_eq!(parse_suffix_key("0123456789abcdeg"), None);
    }
}
