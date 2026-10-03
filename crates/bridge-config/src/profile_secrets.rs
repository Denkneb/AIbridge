//! Private profile-instruction gate, ported from the pinned secret_scanner.py.
//! Only stable categories escape this module; matches and decoded JWTs never do.

use std::sync::LazyLock;

use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use regex::Regex;

static PATTERNS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    [
        ("aws_access_key", r"(?:AKIA|ASIA)[0-9A-Z]{16}"),
        ("github_token", r"(?:ghp|gho|ghu|ghs|ghr)_[0-9A-Za-z]{36}"),
        (
            "github_token",
            r"github_pat_[0-9A-Za-z]{22}_[0-9A-Za-z]{59}",
        ),
        ("openai_api_key", r"sk-proj-[0-9A-Za-z_-]{20,}"),
        ("openai_api_key", r"sk-[0-9A-Za-z]{32,}"),
        (
            "anthropic_api_key",
            r"sk-ant-api[0-9]{2}-[0-9A-Za-z_-]{20,}",
        ),
        ("google_api_key", r"AIza[0-9A-Za-z_-]{35}"),
        ("slack_token", r"(?:xox[baprs]|xapp)-[0-9A-Za-z-]{20,}"),
        ("stripe_token", r"(?:sk|rk)_(?:live|test)_[0-9A-Za-z]{16,}"),
        (
            "jwt",
            r"eyJ[0-9A-Za-z_-]{5,}\.[0-9A-Za-z_-]{5,}\.[0-9A-Za-z_-]{5,}",
        ),
        (
            "pem_private_key",
            r"-----BEGIN (?:[A-Z][A-Z0-9 ]* )?PRIVATE KEY-----",
        ),
    ]
    .into_iter()
    .map(|(category, pattern)| {
        (
            category,
            Regex::new(pattern).expect("fixed scanner pattern"),
        )
    })
    .collect()
});

// Python's Unicode \w is alphanumeric or underscore. Rust regex's default
// \w also includes marks/join controls, so implement the Python boundaries.
fn boundary(text: &str, offset: usize) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text[..offset].chars().next_back().is_some_and(word)
        != text[offset..].chars().next().is_some_and(word)
}

fn is_jwt(candidate: &str) -> bool {
    let header = candidate.split('.').next().unwrap_or_default();
    let engine = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let Ok(decoded) = engine.decode(header) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()
        .is_some_and(|value| {
            value.is_object() && value.get("alg").and_then(|alg| alg.as_str()).is_some()
        })
}

fn matches(text: &str, category: &str, pattern: &Regex) -> bool {
    if category == "pem_private_key" {
        return pattern.is_match(text);
    }
    let mut offset = 0;
    while let Some(found) = pattern.find_at(text, offset) {
        if boundary(text, found.start()) {
            let mut end = found.end();
            loop {
                let candidate = &text[found.start()..end];
                if !pattern
                    .find(candidate)
                    .is_some_and(|m| m.start() == 0 && m.end() == candidate.len())
                {
                    break;
                }
                if boundary(text, end) && (category != "jwt" || is_jwt(candidate)) {
                    return true;
                }
                // Python's final \b may backtrack to an earlier boundary inside
                // a variable-length ASCII body (including before a '-' suffix).
                end -= 1;
            }
        }
        // Rejected boundary matches must not consume a later overlapping prefix.
        offset = found.start() + 1; // every fixed prefix begins with an ASCII byte
    }
    false
}

pub(super) fn categories(text: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for (category, pattern) in PATTERNS.iter() {
        if !found.contains(category) && matches(text, category, pattern) {
            found.push(*category);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_matches_frozen_secret_corpus_without_returning_values() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/security-cases.json"))
                .unwrap();
        let mut checked = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if case["operation"] != "secret_gate" || case["input"]["allow"] == true {
                continue;
            }
            checked += 1;
            let actual = categories(case["input"]["text"].as_str().unwrap());
            let expected: Vec<&str> = case["expect"]["categories"]
                .as_array()
                .map(|values| values.iter().map(|v| v.as_str().unwrap()).collect())
                .unwrap_or_default();
            assert_eq!(actual, expected, "case {}", case["id"]);
        }
        assert!(checked >= 9);
    }

    #[test]
    fn scanner_respects_unicode_boundaries_backtracking_and_jwt_header_shape() {
        let key = format!("sk-proj-{}", "a".repeat(20));
        assert!(categories(&format!("я{key}")).is_empty());
        assert!(categories(&format!("{key}я")).is_empty());
        assert_eq!(categories(&format!("{key}\u{301}")), ["openai_api_key"]);
        assert_eq!(categories(&format!("{key}---")), ["openai_api_key"]);
        assert_eq!(categories(&format!("{key}-aaaaя")), ["openai_api_key"]);
        assert!(categories(&format!("sk-proj-{}-", "a".repeat(19))).is_empty());
        assert_eq!(
            categories(&format!("xsk-proj-{}-{key}", "a".repeat(20))),
            ["openai_api_key"]
        );
        assert!(categories("password=example secret=placeholder").is_empty());
        let encode = |value: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        for header in [r#"{"alg":"HS256"}"#, r#"{"alg":""}"#] {
            assert_eq!(
                categories(&format!("{}.aaaaaa.bbbbbb", encode(header))),
                ["jwt"]
            );
        }
        for header in [r#"{"x":"HS256"}"#, r#"{"alg":1}"#, "{bad-json}", "[]"] {
            assert!(categories(&format!("{}.aaaaaa.bbbbbb", encode(header))).is_empty());
        }
    }
}
