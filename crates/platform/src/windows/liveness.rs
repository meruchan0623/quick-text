//! Input-source liveness (B-06, S-06), pure logic.
//!
//! Windows removes a low-level hook without telling us when a callback is
//! too slow (`LowLevelHooksTimeout`), and another component in the process
//! can take over or remove the RawInput registration (one registration per
//! usage page and process; the last caller wins). Neither event is
//! reported, so each source is checked against the other: a physical key
//! seen by one source should be seen by the other within [`MATCH_WINDOW`].
//!
//! [`CrossCheck`] only counts. It never sees key text, only virtual-key
//! codes and key-ups (to collapse autorepeat), and takes time as an
//! argument (the caller reads it from `traits::Clock`). [`HealthTable`]
//! holds the per-component health the input facade reports. Nothing here
//! makes a system call.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use crate::health::{Component, ComponentHealth, HealthState, ReasonCode};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_SHIFT,
};

/// N: how long a physical key seen by one source waits for the other.
/// 待 0.4 基线确定: both sources normally report a key within a few
/// milliseconds of each other; the window also covers a descheduled hook or
/// detector thread.
pub(super) const MATCH_WINDOW: Duration = Duration::from_millis(500);
/// K: consecutive unmatched keys before a source is declared lost.
/// 待 0.4 基线确定: high enough that one swallowed or remapped key (AltGr's
/// synthetic Ctrl, a key only one source reports) never triggers a verdict.
pub(super) const MISS_LIMIT: u32 = 5;
/// Groups of unmatched sightings kept per source. Sightings made at the
/// same instant share a group (one consumer pass stamps all it drained with
/// one instant), so this bounds the memory, not the sightings: one that
/// finds every group taken joins the newest, which then counts from its
/// instant. Its older sightings so wait longer than [`MATCH_WINDOW`], never
/// shorter: a full queue delays a verdict and never makes one (F5-2
/// review). A local capacity limit is no evidence that a source was lost.
/// The oldest group is never the one joined (the bound is above one), so
/// its instant never moves.
pub(super) const PENDING_CAP: usize = 64;
/// Unmatched sightings kept per source as evidence, however grouped: far
/// more than anyone types or clicks within [`MATCH_WINDOW`]. Beyond it the
/// newest are kept as spare only (F5-2 review 2): they still pair with the
/// other source's sightings like any other, but one that expires is
/// nothing, neither a miss nor a match. They are counted for a debug log
/// ([`CrossCheck::take_dropped`]). The oldest evidence is never dropped,
/// so it always waits out its window and yields its misses: a flood from
/// one source cannot hide the other's loss.
pub(super) const PENDING_LIMIT: usize = 4096;
/// Each verdict against a source that is not followed by an agreed key
/// doubles the misses the next verdict needs, up to `K << MAX_STRIKE_SHIFT`.
/// A source that stays one-sided for a reason a reattach cannot fix (待实测:
/// an elevated foreground window that only one source can see) is then
/// retried ever more rarely instead of every K keys.
pub(super) const MAX_STRIKE_SHIFT: u32 = 4;
/// R: downs of one key with no up from the same source in between that
/// count as one press; the next down counts again (H4). A key held down
/// thus counts once per R autorepeats on each source alike, and a source
/// that drops or never reports a key-up still reaches a verdict: K presses
/// need at most `(K - 1) * R + 1` downs, 33 with K = 5 (S-06: 50 keys).
/// 待实测: autorepeat runs at 2.5-30 downs per second, so both sources
/// count a held key every 0.3-3 s and pair well inside [`MATCH_WINDOW`] as
/// long as they report the same repeats.
pub(super) const REPEAT_RUN: u32 = 8;

/// The two sources that see physical key-downs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// The low-level keyboard hook (hardware origin only, never injected).
    Hook,
    /// The RawInput detector (`hDevice != 0` only).
    Raw,
}

/// What the cross-check concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// RawInput saw [`MISS_LIMIT`] keys in a row the hook never saw.
    HookLost,
    /// The hook saw [`MISS_LIMIT`] keys in a row RawInput never received.
    RawLost,
}

/// The hook reports sided modifier codes, RawInput the generic ones.
const fn generic(vk: u16) -> u16 {
    match vk {
        VK_LSHIFT | VK_RSHIFT => VK_SHIFT,
        VK_LCONTROL | VK_RCONTROL => VK_CONTROL,
        VK_LMENU | VK_RMENU => VK_MENU,
        other => other,
    }
}

/// Unmatched sightings one source made at one instant.
struct Group {
    at: Instant,
    /// Evidence: each that expires unmatched is a miss of the other source.
    kept: usize,
    /// Beyond [`PENDING_LIMIT`]: pairs like the kept ones, expires as
    /// nothing.
    spare: usize,
}

