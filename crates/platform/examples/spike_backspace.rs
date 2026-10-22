//! S-29 spike, item 3: B-34 "undo expansion on Backspace" deletion arithmetic.
//!
//! PURE COMPUTATION. This program calls no Win32 API, sends no input, reads no
//! clipboard and opens no window. It answers one question: after an expansion
//! has been typed and the user presses Backspace once, how many more
//! Backspaces must the engine send, and what must it retype, so that the text
//! returns to what the user typed?
//!
//! The answer depends on how the *target* control shrinks its text for one
//! Backspace, which is not the same everywhere (UAX #29 itself says backspace
//! "might delete by code point, while the delete key may delete an entire
//! cluster"). Four erase models are therefore parameterised:
//!
//! * `Utf16Unit`  one UTF-16 code unit per press (worst case, legacy controls)
//! * `CodePoint`  one scalar value per press (surrogate pair is one press)
//! * `EmojiAware` Blink-like: emoji sequences (ZWJ, flags, keycaps, skin tone)
//!   go as one unit, other combining marks one scalar at a time
//! * `Grapheme`   extended grapheme cluster (UAX #29), the plan's assumption
//!
//! Every model treats a line break (CRLF, LF or CR as typed text) as ONE unit.
//! The engine types a line break as a single VK_RETURN press (`windows.rs::unicode`
//! maps `\r\n`, `\r` and `\n` to one press). That a single Backspace also removes
//! the whole CRLF a target stored is only an expectation for Edit, RichEdit and
//! Chromium; it still has to be measured (docs/spikes/s-29.md, T-15).
//!
//! The grapheme segmenter here is a deliberately small approximation (no new
//! dependency, so Cargo.lock stays untouched). The real implementation should
//! use `unicode-segmentation`, which is already in Cargo.lock.
//!
//! Run:   cargo run  -p quicker-platform --example spike_backspace            (demo table)
//!        cargo run  -p quicker-platform --example spike_backspace -- ";sig" "cafe\u{301}" --mode immediate
//! Test:  cargo test -p quicker-platform --example spike_backspace

use std::fmt;

/// Mirrors `quicker_domain::TriggerMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerMode {
    Immediate,
    Terminator,
}

/// Mirrors `quicker_domain::ImeErasure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImeErase {
    Escape,
    Backspace,
}

/// How one Backspace press shrinks the text in the target control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EraseModel {
    Utf16Unit,
    CodePoint,
    EmojiAware,
    Grapheme,
}

impl EraseModel {
    const ALL: [EraseModel; 4] = [
        EraseModel::Utf16Unit,
        EraseModel::CodePoint,
        EraseModel::EmojiAware,
        EraseModel::Grapheme,
    ];

    fn name(self) -> &'static str {
        match self {
            EraseModel::Utf16Unit => "utf16",
            EraseModel::CodePoint => "codepoint",
            EraseModel::EmojiAware => "emoji",
            EraseModel::Grapheme => "grapheme",
        }
    }

    fn cluster_based(self) -> bool {
        matches!(self, EraseModel::EmojiAware | EraseModel::Grapheme)
    }
}

// ---------------------------------------------------------------------------
// Approximate grapheme segmentation
//
// All rules of UAX #29 are implemented (GB3-GB13), but the property tables are small
// hand-written subsets of the Unicode data (common Extend, Prepend, SpacingMark and
// Extended_Pictographic ranges, the Hangul Jamo blocks and the precomposed Hangul
// syllables). It is good enough to show how the erase models diverge. It is NOT
// authoritative: the first-release planner (`plan_undo_invariant`) does not depend on
// it for soundness, it only accepts the whitelist in `is_simple_scalar`. A real
// implementation of a per-app model table must use `unicode-segmentation`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gcb {
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    Ri,
    Prepend,
    SpacingMark,
    ExtPict,
    /// Hangul leading consonant (Jamo L).
    L,
    /// Hangul vowel (Jamo V).
    V,
    /// Hangul trailing consonant (Jamo T).
    T,
    /// Precomposed Hangul syllable without a trailing consonant.
    Lv,
    /// Precomposed Hangul syllable with a trailing consonant.
    Lvt,
    Other,
}

fn in_any(u: u32, ranges: &[(u32, u32)]) -> bool {
    ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&u))
}

fn gcb(c: char) -> Gcb {
    let u = c as u32;
    if c == '\r' {
        return Gcb::Cr;
    }
    if c == '\n' {
        return Gcb::Lf;
    }
    if c == '\u{200D}' {
        return Gcb::Zwj;
    }
    if c.is_control() || matches!(u, 0x200B | 0x2028 | 0x2029 | 0xFEFF) {
        return Gcb::Control;
    }
    if (0x1F1E6..=0x1F1FF).contains(&u) {
        return Gcb::Ri;
    }
    if in_any(u, &[(0x1100, 0x115F), (0xA960, 0xA97C)]) {
        return Gcb::L;
    }
    if in_any(u, &[(0x1160, 0x11A7), (0xD7B0, 0xD7C6)]) {
        return Gcb::V;
    }
    if in_any(u, &[(0x11A8, 0x11FF), (0xD7CB, 0xD7FB)]) {
        return Gcb::T;
    }
    if (0xAC00..=0xD7A3).contains(&u) {
        return if (u - 0xAC00).is_multiple_of(28) {
            Gcb::Lv
        } else {
            Gcb::Lvt
        };
    }
    const EXTEND: &[(u32, u32)] = &[
        (0x0300, 0x036F),
        (0x0483, 0x0489),
        (0x0591, 0x05BD),
        (0x0610, 0x061A),
        (0x064B, 0x065F),
        (0x0900, 0x0902),
        (0x093A, 0x093A),
        (0x093C, 0x093C),
        (0x0941, 0x0948),
        (0x094D, 0x094D),
        (0x0951, 0x0957),
        (0x0E31, 0x0E31),
        (0x0E34, 0x0E3A),
        (0x0E47, 0x0E4E),
        (0x1AB0, 0x1AFF),
        (0x1DC0, 0x1DFF),
        (0x200C, 0x200C),
        (0x20D0, 0x20F0),
        (0x302A, 0x302F),
        (0x3099, 0x309A),
        (0xFE00, 0xFE0F),
        (0xFE20, 0xFE2F),
        (0x1F3FB, 0x1F3FF),
        (0xE0020, 0xE007F),
        (0xE0100, 0xE01EF),
    ];
    if in_any(u, EXTEND) {
        return Gcb::Extend;
    }
    if in_any(u, &[(0x0600, 0x0605), (0x06DD, 0x06DD), (0x070F, 0x070F)]) {
        return Gcb::Prepend;
    }
    if in_any(
        u,
        &[
            (0x0903, 0x0903),
            (0x093B, 0x093B),
            (0x093E, 0x0940),
            (0x0949, 0x094C),
            (0x094E, 0x094F),
        ],
    ) {
        return Gcb::SpacingMark;
    }
    const PICT: &[(u32, u32)] = &[
        (0x00A9, 0x00A9),
        (0x00AE, 0x00AE),
        (0x203C, 0x203C),
        (0x2049, 0x2049),
        (0x2122, 0x2122),
        (0x2139, 0x2139),
        (0x2194, 0x21AA),
        (0x231A, 0x23FF),
        (0x24C2, 0x24C2),
        (0x25AA, 0x25FE),
        (0x2600, 0x27BF),
        (0x2934, 0x2935),
        (0x2B05, 0x2B55),
        (0x3030, 0x3030),
        (0x303D, 0x303D),
        (0x3297, 0x3299),
        (0x1F000, 0x1FAFF),
    ];
    if in_any(u, PICT) {
        return Gcb::ExtPict;
    }
    Gcb::Other
}

