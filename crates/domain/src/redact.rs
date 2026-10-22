//! `Debug` for types that carry phrase bodies, abbreviations, clipboard contents or detection
//! buffers (B-09): the text itself never reaches `{:?}` (so neither a log line nor a panic
//! message), only `<redacted len=N>`. Ids, kinds, methods, key and field names stay visible.
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
