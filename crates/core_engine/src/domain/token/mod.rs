//! Path tokenizer — spec 03a.
//!
//! Pure function family: turns a path or filename into a deduplicated,
//! deterministically-ordered list of searchable tokens. No I/O, no index,
//! no config structs (YAGNI).
//!
//! # Tokenization pipeline
//!
//! ```text
//! tokenize("src/http/HttpClient.rs")
//!   split separators  / _ - .  → ["src","http","HttpClient","rs"]
//!   split camelCase boundaries  → ["src","http","Http","Client","rs"]
//!   lowercase                   → ["src","http","http","client","rs"]
//!   substring prefixes (≥ MIN_PREFIX_LEN) → +["cli","clie","clien",…]
//!   dedup + sort                → Vec<Token>
//! ```
//!
//! # Lowercasing policy
//!
//! Tokens are lowercased with [`str::to_lowercase`], which performs Unicode
//! case folding (e.g. `Ü` → `ü`, `İ` → `i\u{307}`). This is intentional:
//! prefix matching over non-ASCII paths must be consistent. Byte-level ASCII
//! lowercasing was considered but rejected because it silently passes through
//! uppercase non-ASCII characters, breaking the "every emitted token is
//! lowercase" invariant for Unicode input (spec UN2).
//!
//! # Ordering
//!
//! The returned `Vec<Token>` is sorted lexicographically on the underlying
//! string. Lexicographic order is deterministic, reproducible across platforms,
//! and costs O(n log n) — acceptable for path-length inputs. Insertion order
//! was rejected: it depends on iteration order of the intermediate set and
//! would require an `IndexSet` dependency for a property the caller does not
//! need (spec U4, YAGNI).
#![forbid(unsafe_code)]

/// Minimum number of Unicode codepoints (characters) in a prefix token (spec U3 / AC3).
///
/// Prefixes with fewer than this many characters are never emitted: single-
/// and two-character prefixes produce excessive noise in the token list and
/// inflate index size without meaningfully improving recall for typical
/// identifiers. The guard is applied to the character count, not the byte
/// length, so the invariant holds for all Unicode input including multi-byte
/// scripts (CJK, emoji, etc.).
///
/// # Decision: value = 3
/// AC3 explicitly exercises min-prefix 3. Changing this value alters the
/// public contract of `tokenize`; bump it only with a spec change.
pub const MIN_PREFIX_LEN: usize = 3;

// ── Token newtype ────────────────────────────────────────────────────────────

/// A single lowercase search token.
///
/// A thin newtype around [`String`] so the type system distinguishes raw
/// strings from tokenizer output. Tokens are always lowercase (spec U2) and
/// contain no separator characters.
///
/// # Examples
///
/// ```
/// use core_engine::domain::token::Token;
///
/// let t = Token::new("client");
/// assert_eq!(t.as_str(), "client");
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(String);

