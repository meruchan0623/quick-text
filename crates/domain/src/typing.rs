//! The typed-key buffer both input backends match against: what one
//! key-down does to it, which keys are terminators, and how a terminator
//! match is tried before the plain one. Each platform only classifies its
//! own key codes ([`KeyClass`]); the edits, the bound and the matching order
//! are shared, so an abbreviation behaves the same on Windows and macOS.
use crate::{CompiledConfig, ForegroundContext, MatchResult, Redacted};
use std::fmt;

/// The longest buffer kept, in characters; older characters drop off the
/// front, so a long run of typing never loses the abbreviation at its end.
pub const TYPED_BUFFER_CHARS: usize = 1024;

/// What one key-down does to the typed-key buffer.
#[derive(Clone, PartialEq, Eq)]
pub enum KeyEdit {
    /// Characters the key typed (never a control character).
    Text(String),
    /// Removes the last character.
    Backspace,
    /// Leaves the buffer as it is (Shift, Caps Lock, Ctrl, Alt, Cmd alone).
    Neutral,
    /// Clears the buffer: the caret may have moved, or the key typed nothing
    /// a rule can match.
    Reset,
}

/// The typed text is redacted (B-09).
impl fmt::Debug for KeyEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(&Redacted(text)).finish(),
            Self::Backspace => f.write_str("Backspace"),
            Self::Neutral => f.write_str("Neutral"),
            Self::Reset => f.write_str("Reset"),
        }
    }
}

/// A key as the platform classifies it, before its text is looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyClass {
    /// Shift, Caps Lock, Ctrl, Alt/Option, Cmd/Win.
    Modifier,
    Backspace,
    /// Tab, Enter, Esc, Delete, arrows, Home, End, Page Up/Down: keys that
    /// move the caret or leave the field. Tab and Enter are not terminators
    /// (B-12).
    Navigation,
    /// Any other key: it counts through the text it typed.
    Other,
}

/// The edit a key-down makes. `text` is what the key typed under the current
/// layout; `modified` is whether Ctrl, Alt/Option or Cmd/Win was held, which
/// makes every key but a modifier a reset (a shortcut, not typing).
pub fn key_edit(class: KeyClass, text: Option<String>, modified: bool) -> KeyEdit {
    match class {
        KeyClass::Modifier => KeyEdit::Neutral,
        KeyClass::Backspace if !modified => KeyEdit::Backspace,
        KeyClass::Backspace | KeyClass::Navigation => KeyEdit::Reset,
        KeyClass::Other if modified => KeyEdit::Reset,
        KeyClass::Other => text
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .map(KeyEdit::Text)
            .unwrap_or(KeyEdit::Reset),
    }
}

/// Applies one edit, then drops characters off the front beyond
/// [`TYPED_BUFFER_CHARS`].
pub fn apply_key_edit(text: &mut String, edit: &KeyEdit) {
    match edit {
        KeyEdit::Text(typed) => text.push_str(typed),
        KeyEdit::Backspace => {
            text.pop();
        }
        KeyEdit::Neutral => {}
        KeyEdit::Reset => text.clear(),
    }
    let excess = text.chars().count().saturating_sub(TYPED_BUFFER_CHARS);
    if excess > 0 {
        let cut = text
            .char_indices()
            .nth(excess)
            .map_or(text.len(), |(index, _)| index);
        text.drain(..cut);
    }
}

/// The terminator an edit types, if it is one: a space or a semicolon. The
/// character also joins the buffer, so a rule whose abbreviation contains it
/// can still match as typed.
pub fn typed_terminator(edit: &KeyEdit) -> Option<&'static str> {
    match edit {
        KeyEdit::Text(value) if value == " " => Some("space"),
        KeyEdit::Text(value) if value == ";" => Some("semicolon"),
        _ => None,
    }
}

/// Matches the buffer after a key. When the key was a terminator,
/// `previous` is the buffer before it and the terminator's name: a
/// terminator rule on that prefix wins; otherwise the whole buffer (the
/// terminator character included) is tried as an immediate match. Returns
/// the match and the terminator it used.
pub fn match_typed(
    compiled: &CompiledConfig,
    text: &str,
    previous: Option<(&str, &'static str)>,
    foreground: &ForegroundContext,
) -> Option<(MatchResult, Option<&'static str>)> {
    previous
        .and_then(|(input, key)| {
            compiled
                .match_input(input, Some(key), foreground)
                .map(|matched| (matched, Some(key)))
        })
        .or_else(|| {
            compiled
                .match_input(text, None, foreground)
                .map(|matched| (matched, None))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> KeyEdit {
        KeyEdit::Text(s.into())
    }

    #[test]
    fn keys_classify_the_same_on_every_platform() {
        assert_eq!(key_edit(KeyClass::Modifier, None, true), KeyEdit::Neutral);
        assert_eq!(
            key_edit(KeyClass::Backspace, None, false),
            KeyEdit::Backspace
        );
        assert_eq!(key_edit(KeyClass::Backspace, None, true), KeyEdit::Reset);
        assert_eq!(
            key_edit(KeyClass::Navigation, Some("\r".into()), false),
            KeyEdit::Reset
        );
        assert_eq!(
            key_edit(KeyClass::Other, Some("é".into()), false),
            text("é")
        );
        assert_eq!(
            key_edit(KeyClass::Other, Some("c".into()), true),
            KeyEdit::Reset
        );
        assert_eq!(
            key_edit(KeyClass::Other, Some("\u{1b}".into()), false),
            KeyEdit::Reset
        );
        assert_eq!(
            key_edit(KeyClass::Other, Some(String::new()), false),
            KeyEdit::Reset
        );
        assert_eq!(key_edit(KeyClass::Other, None, false), KeyEdit::Reset);
    }

    #[test]
    fn edits_apply_and_the_buffer_keeps_its_newest_characters() {
        let mut buffer = String::new();
        for edit in [
            text("a"),
            text("é"),
            KeyEdit::Neutral,
            text("中"),
            KeyEdit::Backspace,
        ] {
            apply_key_edit(&mut buffer, &edit);
        }
        assert_eq!(buffer, "aé");
        apply_key_edit(&mut buffer, &KeyEdit::Reset);
        assert!(buffer.is_empty());
        for _ in 0..TYPED_BUFFER_CHARS {
            apply_key_edit(&mut buffer, &text("中"));
        }
        apply_key_edit(&mut buffer, &text("ab"));
        assert_eq!(buffer.chars().count(), TYPED_BUFFER_CHARS);
        assert!(buffer.ends_with("中ab"));
    }

    #[test]
    fn only_space_and_semicolon_are_terminators() {
        assert_eq!(typed_terminator(&text(" ")), Some("space"));
        assert_eq!(typed_terminator(&text(";")), Some("semicolon"));
        assert_eq!(typed_terminator(&text("\t")), None);
        assert_eq!(typed_terminator(&KeyEdit::Reset), None);
    }

    #[test]
    fn typed_text_is_redacted() {
        assert!(!format!("{:?}", text("secret")).contains("secret"));
    }
}