fn is_break(prev: Gcb, cur: Gcb, pict_state: u8, ri_run: usize) -> bool {
    use Gcb::{Control, Cr, ExtPict, Extend, Lf, Lv, Lvt, Prepend, Ri, SpacingMark, Zwj, L, T, V};
    match (prev, cur) {
        (Cr, Lf) => false,                        // GB3
        (Control | Cr | Lf, _) => true,           // GB4
        (_, Control | Cr | Lf) => true,           // GB5
        (L, L | V | Lv | Lvt) => false,           // GB6
        (Lv | V, V | T) => false,                 // GB7
        (Lvt | T, T) => false,                    // GB8
        (_, Extend | Zwj | SpacingMark) => false, // GB9, GB9a
        (Prepend, _) => false,                    // GB9b
        (_, ExtPict) if pict_state == 2 => false, // GB11
        (Ri, Ri) if ri_run % 2 == 1 => false,     // GB12, GB13
        _ => true,                                // GB999
    }
}

fn clusters(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<Gcb> = None;
    // 0: none, 1: ExtPict Extend*, 2: ExtPict Extend* ZWJ
    let mut pict_state = 0u8;
    let mut ri_run = 0usize;
    for (i, c) in s.char_indices() {
        let cur = gcb(c);
        if let Some(p) = prev {
            if is_break(p, cur, pict_state, ri_run) {
                out.push(&s[start..i]);
                start = i;
            }
        }
        pict_state = match (cur, pict_state) {
            (Gcb::ExtPict, _) | (Gcb::Extend, 1) => 1,
            (Gcb::Zwj, 1) => 2,
            _ => 0,
        };
        ri_run = if cur == Gcb::Ri { ri_run + 1 } else { 0 };
        prev = Some(cur);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn emoji_like(cluster: &str) -> bool {
    cluster
        .chars()
        .any(|c| matches!(gcb(c), Gcb::ExtPict | Gcb::Ri) || c == '\u{20E3}')
}

/// UTF-16 length of every erase unit, in text order. CRLF is always one unit.
fn unit_lens_u16(s: &str, model: EraseModel) -> Vec<usize> {
    fn per_scalar(s: &str, out: &mut Vec<usize>, split_surrogates: bool) {
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
                out.push(2);
            } else if split_surrogates {
                out.extend(std::iter::repeat_n(1, c.len_utf16()));
            } else {
                out.push(c.len_utf16());
            }
        }
    }
    let mut out = Vec::new();
    match model {
        EraseModel::Utf16Unit => per_scalar(s, &mut out, true),
        EraseModel::CodePoint => per_scalar(s, &mut out, false),
        EraseModel::Grapheme => out.extend(clusters(s).iter().map(|c| c.encode_utf16().count())),
        EraseModel::EmojiAware => {
            for cluster in clusters(s) {
                if emoji_like(cluster) {
                    out.push(cluster.encode_utf16().count());
                } else {
                    per_scalar(cluster, &mut out, false);
                }
            }
        }
    }
    out
}

/// The engine types CRLF, LF and CR as one VK_RETURN press each.
fn normalize_line_breaks(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

// ---------------------------------------------------------------------------
// The B-34 computation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct UndoRequest<'a> {
    abbreviation: &'a str,
    /// Text injected for the expansion (rendered; line breaks as authored).
    expansion: &'a str,
    trigger: TriggerMode,
    /// Terminator mode: the key that fired the expansion.
    terminator: Option<char>,
    /// Engine swallowed the terminator (true) or re-emitted it after the expansion (false).
    consume_terminator: bool,
    ime: ImeErase,
    composition_active: bool,
    /// The rule contains a `{cursor}` placeholder.
    cursor_placeholder: bool,
    /// Committed text before the abbreviation, if the observation provides it.
    preceding: Option<&'a str>,
    model: EraseModel,
}

struct Policy {
    max_chars: usize,
    allow_multiline: bool,
    /// Refuse when a grapheme cluster could straddle the text boundary.
    check_junction: bool,
}

