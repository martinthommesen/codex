use regex::Regex;
use std::sync::LazyLock;

static ANTHROPIC_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"sk-ant-[A-Za-z0-9_-]{20,}"));
static OPENAI_KEY_REGEX: LazyLock<Regex> = LazyLock::new(|| compile_regex(r"sk-[A-Za-z0-9]{20,}"));
static AWS_ACCESS_KEY_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"\bAKIA[0-9A-Z]{16}\b"));
static BEARER_TOKEN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile_regex(r"(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]{16,}=*"));
static SECRET_ASSIGNMENT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    compile_regex(r#"(?i)\b(api[_-]?key|token|secret|password)\b(\s*[:=]\s*)(["']?)[^\s"']{8,}"#)
});

/// Remove secret and keys from a String. This is done on best effort basis following some
/// well-known REGEX.
pub fn redact_secrets(input: String) -> String {
    let redacted = BEARER_TOKEN_REGEX.replace_all(&input, "Bearer [REDACTED_SECRET]");
    let redacted = ANTHROPIC_KEY_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = OPENAI_KEY_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = AWS_ACCESS_KEY_ID_REGEX.replace_all(&redacted, "[REDACTED_SECRET]");
    let redacted = SECRET_ASSIGNMENT_REGEX.replace_all(&redacted, "$1$2$3[REDACTED_SECRET]");

    redacted.to_string()
}

fn compile_regex(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(err) => panic!("invalid regex pattern `{pattern}`: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn redacts_supported_bearer_tokens() {
        let cases = [
            (
                "Bearer abcde+fghijklmnopqrstuvwxyz012345",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer abcdefghijklmnop+secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer sk-abcdefghijklmnopqrst+secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer AKIAABCDEFGHIJKLMNOP/~secret_suffix",
                "Bearer [REDACTED_SECRET]",
            ),
            (
                "Bearer AbcdefghijklMN09._~+/-==; echo done",
                "Bearer [REDACTED_SECRET]; echo done",
            ),
            (
                "authorization: bEaReR\tabcdefghijklmnop",
                "authorization: Bearer [REDACTED_SECRET]",
            ),
            ("Bearer   abcdefghijklmnop", "Bearer [REDACTED_SECRET]"),
        ];

        for (input, expected) in cases {
            assert_eq!(redact_secrets(input.to_string()), expected);
        }
    }

    #[test]
    fn redacts_anthropic_api_keys() {
        let cases = [
            (
                "key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz_0123456789-AA done",
                "key [REDACTED_SECRET] done",
            ),
            (
                "x-api-key: sk-ant-admin01-abcdefghijklmnopqrstuvwx",
                "x-api-key: [REDACTED_SECRET]",
            ),
            (
                r#"{"api_key":"sk-ant-api03-abcdefghijklmnopqrst-_xyz"}"#,
                r#"{"api_key":"[REDACTED_SECRET]"}"#,
            ),
            (
                "ANTHROPIC_API_KEY=sk-ant-oat01-abcdefghijklmnopqrstu\nnext",
                "ANTHROPIC_API_KEY=[REDACTED_SECRET]\nnext",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(redact_secrets(input.to_string()), expected);
        }
    }

    #[test]
    fn keeps_short_anthropic_like_strings() {
        let input = "sk-ant-short and sk-ant- prefix";
        assert_eq!(redact_secrets(input.to_string()), input);
    }

    #[test]
    fn avoids_bearer_false_positives() {
        let cases = [
            "Bearer of good news",
            "Bearer abcdefghijklmno",
            "NotABearer abcdefghijklmnop",
            "Bearerabcdefghijklmnop",
            "Bearer\nabcdefghijklmnop",
            "Bearer\u{a0}abcdefghijklmnop",
            "Bearer abcdefghijklmno\u{212a}",
        ];

        for input in cases {
            assert_eq!(redact_secrets(input.to_string()), input);
        }
    }
}
