//! Recognizes names that mark a value as secret, such as an environment variable or a launch flag.
//!
//! Two predicates serve two needs, both case-insensitive:
//!
//! - `is_sensitive_name` splits a name into words on `-` and `_` and matches a sensitive word only
//!   as a whole word: an API token variable matches, a tokenizer path does not. Refusing literal
//!   values in the master catalog uses it, since a false positive blocks a catalog entry.
//! - `may_carry_secret` matches a sensitive word anywhere in the name, so run-together names such
//!   as `HFTOKEN` or `DBPASSWORD` match too. Redacting recorded output uses it, since a false
//!   positive only hides a harmless value.

/// Words, or runs of words joined by `_`, whose presence marks a name as carrying a secret.
// `TOKENS` is left out: engine flags such as a batch's maximum tokens count tokens and carry no
// secret.
const SENSITIVE_NAMES: [&str; 9] = [
    "TOKEN",
    "SECRET",
    "SECRETS",
    "PASSWORD",
    "PASSWORDS",
    "API_KEY",
    "API_KEYS",
    "CREDENTIAL",
    "CREDENTIALS",
];

/// Returns whether `name` contains a sensitive name as whole words, for refusing literal values.
pub fn is_sensitive_name(name: &str) -> bool {
    let words = name
        .split(['-', '_'])
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    SENSITIVE_NAMES.iter().any(|sensitive| {
        let sensitive_words = sensitive.split('_').collect::<Vec<_>>();
        words.windows(sensitive_words.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(sensitive_words.iter().copied())
        })
    })
}

/// Returns whether `name` contains a sensitive word anywhere, ignoring `-` and `_`, for redacting
/// recorded output.
pub fn may_carry_secret(name: &str) -> bool {
    let squeezed = name
        .chars()
        .filter(|character| !matches!(character, '-' | '_'))
        .collect::<String>()
        .to_ascii_uppercase();
    SENSITIVE_NAMES
        .iter()
        .any(|sensitive| squeezed.contains(&sensitive.replace('_', "")))
}

#[cfg(test)]
mod tests {
    use super::{is_sensitive_name, may_carry_secret};

    #[test]
    fn sensitive_names_match_whole_words_in_any_case() {
        for name in [
            "hf_token",
            "HF_TOKEN",
            "MY_API_KEY_FILE",
            "api-key",
            "N8N_BASIC_AUTH_PASSWORD",
            "CLIENT_SECRET",
            "aws-credential",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "OPENAI_API_KEYS",
            "DB_PASSWORDS",
            "app-secrets",
        ] {
            assert!(is_sensitive_name(name), "{name}");
        }
        for name in [
            "TOKENIZER_PATH",
            "max-num-batched-tokens",
            "CUDA_VISIBLE_DEVICES",
            "API_URL",
            "KEY",
            "PASSWORDLESS",
            "KEYS",
            "",
        ] {
            assert!(!is_sensitive_name(name), "{name}");
        }
    }

    #[test]
    fn secret_carrying_names_match_anywhere_in_the_name() {
        for name in [
            "HFTOKEN",
            "APIKEY",
            "ACCESSTOKEN",
            "DBPASSWORD",
            "hf_token",
            "MY_API_KEY_FILE",
            "api-key",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(may_carry_secret(name), "{name}");
        }
        for name in ["THREADS", "CUDA_VISIBLE_DEVICES", "API_URL", "KEY", ""] {
            assert!(!may_carry_secret(name), "{name}");
        }
    }
}
