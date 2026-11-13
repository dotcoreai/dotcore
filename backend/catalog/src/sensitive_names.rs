//! Recognizes names that mark a value as secret, such as an environment variable or a launch flag.
//!
//! A name is split into words on `-` and `_` and compared without regard to case, so a sensitive
//! word matches only as a whole word: an API token variable matches, a tokenizer path does not.
//! Callers use it to mask values they record and to refuse literal values where a secret belongs.

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

/// Returns whether `name` contains a sensitive name as whole words.
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

#[cfg(test)]
mod tests {
    use super::is_sensitive_name;

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
}