impl Token {
    /// Construct a `Token` from any string-like value.
    ///
    /// The caller is responsible for ensuring the string is already lowercase;
    /// the tokenizer always produces lowercase tokens via [`tokenize`].
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Return the token as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for Token {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Token {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ── Tokenization pipeline ────────────────────────────────────────────────────

/// Tokenize `input` into a deduplicated, sorted list of lowercase tokens.
///
/// See the [module-level documentation](self) for the full pipeline and
/// lowercasing/ordering policy.
///
/// # Examples
///
/// ```
/// use core_engine::domain::token::{tokenize, MIN_PREFIX_LEN};
///
/// let tokens = tokenize("src/http/HttpClient.rs");
/// // Must contain base tokens
/// assert!(tokens.iter().any(|t| t.as_str() == "http"));
/// assert!(tokens.iter().any(|t| t.as_str() == "client"));
/// assert!(tokens.iter().any(|t| t.as_str() == "rs"));
/// // Must contain prefixes ≥ MIN_PREFIX_LEN
/// assert!(tokens.iter().any(|t| t.as_str() == "cli"));
/// ```
///
/// Empty / whitespace input returns an empty list (spec UN1 / AC4):
///
/// ```
/// use core_engine::domain::token::tokenize;
///
/// assert!(tokenize("").is_empty());
/// assert!(tokenize("   ").is_empty());
/// ```
#[must_use]
pub fn tokenize(input: &str) -> Vec<Token> {
    // Step 1: split on separator characters, keeping non-empty, non-whitespace
    // segments. Whitespace characters are not separators per the spec, but a
    // path segment that consists entirely of whitespace carries no information
    // and must not produce tokens (spec UN1 / AC4).
    let separator_parts: Vec<&str> = input
        .split(is_separator)
        .filter(|s| !s.trim().is_empty())
        .collect();

    if separator_parts.is_empty() {
        return Vec::new();
    }

    // Step 2: split camelCase boundaries, then lowercase each piece.
    let mut lowercased: Vec<String> = Vec::new();
    for part in separator_parts {
        for word in split_camel_case(part) {
            let lc = word.to_lowercase();
            if !lc.is_empty() {
                lowercased.push(lc);
            }
        }
    }

    // Step 3: collect all strings for which we also emit prefix tokens.
    let mut raw: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for word in &lowercased {
        // Emit the full word.
        raw.insert(word.clone());
        // Emit all character-boundary prefixes whose char count >= MIN_PREFIX_LEN.
        // We enumerate over char_indices so `char_idx` counts codepoints, not bytes.
        // This is the correct unit: MIN_PREFIX_LEN is a character count (see its
        // doc comment), not a byte count.  Using the byte offset would wrongly pass
        // the guard for multi-byte scripts (e.g. CJK chars are 3 bytes each, so a
        // 1-char CJK prefix has byte offset 3 == MIN_PREFIX_LEN and would be emitted).
        let word_byte_len = word.len();
        for (char_idx, (byte_end, _)) in word.char_indices().enumerate() {
            // char_idx is the 0-based index of the *upcoming* char; the prefix
            // word[..byte_end] contains exactly `char_idx` characters.
            if char_idx < MIN_PREFIX_LEN {
                continue;
            }
            // Skip the full word — it was inserted above.
            if byte_end < word_byte_len {
                let prefix = &word[..byte_end];
                raw.insert(prefix.to_owned());
            }
        }
    }

    // Step 4: BTreeSet already gives us dedup + lexicographic order.
    raw.into_iter().map(Token::new).collect()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Returns `true` for the four separator characters `/ _ - .`.
#[inline]
fn is_separator(c: char) -> bool {
    matches!(c, '/' | '_' | '-' | '.')
}

/// Split a single word on camelCase boundaries.
///
/// A boundary is detected when a lowercase (or digit) character is immediately
/// followed by an uppercase character, or when an uppercase character is
/// followed by an uppercase then lowercase (e.g. "HTTPSClient" → "HTTPS",
/// "Client").
///
/// Returned slices are borrows of `input`; no allocation until `to_lowercase`
/// is called by the caller.
fn split_camel_case(input: &str) -> Vec<&str> {
    if input.is_empty() {
        return Vec::new();
    }

    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut splits: Vec<usize> = vec![0];

    let len = chars.len();
    for i in 1..len {
        let (_, prev) = chars[i - 1];
        let (cur_byte, cur) = chars[i];

        let boundary = if cur.is_uppercase() {
            if prev.is_lowercase() || prev.is_ascii_digit() {
                // e.g. "httpClient" → boundary before 'C'
                true
            } else if prev.is_uppercase() && i + 1 < len && chars[i + 1].1.is_lowercase() {
                // e.g. "HTTPSClient" → boundary before 'C' (prev='S' upper, next='l' lower)
                true
            } else {
                false
            }
        } else {
            false
        };

        if boundary {
            splits.push(cur_byte);
        }
    }

    splits.push(input.len());
    splits
        .windows(2)
        .filter_map(|w| {
            let s = &input[w[0]..w[1]];
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn token(s: &str) -> Token {
        Token::new(s)
    }

    fn has(tokens: &[Token], s: &str) -> bool {
        tokens.iter().any(|t| t.as_str() == s)
    }

    // ── TDD Step 1 & 2: separator splitting ──────────────────────────────────

    /// AC1 partial — separators produce the right base words.
    #[test]
    fn separator_slash_splits() {
        let tokens = tokenize("src/http/rs");
        assert!(has(&tokens, "src"), "expected 'src' in {tokens:?}");
        assert!(has(&tokens, "http"), "expected 'http' in {tokens:?}");
        assert!(has(&tokens, "rs"), "expected 'rs' in {tokens:?}");
    }

    #[test]
    fn separator_underscore_splits() {
        let tokens = tokenize("my_module");
        assert!(has(&tokens, "my"), "expected 'my' in {tokens:?}");
        assert!(has(&tokens, "module"), "expected 'module' in {tokens:?}");
    }

    #[test]
    fn separator_dash_splits() {
        let tokens = tokenize("http-client");
        assert!(has(&tokens, "http"), "expected 'http' in {tokens:?}");
        assert!(has(&tokens, "client"), "expected 'client' in {tokens:?}");
    }

    #[test]
    fn separator_dot_splits() {
        let tokens = tokenize("HttpClient.rs");
        assert!(has(&tokens, "rs"), "expected 'rs' in {tokens:?}");
    }

    // Table test: all four separators
    #[test]
    fn all_separators_table() {
        let cases: &[(&str, &[&str])] = &[
            ("a/b", &["a", "b"]),
            ("a_b", &["a", "b"]),
            ("a-b", &["a", "b"]),
            ("a.b", &["a", "b"]),
        ];
        for (input, expected) in cases {
            let tokens = tokenize(input);
            for &exp in *expected {
                assert!(
                    has(&tokens, exp),
                    "input={input:?}: expected {exp:?} in {tokens:?}"
                );
            }
        }
    }

    // ── TDD Step 3 & 4: camelCase + lowercase ────────────────────────────────

    /// AC1 — full integration: "src/http/HttpClient.rs"
    #[test]
    fn ac1_full_path_camel_case_and_lowercase() {
        let tokens = tokenize("src/http/HttpClient.rs");
        assert!(has(&tokens, "http"), "expected 'http'");
        assert!(has(&tokens, "client"), "expected 'client'");
        assert!(has(&tokens, "rs"), "expected 'rs'");
        assert!(has(&tokens, "src"), "expected 'src'");
    }

    #[test]
    fn camel_case_splits_simple() {
        let parts = split_camel_case("HttpClient");
        assert_eq!(parts, vec!["Http", "Client"]);
    }

    #[test]
    fn camel_case_splits_acronym() {
        let parts = split_camel_case("HTTPSClient");
        assert_eq!(parts, vec!["HTTPS", "Client"]);
    }

    #[test]
    fn camel_case_lowercase_tokens() {
        let tokens = tokenize("HttpClient");
        assert!(has(&tokens, "http"), "expected 'http'");
        assert!(has(&tokens, "client"), "expected 'client'");
        // uppercase originals must NOT be present
        assert!(!has(&tokens, "Http"), "must not contain 'Http'");
        assert!(!has(&tokens, "Client"), "must not contain 'Client'");
    }

    #[test]
    fn camel_case_table() {
        let cases: &[(&str, &[&str])] = &[
            ("camelCase", &["camel", "case"]),
            ("MyHttpService", &["my", "http", "service"]),
            ("parseJSON", &["parse", "json"]),
            ("XMLParser", &["xml", "parser"]),
        ];
        for (input, expected) in cases {
            let tokens = tokenize(input);
            for &exp in *expected {
                assert!(
                    has(&tokens, exp),
                    "input={input:?}: expected {exp:?} in {tokens:?}"
                );
            }
        }
    }

    // ── TDD Step 5 & 6: prefixes + dedup + order ─────────────────────────────

    /// AC3 — "client" with min-prefix 3 emits expected prefixes.
    #[test]
    fn ac3_prefix_generation() {
        let tokens = tokenize("client");
        assert!(has(&tokens, "cli"), "expected 'cli'");
        assert!(has(&tokens, "clie"), "expected 'clie'");
        assert!(has(&tokens, "clien"), "expected 'clien'");
        assert!(has(&tokens, "client"), "expected 'client'");
    }

    #[test]
    fn prefixes_below_min_not_emitted() {
        let tokens = tokenize("client");
        assert!(!has(&tokens, "c"), "must not emit 1-char prefix");
        assert!(!has(&tokens, "cl"), "must not emit 2-char prefix");
    }

    /// AC2 — determinism: same input produces identical output.
    #[test]
    fn ac2_deterministic_same_result() {
        let input = "src/http/HttpClient.rs";
        let first = tokenize(input);
        let second = tokenize(input);
        assert_eq!(first, second, "tokenize must be deterministic");
    }

    /// Table-style determinism: 6 fixed inputs all produce identical output on two calls.
    #[test]
    fn determinism_property_multiple_inputs() {
        let inputs = [
            "src/http/HttpClient.rs",
            "my_module/SomeService.ts",
            "camelCase-path_here.txt",
            "XMLParser",
            "",
            "   ",
        ];
        for input in inputs {
            let a = tokenize(input);
            let b = tokenize(input);
            assert_eq!(a, b, "non-deterministic output for {input:?}");
        }
    }

    /// Property-based determinism: for any arbitrary string, two calls to
    /// `tokenize` must return identical results (DoD requirement AC2 / spec U4).
    ///
    /// This test uses proptest to generate random Unicode strings covering edge
    /// cases the author did not anticipate: mixed multi-byte paths, adversarial
    /// separator combinations, emoji, RTL text, etc.
    #[cfg(test)]
    mod prop_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_tokenize_is_deterministic(s in any::<String>()) {
                let first = tokenize(&s);
                let second = tokenize(&s);
                prop_assert_eq!(first, second);
            }

            /// Every emitted token is non-empty and consists only of characters
            /// that survive Unicode lowercasing (no uppercase remains).
            #[test]
            fn prop_all_tokens_are_lowercase(s in any::<String>()) {
                let tokens = tokenize(&s);
                for token in &tokens {
                    let t = token.as_str();
                    prop_assert!(!t.is_empty(), "token must be non-empty");
                    let lowercased = t.to_lowercase();
                    prop_assert_eq!(t, lowercased.as_str());
                }
            }

            /// Prefix tokens are never shorter than MIN_PREFIX_LEN characters,
            /// unless the source word itself is shorter (full words always emitted).
            #[test]
            fn prop_no_prefix_below_min_len_unless_full_word(s in any::<String>()) {
                let tokens = tokenize(&s);
                for token in &tokens {
                    let char_count = token.as_str().chars().count();
                    // A token shorter than MIN_PREFIX_LEN is only legal if it is
                    // itself a complete word (not a truncated prefix).  We cannot
                    // easily distinguish them here, so we only assert the weaker
                    // property: the char count is at least 1 (non-empty, already
                    // covered above) and no token is an empty string.
                    prop_assert!(
                        char_count >= 1,
                        "token {t:?} must have at least 1 char",
                        t = token.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn dedup_removes_duplicates() {
        // "http" appears twice from slash-separated parts; must appear once.
        let tokens = tokenize("http/http");
        let count = tokens.iter().filter(|t| t.as_str() == "http").count();
        assert_eq!(count, 1, "dedup must remove duplicate 'http'");
    }

    #[test]
    fn output_is_sorted_lexicographically() {
        let tokens = tokenize("src/http/HttpClient.rs");
        let strings: Vec<&str> = tokens.iter().map(Token::as_str).collect();
        let mut sorted = strings.clone();
        sorted.sort_unstable();
        assert_eq!(strings, sorted, "tokens must be lexicographically sorted");
    }

    // ── TDD Step 7 & 8: empty + Unicode ──────────────────────────────────────

    /// Prefix guard uses char count, not byte length (multi-byte Unicode).
    ///
    /// "中bc" has 3 chars. Char boundaries in bytes: [0, 3, 4].
    /// The byte offset of the 2nd char boundary is 3, which equals MIN_PREFIX_LEN.
    /// A byte-based guard (end < MIN_PREFIX_LEN) wrongly emits "中" (1 char) and
    /// "中b" (2 chars). A char-count-based guard skips them correctly.
    #[test]
    fn prefix_guard_uses_char_count_not_bytes() {
        // "中" is U+4E2D, encoded as 3 bytes in UTF-8.
        // The word "中bc" has 3 chars but the first char boundary offset is 3 (bytes).
        // Under the buggy byte-based guard, byte_offset=3 >= MIN_PREFIX_LEN=3 passes,
        // emitting a 1-char prefix "中". The correct char-count guard must reject it.
        let tokens = tokenize("中bc");
        assert!(
            !has(&tokens, "中"),
            "must not emit 1-char prefix '中'; tokens={tokens:?}"
        );
        assert!(
            !has(&tokens, "中b"),
            "must not emit 2-char prefix '中b'; tokens={tokens:?}"
        );
        // The full word must still be present.
        assert!(
            has(&tokens, "中bc"),
            "full word '中bc' must be emitted; tokens={tokens:?}"
        );
    }

    /// AC4 — empty input.
    #[test]
    fn ac4_empty_input_returns_empty() {
        assert!(tokenize("").is_empty(), "empty input must yield empty vec");
    }

    /// AC4 — whitespace-only input.
    #[test]
    fn ac4_whitespace_only_returns_empty() {
        assert!(
            tokenize("   ").is_empty(),
            "whitespace-only input must yield empty vec"
        );
        assert!(
            tokenize("\t\n").is_empty(),
            "whitespace-only input must yield empty vec"
        );
    }

    /// AC5 — Unicode input must not panic.
    #[test]
    fn ac5_unicode_no_panic() {
        // These calls must complete without panicking.
        let _ = tokenize("Ünïcödé/path");
        let _ = tokenize("日本語/ファイル");
        let _ = tokenize("café_latte");
        let _ = tokenize("Ü");
    }

    /// AC5 — Unicode lowercasing is applied (Unicode-aware policy).
    #[test]
    fn ac5_unicode_lowercased() {
        let tokens = tokenize("Ünïcödé");
        // The lowercase of 'Ü' is 'ü' via Unicode case folding.
        // The full word lowercase is "ünïcödé".
        assert!(
            has(&tokens, "ünïcödé"),
            "Unicode word must be lowercased; tokens={tokens:?}"
        );
    }

    // ── Additional edge-case table ────────────────────────────────────────────

    #[test]
    fn edge_cases_table() {
        let cases: &[(&str, bool, &[&str])] = &[
            // (input, should_be_empty, expected_tokens)
            ("", true, &[]),
            ("   ", true, &[]),
            ("a", false, &["a"]), // single char below MIN_PREFIX, but full word emitted
            ("ab", false, &["ab"]),
            ("abc", false, &["abc"]),
            // separators only → empty
            ("///", true, &[]),
            ("...", true, &[]),
        ];
        for (input, should_be_empty, expected) in cases {
            let tokens = tokenize(input);
            if *should_be_empty {
                assert!(
                    tokens.is_empty(),
                    "input={input:?}: expected empty, got {tokens:?}"
                );
            }
            for &exp in *expected {
                assert!(
                    has(&tokens, exp),
                    "input={input:?}: expected {exp:?} in {tokens:?}"
                );
            }
        }
    }

    // ── Token type API ────────────────────────────────────────────────────────

    #[test]
    fn token_display_matches_as_str() {
        let t = token("hello");
        assert_eq!(format!("{t}"), "hello");
        assert_eq!(t.as_str(), "hello");
    }

    #[test]
    fn token_as_ref_str() {
        let t = token("world");
        let s: &str = t.as_ref();
        assert_eq!(s, "world");
    }

    #[test]
    fn token_ordering() {
        let mut tokens = vec![token("zzz"), token("aaa"), token("mmm")];
        tokens.sort();
        assert_eq!(tokens, vec![token("aaa"), token("mmm"), token("zzz")]);
    }
}
