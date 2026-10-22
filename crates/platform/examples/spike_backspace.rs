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
// Minimal grapheme segmentation (UAX #29 subset: GB3-GB5, GB9-GB9b, GB11-GB13)
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
    use Gcb::{Control, Cr, ExtPict, Extend, Lf, Prepend, Ri, SpacingMark, Zwj};
    match (prev, cur) {
        (Cr, Lf) => false,                        // GB3
        (Control | Cr | Lf, _) => true,           // GB4
        (_, Control | Cr | Lf) => true,           // GB5
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

/// Conservative variant for the first release of B-34: the undo is only planned when
/// the answer is the same under all of `MODERN_MODELS`, so it is right in any target
/// without knowing which model that target uses. Text with combining marks, ZWJ
/// sequences, flags, skin tones or keycaps is refused (`ModelDependent`) until a
/// per-app model table exists (S-29 measurements).
fn plan_undo_invariant(req: &UndoRequest<'_>, policy: &Policy) -> Result<UndoPlan, Refusal> {
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
            "invariant (codepoint/emoji/grapheme agree): extra_backspaces={} retype={:?}",
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
    fn invariant_plan_accepts_only_text_every_modern_target_agrees_on() {
        let p = Policy::default();
        let at = |text: &str| plan_undo_invariant(&req(";e", text, EraseModel::Grapheme), &p);
        // Plain text and single-scalar emoji: same count under codepoint / emoji / grapheme.
        assert_eq!(at("Best regards").expect("plan").extra_backspaces, 11);
        assert_eq!(at("ok \u{1F600}").expect("plan").extra_backspaces, 3);
        assert_eq!(at("\u{4E2D}\u{6587}").expect("plan").extra_backspaces, 1);
        // Everything with a sequence is model dependent and refused.
        assert_eq!(
            at("cafe\u{301}"),
            Err(Refusal::ModelDependent {
                codepoint: 4,
                emoji: 4,
                grapheme: 3
            })
        );
        assert!(matches!(
            at("\u{1F468}\u{200D}\u{1F469}"),
            Err(Refusal::ModelDependent { .. })
        ));
        assert!(matches!(
            at("a\u{1F44D}\u{1F3FD}"),
            Err(Refusal::ModelDependent { .. })
        ));
        assert!(matches!(
            at("1\u{FE0F}\u{20E3}"),
            Err(Refusal::ModelDependent { .. })
        ));
        // Underlying refusals still win over the invariance check.
        assert_eq!(at(""), Err(Refusal::Empty));
    }

    #[test]
    fn invariant_plan_is_correct_in_every_modern_target() {
        // Whatever the invariant planner accepts must restore the buffer in each modern model.
        let mut rng = Lcg(0x1eaf);
        let (mut accepted, mut refused) = (0, 0);
        for crlf in [false, true] {
            for (trigger, terminator, consume) in TRIGGERS {
                for _ in 0..2500 {
                    let prefix = rng.text(ATOMS, 4);
                    let abbreviation = rng.text(&["a", "b", ";", "x"], 3);
                    let expansion = rng.text(ATOMS, 6);
                    let mut r = req(&abbreviation, &expansion, EraseModel::Grapheme);
                    r.trigger = trigger;
                    r.terminator = terminator;
                    r.consume_terminator = consume;
                    r.preceding = Some(&prefix);
                    match plan_undo_invariant(&r, &relaxed()) {
                        Ok(plan) => {
                            accepted += 1;
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
            accepted > 500 && refused > 500,
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
