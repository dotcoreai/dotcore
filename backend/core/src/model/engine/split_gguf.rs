//! Split GGUF sets shared by the engine adapters.
//!
//! A model artifact can hold one GGUF model split into numbered shards in one folder. Engines load
//! such a set from its first shard and find the others by name, so this module checks that a list of
//! paths is exactly one complete set and names its first shard.

use std::collections::BTreeSet;

/// Returns the relative path of the first shard when `paths` form exactly one complete split GGUF
/// set, such as `Q4/model-00001-of-00003.gguf` through `Q4/model-00003-of-00003.gguf`. Every shard
/// shares one directory, prefix, and shard count.
pub(crate) fn split_gguf_entry<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<String, SplitGgufError> {
    let mut set: Option<(&str, &str, u32)> = None;
    let mut indexes = BTreeSet::new();
    let mut entry = None;
    for path in paths {
        let shard = SplitGgufShard::parse(path)?;
        let (directory, prefix, count) =
            *set.get_or_insert((shard.directory, shard.prefix, shard.count));
        if shard.directory != directory {
            return Err(SplitGgufError::MixedDirectories);
        }
        if shard.prefix != prefix {
            return Err(SplitGgufError::MixedPrefixes);
        }
        if shard.count != count {
            return Err(SplitGgufError::InconsistentCount);
        }
        if shard.index == 0 || shard.index > count {
            return Err(SplitGgufError::IndexOutOfRange { index: shard.index });
        }
        if !indexes.insert(shard.index) {
            return Err(SplitGgufError::DuplicateIndex { index: shard.index });
        }
        if shard.index == 1 {
            entry = Some(path.to_owned());
        }
    }

    let Some((_, _, count)) = set else {
        return Err(SplitGgufError::TooFewShards);
    };
    if count < 2 {
        return Err(SplitGgufError::TooFewShards);
    }
    if let Some(index) = (1..=count).find(|index| !indexes.contains(index)) {
        return Err(SplitGgufError::MissingShard { index });
    }
    entry.ok_or(SplitGgufError::MissingShard { index: 1 })
}

/// Reports why a set of paths is not one complete split GGUF set.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SplitGgufError {
    #[error(
        "split GGUF file {path:?} is not named <prefix>-NNNNN-of-NNNNN.gguf with five-digit shard \
         numbers"
    )]
    InvalidShardName { path: String },

    #[error("split GGUF set needs at least two shards")]
    TooFewShards,

    #[error("split GGUF shards are in different directories")]
    MixedDirectories,

    #[error("split GGUF shards have different name prefixes")]
    MixedPrefixes,

    #[error("split GGUF shards declare different shard counts")]
    InconsistentCount,

    #[error("split GGUF shard {index} is outside the declared shard count")]
    IndexOutOfRange { index: u32 },

    #[error("split GGUF shard {index} is declared more than once")]
    DuplicateIndex { index: u32 },

    #[error("split GGUF shard {index} is missing")]
    MissingShard { index: u32 },
}

/// One file name parsed against the `<prefix>-%05d-of-%05d.gguf` split naming.
struct SplitGgufShard<'a> {
    directory: &'a str,
    prefix: &'a str,
    index: u32,
    count: u32,
}

impl<'a> SplitGgufShard<'a> {
    fn parse(path: &'a str) -> Result<Self, SplitGgufError> {
        let invalid = || SplitGgufError::InvalidShardName {
            path: path.to_owned(),
        };
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        let stem = name.strip_suffix(".gguf").ok_or_else(invalid)?;
        // The suffix is "-NNNNN-of-NNNNN": 15 ASCII bytes after a non-empty prefix.
        let split = stem
            .len()
            .checked_sub(15)
            .filter(|&split| split > 0 && stem.is_char_boundary(split))
            .ok_or_else(invalid)?;
        let (prefix, suffix) = stem.split_at(split);
        let digits = |value: &str| {
            (value.len() == 5 && value.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| value.parse::<u32>().ok())
                .flatten()
        };
        let (index, count) = suffix
            .strip_prefix('-')
            .and_then(|rest| rest.split_once("-of-"))
            .and_then(|(index, count)| Some((digits(index)?, digits(count)?)))
            .ok_or_else(invalid)?;
        Ok(Self {
            directory,
            prefix,
            index,
            count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_gguf_entry_returns_the_first_shard_of_a_complete_set() {
        assert_eq!(
            split_gguf_entry([
                "UD-Q4_K_XL/Model-UD-Q4_K_XL-00003-of-00003.gguf",
                "UD-Q4_K_XL/Model-UD-Q4_K_XL-00001-of-00003.gguf",
                "UD-Q4_K_XL/Model-UD-Q4_K_XL-00002-of-00003.gguf",
            ]),
            Ok("UD-Q4_K_XL/Model-UD-Q4_K_XL-00001-of-00003.gguf".to_owned())
        );
        assert_eq!(
            split_gguf_entry(["model-00002-of-00002.gguf", "model-00001-of-00002.gguf"]),
            Ok("model-00001-of-00002.gguf".to_owned())
        );
    }

    #[test]
    fn split_gguf_entry_rejects_incomplete_or_ambiguous_sets() {
        for (paths, expected) in [
            (
                vec!["m-00001-of-00003.gguf", "m-00003-of-00003.gguf"],
                SplitGgufError::MissingShard { index: 2 },
            ),
            (
                vec![
                    "m-00001-of-00002.gguf",
                    "m-00001-of-00002.gguf",
                    "m-00002-of-00002.gguf",
                ],
                SplitGgufError::DuplicateIndex { index: 1 },
            ),
            (
                vec!["a-00001-of-00002.gguf", "b-00002-of-00002.gguf"],
                SplitGgufError::MixedPrefixes,
            ),
            (
                vec!["Q4/m-00001-of-00002.gguf", "Q5/m-00002-of-00002.gguf"],
                SplitGgufError::MixedDirectories,
            ),
            (
                vec!["m-00001-of-00002.gguf", "m-00002-of-00003.gguf"],
                SplitGgufError::InconsistentCount,
            ),
            (
                vec!["m-00001-of-00002.gguf", "m-00003-of-00002.gguf"],
                SplitGgufError::IndexOutOfRange { index: 3 },
            ),
            (
                vec!["m-00001-of-00002.gguf", "README.md"],
                SplitGgufError::InvalidShardName {
                    path: "README.md".to_owned(),
                },
            ),
            (
                vec!["m-0001-of-0002.gguf", "m-0002-of-0002.gguf"],
                SplitGgufError::InvalidShardName {
                    path: "m-0001-of-0002.gguf".to_owned(),
                },
            ),
            (
                vec!["-00001-of-00002.gguf", "-00002-of-00002.gguf"],
                SplitGgufError::InvalidShardName {
                    path: "-00001-of-00002.gguf".to_owned(),
                },
            ),
            (vec!["m-00001-of-00001.gguf"], SplitGgufError::TooFewShards),
            (vec![], SplitGgufError::TooFewShards),
        ] {
            assert_eq!(
                split_gguf_entry(paths.iter().copied()),
                Err(expected),
                "{paths:?}"
            );
        }
    }
}