/// One source's unmatched sightings, oldest first, as groups of those made
/// at the same instant. [`PENDING_CAP`] bounds the groups and
/// [`PENDING_LIMIT`] the evidence (see there); no sighting is ever dropped
/// before its window ran out. Every operation is O(groups), whatever the
/// counts, and saturates rather than overflows.
#[derive(Default)]
struct Pending {
    groups: VecDeque<Group>,
    /// Kept sightings over all groups, at most [`PENDING_LIMIT`].
    kept: usize,
}
impl Pending {
    /// Adds `count` sightings made at `now`, kept up to [`PENDING_LIMIT`]
    /// and spare beyond it. Returns how many are spare.
    fn push(&mut self, count: usize, now: Instant) -> usize {
        if count == 0 {
            return 0;
        }
        let kept = count.min(PENDING_LIMIT.saturating_sub(self.kept));
        let spare = count.saturating_sub(kept);
        let full = self.groups.len() >= PENDING_CAP;
        match self.groups.back_mut() {
            Some(group) if group.at == now || full => {
                // When full, the newest group counts from now: later for
                // the sightings already in it, never earlier.
                group.at = group.at.max(now);
                group.kept = group.kept.saturating_add(kept);
                group.spare = group.spare.saturating_add(spare);
            }
            _ => self.groups.push_back(Group {
                at: now,
                kept,
                spare,
            }),
        }
        self.kept = self.kept.saturating_add(kept);
        spare
    }
    /// Takes up to `count` sightings to pair with the other source's,
    /// oldest group first; within a group the spare ones first, so the
    /// evidence is what is left to wait out its window. Returns how many
    /// it took.
    fn take(&mut self, count: usize) -> usize {
        let mut left = count;
        while left > 0 {
            let Some(group) = self.groups.front_mut() else {
                break;
            };
            let spare = group.spare.min(left);
            group.spare = group.spare.saturating_sub(spare);
            left = left.saturating_sub(spare);
            let kept = group.kept.min(left);
            group.kept = group.kept.saturating_sub(kept);
            left = left.saturating_sub(kept);
            self.kept = self.kept.saturating_sub(kept);
            if group.kept == 0 && group.spare == 0 {
                self.groups.pop_front();
            }
        }
        count.saturating_sub(left)
    }
    /// Removes the groups made [`MATCH_WINDOW`] or more before `now` and
    /// returns how many kept sightings they held: the misses. Their spare
    /// sightings expire as nothing.
    fn expire(&mut self, now: Instant) -> usize {
        let mut expired = 0_usize;
        while let Some(group) = self.groups.front() {
            if now.saturating_duration_since(group.at) < MATCH_WINDOW {
                break;
            }
            expired = expired.saturating_add(group.kept);
            self.groups.pop_front();
        }
        self.kept = self.kept.saturating_sub(expired);
        expired
    }
    /// When the oldest sighting was made, if any.
    fn oldest(&self) -> Option<Instant> {
        self.groups.front().map(|group| group.at)
    }
    fn clear(&mut self) {
        self.groups.clear();
        self.kept = 0;
    }
    #[cfg(test)]
    fn spare(&self) -> usize {
        self.groups
            .iter()
            .fold(0, |sum, group| sum.saturating_add(group.spare))
    }
}

#[derive(Default)]
struct Side {
    /// This source's unmatched sightings.
    pending: Pending,
    /// The key this source last reported down and has not reported up
    /// since: another down of it is autorepeat.
    held: Option<u16>,
    /// Downs of `held` merged since the last one that counted.
    merged: u32,
    /// Keys the other source saw and this one missed, in a row.
    misses: u32,
    /// Verdicts against this source since both sources last agreed on a key.
    strikes: u32,
}
impl Side {
    /// Misses the next verdict against this source needs.
    fn limit(&self) -> u32 {
        MISS_LIMIT << self.strikes.min(MAX_STRIKE_SHIFT)
    }
}

