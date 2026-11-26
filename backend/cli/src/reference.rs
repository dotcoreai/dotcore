//! How an operator names one object on the command line, such as a deployment, a node or an
//! inference route: by its id, its name, or a unique part of either.
//!
//! A reference is tried against the candidates in a fixed order of steps, and the first step that
//! finds anything decides: one match resolves, several fail and list them. Names and partial ids
//! ignore case, and a partial match needs a reference of at least two characters.
//!
//! 1. The exact id.
//! 2. The exact name.
//! 3. An id fragment: a prefix of the id, or one or more whole consecutive hyphen-separated parts
//!    of it, such as `27b` or `8-27b` of `qwen3-8-27b-3cfmn`.
//! 4. A prefix of the name.
//! 5. Name words: the reference's words are consecutive words of the name, the last one possibly
//!    cut short. Words are separated by any character that is not a letter or a digit, so `27b`,
//!    `qwen`, `3.8` and `8 27` all name `Qwen 3.8 27B`.

use crate::management::{DeploymentGroup, single_line};

/// The shortest reference that may match an id or a name partially.
const MIN_PARTIAL_CHARS: usize = 2;

/// An object an operator can name by its id or its names.
pub(crate) trait Referable {
    /// The stable id.
    fn id(&self) -> &str;

    /// The name shown to the operator, which need not be unique.
    fn name(&self) -> &str;

    /// Other names that also name the object, such as the name it was given when the shown name
    /// comes from elsewhere.
    fn aliases(&self) -> Vec<&str> {
        Vec::new()
    }
}

/// Finds the one candidate that `reference` names, following the steps of this module's order.
pub(crate) fn resolve<'a, T: Referable>(
    kind: ReferenceKind,
    reference: &str,
    candidates: &'a [T],
) -> Result<&'a T, ReferenceError> {
    let lowered = reference.to_lowercase();
    let partial = reference.chars().count() >= MIN_PARTIAL_CHARS;
    let steps: [&dyn Fn(&T) -> bool; 5] = [
        &|candidate| candidate.id() == reference,
        &|candidate| names(candidate).any(|name| name == lowered),
        &|candidate| partial && id_fragment_matches(&candidate.id().to_lowercase(), &lowered),
        &|candidate| partial && names(candidate).any(|name| name.starts_with(&lowered)),
        &|candidate| partial && names(candidate).any(|name| words_match(&name, &lowered)),
    ];

    for step in steps {
        let found = candidates
            .iter()
            .filter(|candidate| step(candidate))
            .collect::<Vec<_>>();
        match found.as_slice() {
            [] => {}
            [one] => return Ok(one),
            several => {
                return Err(ReferenceError::Ambiguous {
                    kind,
                    reference: reference.to_owned(),
                    matches: several
                        .iter()
                        .map(|candidate| Candidate::of(*candidate))
                        .collect(),
                });
            }
        }
    }

    Err(ReferenceError::Unknown {
        kind,
        reference: reference.to_owned(),
        known: candidates.iter().map(Candidate::of).collect(),
    })
}

/// The kind of object a reference names, which picks the words of its failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceKind {
    Deployment(DeploymentGroup),

    Node,

    Route,

    CatalogModel,

    Profile,

    ExecutionTarget,
}

impl ReferenceKind {
    /// The operator word for one object, for example `node`.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Deployment(group) => group.noun(),
            Self::Node => "node",
            Self::Route => "inference route",
            Self::CatalogModel => "catalog model",
            Self::Profile => "profile",
            Self::ExecutionTarget => "execution target",
        }
    }

    /// Whether the objects have an id apart from their name; a route is named by its name alone.
    pub(crate) fn has_ids(self) -> bool {
        !matches!(self, Self::Route)
    }

    /// Whether an unknown reference lists the known objects, for kinds without a command that
    /// lists them wherever the reference was given.
    pub(crate) fn lists_known(self) -> bool {
        !matches!(self, Self::Deployment(_))
    }
}