impl Default for Policy {
    /// B-34: single line, at most 200 characters.
    fn default() -> Self {
        Self {
            max_chars: 200,
            allow_multiline: false,
            check_junction: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UndoPlan {
    /// Backspaces to send AFTER the user's own one.
    extra_backspaces: usize,
    /// Text to inject (Unicode injection, never VK keys, so an IME stays out of it).
    retype: String,
    /// Erase units the expansion occupies in the target.
    expansion_units: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Empty,
    CompositionActive(ImeErase),
    CursorPlaceholder,
    TooLong {
        chars: usize,
        max: usize,
    },
    MultiLine,
    UnexpectedTerminator,
    MissingTerminator,
    TerminatorNotUndoable(char),
    /// The re-emitted terminator would fuse with the expansion's last cluster.
    TerminatorMerges,
    /// The expansion's first cluster fuses with the preceding text.
    JunctionMerge,
    /// Preceding text unknown and the first scalar could fuse backwards.
    JunctionRisk,
    /// The deletion count differs between the code point / emoji / grapheme models, so it
    /// cannot be computed without knowing how the target's Backspace behaves.
    ModelDependent {
        codepoint: usize,
        emoji: usize,
        grapheme: usize,
    },
    /// First release: this scalar of the expansion (or terminator) is not on the whitelist
    /// (`is_simple_scalar`), so no claim is made about how a target erases it.
    UnsupportedText(char),
    /// First release: the text before the abbreviation is unknown, so a join between it and
    /// the expansion cannot be ruled out.
    UnknownContext,
    /// First release: the last scalar before the abbreviation is not on the whitelist.
    UnsupportedContext(char),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Empty => write!(f, "empty abbreviation or expansion"),
            Refusal::CompositionActive(ime) => write!(
                f,
                "IME composition active (configured erase mode {ime:?}); B-34 does not register undo during composition"
            ),
            Refusal::CursorPlaceholder => write!(f, "rule contains {{cursor}}; not registered (B-34)"),
            Refusal::TooLong { chars, max } => write!(f, "{chars} characters exceeds the limit of {max}"),
            Refusal::MultiLine => write!(f, "multi-line expansion; not registered (B-34)"),
            Refusal::UnexpectedTerminator => write!(f, "immediate mode must not carry a terminator"),
            Refusal::MissingTerminator => write!(f, "terminator mode needs the terminator character"),
            Refusal::TerminatorNotUndoable(c) => write!(
                f,
                "terminator {c:?} has side effects (submit / focus change) or is not one erase unit; never retype it"
            ),
            Refusal::TerminatorMerges => write!(f, "re-emitted terminator fuses with the expansion tail"),
            Refusal::JunctionMerge => write!(f, "expansion fuses with the preceding text; undo would eat user text"),
            Refusal::JunctionRisk => write!(
                f,
                "preceding text unknown and the expansion starts with a combining/joining/regional-indicator scalar"
            ),
            Refusal::UnsupportedText(c) => write!(
                f,
                "U+{:04X} is not on the first-release whitelist (plain letters, CJK, Hangul syllables, single emoticons); no claim about how targets erase it",
                u32::from(*c)
            ),
            Refusal::UnknownContext => write!(
                f,
                "preceding text unknown; a join with the expansion cannot be ruled out (pass --preceding)"
            ),
            Refusal::UnsupportedContext(c) => write!(
                f,
                "the scalar before the abbreviation, U+{:04X}, is not on the whitelist; it might fuse with the expansion",
                u32::from(*c)
            ),
            Refusal::ModelDependent { codepoint, emoji, grapheme } => write!(
                f,
                "count depends on the target's Backspace model (codepoint={codepoint} emoji={emoji} grapheme={grapheme}); needs a per-app model table"
            ),
        }
    }
}

fn plan_undo(req: &UndoRequest<'_>, policy: &Policy) -> Result<UndoPlan, Refusal> {
    if req.composition_active {
        return Err(Refusal::CompositionActive(req.ime));
    }
    if req.cursor_placeholder {
        return Err(Refusal::CursorPlaceholder);
    }
    if req.abbreviation.is_empty() || req.expansion.is_empty() {
        return Err(Refusal::Empty);
    }
    let chars = req.expansion.chars().count();
    if chars > policy.max_chars {
        return Err(Refusal::TooLong {
            chars,
            max: policy.max_chars,
        });
    }
    if !policy.allow_multiline && req.expansion.contains(['\n', '\r']) {
        return Err(Refusal::MultiLine);
    }

    let terminator = match (req.trigger, req.terminator) {
        (TriggerMode::Immediate, None) => None,
        (TriggerMode::Immediate, Some(_)) => return Err(Refusal::UnexpectedTerminator),
        (TriggerMode::Terminator, None) => return Err(Refusal::MissingTerminator),
        (TriggerMode::Terminator, Some(t)) => {
            let one_unit = unit_lens_u16(&t.to_string(), req.model).len() == 1;
            if matches!(t, '\n' | '\r' | '\t') || !one_unit {
                return Err(Refusal::TerminatorNotUndoable(t));
            }
            Some(t)
        }
    };

    let typed = normalize_line_breaks(req.expansion);
    let expansion_lens = unit_lens_u16(&typed, req.model);

    if policy.check_junction {
        match req.preceding {
            Some(preceding) => {
                let joined = format!("{preceding}{typed}");
                let separate =
                    [unit_lens_u16(preceding, req.model), expansion_lens.clone()].concat();
                if unit_lens_u16(&joined, req.model) != separate {
                    return Err(Refusal::JunctionMerge);
                }
            }
            None => {
                let risky = typed.chars().next().is_some_and(|c| {
                    matches!(gcb(c), Gcb::Extend | Gcb::Zwj | Gcb::SpacingMark | Gcb::Ri)
                });
                if req.model.cluster_based() && risky {
                    return Err(Refusal::JunctionRisk);
                }
            }
        }
        if let (Some(t), false) = (terminator, req.consume_terminator) {
            let with_tail = format!("{typed}{t}");
            let separate = [expansion_lens.clone(), vec![t.len_utf16()]].concat();
            if unit_lens_u16(&with_tail, req.model) != separate {
                return Err(Refusal::TerminatorMerges);
            }
        }
    }

    let units = expansion_lens.len();
    let (extra_backspaces, retype) = match terminator {
        // The re-emitted terminator is the last thing in the target, so the user's
        // Backspace removed it and the whole expansion is still there.
        // The plan text (units - 1) is wrong for this case.
        Some(_) if !req.consume_terminator => (units, req.abbreviation.to_owned()),
        // Consumed terminator: the user's Backspace removed the expansion's last unit.
        Some(t) => (units - 1, format!("{}{t}", req.abbreviation)),
        None => (units - 1, req.abbreviation.to_owned()),
    };
    Ok(UndoPlan {
        extra_backspaces,
        retype,
        expansion_units: units,
    })
}

/// The erase models every mainstream target falls into. `Utf16Unit` is the legacy
/// worst case (a control that splits surrogate pairs) and is probed separately.
const MODERN_MODELS: [EraseModel; 3] = [
    EraseModel::CodePoint,
    EraseModel::EmojiAware,
    EraseModel::Grapheme,
];

/// Scalars on the first-release whitelist, written by hand from the Unicode blocks and
/// deliberately independent of `gcb`. Everything here is a "simple" scalar: it never
/// takes part in a join with a neighbouring simple scalar.
///
/// * printable ASCII (U+0020-U+007E)
/// * Latin-1 letters and symbols, without (c), (r) and the soft hyphen
/// * Latin Extended-A/B, Greek, Cyrillic (without the combining range U+0483-U+0489)
/// * CJK punctuation, Hiragana, Katakana, CJK ideographs, fullwidth ASCII forms
/// * precomposed Hangul syllables (no conjoining Jamo, no Hangul filler)
/// * the single-scalar emoticons U+1F600-U+1F64F
///
/// Missing on purpose: every combining mark, variation selector, ZWJ, Prepend and
/// SpacingMark scalar, Hangul Jamo, regional indicators, skin-tone modifiers, keycap
/// parts and the other pictographs (all of which can start or join a sequence), and the
/// scripts (Arabic, Hebrew, Thai, Indic ...) whose clusters were not measured.
const SIMPLE_RANGES: &[(u32, u32)] = &[
    (0x0020, 0x007E),
    (0x00A0, 0x00A8),
    (0x00AA, 0x00AC),
    (0x00AF, 0x00FF),
    (0x0100, 0x024F),
    (0x0370, 0x03FF),
    (0x0400, 0x0482),
    (0x048A, 0x04FF),
    (0x3000, 0x3003),
    (0x3005, 0x3011),
    (0x3041, 0x3096),
    (0x309D, 0x309F),
    (0x30A1, 0x30FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xAC00, 0xD7A3),
    (0xFF01, 0xFF5E),
    (0x1F600, 0x1F64F),
];

/// Whether `c` is on the first-release whitelist (see `SIMPLE_RANGES`), plus Tab and
/// line feed, which UAX #29 always separates (GB4, GB5) and the engine types as one key.
///
/// Soundness, independent of any segmenter: in UAX #29 every rule that forbids a break
/// between two scalars needs a CR, a conjoining Hangul Jamo, an Extend, ZWJ, SpacingMark
/// or Prepend scalar, or a regional indicator (GB3, GB6-GB9c, GB12, GB13), or a ZWJ
/// (GB11); the conjunct Linker of GB9c is itself an Extend scalar. None of those is
/// simple, and the Hangul syllables only join a following Jamo V or T. So between two simple scalars there is always a boundary: every simple
/// scalar is its own grapheme cluster, its own emoji unit and its own code point, and
/// all three models count the same.
fn is_simple_scalar(c: char) -> bool {
    matches!(c, '\t' | '\n') || in_any(u32::from(c), SIMPLE_RANGES)
}

/// Conservative variant for the first release of B-34. The undo is planned only when
/// the deletion count cannot depend on the target's Backspace model, which is decided
/// by a whitelist of scalars proven not to join (`is_simple_scalar`), never by running
/// a segmenter over the text:
///
/// * every scalar of the expansion and the terminator must be simple;
/// * the text before the abbreviation must be known and end in a simple scalar (a
///   Prepend scalar, a CR, or a Hangul Jamo there could fuse with the expansion).
///
/// Anything else is refused until a per-app model table exists (S-29 measurements).
/// The three-model comparison below is a second, redundant check; for whitelisted text
/// it cannot disagree.
fn plan_undo_invariant(req: &UndoRequest<'_>, policy: &Policy) -> Result<UndoPlan, Refusal> {
    let typed = normalize_line_breaks(req.expansion);
    if let Some(c) = typed
        .chars()
        .chain(req.terminator)
        .find(|c| !is_simple_scalar(*c))
    {
        return Err(Refusal::UnsupportedText(c));
    }
    match req.preceding {
        None => return Err(Refusal::UnknownContext),
        Some(preceding) => {
            if let Some(c) = preceding.chars().next_back() {
                if !is_simple_scalar(c) {
                    return Err(Refusal::UnsupportedContext(c));
                }
            }
        }
    }
    let mut plans = Vec::with_capacity(MODERN_MODELS.len());
    for model in MODERN_MODELS {
        plans.push(plan_undo(&UndoRequest { model, ..*req }, policy)?);
    }
    if plans.iter().all(|p| p == &plans[0]) {
        return Ok(plans.swap_remove(0));
    }
    Err(Refusal::ModelDependent {
        codepoint: plans[0].extra_backspaces,
        emoji: plans[1].extra_backspaces,
        grapheme: plans[2].extra_backspaces,
    })
}

// ---------------------------------------------------------------------------
// CLI and demo table
// ---------------------------------------------------------------------------

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                if it.next() != Some('{') {
                    return Err("expected \\u{HEX}".into());
                }
                let hex: String = it.by_ref().take_while(|c| *c != '}').collect();
                let ch = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("bad scalar value in \\u{{{hex}}}"))?;
                out.push(ch);
            }
            other => {
                return Err(format!(
                    "bad escape \\{}",
                    other.map(String::from).unwrap_or_default()
                ))
            }
        }
    }
    Ok(out)
}