/// Counts physical key presses from both sources and pairs them up in
/// order.
///
/// Matching is by count, not by code: the two sources disagree on some
/// codes (sided modifiers, numpad, AltGr) but not on how many keys went
/// down. Each source's key-ups are tracked separately: a down after that
/// source reported the key up (or after a different key) is a new press,
/// and downs with no up in between are autorepeat, merged on both sides
/// alike up to [`REPEAT_RUN`] at a time. Fifty presses of the same key are
/// fifty presses, and a lost key-up only delays a verdict (H4; 待实测:
/// RawInput and hook autorepeat behaviour).
///
/// Presses are paired a consumer pass at a time ([`CrossCheck::pair_up`]),
/// in the order they were seen: first with what the other source saw
/// before, oldest first, then with the other source's in the same pass.
/// With both sources healthy (each sees every press within
/// [`MATCH_WINDOW`] of the other) nothing ever expires unmatched, however
/// the presses are split over passes and however many there are: the
/// group and evidence bounds only make sightings wait longer or expire as
/// nothing. With one source lost (it sees nothing), every kept sighting of
/// the other expires as a miss, and the oldest one is never dropped or
/// moved: the K-th unmatched sighting gives the verdict at most
/// [`MATCH_WINDOW`] after it was seen, at the first expiry after that,
/// however hard the other source floods (one window more only when all
/// [`PENDING_CAP`] groups were taken when it came).
#[derive(Default)]
pub(super) struct CrossCheck {
    hook: Side,
    raw: Side,
    /// Sightings kept as spare at [`PENDING_LIMIT`] since
    /// [`CrossCheck::take_dropped`].
    dropped: usize,
}
impl CrossCheck {
    fn side(&mut self, source: Source) -> &mut Side {
        match source {
            Source::Hook => &mut self.hook,
            Source::Raw => &mut self.raw,
        }
    }
    fn other(&mut self, source: Source) -> &mut Side {
        match source {
            Source::Hook => &mut self.raw,
            Source::Raw => &mut self.hook,
        }
    }
    /// Whether a key-down `source` saw is a press, to be counted for
    /// [`CrossCheck::pair_up`]; false when it is merged as autorepeat.
    pub(super) fn counts(&mut self, source: Source, vk: u16) -> bool {
        let vk = generic(vk);
        let side = self.side(source);
        if side.held == Some(vk) {
            side.merged = side.merged.saturating_add(1);
            if side.merged < REPEAT_RUN {
                return false;
            }
        }
        side.held = Some(vk);
        side.merged = 0;
        true
    }
    /// Records the presses one consumer pass brought, `hook` and `raw` of
    /// them, all seen at `now` (a pass stamps all it drained with one
    /// instant). Each source's presses first pair with what the other saw
    /// before, oldest first; what is left of both pairs within the pass
    /// (same instant, same window), and only the rest waits, however many
    /// wait already: only an expired sighting is a miss. Returns how many
    /// pairs it made. O(groups), whatever the counts.
    pub(super) fn pair_up(&mut self, hook: usize, raw: usize, now: Instant) -> usize {
        let hook_old = self.raw.pending.take(hook);
        let raw_old = self.hook.pending.take(raw);
        let (hook, raw) = (hook.saturating_sub(hook_old), raw.saturating_sub(raw_old));
        let within = hook.min(raw);
        let paired = hook_old.saturating_add(raw_old).saturating_add(within);
        if paired > 0 {
            // Both sources are alive right now.
            for side in [&mut self.hook, &mut self.raw] {
                side.misses = 0;
                side.strikes = 0;
            }
        }
        // At most one of the two is not zero: a source whose presses are
        // left over took all the other had waiting.
        let spare = self
            .hook
            .pending
            .push(hook.saturating_sub(within), now)
            .saturating_add(self.raw.pending.push(raw.saturating_sub(within), now));
        self.dropped = self.dropped.saturating_add(spare);
        paired
    }
    /// One key-down `source` saw at `now`, as its own pass. Returns true
    /// when it paired with a pending key from the other source; false when
    /// it was merged as autorepeat or is waiting for the other source.
    #[cfg(test)]
    pub(super) fn saw(&mut self, source: Source, vk: u16, now: Instant) -> bool {
        if !self.counts(source, vk) {
            return false;
        }
        let (hook, raw) = match source {
            Source::Hook => (1, 0),
            Source::Raw => (0, 1),
        };
        self.pair_up(hook, raw, now) > 0
    }
    /// Sightings kept as spare at [`PENDING_LIMIT`] since the last call,
    /// for a debug log.
    pub(super) fn take_dropped(&mut self) -> usize {
        std::mem::take(&mut self.dropped)
    }
    /// Records a key-up `source` saw: its next down of that key is a new
    /// press, not autorepeat.
    pub(super) fn released(&mut self, source: Source, vk: u16) {
        let side = self.side(source);
        if side.held == Some(generic(vk)) {
            side.held = None;
            side.merged = 0;
        }
    }
    /// Expires sightings older than [`MATCH_WINDOW`] and returns a verdict
    /// once a source missed [`MISS_LIMIT`] keys in a row (more after earlier
    /// verdicts, see [`MAX_STRIKE_SHIFT`]). Each verdict is given once; call
    /// again until it returns `None`.
    pub(super) fn expire(&mut self, now: Instant) -> Option<Verdict> {
        for source in [Source::Hook, Source::Raw] {
            let expired = self.side(source).pending.expire(now);
            let other = self.other(source);
            other.misses = other
                .misses
                .saturating_add(u32::try_from(expired).unwrap_or(u32::MAX));
        }
        for (source, verdict) in [
            (Source::Hook, Verdict::HookLost),
            (Source::Raw, Verdict::RawLost),
        ] {
            let side = self.side(source);
            if side.misses >= side.limit() {
                side.strikes = side.strikes.saturating_add(1);
                side.misses = 0;
                return Some(verdict);
            }
        }
        None
    }
    /// Whether `source` missed at least one key the other source saw (its
    /// sighting expired unmatched) since the two last agreed on a key: the
    /// evidence of a delivery gap, before it is enough for a verdict
    /// (review A5). A key still inside [`MATCH_WINDOW`] is not a miss yet.
    /// A verdict starts the count again, as does [`CrossCheck::forget`].
    pub(super) fn missed(&self, source: Source) -> bool {
        match source {
            Source::Hook => self.hook.misses > 0,
            Source::Raw => self.raw.misses > 0,
        }
    }
    /// When the oldest pending sighting expires, if any.
    pub(super) fn next_due(&self) -> Option<Instant> {
        [&self.hook, &self.raw]
            .into_iter()
            .filter_map(|side| side.pending.oldest())
            .min()
            .and_then(|seen| seen.checked_add(MATCH_WINDOW))
    }
    /// Unmatched keys `source` saw, waiting for the other source's, kept as
    /// evidence.
    #[cfg(test)]
    pub(super) fn pending(&self, source: Source) -> usize {
        match source {
            Source::Hook => self.hook.pending.kept,
            Source::Raw => self.raw.pending.kept,
        }
    }
    /// Unmatched keys `source` saw beyond [`PENDING_LIMIT`], spare.
    #[cfg(test)]
    pub(super) fn spare(&self, source: Source) -> usize {
        match source {
            Source::Hook => self.hook.pending.spare(),
            Source::Raw => self.raw.pending.spare(),
        }
    }
    /// Forgets pending keys, misses and held keys after a source was
    /// reattached: what the old source missed (a key-up among it) says
    /// nothing about the new one. Strikes stay until both sources agree on
    /// a key.
    pub(super) fn forget(&mut self) {
        for side in [&mut self.hook, &mut self.raw] {
            side.pending.clear();
            side.misses = 0;
            side.held = None;
            side.merged = 0;
        }
    }
}

/// The components the input facade reports, in report order.
pub(super) const INPUT_COMPONENTS: [Component; 4] = [
    Component::KeyboardHook,
    Component::MouseHook,
    Component::FocusEvents,
    Component::RawInput,
];
const fn slot(component: Component) -> Option<usize> {
    match component {
        Component::KeyboardHook => Some(0),
        Component::MouseHook => Some(1),
        Component::FocusEvents => Some(2),
        Component::RawInput => Some(3),
        _ => None,
    }
}
const fn starting(component: Component) -> ComponentHealth {
    ComponentHealth {
        component,
        state: HealthState::Starting,
        reason: None,
        since_ms: 0,
    }
}

/// Health of the input components. `since_ms` moves only when the state
/// changes; 0 means "never set" and is reported as the observation time.
pub(super) struct HealthTable([ComponentHealth; 4]);
impl HealthTable {
    pub(super) const fn new() -> Self {
        Self([
            starting(INPUT_COMPONENTS[0]),
            starting(INPUT_COMPONENTS[1]),
            starting(INPUT_COMPONENTS[2]),
            starting(INPUT_COMPONENTS[3]),
        ])
    }
    pub(super) fn get(&self, component: Component) -> Option<&ComponentHealth> {
        self.0.get(slot(component)?)
    }
    /// Sets one component; returns true when its state or reason changed.
    /// Components outside the input set are ignored.
    pub(super) fn set(
        &mut self,
        component: Component,
        state: HealthState,
        reason: Option<ReasonCode>,
        now_ms: i64,
    ) -> bool {
        let Some(entry) = slot(component).and_then(|slot| self.0.get_mut(slot)) else {
            return false;
        };
        let changed = entry.state != state || entry.reason != reason;
        if entry.state != state {
            entry.since_ms = now_ms;
        }
        entry.state = state;
        entry.reason = reason;
        changed
    }
    pub(super) fn snapshot(&self, now_ms: i64) -> Vec<ComponentHealth> {
        self.0
            .iter()
            .map(|entry| ComponentHealth {
                since_ms: if entry.since_ms == 0 {
                    now_ms
                } else {
                    entry.since_ms
                },
                ..entry.clone()
            })
            .collect()
    }
}

