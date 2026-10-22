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
/// Unmatched sightings kept per source; the oldest one beyond this counts as
/// a miss at once.
pub(super) const PENDING_CAP: usize = 64;
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

#[derive(Default)]
struct Side {
    /// When each of this source's unmatched sightings happened.
    pending: VecDeque<Instant>,
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
#[derive(Default)]
pub(super) struct CrossCheck {
    hook: Side,
    raw: Side,
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
    /// Records a key-down `source` saw at `now`. Returns true when it paired
    /// with a pending key from the other source; false when it was merged
    /// as autorepeat or is waiting for the other source.
    pub(super) fn saw(&mut self, source: Source, vk: u16, now: Instant) -> bool {
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
        if self.other(source).pending.pop_front().is_some() {
            // Both sources are alive right now.
            for side in [&mut self.hook, &mut self.raw] {
                side.misses = 0;
                side.strikes = 0;
            }
            return true;
        }
        let side = self.side(source);
        side.pending.push_back(now);
        if side.pending.len() > PENDING_CAP {
            side.pending.pop_front();
            self.other(source).misses += 1;
        }
        false
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
            let mut expired = 0;
            let side = self.side(source);
            while side
                .pending
                .front()
                .is_some_and(|seen| now.saturating_duration_since(*seen) >= MATCH_WINDOW)
            {
                side.pending.pop_front();
                expired += 1;
            }
            let other = self.other(source);
            other.misses = other.misses.saturating_add(expired);
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
    /// When the oldest pending sighting expires, if any.
    pub(super) fn next_due(&self) -> Option<Instant> {
        [&self.hook, &self.raw]
            .into_iter()
            .filter_map(|side| side.pending.front())
            .min()
            .and_then(|seen| seen.checked_add(MATCH_WINDOW))
    }
    /// Unmatched keys waiting for `source`'s counterpart.
    #[cfg(test)]
    pub(super) fn pending(&self, source: Source) -> usize {
        match source {
            Source::Hook => self.hook.pending.len(),
            Source::Raw => self.raw.pending.len(),
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

    #[test]
    fn an_overflowing_pending_queue_counts_misses_at_once() {
        let mut check = CrossCheck::default();
        let t = Instant::now();
        one_sided(
            &mut check,
            Source::Hook,
            t,
            (PENDING_CAP + MISS_LIMIT as usize) as u16,
            Duration::ZERO,
        );
        assert_eq!(check.pending(Source::Hook), PENDING_CAP);
        assert_eq!(check.expire(t), Some(Verdict::RawLost));
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