/// One object a failed reference lists, its id and shown name made safe to print.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) id: String,

    pub(crate) name: String,
}

impl Candidate {
    fn of(object: &impl Referable) -> Self {
        Self {
            id: single_line(object.id()),
            name: single_line(object.name()),
        }
    }

    /// The candidate on one line: its id followed by its name when that differs.
    fn label(&self) -> String {
        if self.name.is_empty() || self.name == self.id {
            self.id.clone()
        } else {
            format!("{} ({})", self.id, self.name)
        }
    }
}

/// An operator reference that names no object of its kind, or several.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReferenceError {
    #[error("no {} matches {reference:?}{}", .kind.noun(), known_suffix(*.kind, .known))]
    Unknown {
        kind: ReferenceKind,
        reference: String,
        known: Vec<Candidate>,
    },

    #[error(
        "{} {}s match {reference:?}: {}",
        .matches.len(),
        .kind.noun(),
        labels(.matches)
    )]
    Ambiguous {
        kind: ReferenceKind,
        reference: String,
        matches: Vec<Candidate>,
    },
}

/// The candidate's shown name and aliases, lowercased, without empty ones.
fn names(candidate: &impl Referable) -> impl Iterator<Item = String> {
    std::iter::once(candidate.name())
        .chain(candidate.aliases())
        .filter(|name| !name.is_empty())
        .map(str::to_lowercase)
}

/// Whether `fragment` is a prefix of `id` or one or more whole consecutive hyphen-separated
/// parts of it; both are lowercase.
fn id_fragment_matches(id: &str, fragment: &str) -> bool {
    if id.starts_with(fragment) {
        return true;
    }
    let parts = id.split('-').collect::<Vec<_>>();
    let wanted = fragment.split('-').collect::<Vec<_>>();
    parts.windows(wanted.len()).any(|window| window == wanted)
}

/// Whether the words of `reference` are consecutive words of `name`, the last one possibly cut
/// short; both are lowercase.
fn words_match(name: &str, reference: &str) -> bool {
    let reference = words(reference);
    let Some((last, leading)) = reference.split_last() else {
        return false;
    };
    words(name).windows(reference.len()).any(|window| {
        window[..leading.len()] == *leading && window[leading.len()].starts_with(last)
    })
}

/// Splits text into words at every character that is not a letter or a digit.
fn words(text: &str) -> Vec<&str> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn known_suffix(kind: ReferenceKind, known: &[Candidate]) -> String {
    if kind.lists_known() && !known.is_empty() {
        format!("; known {}s: {}", kind.noun(), labels(known))
    } else {
        String::new()
    }
}

