//! Text redaction for crash records and the diagnostics bundle (§7.1).
//!
//! - [`redact_user_paths`]: user profile directories become `%USERNAME%`.
//! - [`sanitize_quoted`]: quoted spans in panic messages (std quotes the
//!   offending string, e.g. a phrase body sliced at a bad index) are replaced
//!   by their length.
//! - [`truncate_utf8`]: byte budget on a char boundary.
//! - [`fingerprint`]: an irreversible short hash that groups equal texts
//!   without recording them.
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// What replaces the user name in a path.
pub const USER_PLACEHOLDER: &str = "%USERNAME%";

/// The current user's home directories (`USERPROFILE`, `HOME`), for
/// [`redact_user_paths`].
pub fn user_homes() -> Vec<String> {
    ["USERPROFILE", "HOME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|home| !home.is_empty())
        .collect()
}

/// The longest prefix of `text` that fits in `max_bytes` and ends on a char
/// boundary.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 8 hex digits of SHA-256 over a random per-process salt and `text`, so
/// equal texts can be grouped without being recorded. The salt is never
/// written down, so a fingerprint cannot be checked against guessed
/// phrases; equal texts match within one run only.
pub fn fingerprint(text: &str) -> String {
    static SALT: OnceLock<[u8; 16]> = OnceLock::new();
    let salt = SALT.get_or_init(|| uuid::Uuid::new_v4().into_bytes());
    let digest = Sha256::new()
        .chain_update(salt)
        .chain_update(text.as_bytes())
        .finalize();
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes that can appear inside a path segment name (no separators).
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'%' | b'~' | b'$') || b >= 0x80
}

