//! `Debug` for types that carry phrase bodies, abbreviations, clipboard contents or detection
//! buffers (B-09): the text itself never reaches `{:?}` (so neither a log line nor a panic
//! message), only `<redacted len=N>` (`<redacted>` for a key). Ids, kinds, methods, field
//! names and map keys stay visible.
//! None of these types implement `Display`, and their serde shapes are unchanged.
use serde_json::Value;
use std::fmt;

fn redacted(f: &mut fmt::Formatter<'_>, len: usize) -> fmt::Result {
    write!(f, "<redacted len={len}>")
}

/// Text that `Debug`-prints as `<redacted len=N>`, N its length in bytes.
#[derive(Clone, Copy)]
pub struct Redacted<'a>(pub &'a str);
impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        redacted(f, self.0.len())
    }
}

/// Content that is not text, `Debug`-printed as `<redacted len=N>` with N given by the
/// caller (bytes of clipboard data, steps of a plan).
#[derive(Clone, Copy)]
pub struct RedactedLen(pub usize);
impl fmt::Debug for RedactedLen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        redacted(f, self.0)
    }
}

/// Texts (an `Open` action's arguments), `Debug`-printed as a list of [`Redacted`]: their
/// count and lengths only.
#[derive(Clone, Copy)]
pub struct RedactedList<'a, T>(pub &'a [T]);
impl<T: AsRef<str>> fmt::Debug for RedactedList<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|text| Redacted(text.as_ref())))
            .finish()
    }
}

/// Which key: a [`Key`](crate::Key) in a key operation, a virtual-key or scan code. Printed as
/// `<redacted>`, with no length: a sequence of keys (a recorded macro, the hook's records)
/// spells out what was typed, key by key.
#[derive(Clone, Copy)]
pub struct RedactedKey;
impl fmt::Debug for RedactedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A JSON value, `Debug`-printed as `<redacted len=N>`: N is the byte length of a string, or
/// of the compact JSON text of any other value.
#[derive(Clone, Copy)]
pub struct RedactedValue<'a>(pub &'a Value);
impl fmt::Debug for RedactedValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::String(text) => redacted(f, text.len()),
            other => redacted(f, other.to_string().len()),
        }
    }
}

/// A map of JSON values (unknown fields kept in `extra`, action parameters): keys are shown,
/// each value is a [`RedactedValue`].
pub struct RedactedValues<'a, M: ?Sized>(pub &'a M);
impl<'a, M: ?Sized> fmt::Debug for RedactedValues<'a, M>
where
    &'a M: IntoIterator<Item = (&'a String, &'a Value)>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.0
                    .into_iter()
                    .map(|(key, value)| (key, RedactedValue(value))),
            )
            .finish()
    }
}