fn labels(candidates: &[Candidate]) -> String {
    candidates
        .iter()
        .map(Candidate::label)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: ReferenceKind = ReferenceKind::Deployment(DeploymentGroup::Model);

    struct Named {
        id: &'static str,
        name: &'static str,
        alias: &'static str,
    }

    impl Referable for Named {
        fn id(&self) -> &str {
            self.id
        }

        fn name(&self) -> &str {
            self.name
        }

        fn aliases(&self) -> Vec<&str> {
            vec![self.alias]
        }
    }

    fn named(id: &'static str, name: &'static str) -> Named {
        Named {
            id,
            name,
            alias: "",
        }
    }

    fn site() -> Vec<Named> {
        vec![
            named("qwen3-8-27b-3cfmn", "Qwen 3.8 27B"),
            Named {
                alias: "Chat",
                ..named("gemma-4-e4b-7tq2m", "Gemma 4 E4B")
            },
            named("open-webui-m2rpw", "Open WebUI"),
            named("tiny-k3f9x", "x"),
        ]
    }

    /// The id the reference resolves to, or the failure it reports.
    fn resolved(candidates: &[Named], reference: &str) -> Result<&'static str, String> {
        resolve(MODELS, reference, candidates)
            .map(|candidate| candidate.id)
            .map_err(|error| error.to_string())
    }

    fn unknown(reference: &str) -> Result<&'static str, String> {
        Err(format!("no model deployment matches {reference:?}"))
    }

    #[test]
    fn an_exact_id_or_name_resolves_ignoring_case_for_names() {
        let site = site();

        assert_eq!(
            resolved(&site, "qwen3-8-27b-3cfmn"),
            Ok("qwen3-8-27b-3cfmn")
        );
        assert_eq!(resolved(&site, "Qwen 3.8 27B"), Ok("qwen3-8-27b-3cfmn"));
        assert_eq!(resolved(&site, "qwen 3.8 27b"), Ok("qwen3-8-27b-3cfmn"));
        assert_eq!(resolved(&site, "chat"), Ok("gemma-4-e4b-7tq2m"), "an alias");
        assert_eq!(
            resolved(&site, "X"),
            Ok("tiny-k3f9x"),
            "exact names may be short"
        );
    }

    #[test]
    fn an_id_fragment_is_an_id_prefix_or_whole_consecutive_id_parts() {
        let site = site();

        for fragment in ["3cfmn", "27b", "8-27b", "qwen3", "qwen3-8-2", "3CFMN"] {
            assert_eq!(
                resolved(&site, fragment),
                Ok("qwen3-8-27b-3cfmn"),
                "{fragment}"
            );
        }
        assert_eq!(resolved(&site, "webui-m2rpw"), Ok("open-webui-m2rpw"));
        assert_eq!(resolved(&site, "OPEN-WEB"), Ok("open-webui-m2rpw"));
        assert_eq!(resolved(&site, "cfmn"), unknown("cfmn"), "a partial suffix");
        assert_eq!(resolved(&site, "m2rp"), unknown("m2rp"), "a partial part");
    }

    #[test]
    fn a_name_prefix_or_name_words_resolve() {
        let site = site();

        assert_eq!(
            resolved(&site, "Qwen 3"),
            Ok("qwen3-8-27b-3cfmn"),
            "name prefix"
        );
        assert_eq!(
            resolved(&site, "open w"),
            Ok("open-webui-m2rpw"),
            "name prefix"
        );
        assert_eq!(resolved(&site, "3.8"), Ok("qwen3-8-27b-3cfmn"), "two words");
        assert_eq!(
            resolved(&site, "8 27"),
            Ok("qwen3-8-27b-3cfmn"),
            "word prefix"
        );
        assert_eq!(
            resolved(&site, "E4"),
            Ok("gemma-4-e4b-7tq2m"),
            "word prefix"
        );
        assert_eq!(
            resolved(&site, "web"),
            Ok("open-webui-m2rpw"),
            "word prefix"
        );
        assert_eq!(resolved(&site, "ui"), unknown("ui"), "a word suffix");
        assert_eq!(
            resolved(&site, "4 27b"),
            unknown("4 27b"),
            "words of two names"
        );
    }

    #[test]
    fn a_partial_match_needs_two_characters() {
        let site = site();

        // `8` is a whole id part and a name word of the Qwen deployment, `q` a prefix of both.
        assert_eq!(resolved(&site, "8"), unknown("8"));
        assert_eq!(resolved(&site, "q"), unknown("q"));
        assert_eq!(resolved(&site, ""), unknown(""));
    }

    #[test]
    fn an_earlier_step_wins_over_a_later_one() {
        let id_over_name = [
            named("gemma-aaaaa", "Gemma"),
            named("other-bbbbb", "gemma-aaaaa"),
        ];
        let name_over_fragment = [named("chat-aaaaa", "Coder"), named("qwen-bbbbb", "Chat")];
        let fragment_over_name = [
            named("coder-aaaaa", "Qwen"),
            named("gemma-bbbbb", "Coder Large"),
        ];
        let prefix_over_word = [
            named("a-aaaaa", "Coder Large"),
            named("b-bbbbb", "Big Coder"),
        ];

        assert_eq!(resolved(&id_over_name, "gemma-aaaaa"), Ok("gemma-aaaaa"));
        assert_eq!(resolved(&name_over_fragment, "chat"), Ok("qwen-bbbbb"));
        assert_eq!(resolved(&fragment_over_name, "coder"), Ok("coder-aaaaa"));
        assert_eq!(resolved(&prefix_over_word, "coder"), Ok("a-aaaaa"));
    }

    #[test]
    fn a_slug_id_part_that_is_also_a_name_word_resolves_at_the_id_step() {
        let same = [
            named("qwen3-8-27b-3cfmn", "Qwen 3.8 27B"),
            named("gemma-4-e4b-7tq2m", "Gemma 4 E4B"),
        ];
        let other = [
            named("qwen3-8-27b-3cfmn", "Coder"),
            named("gemma-4-e4b-7tq2m", "Gemma 27B"),
        ];

        assert_eq!(resolved(&same, "27b"), Ok("qwen3-8-27b-3cfmn"));
        assert_eq!(
            resolved(&other, "27b"),
            Ok("qwen3-8-27b-3cfmn"),
            "the id part wins over another deployment's name word"
        );
    }

    #[test]
    fn several_matches_at_a_step_fail_there_listing_them() {
        let ids = [named("x-aaaaa", "One"), named("x-aaaaa", "Two")];
        let names = [named("gemma-aaaaa", "Gemma"), named("gemma-bbbbb", "gemma")];
        let fragments = [
            named("qwen3-8-27b-aaaaa", "Large"),
            named("qwen3-8-8b-bbbbb", "Small"),
            named("coder-ccccc", "Qwen3 coder"),
        ];
        let prefixes = [
            named("a-aaaaa", "Open WebUI"),
            named("b-bbbbb", "Open Notebook"),
            named("c-ccccc", "Big open"),
        ];
        let words = [
            named("a-aaaaa", "Qwen Coder"),
            named("b-bbbbb", "Gemma Coder"),
        ];

        assert_eq!(
            resolved(&ids, "x-aaaaa"),
            Err("2 model deployments match \"x-aaaaa\": x-aaaaa (One), x-aaaaa (Two)".to_owned())
        );
        assert_eq!(
            resolved(&names, "GEMMA"),
            Err(
                "2 model deployments match \"GEMMA\": gemma-aaaaa (Gemma), gemma-bbbbb (gemma)"
                    .to_owned()
            )
        );
        assert_eq!(
            resolved(&fragments, "qwen3-8"),
            Err(
                "2 model deployments match \"qwen3-8\": qwen3-8-27b-aaaaa (Large), \
                 qwen3-8-8b-bbbbb (Small)"
                    .to_owned()
            )
        );
        assert_eq!(
            resolved(&prefixes, "open"),
            Err(
                "2 model deployments match \"open\": a-aaaaa (Open WebUI), b-bbbbb (Open \
                 Notebook)"
                    .to_owned()
            )
        );
        assert_eq!(
            resolved(&words, "cod"),
            Err(
                "2 model deployments match \"cod\": a-aaaaa (Qwen Coder), b-bbbbb (Gemma Coder)"
                    .to_owned()
            )
        );
    }

    #[test]
    fn an_unknown_reference_lists_the_known_objects_for_kinds_without_a_list_command() {
        let nodes = [named("node-00aa", "gpu1"), named("node-00bb", "")];

        let error = resolve(ReferenceKind::Node, "gpu9", &nodes)
            .err()
            .expect("no node matches");

        assert_eq!(
            error.to_string(),
            "no node matches \"gpu9\"; known nodes: node-00aa (gpu1), node-00bb"
        );
        assert_eq!(resolved(&nodes, "gpu9"), unknown("gpu9"));
    }

    #[test]
    fn printed_candidates_are_single_lines() {
        let deployments = [
            named("a-aaaaa", "Evil\n✔ forged"),
            named("b-bbbbb", "Evil\ttwo"),
        ];

        assert_eq!(
            resolved(&deployments, "evil"),
            Err(
                "2 model deployments match \"evil\": a-aaaaa (Evil ✔ forged), b-bbbbb (Evil two)"
                    .to_owned()
            )
        );
    }
}
