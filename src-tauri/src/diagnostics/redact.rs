//! Text redaction for crash records and the diagnostics bundle (§7.1).
//!
//! - [`redact_user_paths`]: user profile directories become `%USERNAME%`.
//! - [`PanicPayload`]: what is kept of a panic message (std quotes the
//!   offending string, e.g. a phrase body sliced at a bad index): a class
//!   from std's fixed wording, the length and a fingerprint, never the text.
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

/// What std's own panics are about, read from their fixed wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicClass {
    /// `index out of bounds: the len is 3 but the index is 5`.
    IndexOutOfBounds,
    /// A `str` sliced off a char boundary or past its end (`byte index 2 is
    /// not a char boundary ...`, `begin <= end (4 <= 2) when slicing ...`).
    ByteIndex,
    /// A slice range out of order or past the end (`range end index 5 out
    /// of range ...`, `slice index starts at 4 but ends at 2`).
    SliceRange,
    /// ``called `Option::unwrap()` on a `None` value``.
    OptionUnwrap,
    /// ``called `Result::unwrap()` on an `Err` value: ...`` (and
    /// `unwrap_err` on `Ok`).
    ResultUnwrap,
    /// `attempt to add with overflow`, `attempt to divide by zero`, ...
    Arithmetic,
    /// A `RefCell` borrowed twice (`already borrowed`, `already mutably
    /// borrowed`).
    AlreadyBorrowed,
    /// The payload is not text (`std::panic::panic_any`).
    NonString,
    /// Any other text, including every `expect` and `panic!` message.
    Other,
}

/// Payload prefixes of std's panics (current and older wording); first
/// match wins.
const PANIC_PREFIXES: [(&str, PanicClass); 16] = [
    ("index out of bounds", PanicClass::IndexOutOfBounds),
    ("start byte index ", PanicClass::ByteIndex),
    ("end byte index ", PanicClass::ByteIndex),
    ("byte index ", PanicClass::ByteIndex),
    ("begin <= end ", PanicClass::ByteIndex),
    ("range start index ", PanicClass::SliceRange),
    ("range end index ", PanicClass::SliceRange),
    ("slice index starts at ", PanicClass::SliceRange),
    ("called `Option::unwrap()`", PanicClass::OptionUnwrap),
    ("called `Result::unwrap()`", PanicClass::ResultUnwrap),
    ("called `Result::unwrap_err()`", PanicClass::ResultUnwrap),
    ("attempt to ", PanicClass::Arithmetic),
    ("already borrowed", PanicClass::AlreadyBorrowed),
    ("already mutably borrowed", PanicClass::AlreadyBorrowed),
    ("RefCell already borrowed", PanicClass::AlreadyBorrowed),
    (
        "RefCell already mutably borrowed",
        PanicClass::AlreadyBorrowed,
    ),
];

impl PanicClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IndexOutOfBounds => "index_out_of_bounds",
            Self::ByteIndex => "byte_index",
            Self::SliceRange => "slice_range",
            Self::OptionUnwrap => "option_unwrap",
            Self::ResultUnwrap => "result_unwrap",
            Self::Arithmetic => "arithmetic",
            Self::AlreadyBorrowed => "already_borrowed",
            Self::NonString => "non_string",
            Self::Other => "other",
        }
    }
}

/// What is recorded of a panic payload: its class, byte length and
/// [`fingerprint`], never its text. A payload can quote anything (std quotes
/// the string it was slicing, `Result::unwrap` appends the error's `Debug`),
/// and the quoted text may itself contain quotes, so no rule can tell where
/// a quoted phrase ends; the whole payload is treated as sensitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanicPayload {
    pub class: PanicClass,
    pub len: usize,
    pub hash: String,
}
impl PanicPayload {
    /// `text` is `None` when the payload is not a string.
    pub fn summarize(text: Option<&str>) -> Self {
        let Some(text) = text else {
            return Self {
                class: PanicClass::NonString,
                len: 0,
                hash: fingerprint(""),
            };
        };
        let class = PANIC_PREFIXES
            .into_iter()
            .find(|(prefix, _)| text.starts_with(prefix))
            .map_or(PanicClass::Other, |(_, class)| class);
        Self {
            class,
            len: text.len(),
            hash: fingerprint(text),
        }
    }
    /// The summary of a panic payload as caught: the hook's
    /// `info.payload()`, a `catch_unwind` error or a join error's
    /// `into_panic()`. Only `&str` and `String` payloads have text.
    pub fn of(payload: &(dyn std::any::Any + Send)) -> Self {
        Self::summarize(
            payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str)),
        )
    }
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

    /// Quoted spans are not found and cut out any more: the whole payload is
    /// reduced to its class, length and fingerprint.
    #[test]
    fn quoted_spans_in_panic_messages_are_redacted() {
        let cases = [
            (
                "byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4) of `SENTINEL phrase`",
                PanicClass::ByteIndex,
            ),
            (
                "end byte index 1 is not a char boundary; it is inside 'é' (bytes 0..2) of `é`SENTINEL`é`",
                PanicClass::ByteIndex,
            ),
            (
                "start byte index 9 is out of bounds of `SENTINEL`",
                PanicClass::ByteIndex,
            ),
            ("begin <= end (4 <= 2) when slicing `SENTINEL`", PanicClass::ByteIndex),
            (
                "range end index 9 out of range for slice of length 3",
                PanicClass::SliceRange,
            ),
            (
                "slice index starts at 4 but ends at 2",
                PanicClass::SliceRange,
            ),
            (
                "called `Option::unwrap()` on a `None` value",
                PanicClass::OptionUnwrap,
            ),
            (
                r#"called `Result::unwrap()` on an `Err` value: Custom { error: "C:\x \"SENTINEL\"" }"#,
                PanicClass::ResultUnwrap,
            ),
            (
                "index out of bounds: the len is 3 but the index is 5",
                PanicClass::IndexOutOfBounds,
            ),
            ("attempt to add with overflow", PanicClass::Arithmetic),
            ("already mutably borrowed: BorrowError", PanicClass::AlreadyBorrowed),
            ("RefCell already borrowed", PanicClass::AlreadyBorrowed),
            ("can't open `unclosed SENTINEL", PanicClass::Other),
            (r#""a\"SENTINEL\"b""#, PanicClass::Other),
            ("SENTINEL index out of bounds", PanicClass::Other),
            ("", PanicClass::Other),
        ];
        for (text, class) in cases {
            let payload = PanicPayload::summarize(Some(text));
            assert_eq!(
                payload,
                PanicPayload {
                    class,
                    len: text.len(),
                    hash: fingerprint(text),
                },
                "{text}"
            );
        }
        let other = PanicPayload::summarize(None);
        assert_eq!((other.class, other.len), (PanicClass::NonString, 0));
        // As caught: `&str` and `String` payloads are read, others are not.
        let caught: [(Box<dyn std::any::Any + Send>, PanicClass, usize); 3] = [
            (
                Box::new("index out of bounds: x"),
                PanicClass::IndexOutOfBounds,
                22,
            ),
            (Box::new(String::from("SENTINEL")), PanicClass::Other, 8),
            (Box::new(7_u32), PanicClass::NonString, 0),
        ];
        for (payload, class, len) in caught {
            let summary = PanicPayload::of(payload.as_ref());
            assert_eq!((summary.class, summary.len), (class, len));
        }
        assert_eq!(PanicClass::ByteIndex.as_str(), "byte_index");
        assert_eq!(PanicClass::NonString.as_str(), "non_string");
    }
}