fn is_separator(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

/// Bytes that end a user name in the generic `\Users\<name>` scan.
fn ends_name(b: u8) -> bool {
    matches!(
        b,
        b'\\'
            | b'/'
            | b'"'
            | b'\''
            | b'`'
            | b':'
            | b'*'
            | b'?'
            | b'<'
            | b'>'
            | b'|'
            | b','
            | b';'
            | b'='
            | b'('
            | b')'
            | b'['
            | b']'
            | b'{'
            | b'}'
    ) || b.is_ascii_whitespace()
        || b.is_ascii_control()
}

/// Replaces the user name in user profile paths with `%USERNAME%`:
///
/// 1. every home directory in `homes` (e.g. `USERPROFILE`), also written
///    with `/` or with JSON-escaped `\\`, compared ASCII case-insensitively;
/// 2. any `<drive>:\Users\<name>` or `/Users/<name>` path (any separators,
///    any case) where `Users` starts a path, not the middle of a URL or of
///    another path such as `example.com/users/1`.
///
/// Ordinary words, including a bare user name, are left alone.
pub fn redact_user_paths(text: &str, homes: &[String]) -> String {
    let mut out = text.to_string();
    for home in homes {
        let home = home.trim_end_matches(['\\', '/']);
        match home.rfind(['\\', '/']) {
            Some(cut) if cut > 0 => {}
            _ => continue,
        }
        for needle in [
            home.to_string(),
            home.replace('\\', "/"),
            home.replace('\\', "\\\\"),
        ] {
            out = replace_home(&out, &needle);
        }
    }
    redact_users_segments(&out)
}

/// Replaces the last segment of `needle` (a home directory, matched ASCII
/// case-insensitively) with `%USERNAME%` where it appears as a whole path:
/// not preceded by a letter or digit and not followed by a name byte.
fn replace_home(text: &str, needle: &str) -> String {
    let Some(keep) = needle.rfind(['\\', '/']).map(|cut| cut + 1) else {
        return text.to_string();
    };
    let lower = text.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !is_name_byte(bytes[end]);
        if before_ok && after_ok {
            out.push_str(&text[copied..start + keep]);
            out.push_str(USER_PLACEHOLDER);
            copied = end;
        }
        from = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn redact_users_segments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if !bytes[i..i + 5].eq_ignore_ascii_case(b"users") {
            i += 1;
            continue;
        }
        // At least one separator before `Users`, and what precedes the
        // separators must not be part of a name (`C:`, start, quote, space).
        let mut before = i;
        while before > 0 && is_separator(bytes[before - 1]) {
            before -= 1;
        }
        let starts_path = before < i
            && (before == 0 || bytes[before - 1] == b':' || !is_name_byte(bytes[before - 1]));
        // At least one separator after it, then the name.
        let mut name_start = i + 5;
        while name_start < bytes.len() && is_separator(bytes[name_start]) {
            name_start += 1;
        }
        let mut name_end = name_start;
        while name_end < bytes.len() && !ends_name(bytes[name_end]) {
            name_end += 1;
        }
        let name = &text[name_start..name_end];
        if starts_path && name_start > i + 5 && !name.is_empty() && name != USER_PLACEHOLDER {
            out.push_str(&text[copied..name_start]);
            out.push_str(USER_PLACEHOLDER);
            copied = name_end;
            i = name_end;
        } else {
            i += 5;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Code tokens std puts in backticks that never carry user data.
const SAFE_CODE: [&str; 9] = [
    "Option::unwrap()",
    "Option::expect()",
    "Result::unwrap()",
    "Result::expect()",
    "Result::unwrap_err()",
    "None",
    "Some",
    "Ok",
    "Err",
];

/// Replaces quoted spans with `<redacted len=N>`: `` `...` `` (std quotes
/// sliced strings this way) unless it is a known code token, `"..."` (Debug
/// of strings), and single-char `'x'`. An unclosed quote redacts to the end.
pub fn sanitize_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(open) = rest.find(['`', '"', '\'']) {
        let quote = rest[open..].chars().next().unwrap_or('"');
        let after = &rest[open + 1..];
        out.push_str(&rest[..open]);
        if quote == '\'' {
            // Only `'x'`: apostrophes in English text stay.
            let mut chars = after.chars();
            match (chars.next(), chars.next()) {
                (Some(c), Some('\'')) => {
                    out.push_str(&format!("'<redacted len={}>'", c.len_utf8()));
                    rest = &after[c.len_utf8() + 1..];
                }
                _ => {
                    out.push('\'');
                    rest = after;
                }
            }
            continue;
        }
        let close = after.find(quote);
        let inner = &after[..close.unwrap_or(after.len())];
        if quote == '`' && SAFE_CODE.contains(&inner) {
            out.push('`');
            out.push_str(inner);
            out.push('`');
        } else {
            out.push(quote);
            out.push_str(&format!("<redacted len={}>", inner.len()));
            if close.is_some() {
                out.push(quote);
            }
        }
        rest = match close {
            Some(close) => &after[close + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn homes(list: &[&str]) -> Vec<String> {
        list.iter().map(|home| home.to_string()).collect()
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("abc", 8), "abc");
        assert_eq!(truncate_utf8("abcdef", 3), "abc");
        // "é" is two bytes, "中" three.
        assert_eq!(truncate_utf8("aé", 2), "a");
        assert_eq!(truncate_utf8("中文", 4), "中");
        assert_eq!(truncate_utf8("中文", 0), "");
    }

    #[test]
    fn fingerprints_are_short_hex_and_group_equal_texts() {
        let a = fingerprint("SENTINEL phrase");
        assert_eq!(a.len(), 8);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()), "{a}");
        assert_eq!(fingerprint("SENTINEL phrase"), a);
        assert_ne!(fingerprint("SENTINEL phrase."), a);
        // Salted: not the plain SHA-256 prefix of the text.
        let plain = Sha256::digest(b"SENTINEL phrase");
        let plain: String = plain[..4].iter().map(|b| format!("{b:02x}")).collect();
        assert_ne!(a, plain);
    }

    #[test]
    fn standard_profile_paths_lose_the_user_name() {
        let cases = [
            (
                r"C:\Users\alice\AppData\Roaming",
                r"C:\Users\%USERNAME%\AppData\Roaming",
            ),
            (r"c:\users\Alice", r"c:\users\%USERNAME%"),
            ("C:/Users/alice/x.log", "C:/Users/%USERNAME%/x.log"),
            (
                r#"{"path":"C:\\Users\\alice\\data"}"#,
                r#"{"path":"C:\\Users\\%USERNAME%\\data"}"#,
            ),
            (
                "file:///C:/Users/bob%20x/a",
                "file:///C:/Users/%USERNAME%/a",
            ),
            (
                r"open C:\Users\ALICE~1\x failed",
                r"open C:\Users\%USERNAME%\x failed",
            ),
            (
                "at /Users/carol/src/main.rs:1",
                "at /Users/%USERNAME%/src/main.rs:1",
            ),
            (r"'C:\Users\张三\桌面'", r"'C:\Users\%USERNAME%\桌面'"),
            (r"\\?\C:\Users\dave", r"\\?\C:\Users\%USERNAME%"),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_user_paths(input, &[]), expected, "{input}");
        }
    }

    #[test]
    fn ordinary_words_and_urls_are_untouched() {
        let untouched = [
            "users",
            "Users of this app: alice, bob",
            "https://example.com/users/42",
            r"D:\data\Users\alice",
            "the users/ folder",
            r"C:\Usersettings\alice",
            "alice opened the settings",
            r"C:\Users\%USERNAME%\x",
        ];
        for text in untouched {
            assert_eq!(
                redact_user_paths(text, &homes(&[r"C:\Users\alice"])),
                text,
                "{text}"
            );
        }
    }

    #[test]
    fn custom_profile_locations_from_userprofile_are_replaced() {
        let homes = homes(&[r"D:\Profiles\alice"]);
        assert_eq!(
            redact_user_paths(r"log at D:\Profiles\alice\AppData\x.log", &homes),
            r"log at D:\Profiles\%USERNAME%\AppData\x.log"
        );
        assert_eq!(
            redact_user_paths("d:/profiles/ALICE/x", &homes),
            "d:/profiles/%USERNAME%/x"
        );
        assert_eq!(
            redact_user_paths(r#""D:\\Profiles\\alice\\x""#, &homes),
            r#""D:\\Profiles\\%USERNAME%\\x""#
        );
        // A longer sibling name is a different directory.
        assert_eq!(
            redact_user_paths(r"D:\Profiles\alice2\x", &homes),
            r"D:\Profiles\alice2\x"
        );
        // Degenerate homes are ignored.
        assert_eq!(
            redact_user_paths("alice", &self::homes(&["alice", "", "/"])),
            "alice"
        );
    }

    #[test]
    fn quoted_spans_in_panic_messages_are_redacted() {
        assert_eq!(
            sanitize_quoted("byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4) of `SENTINEL phrase`"),
            "byte index 3 is not a char boundary; it is inside '<redacted len=2>' (bytes 2..4) of `<redacted len=15>`"
        );
        assert_eq!(
            sanitize_quoted("called `Option::unwrap()` on a `None` value"),
            "called `Option::unwrap()` on a `None` value"
        );
        assert_eq!(
            sanitize_quoted(
                r#"called `Result::unwrap()` on an `Err` value: Custom { error: "C:\x secret" }"#
            ),
            r#"called `Result::unwrap()` on an `Err` value: Custom { error: "<redacted len=11>" }"#
        );
        assert_eq!(
            sanitize_quoted("can't open `unclosed secret"),
            "can't open `<redacted len=15>"
        );
        assert_eq!(
            sanitize_quoted("index out of bounds: the len is 3 but the index is 5"),
            "index out of bounds: the len is 3 but the index is 5"
        );
    }
}