fn run_cli(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();
    let mut trigger = TriggerMode::Terminator;
    let mut terminator = Some(' ');
    let mut consume = true;
    let mut ime = ImeErase::Escape;
    let mut composing = false;
    let mut models: Vec<EraseModel> = EraseModel::ALL.to_vec();
    let mut preceding: Option<String> = None;
    let mut policy = Policy::default();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--mode" => match value("--mode")?.as_str() {
                "immediate" => {
                    trigger = TriggerMode::Immediate;
                    terminator = None;
                }
                "terminator" => trigger = TriggerMode::Terminator,
                other => return Err(format!("unknown mode {other}")),
            },
            "--terminator" => terminator = unescape(&value("--terminator")?)?.chars().next(),
            "--keep-terminator" => consume = false,
            "--ime" => {
                ime = match value("--ime")?.as_str() {
                    "escape" => ImeErase::Escape,
                    "backspace" => ImeErase::Backspace,
                    other => return Err(format!("unknown ime mode {other}")),
                }
            }
            "--composing" => composing = true,
            "--allow-multiline" => policy.allow_multiline = true,
            "--preceding" => preceding = Some(unescape(&value("--preceding")?)?),
            "--model" => {
                let name = value("--model")?;
                models = EraseModel::ALL
                    .iter()
                    .copied()
                    .filter(|m| name == "all" || m.name() == name)
                    .collect();
                if models.is_empty() {
                    return Err(format!("unknown model {name}"));
                }
            }
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => positional.push(unescape(other)?),
        }
    }
    let [abbreviation, expansion] = positional.as_slice() else {
        return Err("usage: spike_backspace <ABBR> <EXPANSION> [--mode immediate|terminator] [--terminator C] \
                    [--keep-terminator] [--ime escape|backspace] [--composing] [--allow-multiline] \
                    [--preceding TEXT] [--model utf16|codepoint|emoji|grapheme|all]  (escapes: \\n \\r \\t \\u{HEX})"
            .into());
    };
    let base = UndoRequest {
        abbreviation,
        expansion,
        trigger,
        terminator,
        consume_terminator: consume,
        ime,
        composition_active: composing,
        cursor_placeholder: false,
        preceding: preceding.as_deref(),
        model: EraseModel::Grapheme,
    };
    for model in models {
        let req = UndoRequest { model, ..base };
        match plan_undo(&req, &policy) {
            Ok(plan) => println!(
                "{:<9} units={:<3} extra_backspaces={:<3} retype={:?}",
                model.name(),
                plan.expansion_units,
                plan.extra_backspaces,
                plan.retype
            ),
            Err(why) => println!("{:<9} refused: {why}", model.name()),
        }
    }
    match plan_undo_invariant(&base, &policy) {
        Ok(plan) => println!(
            "invariant (whitelisted text, known context): extra_backspaces={} retype={:?}",
            plan.extra_backspaces, plan.retype
        ),
        Err(why) => println!("invariant: refused: {why}"),
    }
    Ok(())
}

fn demo() {
    struct Case {
        label: &'static str,
        expansion: &'static str,
        trigger: TriggerMode,
        terminator: Option<char>,
        consume: bool,
    }
    let cases = [
        Case {
            label: "ascii, immediate",
            expansion: "Best regards",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "ascii, space consumed",
            expansion: "Best regards",
            trigger: TriggerMode::Terminator,
            terminator: Some(' '),
            consume: true,
        },
        Case {
            label: "ascii, ';' re-emitted",
            expansion: "Best regards",
            trigger: TriggerMode::Terminator,
            terminator: Some(';'),
            consume: false,
        },
        Case {
            label: "emoji U+1F600 tail",
            expansion: "ok \u{1F600}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "ZWJ family",
            expansion: "hi \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "flag JP",
            expansion: "\u{1F1EF}\u{1F1F5}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "skin tone",
            expansion: "\u{1F44D}\u{1F3FD}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "keycap 1",
            expansion: "1\u{FE0F}\u{20E3}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "cafe + U+0301",
            expansion: "cafe\u{301}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "Hangul jamo L+V",
            expansion: "\u{1100}\u{1161}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "Hangul syllables",
            expansion: "\u{D55C}\u{AE00}",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "CRLF (line1/line2)",
            expansion: "line1\r\nline2",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
        Case {
            label: "LF (line1/line2)",
            expansion: "line1\nline2",
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume: true,
        },
    ];
    let policy = Policy {
        allow_multiline: true,
        ..Policy::default()
    };
    println!("extra Backspaces to send after the user's own one (abbreviation \";sig\")");
    println!(
        "{:<24}{:>7}{:>11}{:>9}{:>10}  {:<10}  retype",
        "case", "utf16", "codepoint", "emoji", "grapheme", "invariant"
    );
    for case in &cases {
        let mut counts = Vec::new();
        let mut retype = String::new();
        let base = UndoRequest {
            abbreviation: ";sig",
            expansion: case.expansion,
            trigger: case.trigger,
            terminator: case.terminator,
            consume_terminator: case.consume,
            ime: ImeErase::Escape,
            composition_active: false,
            cursor_placeholder: false,
            preceding: Some("x "),
            model: EraseModel::Grapheme,
        };
        for model in EraseModel::ALL {
            let req = UndoRequest { model, ..base };
            match plan_undo(&req, &policy) {
                Ok(plan) => {
                    counts.push(plan.extra_backspaces.to_string());
                    retype = plan.retype;
                }
                Err(why) => counts.push(format!("({why})")),
            }
        }
        let invariant = match plan_undo_invariant(&base, &policy) {
            Ok(plan) => format!("yes ({})", plan.extra_backspaces),
            Err(_) => "NO".to_owned(),
        };
        println!(
            "{:<24}{:>7}{:>11}{:>9}{:>10}  {:<10}  {:?}",
            case.label, counts[0], counts[1], counts[2], counts[3], invariant, retype
        );
    }
    println!("\nWith the plan formula (units - 1) applied to the ';' re-emitted row, one unit of the expansion would be left behind.");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--demo" {
        demo();
        return;
    }
    if let Err(message) = run_cli(&args) {
        eprintln!("{message}");
        std::process::exit(2);
    }
}