/// Log names (the snake_case serde names) of the input components.
pub(super) const fn component_name(component: Component) -> &'static str {
    match component {
        Component::KeyboardHook => "keyboard_hook",
        Component::MouseHook => "mouse_hook",
        Component::FocusEvents => "focus_events",
        Component::RawInput => "raw_input",
        _ => "other",
    }
}
/// Log names of the reason codes the input facade uses; `None` is `ok`.
pub(super) const fn reason_name(reason: Option<ReasonCode>) -> &'static str {
    match reason {
        None | Some(ReasonCode::Ok) => "ok",
        Some(ReasonCode::HookMissing) => "hook_missing",
        Some(ReasonCode::RawInputMissing) => "raw_input_missing",
        Some(ReasonCode::HookInstallFailed) => "hook_install_failed",
        Some(_) => "other",
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    const A: u16 = 0x41;
    const B: u16 = 0x42;
    /// A code `one_sided` never uses.
    const C: u16 = 0x43;
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    /// `count` keys seen by `source` only, `gap` apart, each differing from
    /// the one before (so none is skipped as a repeat).
    fn one_sided(
        check: &mut CrossCheck,
        source: Source,
        start: Instant,
        count: u16,
        gap: Duration,
    ) -> Instant {
        let mut now = start;
        for _ in 0..count {
            let vk = if check.side(source).held == Some(A) {
                B
            } else {
                A
            };
            check.saw(source, vk, now);
            now += gap;
        }
        now
    }

    #[test]
    fn keys_seen_by_both_sources_pair_up_in_either_order() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        assert!(!check.saw(Source::Raw, A, t));
        assert!(check.saw(Source::Hook, A, t + ms(3)));
        assert!(!check.saw(Source::Hook, B, t + ms(10)));
        assert!(check.saw(Source::Raw, B, t + ms(11)));
        assert_eq!(check.next_due(), None);
        assert_eq!(check.expire(t + Duration::from_secs(60)), None);
    }

    #[test]
    fn k_unmatched_raw_keys_declare_the_hook_lost_then_back_off() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let end = one_sided(&mut check, Source::Raw, t, MISS_LIMIT as u16, ms(50));
        assert_eq!(check.pending(Source::Raw), MISS_LIMIT as usize);
        assert_eq!(check.next_due(), Some(t + MATCH_WINDOW));
        assert_eq!(check.expire(t + MATCH_WINDOW - ms(1)), None, "not yet");
        let last = end - ms(50);
        assert_eq!(check.expire(last + MATCH_WINDOW), Some(Verdict::HookLost));
        assert_eq!(check.expire(last + MATCH_WINDOW), None, "given once");
        // Still one-sided: the next verdicts need 2K, then 4K misses.
        let mut now = end + MATCH_WINDOW;
        for limit in [2 * MISS_LIMIT, 4 * MISS_LIMIT] {
            let end = one_sided(&mut check, Source::Raw, now, limit as u16 - 1, ms(10));
            assert_eq!(check.expire(end + MATCH_WINDOW), None, "{limit}");
            let end = one_sided(&mut check, Source::Raw, end + MATCH_WINDOW, 1, ms(10));
            assert_eq!(
                check.expire(end + MATCH_WINDOW),
                Some(Verdict::HookLost),
                "{limit}"
            );
            now = end + MATCH_WINDOW;
        }
        // One key both sources agree on resets the backoff.
        assert!(!check.saw(Source::Raw, C, now));
        assert!(check.saw(Source::Hook, C, now));
        let end = one_sided(
            &mut check,
            Source::Raw,
            now + ms(600),
            MISS_LIMIT as u16,
            ms(50),
        );
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::HookLost));
    }

    #[test]
    fn the_backoff_is_capped() {
        let mut check = CrossCheck::default();
        check.raw.strikes = u32::MAX;
        assert_eq!(check.raw.limit(), MISS_LIMIT << MAX_STRIKE_SHIFT);
        let t = Instant::now();
        let end = one_sided(
            &mut check,
            Source::Hook,
            t,
            (MISS_LIMIT << MAX_STRIKE_SHIFT) as u16,
            ms(1),
        );
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::RawLost));
        assert_eq!(check.raw.strikes, u32::MAX, "saturates");
    }

    #[test]
    fn k_unmatched_hook_keys_declare_raw_input_lost() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let end = one_sided(&mut check, Source::Hook, t, MISS_LIMIT as u16, ms(10));
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::RawLost));
    }

    #[test]
    fn a_match_resets_the_miss_count() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let end = one_sided(&mut check, Source::Raw, t, MISS_LIMIT as u16 - 1, ms(10));
        assert_eq!(check.expire(end + MATCH_WINDOW), None);
        let now = end + MATCH_WINDOW;
        assert!(!check.saw(Source::Raw, C, now));
        assert!(check.saw(Source::Hook, C, now));
        let end = one_sided(&mut check, Source::Raw, now, MISS_LIMIT as u16 - 1, ms(10));
        assert_eq!(check.expire(end + MATCH_WINDOW), None, "count restarted");
    }

    /// Review A5: `missed` reports a gap from the first expired key, long
    /// before a verdict; an agreed key, a verdict or `forget` clear it.
    #[test]
    fn missed_reports_a_gap_from_the_first_expired_key() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        assert!(!check.missed(Source::Hook) && !check.missed(Source::Raw));
        let end = one_sided(&mut check, Source::Raw, t, 1, ms(10));
        assert_eq!(check.expire(end), None);
        assert!(!check.missed(Source::Hook), "still inside the window");
        assert_eq!(check.expire(t + MATCH_WINDOW), None, "no verdict at one");
        assert!(check.missed(Source::Hook) && !check.missed(Source::Raw));
        // An agreed key clears it.
        let now = t + MATCH_WINDOW;
        assert!(!check.saw(Source::Raw, C, now));
        assert!(check.saw(Source::Hook, C, now));
        assert!(!check.missed(Source::Hook));
        // So does forget (a reattach).
        let end = one_sided(&mut check, Source::Hook, now, 1, ms(10));
        assert_eq!(check.expire(end + MATCH_WINDOW), None);
        assert!(check.missed(Source::Raw));
        check.forget();
        assert!(!check.missed(Source::Raw));
        // And a verdict starts the count again.
        let end = one_sided(&mut check, Source::Raw, end, MISS_LIMIT as u16, ms(10));
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::HookLost));
        assert!(!check.missed(Source::Hook));
    }

    #[test]
    fn repeats_and_sided_modifiers_count_once_on_each_side() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        // Hook: LShift, autorepeated A, A again, then B. RawInput: generic
        // Shift and one A, then B.
        for vk in [VK_LSHIFT, A, A, A, B] {
            check.saw(Source::Hook, vk, t);
        }
        for vk in [VK_SHIFT, A, B] {
            check.saw(Source::Raw, vk, t);
        }
        assert_eq!(check.pending(Source::Hook), 0);
        assert_eq!(check.pending(Source::Raw), 0);
        assert_eq!(check.expire(t + Duration::from_secs(5)), None);
    }

    /// `count` presses of `vk` seen by `source` only, `gap` apart; each
    /// down is followed by its up when `ups`.
    fn same_key(
        check: &mut CrossCheck,
        source: Source,
        vk: u16,
        start: Instant,
        count: u32,
        gap: Duration,
        ups: bool,
    ) -> Instant {
        let mut now = start;
        for _ in 0..count {
            check.saw(source, vk, now);
            if ups {
                check.released(source, vk);
            }
            now += gap;
        }
        now
    }

    /// H4: fifty presses of one key (Backspace, Enter) that only one
    /// source sees reach a verdict after K of them, like K different keys.
    #[test]
    fn presses_of_the_same_key_each_count() {
        for (source, verdict) in [
            (Source::Raw, Verdict::HookLost),
            (Source::Hook, Verdict::RawLost),
        ] {
            let mut check = CrossCheck::default();
            let t = Instant::now();
            let vk = 0x08;
            let end = same_key(&mut check, source, vk, t, MISS_LIMIT - 1, ms(50), true);
            assert_eq!(check.pending(source), MISS_LIMIT as usize - 1);
            assert_eq!(check.expire(end + MATCH_WINDOW), None, "{source:?}");
            let end = same_key(&mut check, source, vk, end, 1, ms(50), true);
            assert_eq!(
                check.expire(end + MATCH_WINDOW),
                Some(verdict),
                "{source:?}"
            );
        }
    }

    /// H4: a source that never reports a key-up (or lost them) still
    /// reaches a verdict on sustained input of one key: every
    /// [`REPEAT_RUN`] downs count as one press, (K - 1) * R + 1 downs make
    /// K presses.
    #[test]
    fn downs_without_ups_merge_only_up_to_a_bound() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let downs = (MISS_LIMIT - 1) * REPEAT_RUN;
        let end = same_key(&mut check, Source::Raw, A, t, downs, ms(30), false);
        assert_eq!(check.pending(Source::Raw), MISS_LIMIT as usize - 1);
        assert_eq!(check.expire(end + MATCH_WINDOW), None);
        let end = same_key(&mut check, Source::Raw, A, end, 1, ms(30), false);
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::HookLost));
        assert!(downs < 50, "S-06: within 50 keys");
    }

    /// A key held down with autorepeat on both sources: the downs merge the
    /// same way on each side and pair up, however long it is held; its up
    /// on either side ends the run there only.
    #[test]
    fn a_held_key_both_sources_see_pairs_up_for_as_long_as_it_is_held() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let mut now = t;
        for _ in 0..50 * REPEAT_RUN {
            check.saw(Source::Hook, VK_LSHIFT, now);
            check.saw(Source::Raw, VK_SHIFT, now);
            now += ms(33);
            assert_eq!(check.expire(now), None);
        }
        assert_eq!(check.pending(Source::Hook), 0);
        assert_eq!(check.pending(Source::Raw), 0);
        check.released(Source::Hook, VK_LSHIFT);
        check.released(Source::Raw, VK_SHIFT);
        // Released on both sides: the next press counts on both.
        assert!(!check.saw(Source::Hook, VK_RSHIFT, now));
        assert!(check.saw(Source::Raw, VK_SHIFT, now));
        // An up of another key does not end a run.
        check.released(Source::Raw, A);
        assert!(!check.saw(Source::Raw, VK_SHIFT, now));
        assert_eq!(check.pending(Source::Raw), 0, "still merged");
    }

    #[test]
    fn forget_ends_runs_so_the_next_down_counts_on_both_sides() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        check.saw(Source::Hook, A, t);
        assert!(check.saw(Source::Raw, A, t));
        check.forget();
        // The ups went missing while a source was being reattached.
        assert!(!check.saw(Source::Hook, A, t));
        assert!(check.saw(Source::Raw, A, t));
    }

    /// F5-2 review: more unmatched sightings than [`PENDING_CAP`] groups,
    /// each at its own instant, are all kept and none is a miss before its
    /// window ran out; the ones that found every group taken wait from the
    /// last of them (longer, never shorter).
    #[test]
    fn a_full_pending_queue_waits_for_the_window() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let count = PENDING_CAP + MISS_LIMIT as usize;
        let end = one_sided(&mut check, Source::Hook, t, count as u16, ms(1));
        let last = end - ms(1);
        assert_eq!(check.pending(Source::Hook), count, "none dropped");
        assert_eq!(check.take_dropped(), 0);
        assert_eq!(check.expire(t), None, "no miss before the window");
        assert!(!check.missed(Source::Raw));
        assert_eq!(check.next_due(), Some(t + MATCH_WINDOW));
        assert_eq!(check.expire(t + MATCH_WINDOW - ms(1)), None);
        assert!(!check.missed(Source::Raw));
        // The groups before the newest expire on time...
        let newest = t + ms(PENDING_CAP as u64 - 1);
        assert_eq!(
            check.expire(newest - ms(1) + MATCH_WINDOW),
            Some(Verdict::RawLost)
        );
        // ...the newest, joined by the rest, waits from the last of them.
        let left = count - (PENDING_CAP - 1);
        assert_eq!(check.pending(Source::Hook), left);
        assert_eq!(check.next_due(), Some(last + MATCH_WINDOW));
        assert_eq!(check.expire(last + MATCH_WINDOW - ms(1)), None);
        assert_eq!(check.pending(Source::Hook), left);
        assert_eq!(check.expire(last + MATCH_WINDOW), None, "2K after a strike");
        assert_eq!(check.pending(Source::Hook), 0);
    }

    /// F5-2 review: a burst larger than [`PENDING_CAP`] that one source
    /// reports in full before the other reports any of it (one consumer
    /// pass each) pairs up whichever source comes first.
    #[test]
    fn a_burst_beyond_the_group_bound_pairs_up_in_either_order() {
        for (first, second) in [(Source::Raw, Source::Hook), (Source::Hook, Source::Raw)] {
            for gap in [Duration::ZERO, ms(1)] {
                let mut check = CrossCheck::default();
                let t = Instant::now();
                let count = PENDING_CAP + 5;
                let end = one_sided(&mut check, first, t, count as u16, gap);
                assert_eq!(check.expire(end), None);
                assert_eq!(check.pending(first), count, "{first:?} {gap:?}");
                let mut now = end + ms(100);
                for index in 0..count {
                    let vk = if index % 2 == 0 { A } else { B };
                    assert!(check.saw(second, vk, now), "{first:?} {gap:?} {index}");
                    now += gap;
                }
                assert_eq!(check.pending(first), 0);
                assert_eq!(check.pending(second), 0);
                assert_eq!(check.expire(now + Duration::from_secs(60)), None);
                assert!(!check.missed(Source::Hook) && !check.missed(Source::Raw));
                assert_eq!(check.take_dropped(), 0);
            }
        }
    }

    /// F5-2 review 2: beyond [`PENDING_LIMIT`] the newest sightings are
    /// spare (counted for a debug log); the oldest evidence stays, at its
    /// instant. The other source's presses take the spare ones of a group
    /// first, so what is left to expire is evidence.
    #[test]
    fn beyond_the_limit_the_newest_sightings_are_spare_and_the_oldest_stay() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        one_sided(
            &mut check,
            Source::Raw,
            t,
            (PENDING_LIMIT + 3) as u16,
            Duration::ZERO,
        );
        assert_eq!(check.pending(Source::Raw), PENDING_LIMIT);
        assert_eq!(check.spare(Source::Raw), 3);
        assert_eq!(check.take_dropped(), 3);
        assert_eq!(check.take_dropped(), 0, "taken");
        assert_eq!(check.expire(t), None);
        assert!(!check.missed(Source::Hook), "spare is not missed");
        // More of them later move nothing: the oldest still counts from t.
        check.pair_up(0, 2, t + ms(100));
        assert_eq!(check.spare(Source::Raw), 5);
        assert_eq!(check.next_due(), Some(t + MATCH_WINDOW));
        // The hook's presses pair with the oldest group first, its spare
        // sightings before its kept ones.
        assert!(check.saw(Source::Hook, C, t + ms(200)));
        assert_eq!(check.pair_up(3, 0, t + ms(200)), 3);
        assert_eq!(check.spare(Source::Raw), 2, "the newer group's");
        assert_eq!(check.pending(Source::Raw), PENDING_LIMIT - 1);
        assert_eq!(
            check.expire(t + MATCH_WINDOW),
            Some(Verdict::HookLost),
            "the evidence was kept"
        );
        assert_eq!(check.expire(t + ms(100) + MATCH_WINDOW), None);
        assert_eq!(check.spare(Source::Raw), 0, "expired as nothing");
    }

    /// How a test brings presses of one source to a consumer pass: clicks
    /// as their count, keys through [`CrossCheck::counts`] (A and B in
    /// turn, each released), as the consumer does.
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Click,
        Key,
    }
    impl Kind {
        fn presses(self, check: &mut CrossCheck, source: Source, count: usize) -> usize {
            match self {
                Kind::Click => count,
                Kind::Key => (0..count).fold(0, |counted, index| {
                    let vk = if index % 2 == 0 { A } else { B };
                    let counts = check.counts(source, vk);
                    check.released(source, vk);
                    counted + usize::from(counts)
                }),
            }
        }
    }
    /// One consumer pass at `now` bringing `hook` and `raw` presses of
    /// `kind`, then the expiry the consumer runs after it. Its verdicts.
    fn pass(
        check: &mut CrossCheck,
        kind: Kind,
        (hook, raw): (usize, usize),
        now: Instant,
    ) -> Vec<Verdict> {
        let hook = kind.presses(check, Source::Hook, hook);
        let raw = kind.presses(check, Source::Raw, raw);
        check.pair_up(hook, raw, now);
        std::iter::from_fn(|| check.expire(now)).collect()
    }
    /// (`first`'s, the other's) as (hook's, raw's).
    fn sided(first: Source, (mine, theirs): (usize, usize)) -> (usize, usize) {
        match first {
            Source::Hook => (mine, theirs),
            Source::Raw => (theirs, mine),
        }
    }
    fn no_miss(check: &CrossCheck) -> bool {
        !check.missed(Source::Hook) && !check.missed(Source::Raw)
    }

    /// F5-2 review 2, I1: the two sources' presses of one pass pair with
    /// each other before either waits, however many (from one to many
    /// times `PENDING_LIMIT`, up to the counts' bound): none waits, none is
    /// spare, no miss, no verdict, whichever source the pass drained
    /// first. Keys alike.
    #[test]
    fn a_balanced_pass_of_any_size_pairs_within_itself() {
        let t = Instant::now();
        for kind in [Kind::Click, Kind::Key] {
            for count in [1, PENDING_LIMIT + MISS_LIMIT as usize, 10 * PENDING_LIMIT] {
                let mut check = CrossCheck::default();
                let verdicts = pass(&mut check, kind, (count, count), t);
                assert!(verdicts.is_empty(), "{kind:?} {count}: {verdicts:?}");
                assert_eq!(check.pending(Source::Hook), 0);
                assert_eq!(check.pending(Source::Raw), 0);
                assert_eq!(check.take_dropped(), 0, "{kind:?} {count}: none spare");
                assert_eq!(check.next_due(), None);
                assert_eq!(check.expire(t + Duration::from_secs(60)), None);
                assert!(no_miss(&check));
            }
        }
        let mut check = CrossCheck::default();
        assert_eq!(check.pair_up(usize::MAX, usize::MAX, t), usize::MAX);
        assert_eq!(check.take_dropped(), 0);
        assert_eq!(check.pair_up(usize::MAX, 0, t), 0, "saturates");
        assert_eq!(check.pending(Source::Hook), PENDING_LIMIT);
        assert_eq!(check.pair_up(0, usize::MAX, t), usize::MAX);
        assert_eq!(check.pending(Source::Hook), 0);
        assert_eq!(check.expire(t + MATCH_WINDOW), None);
        assert!(no_miss(&check));
    }

    /// F5-2 review 2, I1: a healthy burst of at least `PENDING_LIMIT + K`
    /// presses, split over passes in many ways (one source's all before
    /// any of the other's, over many passes, interleaved, both in one pass
    /// and then one alone), either source first, all inside the window,
    /// pairs up: what is beyond the limit waits as spare and still pairs.
    /// No miss and no verdict at any pass, nor once every window ran out.
    /// Keys alike.
    #[test]
    fn a_healthy_burst_beyond_the_limit_split_over_passes_pairs_up() {
        let big = PENDING_LIMIT + MISS_LIMIT as usize;
        let mut spread = vec![(big, 0)];
        spread.extend(std::iter::repeat_n((0, big / 10), 10));
        spread.push((0, big % 10));
        let schedules = [
            vec![(big, 0), (0, big)],
            spread,
            vec![(0, 0), (3000, 0), (0, 1000), (3000, 0), (0, 5000)],
            vec![
                (3 * PENDING_LIMIT, 0),
                (0, PENDING_LIMIT),
                (0, PENDING_LIMIT),
                (0, PENDING_LIMIT),
            ],
            vec![(big, MISS_LIMIT as usize), (0, big - MISS_LIMIT as usize)],
            vec![(big, 0), (big, big), (0, big)],
        ];
        for kind in [Kind::Click, Kind::Key] {
            for first in [Source::Hook, Source::Raw] {
                for (index, schedule) in schedules.iter().enumerate() {
                    let mut check = CrossCheck::default();
                    let t = Instant::now();
                    let mut now = t;
                    for &counts in schedule {
                        let verdicts = pass(&mut check, kind, sided(first, counts), now);
                        assert!(verdicts.is_empty(), "{kind:?} {first:?} {index}");
                        assert!(no_miss(&check), "{kind:?} {first:?} {index}");
                        now += ms(10);
                    }
                    assert!(now - t < MATCH_WINDOW, "inside the window");
                    for source in [Source::Hook, Source::Raw] {
                        assert_eq!(check.pending(source), 0, "{kind:?} {first:?} {index}");
                        assert_eq!(check.spare(source), 0, "{kind:?} {first:?} {index}");
                    }
                    assert_eq!(check.expire(t + Duration::from_secs(60)), None);
                    assert!(no_miss(&check));
                }
            }
        }
    }

    /// F5-2 review 2: spare sightings pair only within their window. One
    /// that expires unmatched is nothing: no miss of the other source then,
    /// and no match for that source's press after the window.
    #[test]
    fn spare_sightings_expire_as_nothing() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let k = MISS_LIMIT as usize;
        check.pair_up(PENDING_LIMIT, 0, t);
        check.pair_up(k, 0, t + ms(10));
        assert_eq!(check.spare(Source::Hook), k);
        assert_eq!(check.take_dropped(), k);
        assert_eq!(check.pair_up(0, PENDING_LIMIT, t + ms(20)), PENDING_LIMIT);
        assert_eq!(check.expire(t + ms(10) + MATCH_WINDOW), None);
        assert!(no_miss(&check), "no miss");
        assert_eq!(check.spare(Source::Hook), 0);
        assert_eq!(check.next_due(), None);
        // RawInput's presses after the window find nothing to pair with:
        // they wait, and are the hook's misses.
        let late = t + ms(10) + MATCH_WINDOW;
        assert_eq!(check.pair_up(0, k, late), 0);
        assert_eq!(check.expire(late + MATCH_WINDOW), Some(Verdict::HookLost));
    }

    /// F5-2 review 2, I2: one source sees nothing while the other floods
    /// without pause far past `PENDING_LIMIT` (512 presses a pass, a pass
    /// every 50 ms, expiring after each as the consumer does). The oldest
    /// evidence is never dropped or moved, so the verdict comes at the
    /// first expiry of the K-th unmatched press, one window after the first
    /// pass, and keeps coming (backed off) while the flood goes on. Keys
    /// alike, either source lost.
    #[test]
    fn a_lost_source_is_found_within_a_window_under_a_non_stop_flood() {
        let tick = ms(50);
        for kind in [Kind::Click, Kind::Key] {
            for (lost, verdict) in [
                (Source::Hook, Verdict::HookLost),
                (Source::Raw, Verdict::RawLost),
            ] {
                let flooding = match lost {
                    Source::Hook => Source::Raw,
                    Source::Raw => Source::Hook,
                };
                let mut check = CrossCheck::default();
                let t = Instant::now();
                let mut found = Vec::new();
                for index in 0..60 {
                    let now = t + tick * index;
                    for given in pass(&mut check, kind, sided(flooding, (512, 0)), now) {
                        assert_eq!(given, verdict, "{kind:?} {lost:?}");
                        found.push(now - t);
                    }
                    assert!(check.pending(flooding) <= PENDING_LIMIT);
                }
                assert!(check.take_dropped() > 0, "the flood overflowed the limit");
                assert_eq!(found.first(), Some(&MATCH_WINDOW), "{kind:?} {lost:?}");
                assert!(found.len() > 1, "{kind:?} {lost:?}: {found:?}");
            }
        }
    }

    /// F5-2 review 2, I2 with spare sightings about: both sources flood
    /// healthily past the limit (a pass brings one source's presses, the
    /// next the other's), then one goes silent while the other keeps on.
    /// What the silent one saw before still pairs; the K-th unmatched
    /// press gives the verdict at its first expiry, one window after it.
    #[test]
    fn a_source_lost_in_the_middle_of_a_flood_is_found_within_a_window() {
        let tick = ms(25);
        let flood = PENDING_LIMIT + 1000;
        for kind in [Kind::Click, Kind::Key] {
            for lost in [Source::Hook, Source::Raw] {
                let mut check = CrossCheck::default();
                let t = Instant::now();
                let mut found = None;
                for index in 0..80_u32 {
                    let now = t + tick * index;
                    // Until pass 20 the silent source brings the even
                    // passes and the other the odd ones; then only the
                    // other, every pass.
                    let counts = match (index < 20, index % 2 == 0) {
                        (true, true) => (flood, 0),
                        _ => (0, flood),
                    };
                    let verdicts = pass(&mut check, kind, sided(lost, counts), now);
                    if index < 20 {
                        assert!(verdicts.is_empty(), "{kind:?} {lost:?} {index}");
                        assert!(no_miss(&check), "{kind:?} {lost:?} {index}");
                    }
                    if found.is_none() && !verdicts.is_empty() {
                        found = Some(now - t);
                    }
                }
                let first_unmatched = tick * 20;
                assert_eq!(
                    found,
                    Some(first_unmatched + MATCH_WINDOW),
                    "{kind:?} {lost:?}"
                );
            }
        }
    }

    /// F5-2 review 2, I2 and I3: a flood in a new pass every millisecond
    /// takes every group: the memory stays bounded (groups and evidence),
    /// and the oldest group, never joined, still gives the verdict one
    /// window after the first press.
    #[test]
    fn a_flood_that_takes_every_group_stays_bounded_and_is_found() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let mut found = None;
        for index in 0..2000 {
            let now = t + ms(index);
            if !pass(&mut check, Kind::Click, (0, 100), now).is_empty() && found.is_none() {
                found = Some(now - t);
            }
            assert!(check.raw.pending.groups.len() <= PENDING_CAP);
            assert!(check.pending(Source::Raw) <= PENDING_LIMIT);
        }
        assert_eq!(found, Some(MATCH_WINDOW));
    }

    #[test]
    fn forget_drops_pending_keys_and_misses_but_keeps_strikes() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        let end = one_sided(&mut check, Source::Raw, t, MISS_LIMIT as u16, ms(1));
        assert_eq!(check.expire(end + MATCH_WINDOW), Some(Verdict::HookLost));
        let end = one_sided(&mut check, Source::Raw, end + MATCH_WINDOW, 3, ms(1));
        check.forget();
        assert_eq!(check.next_due(), None);
        assert_eq!(check.pending(Source::Raw), 0);
        let end = one_sided(
            &mut check,
            Source::Raw,
            end,
            2 * MISS_LIMIT as u16 - 1,
            ms(1),
        );
        assert_eq!(
            check.expire(end + MATCH_WINDOW),
            None,
            "the strike survives; the forgotten keys do not count"
        );
    }

    #[test]
    fn health_table_moves_since_only_on_state_changes() {
        let mut table = HealthTable::new();
        let snapshot = table.snapshot(77);
        assert_eq!(
            snapshot.iter().map(|h| h.component).collect::<Vec<_>>(),
            INPUT_COMPONENTS
        );
        assert!(snapshot
            .iter()
            .all(|h| h.state == HealthState::Starting && h.reason.is_none() && h.since_ms == 77));
        assert!(table.set(Component::RawInput, HealthState::Healthy, None, 100));
        assert!(!table.set(Component::RawInput, HealthState::Healthy, None, 200));
        assert!(table.set(
            Component::RawInput,
            HealthState::Recovering,
            Some(ReasonCode::RawInputMissing),
            300
        ));
        assert!(table.set(
            Component::RawInput,
            HealthState::Recovering,
            Some(ReasonCode::HookInstallFailed),
            400
        ));
        let raw = table.get(Component::RawInput).unwrap();
        assert_eq!(raw.since_ms, 300, "same state, new reason");
        assert_eq!(raw.reason, Some(ReasonCode::HookInstallFailed));
        assert!(!table.set(Component::Executor, HealthState::Failed, None, 500));
        assert!(table.get(Component::Executor).is_none());
    }

    #[test]
    fn log_names_match_the_serde_names() {
        for component in INPUT_COMPONENTS {
            assert_eq!(
                serde_json::to_value(component).unwrap(),
                component_name(component)
            );
        }
        for reason in [
            ReasonCode::Ok,
            ReasonCode::HookMissing,
            ReasonCode::RawInputMissing,
            ReasonCode::HookInstallFailed,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                reason_name(Some(reason))
            );
        }
        assert_eq!(reason_name(None), "ok");
    }
}