// ---------------------------------------------------------------------------
// Tests: arithmetic, refusals, and a target-buffer simulation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn req<'a>(abbreviation: &'a str, expansion: &'a str, model: EraseModel) -> UndoRequest<'a> {
        UndoRequest {
            abbreviation,
            expansion,
            trigger: TriggerMode::Immediate,
            terminator: None,
            consume_terminator: true,
            ime: ImeErase::Escape,
            composition_active: false,
            cursor_placeholder: false,
            preceding: None,
            model,
        }
    }

    fn extra(r: &UndoRequest<'_>, policy: &Policy) -> usize {
        plan_undo(r, policy).expect("plan").extra_backspaces
    }

    fn relaxed() -> Policy {
        Policy {
            allow_multiline: true,
            ..Policy::default()
        }
    }

    // ---- simulation of the target control ---------------------------------

    struct Target {
        buf: Vec<u16>,
        model: EraseModel,
        crlf: bool,
    }

    impl Target {
        fn new(model: EraseModel, crlf: bool) -> Self {
            Self {
                buf: Vec::new(),
                model,
                crlf,
            }
        }

        fn enter(&mut self) {
            if self.crlf {
                self.buf.push(0x0D);
            }
            self.buf.push(0x0A);
        }

        /// Types like the engine: Unicode injection per scalar, one Enter per line break.
        fn type_text(&mut self, s: &str) {
            let mut it = s.chars().peekable();
            while let Some(c) = it.next() {
                match c {
                    '\r' => {
                        if it.peek() == Some(&'\n') {
                            it.next();
                        }
                        self.enter();
                    }
                    '\n' => self.enter(),
                    _ => {
                        let mut unit = [0u16; 2];
                        self.buf.extend_from_slice(c.encode_utf16(&mut unit));
                    }
                }
            }
        }

        fn backspace(&mut self) {
            let n = if self.buf.is_empty() {
                0
            } else if self.buf.ends_with(&[0x0D, 0x0A]) {
                2
            } else if self.model == EraseModel::Utf16Unit {
                1
            } else {
                let text = String::from_utf16_lossy(&self.buf);
                unit_lens_u16(&text, self.model)
                    .last()
                    .copied()
                    .unwrap_or(1)
            };
            self.buf.truncate(self.buf.len() - n);
        }
    }

    /// Runs the whole story and returns (actual, expected) buffers.
    fn simulate(
        r: &UndoRequest<'_>,
        plan: &UndoPlan,
        prefix: &str,
        crlf: bool,
    ) -> (Vec<u16>, Vec<u16>) {
        let mut target = Target::new(r.model, crlf);
        target.type_text(prefix);
        target.type_text(r.expansion);
        if let (TriggerMode::Terminator, false, Some(t)) =
            (r.trigger, r.consume_terminator, r.terminator)
        {
            target.type_text(&t.to_string());
        }
        target.backspace(); // the user's own Backspace
        for _ in 0..plan.extra_backspaces {
            target.backspace();
        }
        target.type_text(&plan.retype);

        let mut want = Target::new(r.model, crlf);
        want.type_text(prefix);
        want.type_text(r.abbreviation);
        if let (TriggerMode::Terminator, true, Some(t)) =
            (r.trigger, r.consume_terminator, r.terminator)
        {
            want.type_text(&t.to_string());
        }
        (target.buf, want.buf)
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }

        fn text(&mut self, atoms: &[&str], max_atoms: usize) -> String {
            let n = 1 + self.next() % max_atoms;
            (0..n).map(|_| atoms[self.next() % atoms.len()]).collect()
        }
    }

    const ATOMS: &[&str] = &[
        "a",
        "b",
        " ",
        ";",
        "\u{E9}",
        "\u{301}",
        "\u{1F600}",
        "\u{1F44D}",
        "\u{1F3FD}",
        "\u{200D}",
        "\u{1F468}",
        "\u{1F469}",
        "\u{1F1EF}",
        "\u{1F1F5}",
        "\u{1F1FA}",
        "\u{FE0F}",
        "1",
        "\u{20E3}",
        "\u{4E2D}",
        "\u{94D}",
        "\u{600}",
        "\n",
        "\r\n",
        "\u{1100}", // Hangul Jamo L
        "\u{1161}", // Hangul Jamo V
        "\u{11A8}", // Hangul Jamo T
        "\u{AC00}", // Hangul syllable LV
        "\u{D55C}", // Hangul syllable LVT
    ];
    /// Only simple scalars (see `is_simple_scalar`): every text built from these is accepted
    /// by the invariant planner when the context is simple too.
    const SIMPLE_ATOMS: &[&str] = &[
        "a",
        "b",
        " ",
        ";",
        "1",
        "\u{E9}",
        "\u{416}",
        "\u{3B1}",
        "\u{4E2D}",
        "\u{D55C}",
        "\u{AC00}",
        "\u{1F600}",
        "\n",
        "\r\n",
    ];
    /// Prefix atoms that cannot fuse with whatever follows.
    const PLAIN: &[&str] = &[
        "a",
        "b",
        " ",
        "\u{E9}",
        "\u{4E2D}",
        "\u{1F600}",
        "\u{1F44D}",
        "1",
        "\n",
    ];
    const TRIGGERS: [(TriggerMode, Option<char>, bool); 4] = [
        (TriggerMode::Immediate, None, true),
        (TriggerMode::Terminator, Some(' '), true),
        (TriggerMode::Terminator, Some(';'), true),
        (TriggerMode::Terminator, Some(';'), false),
    ];

    /// Returns (ok, failed, refused-by-reason) over a random corpus.
    fn fuzz(
        policy: &Policy,
        prefix_atoms: &[&str],
        know_preceding: bool,
        seed: u64,
    ) -> (usize, usize, BTreeMap<String, usize>) {
        let mut rng = Lcg(seed);
        let (mut ok, mut failed) = (0, 0);
        let mut refused: BTreeMap<String, usize> = BTreeMap::new();
        for model in EraseModel::ALL {
            for crlf in [false, true] {
                for (trigger, terminator, consume) in TRIGGERS {
                    for _ in 0..2500 {
                        let prefix = rng.text(prefix_atoms, 4);
                        let abbreviation = rng.text(&["a", "b", ";", "x"], 3);
                        let expansion = rng.text(ATOMS, 7);
                        let r = UndoRequest {
                            abbreviation: &abbreviation,
                            expansion: &expansion,
                            trigger,
                            terminator,
                            consume_terminator: consume,
                            ime: ImeErase::Escape,
                            composition_active: false,
                            cursor_placeholder: false,
                            preceding: know_preceding.then_some(prefix.as_str()),
                            model,
                        };
                        match plan_undo(&r, policy) {
                            Ok(plan) => {
                                let (got, want) = simulate(&r, &plan, &prefix, crlf);
                                if got == want {
                                    ok += 1;
                                } else {
                                    failed += 1;
                                }
                            }
                            Err(why) => {
                                let key = format!("{why:?}");
                                *refused
                                    .entry(key.split(['(', ' ']).next().unwrap_or("").to_owned())
                                    .or_default() += 1;
                            }
                        }
                    }
                }
            }
        }
        (ok, failed, refused)
    }

    // ---- arithmetic --------------------------------------------------------

    #[test]
    fn ascii_immediate_consumed_and_reemitted() {
        let p = Policy::default();
        let mut r = req(";sig", "Best regards", EraseModel::Grapheme);
        assert_eq!(plan_undo(&r, &p).expect("plan").extra_backspaces, 11);
        assert_eq!(plan_undo(&r, &p).expect("plan").retype, ";sig");

        r.trigger = TriggerMode::Terminator;
        r.terminator = Some(' ');
        let plan = plan_undo(&r, &p).expect("plan");
        assert_eq!((plan.extra_backspaces, plan.retype.as_str()), (11, ";sig "));

        // Semicolon is NOT swallowed: the user's Backspace removed it, nothing to retype for it.
        r.terminator = Some(';');
        r.consume_terminator = false;
        let plan = plan_undo(&r, &p).expect("plan");
        assert_eq!((plan.extra_backspaces, plan.retype.as_str()), (12, ";sig"));
    }

    #[test]
    fn surrogate_pairs_differ_only_for_utf16_units() {
        let p = Policy::default();
        // "ok" + U+1F600: 4 UTF-16 units, 3 scalars, 3 clusters.
        let counts: Vec<usize> = EraseModel::ALL
            .iter()
            .map(|m| extra(&req(";e", "ok\u{1F600}", *m), &p))
            .collect();
        assert_eq!(counts, [3, 2, 2, 2]);
    }

    #[test]
    fn zwj_flag_keycap_and_skin_tone_sequences() {
        let p = Policy::default();
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // 5 scalars, 8 UTF-16 units
                                                                    // Flags start with a regional indicator, so the preceding text must be known (see junction_guard_cases).
        let at = |text: &str| -> Vec<usize> {
            EraseModel::ALL
                .iter()
                .map(|m| {
                    let mut r = req(";e", text, *m);
                    r.preceding = Some("x ");
                    extra(&r, &p)
                })
                .collect()
        };
        assert_eq!(at(family), [7, 4, 0, 0]);
        assert_eq!(at("\u{1F1EF}\u{1F1F5}"), [3, 1, 0, 0]); // flag = 2 regional indicators
        assert_eq!(at("\u{1F44D}\u{1F3FD}"), [3, 1, 0, 0]); // thumbs up + skin tone
        assert_eq!(at("1\u{FE0F}\u{20E3}"), [2, 2, 0, 0]); // keycap
                                                           // Three regional indicators: one flag plus a stray one.
        assert_eq!(at("\u{1F1EF}\u{1F1F5}\u{1F1FA}"), [5, 2, 1, 1]);
    }

    #[test]
    fn combining_sequence_is_where_models_really_diverge() {
        let p = Policy::default();
        let counts: Vec<usize> = EraseModel::ALL
            .iter()
            .map(|m| extra(&req(";c", "cafe\u{301}", *m), &p))
            .collect();
        // Only the grapheme model merges e + U+0301; the others take the accent separately.
        assert_eq!(counts, [4, 4, 4, 3]);
    }

    #[test]
    fn line_breaks_are_one_unit_whatever_their_spelling() {
        let r = relaxed();
        for model in EraseModel::ALL {
            for text in ["a\r\nb", "a\nb", "a\rb"] {
                assert_eq!(extra(&req(";m", text, model), &r), 2, "{model:?} {text:?}");
            }
        }
        // A naive scalar count of the raw CRLF text is one too many.
        assert_eq!("a\r\nb".chars().count() - 1, 3);
        // The plan only registers single-line text.
        assert_eq!(
            plan_undo(
                &req(";m", "a\r\nb", EraseModel::Grapheme),
                &Policy::default()
            ),
            Err(Refusal::MultiLine)
        );
        assert_eq!(
            plan_undo(&req(";m", "a\nb", EraseModel::Grapheme), &Policy::default()),
            Err(Refusal::MultiLine)
        );
    }

    #[test]
    fn refusals() {
        let p = Policy::default();
        let base = || req(";s", "text", EraseModel::Grapheme);
        let mut r = base();
        r.composition_active = true;
        assert_eq!(
            plan_undo(&r, &p),
            Err(Refusal::CompositionActive(ImeErase::Escape))
        );
        let mut r = base();
        r.cursor_placeholder = true;
        assert_eq!(plan_undo(&r, &p), Err(Refusal::CursorPlaceholder));
        let long = "x".repeat(201);
        assert_eq!(
            plan_undo(&req(";s", &long, EraseModel::Grapheme), &p),
            Err(Refusal::TooLong {
                chars: 201,
                max: 200
            })
        );
        assert!(plan_undo(&req(";s", &"x".repeat(200), EraseModel::Grapheme), &p).is_ok());
        assert_eq!(
            plan_undo(&req(";s", "", EraseModel::Grapheme), &p),
            Err(Refusal::Empty)
        );
        let mut r = base();
        r.trigger = TriggerMode::Terminator;
        assert_eq!(plan_undo(&r, &p), Err(Refusal::MissingTerminator));
        for t in ['\n', '\t'] {
            r.terminator = Some(t);
            assert_eq!(plan_undo(&r, &p), Err(Refusal::TerminatorNotUndoable(t)));
        }
        let mut r = base();
        r.terminator = Some(' ');
        assert_eq!(plan_undo(&r, &p), Err(Refusal::UnexpectedTerminator));
    }

    #[test]
    fn ime_erase_mode_never_changes_the_plan() {
        let p = Policy::default();
        for model in EraseModel::ALL {
            let mut a = req(";s", "caf\u{E9} \u{1F600}", model);
            let mut b = req(";s", "caf\u{E9} \u{1F600}", model);
            a.ime = ImeErase::Escape;
            b.ime = ImeErase::Backspace;
            assert_eq!(plan_undo(&a, &p), plan_undo(&b, &p));
            // Composition is the only IME state that matters, and it refuses in both modes.
            a.composition_active = true;
            b.composition_active = true;
            assert!(plan_undo(&a, &p).is_err() && plan_undo(&b, &p).is_err());
        }
    }

    #[test]
    fn junction_guard_cases() {
        let p = Policy::default();
        // "e" before an expansion starting with a combining acute: grapheme undo would eat the user's "e".
        let mut r = req(";s", "\u{301}x", EraseModel::Grapheme);
        r.preceding = Some("e");
        assert_eq!(plan_undo(&r, &p), Err(Refusal::JunctionMerge));
        // A stray regional indicator before a flag re-pairs the whole run.
        let mut r = req(";s", "\u{1F1F5}\u{1F1FA}x", EraseModel::Grapheme);
        r.preceding = Some("\u{1F1EF}");
        assert_eq!(plan_undo(&r, &p), Err(Refusal::JunctionMerge));
        // Unknown context: refuse conservatively for cluster models only.
        let r = req(";s", "\u{301}x", EraseModel::Grapheme);
        assert_eq!(plan_undo(&r, &p), Err(Refusal::JunctionRisk));
        assert!(plan_undo(&req(";s", "\u{301}x", EraseModel::CodePoint), &p).is_ok());
        // Re-emitted terminator behind a Prepend character fuses with it.
        let mut r = req(";s", "x\u{600}", EraseModel::Grapheme);
        r.trigger = TriggerMode::Terminator;
        r.terminator = Some(' ');
        r.consume_terminator = false;
        assert_eq!(plan_undo(&r, &p), Err(Refusal::TerminatorMerges));
    }

    // ---- simulation ---------------------------------------------------------

    #[test]
    fn planned_undo_restores_the_text_in_every_matching_model() {
        let (ok, failed, refused) = fuzz(&relaxed(), ATOMS, true, 0x5eed);
        assert_eq!(
            failed, 0,
            "plans that were accepted must restore the buffer exactly"
        );
        assert!(ok > 20_000, "corpus should mostly be accepted, got {ok}");
        assert!(refused.contains_key("JunctionMerge"), "{refused:?}");
        assert!(refused.contains_key("TerminatorMerges"), "{refused:?}");
    }

    #[test]
    fn unknown_context_guard_is_sound_for_plain_prefixes() {
        let (ok, failed, _) = fuzz(&relaxed(), PLAIN, false, 0xbeef);
        assert_eq!(failed, 0);
        assert!(ok > 20_000);
    }

    #[test]
    fn junction_hazard_is_real_without_the_guard() {
        let policy = Policy {
            check_junction: false,
            ..relaxed()
        };
        let (_, failed, _) = fuzz(&policy, ATOMS, false, 0xface);
        assert!(
            failed > 0,
            "disabling the guard must expose at least one corrupted undo"
        );
    }

    #[test]
    fn plan_formula_leaves_one_unit_when_the_terminator_is_reemitted() {
        let mut r = req(";sig", "Best regards", EraseModel::Grapheme);
        r.trigger = TriggerMode::Terminator;
        r.terminator = Some(';');
        r.consume_terminator = false;
        // B-34 as written: units - 1, retype abbreviation (+ terminator only if consumed).
        let units = unit_lens_u16(r.expansion, r.model).len();
        let as_written = UndoPlan {
            extra_backspaces: units - 1,
            retype: ";sig".into(),
            expansion_units: units,
        };
        let (got, want) = simulate(&r, &as_written, "hello ", false);
        assert_ne!(got, want);
        assert_eq!(String::from_utf16_lossy(&got), "hello B;sig");
        let corrected = plan_undo(&r, &Policy::default()).expect("plan");
        let (got, want) = simulate(&r, &corrected, "hello ", false);
        assert_eq!(got, want);
    }

    #[test]
    fn assuming_the_wrong_erase_model_corrupts_the_undo() {
        // Plan for a grapheme-deleting target, but the target deletes by scalar.
        let planned = req(";c", "cafe\u{301}", EraseModel::Grapheme);
        let plan = plan_undo(&planned, &Policy::default()).expect("plan");
        let actual_target = req(";c", "cafe\u{301}", EraseModel::CodePoint);
        let (got, want) = simulate(&actual_target, &plan, "x ", false);
        assert_ne!(got, want);
        // The reverse mistake leaves part of the expansion behind or eats user text; either way it must be probed per target.
        let planned = req(";c", "cafe\u{301}", EraseModel::CodePoint);
        let plan = plan_undo(&planned, &Policy::default()).expect("plan");
        let actual_target = req(";c", "cafe\u{301}", EraseModel::Grapheme);
        let (got, want) = simulate(&actual_target, &plan, "x ", false);
        assert_ne!(got, want);
    }

    #[test]
    fn invariant_plan_accepts_only_whitelisted_text() {
        let p = Policy::default();
        let at = |text: &str| {
            let mut r = req(";e", text, EraseModel::Grapheme);
            r.preceding = Some("x ");
            plan_undo_invariant(&r, &p)
        };
        let extra_of = |text: &str| at(text).expect("plan").extra_backspaces;
        // Expected values are the scalar count minus the user's own Backspace, counted by hand.
        assert_eq!(extra_of("Best regards"), 11);
        assert_eq!(extra_of("caf\u{E9}"), 3);
        assert_eq!(extra_of("ok \u{1F600}"), 3);
        assert_eq!(extra_of("\u{4E2D}\u{6587}"), 1);
        assert_eq!(extra_of("\u{D55C}\u{AE00}"), 1); // two precomposed Hangul syllables
        assert_eq!(
            extra_of("\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442}"),
            5 // six Cyrillic letters
        );
        // Everything that can join is refused, naming the first offending scalar.
        for (text, bad) in [
            ("cafe\u{301}", '\u{301}'),                   // combining acute
            ("\u{1100}\u{1161}", '\u{1100}'),             // Hangul Jamo L + V: one cluster
            ("a\u{1161}", '\u{1161}'),                    // lone Jamo V
            ("a\u{11A8}", '\u{11A8}'),                    // lone Jamo T
            ("\u{600}a", '\u{600}'),                      // Prepend
            ("\u{915}\u{903}", '\u{915}'),                // Devanagari: not measured
            ("\u{1F1EF}\u{1F1F5}\u{1F1FA}", '\u{1F1EF}'), // three regional indicators
            ("\u{1F468}\u{200D}\u{1F469}", '\u{1F468}'),  // ZWJ sequence
            ("a\u{200D}\u{1F600}", '\u{200D}'),           // stray ZWJ
            ("a\u{FE0F}", '\u{FE0F}'),                    // variation selector
            ("\u{1F44D}\u{1F3FD}", '\u{1F44D}'),          // skin tone
            ("1\u{FE0F}\u{20E3}", '\u{FE0F}'),            // keycap
            ("\u{A9}", '\u{A9}'),                         // (c) is Extended_Pictographic
        ] {
            assert_eq!(at(text), Err(Refusal::UnsupportedText(bad)), "{text:?}");
        }
        // Underlying refusals still win over the whitelist for text that passes it.
        assert_eq!(at(""), Err(Refusal::Empty));
        // A terminator is held to the same standard.
        let mut r = req(";e", "abc", EraseModel::Grapheme);
        r.preceding = Some("x ");
        r.trigger = TriggerMode::Terminator;
        r.terminator = Some('\u{301}');
        assert_eq!(
            plan_undo_invariant(&r, &p),
            Err(Refusal::UnsupportedText('\u{301}'))
        );
    }

    #[test]
    fn invariant_plan_needs_known_simple_context() {
        let p = Policy::default();
        let plan = |preceding: Option<&str>| {
            let mut r = req(";e", "abc", EraseModel::Grapheme);
            r.preceding = preceding;
            plan_undo_invariant(&r, &p).map(|plan| plan.extra_backspaces)
        };
        assert_eq!(plan(Some("")), Ok(2)); // empty field
        assert_eq!(plan(Some("x ")), Ok(2));
        assert_eq!(plan(Some("line\n")), Ok(2));
        assert_eq!(plan(None), Err(Refusal::UnknownContext));
        // A Prepend scalar, a CR or a Jamo before the abbreviation can fuse with the expansion.
        assert_eq!(
            plan(Some("\u{600}")),
            Err(Refusal::UnsupportedContext('\u{600}'))
        );
        assert_eq!(plan(Some("x\r")), Err(Refusal::UnsupportedContext('\r')));
        assert_eq!(
            plan(Some("\u{1100}")),
            Err(Refusal::UnsupportedContext('\u{1100}'))
        );
        assert_eq!(
            plan(Some("e\u{301}")),
            Err(Refusal::UnsupportedContext('\u{301}'))
        );
    }

    #[test]
    fn hangul_jamo_is_one_cluster_and_must_not_be_approved() {
        // U+1100 U+1161 is L + V: two scalars, ONE grapheme cluster (UAX #29 GB6).
        let jamo = "\u{1100}\u{1161}";
        let p = Policy::default();
        let mut r = req(";h", jamo, EraseModel::Grapheme);
        r.preceding = Some("x ");
        // Hand-computed. After the expansion the user presses Backspace once: a target that
        // deletes by grapheme removes both Jamo, so nothing more is needed; a target that
        // deletes by scalar needs one more press.
        assert_eq!(extra(&r, &p), 0);
        r.model = EraseModel::CodePoint;
        assert_eq!(extra(&r, &p), 1);
        // Sending the scalar count to a grapheme target eats the user's space.
        let scalar_plan = plan_undo(&r, &p).expect("plan");
        let grapheme_target = UndoRequest {
            model: EraseModel::Grapheme,
            ..r
        };
        let (got, want) = simulate(&grapheme_target, &scalar_plan, "x ", false);
        assert_eq!(String::from_utf16_lossy(&got), "x;h");
        assert_eq!(String::from_utf16_lossy(&want), "x ;h");
        // The invariant planner must therefore refuse it.
        assert_eq!(
            plan_undo_invariant(&r, &p),
            Err(Refusal::UnsupportedText('\u{1100}'))
        );
    }

    #[test]
    fn whitelist_boundaries() {
        for c in [
            'a',
            ' ',
            '~',
            '\u{A0}',
            '\u{E9}',
            '\u{FF}',
            '\u{100}',
            '\u{24F}',
            '\u{3B1}',
            '\u{416}',
            '\u{482}',
            '\u{48A}',
            '\u{3000}',
            '\u{3042}',
            '\u{30AB}',
            '\u{4E2D}',
            '\u{FF21}',
            '\u{AC00}',
            '\u{D55C}',
            '\u{D7A3}',
            '\u{1F600}',
            '\u{1F64F}',
            '\t',
            '\n',
        ] {
            assert!(is_simple_scalar(c), "{c:?} should be simple");
        }
        for c in [
            '\0',
            '\r',
            '\u{7F}',
            '\u{A9}',
            '\u{AD}',
            '\u{AE}',
            '\u{300}',
            '\u{36F}',
            '\u{483}',
            '\u{489}',
            '\u{600}',
            '\u{903}',
            '\u{915}',
            '\u{1100}',
            '\u{1161}',
            '\u{11A8}',
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{20E3}',
            '\u{3004}',
            '\u{3099}',
            '\u{309A}',
            '\u{D7B0}',
            '\u{FE0F}',
            '\u{FF9E}',
            '\u{1F1E6}',
            '\u{1F3FB}',
            '\u{1F44D}',
            '\u{1F468}',
            '\u{1F650}',
            '\u{E0020}',
        ] {
            assert!(!is_simple_scalar(c), "{c:?} must not be simple");
        }
    }

    #[test]
    fn simple_scalars_carry_no_joining_class_in_the_segmenter_tables() {
        for &(lo, hi) in SIMPLE_RANGES {
            for u in lo..=hi {
                let Some(c) = char::from_u32(u) else { continue };
                let class = gcb(c);
                let emoticon = (0x1F600..=0x1F64F).contains(&u) && class == Gcb::ExtPict;
                let syllable =
                    (0xAC00..=0xD7A3).contains(&u) && matches!(class, Gcb::Lv | Gcb::Lvt);
                assert!(
                    class == Gcb::Other || emoticon || syllable,
                    "U+{u:04X} is {class:?}"
                );
            }
        }
    }

    #[test]
    fn invariant_plan_is_correct_in_every_modern_target() {
        // Whatever the invariant planner accepts must restore the buffer in each modern model.
        let mut rng = Lcg(0x1eaf);
        let (mut accepted, mut refused) = (0, 0);
        for crlf in [false, true] {
            for (trigger, terminator, consume) in TRIGGERS {
                for _ in 0..2500 {
                    // Half of the corpus is simple text, so that plenty of plans are accepted.
                    let pool = if rng.next().is_multiple_of(2) {
                        SIMPLE_ATOMS
                    } else {
                        ATOMS
                    };
                    let prefix = rng.text(pool, 4);
                    let abbreviation = rng.text(&["a", "b", ";", "x"], 3);
                    let expansion = rng.text(pool, 6);
                    let mut r = req(&abbreviation, &expansion, EraseModel::Grapheme);
                    r.trigger = trigger;
                    r.terminator = terminator;
                    r.consume_terminator = consume;
                    r.preceding = Some(&prefix);
                    match plan_undo_invariant(&r, &relaxed()) {
                        Ok(plan) => {
                            accepted += 1;
                            // Independent of any segmenter: one unit per scalar.
                            assert_eq!(
                                plan.expansion_units,
                                normalize_line_breaks(&expansion).chars().count(),
                                "{expansion:?}"
                            );
                            for model in MODERN_MODELS {
                                let actual = UndoRequest { model, ..r };
                                let (got, want) = simulate(&actual, &plan, &prefix, crlf);
                                assert_eq!(got, want, "{model:?} {expansion:?} {trigger:?}");
                            }
                        }
                        Err(_) => refused += 1,
                    }
                }
            }
        }
        assert!(
            accepted > 2000 && refused > 2000,
            "accepted={accepted} refused={refused}"
        );
    }

    #[test]
    fn segmenter_basics() {
        assert_eq!(clusters("a\r\nb"), ["a", "\r\n", "b"]);
        assert_eq!(clusters("e\u{301}x"), ["e\u{301}", "x"]);
        assert_eq!(
            clusters("\u{1F1EF}\u{1F1F5}\u{1F1FA}"),
            ["\u{1F1EF}\u{1F1F5}", "\u{1F1FA}"]
        );
        assert_eq!(clusters("\u{1F468}\u{200D}\u{1F469}").len(), 1);
        assert_eq!(clusters("\u{1F468}\u{200D}a").len(), 2);
        assert_eq!(clusters("1\u{FE0F}\u{20E3}").len(), 1);
    }

    /// Expected cluster counts are written out from the UAX #29 rules (GB4-GB13), not
    /// computed by the segmenter under test.
    #[test]
    fn segmenter_follows_the_uax29_rules_the_first_version_skipped() {
        let n = |s: &str| clusters(s).len();
        // GB6-GB8: conjoining Hangul.
        assert_eq!(n("\u{1100}\u{1161}"), 1); // L V
        assert_eq!(n("\u{1100}\u{1161}\u{11A8}"), 1); // L V T
        assert_eq!(n("\u{1100}\u{1100}"), 1); // L L
        assert_eq!(n("\u{1100}\u{AC00}"), 1); // L LV
        assert_eq!(n("\u{1100}\u{AC01}"), 1); // L LVT
        assert_eq!(n("\u{AC00}\u{1161}"), 1); // LV V
        assert_eq!(n("\u{AC00}\u{11A8}"), 1); // LV T
        assert_eq!(n("\u{AC01}\u{11A8}"), 1); // LVT T
        assert_eq!(n("\u{AC01}\u{1161}"), 2); // LVT V: no rule joins them
        assert_eq!(n("\u{11A8}\u{1161}"), 2); // T V
        assert_eq!(n("\u{1161}\u{1100}"), 2); // V L
        assert_eq!(n("\u{D55C}\u{AE00}"), 2); // two syllables
                                              // GB9b: Prepend joins whatever follows, but not a control (GB5).
        assert_eq!(n("\u{600}a"), 1);
        assert_eq!(n("a\u{600}"), 2);
        assert_eq!(n("\u{600}\u{600}a"), 1);
        assert_eq!(n("\u{600}\n"), 2);
        // GB9, GB9a: Extend and SpacingMark attach to what precedes them.
        assert_eq!(n("a\u{301}\u{301}"), 1);
        assert_eq!(n("\u{915}\u{903}"), 1);
        assert_eq!(n("\u{903}a"), 2);
        // GB12, GB13: regional indicators pair up from the left.
        let ri = |k: usize| "\u{1F1EF}".repeat(k);
        assert_eq!(
            [1, 2, 3, 4, 5].map(|k| n(&ri(k))),
            [1, 1, 2, 2, 3] // 1 | 2 | 2+1 | 2+2 | 2+2+1
        );
        assert_eq!(n(&format!("a{}", ri(2))), 2);
        assert_eq!(n(&format!("{}a", ri(2))), 2);
        // GB11: ExtPict Extend* ZWJ x ExtPict, and nothing else.
        assert_eq!(n("\u{1F468}\u{200D}\u{1F469}"), 1);
        assert_eq!(n("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 1);
        assert_eq!(n("\u{2764}\u{FE0F}\u{200D}\u{1F525}"), 1);
        assert_eq!(n("\u{1F468}\u{1F3FD}\u{200D}\u{1F469}"), 1); // skin tone is Extend
        assert_eq!(n("a\u{200D}\u{1F469}"), 2); // ZWJ after a non-pictograph
        assert_eq!(n("\u{1F468}\u{200D}\u{200D}\u{1F469}"), 2); // second ZWJ is not Extend
        assert_eq!(n("\u{1F468}\u{1F469}"), 2); // no ZWJ
                                                // GB4, GB5: controls always separate.
        assert_eq!(n("\r\r"), 2);
        assert_eq!(n("\n\r"), 2);
    }

    #[test]
    fn cli_unescape() {
        assert_eq!(
            unescape("a\\n\\u{1F600}\\\\").as_deref(),
            Ok("a\n\u{1F600}\\")
        );
        assert!(unescape("\\q").is_err());
        assert!(unescape("\\u{110000}").is_err());
    }
}
