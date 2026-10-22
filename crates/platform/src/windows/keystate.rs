//! Physical key state mirrored by the low-level hooks.
//!
//! B-05: the mirror is one atomic state word per virtual-key code, so the
//! keyboard and mouse hooks update it without taking a lock and readers
//! never observe a "contended" mirror. H2: each word carries a version, so
//! consumer-side corrections never overwrite a newer hook transition. Until the mirror is seeded, readers fall back to the
//! asynchronous key state through [`AsyncKeyProbe`]; tests only use fakes.
//!
//! A key-up the hooks never see (secure desktop, a session switch, a hook
//! gap) would leave a modifier stuck down and block every expansion. The
//! input consumer therefore resynchronises the mirror after session signals
//! and runs [`ModifierWatch`], which re-checks modifiers held for
//! [`MODIFIER_VERIFY_AFTER`] against the asynchronous key state.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::sync::atomic::AtomicU32;

/// Reads the hardware (asynchronous) key state. The production
/// implementation wraps `GetAsyncKeyState`; tests use fakes so no test reads
/// the real desktop.
pub(super) trait AsyncKeyProbe {
    fn down(&self, vk: u16) -> bool;
}

pub(super) struct SystemAsyncKeys;
impl AsyncKeyProbe for SystemAsyncKeys {
    fn down(&self, vk: u16) -> bool {
        // SAFETY: GetAsyncKeyState takes a plain integer and touches no memory we own.
        unsafe { GetAsyncKeyState(i32::from(vk)) & i16::MIN != 0 }
    }
}

/// Number of virtual-key codes the mirror tracks (0..=255).
const KEY_CODES: usize = 256;
/// Bit 0 of a key's state word: the key is down.
const DOWN: u32 = 1;

/// The state word after one more transition from `current`: the version
/// (bits 1..) advances and bit 0 becomes `down`. 31 bits of version wrap
/// only after 2^31 transitions of one key, far beyond any race window.
const fn advance(current: u32, down: bool) -> u32 {
    (current & !DOWN).wrapping_add(2) | down as u32
}

/// Virtual-key codes 0..=255, one atomic state word per code. Codes outside
/// that range are ignored.
///
/// H2: each word packs the down bit with a version that every transition
/// advances, so a consumer that sampled a key before probing the
/// asynchronous state can apply the probe only when nothing happened since
/// ([`KeyBits::replace_if`]), including a down→up→down (ABA) sequence.
pub(super) struct KeyBits {
    keys: [AtomicU32; KEY_CODES],
    seeded: AtomicBool,
}
impl KeyBits {
    pub(super) const fn new() -> Self {
        Self {
            keys: [const { AtomicU32::new(0) }; KEY_CODES],
            seeded: AtomicBool::new(false),
        }
    }
    fn slot(&self, vk: u16) -> Option<&AtomicU32> {
        self.keys.get(usize::from(vk))
    }
    /// Records a key transition. Lock-free and allocation-free (one CAS
    /// loop); safe on the hook thread. Every call advances the version, so a
    /// repeated key-down still invalidates an older sample.
    pub(super) fn set(&self, vk: u16, down: bool) {
        if let Some(word) = self.slot(vk) {
            // The closure never returns None, so the update cannot fail.
            let _ = word.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(advance(current, down))
            });
        }
    }
    /// The full state word (down bit plus version) of `vk`; 0 for codes
    /// outside the mirror.
    pub(super) fn state(&self, vk: u16) -> u32 {
        self.slot(vk).map_or(0, |word| word.load(Ordering::SeqCst))
    }
    /// Sets `vk` to `down` only if its state is still `seen` (no transition
    /// since that sample). Returns whether the write happened; on `false`
    /// the newer transition wins.
    pub(super) fn replace_if(&self, vk: u16, seen: u32, down: bool) -> bool {
        self.slot(vk).is_some_and(|word| {
            word.compare_exchange(
                seen,
                advance(seen, down),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        })
    }
    fn get(&self, vk: u16) -> bool {
        self.state(vk) & DOWN != 0
    }
    /// The hooks report sided codes, so a generic modifier is held when
    /// either side (or the generic code itself) is.
    pub(super) fn held(&self, vk: u16) -> bool {
        match vk {
            VK_CONTROL => self.get(VK_LCONTROL) || self.get(VK_RCONTROL) || self.get(vk),
            VK_SHIFT => self.get(VK_LSHIFT) || self.get(VK_RSHIFT) || self.get(vk),
            VK_MENU => self.get(VK_LMENU) || self.get(VK_RMENU) || self.get(vk),
            _ => self.get(vk),
        }
    }
    /// A shortcut chord is held: Ctrl or Alt alone, or either Windows key.
    /// Ctrl+Alt together is AltGr and still types text.
    pub(super) fn modified(&self) -> bool {
        (self.held(VK_CONTROL) ^ self.held(VK_MENU)) || self.held(VK_LWIN) || self.held(VK_RWIN)
    }
    pub(super) fn seeded(&self) -> bool {
        self.seeded.load(Ordering::SeqCst)
    }
    /// The resync without injection tracking: production always goes
    /// through [`KeyBits::resync_unless_injected`].
    #[cfg(test)]
    pub(super) fn resync(&self, probe: &impl AsyncKeyProbe) {
        Sample::take(self, probe, |_| false).apply(self);
        self.seeded.store(true, Ordering::SeqCst);
    }
    /// Re-reads every code from `probe` and marks the mirror seeded.
    ///
    /// Each key's state is sampled *before* it is probed, and the probe is
    /// applied with [`KeyBits::replace_if`]: any hook transition after the
    /// sample (even one that restores the sampled value) keeps the hook's
    /// value, because it is newer than the probe.
    ///
    /// The generic Shift/Ctrl/Alt codes are not seeded: the asynchronous
    /// state of a generic code is "either side down", while the low-level
    /// hook reports the sided codes, so no key-up would ever clear a seeded
    /// generic bit. The sided bits already make [`KeyBits::held`] true.
    ///
    /// It never imports input this process injected (H1). The asynchronous
    /// key state includes our own injected keys, which the hooks
    /// deliberately ignore, so:
    ///
    /// - keys `own` holds are neither probed nor written: they keep the
    ///   hook's value (a real press of the same key still shows there);
    /// - the whole sample is discarded, and `false` returned, when any
    ///   injection was in flight or started since `own` was observed at
    ///   `seen` ([`OwnInput::seq`]). The caller retries after a quiet
    ///   period, which also covers the lag of the asynchronous state behind
    ///   a key-up we sent.
    pub(super) fn resync_unless_injected(
        &self,
        probe: &impl AsyncKeyProbe,
        own: &OwnInput,
        seen: u64,
    ) -> bool {
        if !own.settled(seen) {
            return false;
        }
        let sample = Sample::take(self, probe, |vk| own.holds(vk));
        if !own.settled(seen) {
            return false;
        }
        sample.apply(self);
        self.seeded.store(true, Ordering::SeqCst);
        true
    }
    /// Test rigs: treat the (empty) mirror as authoritative so readers never
    /// fall back to the real asynchronous key state.
    #[cfg(test)]
    pub(super) fn mark_seeded(&self) {
        self.seeded.store(true, Ordering::SeqCst);
    }
}

/// One resync pass: every key's state word, sampled before that key was
/// probed, and the probe result (`None`: skipped, the mirror keeps its
/// value). Lives on the stack; nothing is allocated.
struct Sample {
    seen: [u32; KEY_CODES],
    down: [Option<bool>; KEY_CODES],
}
impl Sample {
    fn take(keys: &KeyBits, probe: &impl AsyncKeyProbe, skip: impl Fn(u16) -> bool) -> Self {
        let mut sample = Self {
            seen: [0; KEY_CODES],
            down: [None; KEY_CODES],
        };
        for ((vk, seen), down) in (0u16..)
            .zip(sample.seen.iter_mut())
            .zip(sample.down.iter_mut())
        {
            if skip(vk) {
                continue;
            }
            // Sample first: a transition between this load and the probe
            // advances the version and makes `apply` skip the key.
            *seen = keys.state(vk);
            *down = Some(vk != 0 && !GENERIC_MODIFIERS.contains(&vk) && probe.down(vk));
        }
        sample
    }
    /// Writes every probed value whose key has not changed since sampled.
    fn apply(&self, keys: &KeyBits) {
        for ((vk, &seen), &down) in (0u16..).zip(&self.seen).zip(&self.down) {
            if let Some(down) = down {
                if (seen & DOWN != 0) != down {
                    keys.replace_if(vk, seen, down);
                }
            }
        }
    }
}

/// Input this process injected (H1): which keys and mouse buttons it holds
/// down, and whether an injection is in flight.
///
/// The output layer (`inject.rs`) brackets every injector call with
/// [`OwnInput::begin`] and notes the transitions the system accepted. The
/// hooks never see our input in the mirror (they skip `MARK`ed events),
/// but the asynchronous key state does, so mirror corrections consult this:
/// keys held here are never imported, and samples overlapping an injection
/// are discarded. Lock- and allocation-free.
pub(super) struct OwnInput {
    held: [AtomicU64; 4],
    /// Injections in flight.
    active: AtomicU32,
    /// Advanced when each injection starts and ends.
    seq: AtomicU64,
}
impl OwnInput {
    pub(super) const fn new() -> Self {
        Self {
            held: [const { AtomicU64::new(0) }; 4],
            active: AtomicU32::new(0),
            seq: AtomicU64::new(0),
        }
    }
    /// Marks an injection in flight until the returned guard drops (also on
    /// unwind).
    pub(super) fn begin(&self) -> Injecting<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
        Injecting { own: self }
    }
    /// Changes whenever an injection starts or ends.
    pub(super) fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }
    /// No injection is in flight and none started or ended since `seen`.
    pub(super) fn settled(&self, seen: u64) -> bool {
        !self.active() && self.seq() == seen
    }
    fn slot(&self, vk: u16) -> Option<(&AtomicU64, u64)> {
        self.held
            .get(usize::from(vk >> 6))
            .map(|word| (word, 1u64 << (vk & 63)))
    }
    fn note(&self, vk: u16, down: bool) {
        if let Some((word, bit)) = self.slot(vk) {
            if down {
                word.fetch_or(bit, Ordering::SeqCst);
            } else {
                word.fetch_and(!bit, Ordering::SeqCst);
            }
        }
    }
    fn bit(&self, vk: u16) -> bool {
        self.slot(vk)
            .is_some_and(|(word, bit)| word.load(Ordering::SeqCst) & bit != 0)
    }
    /// We hold `vk` down. Modifiers count as one family: an injected generic
    /// Ctrl shows up as the left Ctrl in the asynchronous state, and the
    /// reverse.
    pub(super) fn holds(&self, vk: u16) -> bool {
        let family = match vk {
            VK_CONTROL | VK_LCONTROL | VK_RCONTROL => [VK_CONTROL, VK_LCONTROL, VK_RCONTROL],
            VK_SHIFT | VK_LSHIFT | VK_RSHIFT => [VK_SHIFT, VK_LSHIFT, VK_RSHIFT],
            VK_MENU | VK_LMENU | VK_RMENU => [VK_MENU, VK_LMENU, VK_RMENU],
            _ => [vk; 3],
        };
        family.iter().any(|&v| self.bit(v))
    }
}

/// One injection in flight; see [`OwnInput::begin`].
pub(super) struct Injecting<'a> {
    own: &'a OwnInput,
}
impl Injecting<'_> {
    /// The system accepted our key-down (or button-down) of `vk`.
    pub(super) fn pressed(&self, vk: u16) {
        self.own.note(vk, true);
    }
    /// The system accepted our key-up (or button-up) of `vk`.
    pub(super) fn released(&self, vk: u16) {
        self.own.note(vk, false);
    }
}
impl Drop for Injecting<'_> {
    fn drop(&mut self) {
        self.own.seq.fetch_add(1, Ordering::SeqCst);
        self.own.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How long the input consumer waits after the last injection before it
/// samples the asynchronous key state, so the state has caught up with a
/// key-up we sent (H1). 「待实测」: the lag between `SendInput` returning
/// and `GetAsyncKeyState` reflecting the event, with low-level hooks
/// installed, has not been measured on real machines.
pub(super) const INJECTION_QUIET: Duration = Duration::from_millis(250);

static OWN: OwnInput = OwnInput::new();
#[cfg(test)]
thread_local! {
    static OWN_OVERRIDE: std::cell::Cell<Option<&'static OwnInput>> =
        const { std::cell::Cell::new(None) };
}
/// The process-wide [`OwnInput`]. A test may route the injections of its
/// own thread to a private instance (`track_own_input`) so parallel tests
/// do not see each other's output.
pub(super) fn own_input() -> &'static OwnInput {
    #[cfg(test)]
    if let Some(own) = OWN_OVERRIDE.with(std::cell::Cell::get) {
        return own;
    }
    &OWN
}
/// Test rigs: injections on this thread are tracked in `own` until the
/// returned guard drops.
#[cfg(test)]
pub(super) fn track_own_input(own: &'static OwnInput) -> impl Drop {
    struct Restore(Option<&'static OwnInput>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OWN_OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    Restore(OWN_OVERRIDE.with(|cell| cell.replace(Some(own))))
}

/// Generic codes; the low-level hook reports the sided ones.
const GENERIC_MODIFIERS: [u16; 3] = [VK_SHIFT, VK_CONTROL, VK_MENU];
/// Every code that counts as a held modifier anywhere in the engine.
const MODIFIERS: [u16; 11] = [
    VK_SHIFT,
    VK_CONTROL,
    VK_MENU,
    VK_LSHIFT,
    VK_RSHIFT,
    VK_LCONTROL,
    VK_RCONTROL,
    VK_LMENU,
    VK_RMENU,
    VK_LWIN,
    VK_RWIN,
];
/// How long a modifier may stay down in the mirror before the asynchronous
/// key state is consulted (S-05: a lost key-up heals within 30 s).
pub(super) const MODIFIER_VERIFY_AFTER: Duration = Duration::from_secs(30);

fn modifier_name(vk: u16) -> &'static str {
    match vk {
        VK_SHIFT => "shift",
        VK_CONTROL => "ctrl",
        VK_MENU => "alt",
        VK_LSHIFT => "lshift",
        VK_RSHIFT => "rshift",
        VK_LCONTROL => "lctrl",
        VK_RCONTROL => "rctrl",
        VK_LMENU => "lalt",
        VK_RMENU => "ralt",
        VK_LWIN => "lwin",
        VK_RWIN => "rwin",
        _ => "other",
    }
}

/// Consumer-side check for modifiers whose key-up the hooks never saw.
/// Never used on a hook thread.
#[derive(Default)]
pub(super) struct ModifierWatch {
    /// When each [`MODIFIERS`] bit was first seen down, or last verified.
    since: [Option<Instant>; MODIFIERS.len()],
}
impl ModifierWatch {
    /// Samples the mirror at `now`. A modifier down for
    /// [`MODIFIER_VERIFY_AFTER`] is checked against `probe`: still down
    /// restarts its timer, up clears the bit (`keystate.heal`). Returns when
    /// the next check is due, if any modifier is down.
    ///
    /// H2: the clear is conditional on the key's state word sampled before
    /// the probe, so a press (or any down/up/down sequence) landing between
    /// the probe and the clear survives; the timer then restarts.
    pub(super) fn check(
        &mut self,
        keys: &KeyBits,
        probe: &impl AsyncKeyProbe,
        now: Instant,
    ) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for (&vk, since) in MODIFIERS.iter().zip(self.since.iter_mut()) {
            let seen = keys.state(vk);
            if seen & DOWN == 0 {
                *since = None;
                continue;
            }
            let start = *since.get_or_insert(now);
            if now.saturating_duration_since(start) >= MODIFIER_VERIFY_AFTER {
                if !probe.down(vk) && keys.replace_if(vk, seen, false) {
                    *since = None;
                    tracing::warn!(event = "keystate.heal", modifier = modifier_name(vk));
                    continue;
                }
                // Really held, or a newer transition raced the probe and
                // wins: check again later.
                *since = Some(now);
            }
            let due = since.and_then(|start| start.checked_add(MODIFIER_VERIFY_AFTER));
            next = match (next, due) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        next
    }
    /// Forgets every timer (after a full resync).
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }
}

pub(super) static PHYSICAL: KeyBits = KeyBits::new();

/// Seeds the mirror at engine start. Nothing has been injected yet; keys we
/// hold are masked anyway.
pub(super) fn seed_physical() {
    let own = own_input();
    Sample::take(&PHYSICAL, &SystemAsyncKeys, |vk| own.holds(vk)).apply(&PHYSICAL);
    PHYSICAL.seeded.store(true, Ordering::SeqCst);
}
fn physical_in(keys: &KeyBits, probe: &impl AsyncKeyProbe, own: &OwnInput, v: u16) -> bool {
    if keys.seeded() {
        keys.held(v)
    } else {
        // The asynchronous state includes keys we hold ourselves (H1).
        !own.holds(v) && probe.down(v)
    }
}
pub(super) fn physical(v: u16) -> bool {
    physical_in(&PHYSICAL, &SystemAsyncKeys, own_input(), v)
}
pub(super) fn modifiers_held() -> bool {
    physical(VK_CONTROL)
        || physical(VK_MENU)
        || physical(VK_SHIFT)
        || physical(VK_LWIN)
        || physical(VK_RWIN)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Reports the listed codes as down and counts reads.
    struct FakeKeys {
        down: Vec<u16>,
        reads: Cell<usize>,
    }
    impl FakeKeys {
        fn new(down: &[u16]) -> Self {
            Self {
                down: down.to_vec(),
                reads: Cell::new(0),
            }
        }
    }
    impl AsyncKeyProbe for FakeKeys {
        fn down(&self, vk: u16) -> bool {
            self.reads.set(self.reads.get() + 1);
            self.down.contains(&vk)
        }
    }

    #[test]
    fn transitions_are_recorded_per_code_and_out_of_range_codes_are_ignored() {
        let keys = KeyBits::new();
        for vk in [1, 63, 64, 127, 128, 191, 192, 255] {
            keys.set(vk, true);
            assert!(keys.held(vk), "{vk}");
        }
        keys.set(64, false);
        assert!(!keys.held(64));
        assert!(keys.held(63) && keys.held(127));
        keys.set(256, true);
        keys.set(u16::MAX, true);
        assert!(!keys.held(256) && !keys.held(u16::MAX) && !keys.held(0));
    }

    #[test]
    fn generic_modifiers_cover_both_sides() {
        let keys = KeyBits::new();
        for (generic, left, right) in [
            (VK_CONTROL, VK_LCONTROL, VK_RCONTROL),
            (VK_SHIFT, VK_LSHIFT, VK_RSHIFT),
            (VK_MENU, VK_LMENU, VK_RMENU),
        ] {
            assert!(!keys.held(generic));
            keys.set(right, true);
            assert!(keys.held(generic) && !keys.held(left));
            keys.set(right, false);
            keys.set(left, true);
            assert!(keys.held(generic));
            keys.set(left, false);
            assert!(!keys.held(generic));
        }
    }

    #[test]
    fn shortcut_chords_are_modified_but_altgr_is_not() {
        let keys = KeyBits::new();
        assert!(!keys.modified());
        keys.set(VK_LCONTROL, true);
        assert!(keys.modified(), "Ctrl alone");
        keys.set(VK_RMENU, true);
        assert!(!keys.modified(), "AltGr arrives as Ctrl+Alt");
        keys.set(VK_LCONTROL, false);
        assert!(keys.modified(), "Alt alone");
        keys.set(VK_RMENU, false);
        keys.set(VK_LSHIFT, true);
        assert!(!keys.modified(), "Shift types text");
        keys.set(VK_RWIN, true);
        assert!(keys.modified());
    }

    #[test]
    fn resync_replaces_stale_bits_with_the_probe_and_seeds() {
        let keys = KeyBits::new();
        // A key-up the hook never saw leaves LCONTROL stuck in the mirror.
        keys.set(VK_LCONTROL, true);
        assert!(!keys.seeded());
        keys.resync(&FakeKeys::new(&[VK_LSHIFT, 0x41]));
        assert!(keys.seeded());
        assert!(!keys.held(VK_CONTROL));
        assert!(keys.held(VK_SHIFT) && keys.held(0x41));
    }

    /// Simulates hook transitions racing the probe of the same word.
    struct Racing<'a> {
        keys: &'a KeyBits,
    }
    impl AsyncKeyProbe for Racing<'_> {
        fn down(&self, vk: u16) -> bool {
            if vk == VK_RCONTROL {
                // Both happen after LSHIFT/LCONTROL were sampled.
                self.keys.set(VK_LCONTROL, false);
                self.keys.set(VK_LSHIFT, true);
            }
            // Stale samples: LCONTROL still down, LSHIFT still up.
            vk == VK_LCONTROL
        }
    }

    #[test]
    fn resync_keeps_hook_transitions_that_race_the_probe() {
        let keys = KeyBits::new();
        keys.set(VK_LCONTROL, true);
        keys.resync(&Racing { keys: &keys });
        assert!(!keys.held(VK_LCONTROL), "the newer key-up wins");
        assert!(keys.held(VK_LSHIFT), "the newer key-down wins");
    }

    /// Replays hook transitions `race` on the mirror while `vk` is being
    /// probed, then reports every key as up: a sample older than the race.
    struct RaceOn<'a> {
        keys: &'a KeyBits,
        vk: u16,
        race: &'a [bool],
    }
    impl AsyncKeyProbe for RaceOn<'_> {
        fn down(&self, vk: u16) -> bool {
            if vk == self.vk {
                for &down in self.race {
                    self.keys.set(vk, down);
                }
            }
            false
        }
    }

    #[test]
    fn resync_keeps_a_press_that_lands_while_its_own_key_is_probed() {
        let keys = KeyBits::new();
        keys.resync(&RaceOn {
            keys: &keys,
            vk: 0x41,
            race: &[true],
        });
        assert!(keys.held(0x41), "the press after the sample wins");
        // ABA: down -> up -> down while probed leaves the same down bit as
        // the sample, but a newer version, so the stale "up" is not applied.
        let keys = KeyBits::new();
        keys.set(VK_LCONTROL, true);
        keys.resync(&RaceOn {
            keys: &keys,
            vk: VK_LCONTROL,
            race: &[false, true],
        });
        assert!(keys.held(VK_CONTROL), "the re-press survives the resync");
    }

    #[test]
    fn conditional_writes_only_apply_to_the_sampled_version() {
        let keys = KeyBits::new();
        keys.set(VK_LMENU, true);
        let seen = keys.state(VK_LMENU);
        keys.set(VK_LMENU, false);
        keys.set(VK_LMENU, true);
        assert_eq!(keys.state(VK_LMENU) & DOWN, seen & DOWN, "same bit (ABA)");
        assert!(!keys.replace_if(VK_LMENU, seen, false), "stale version");
        assert!(keys.held(VK_LMENU));
        let fresh = keys.state(VK_LMENU);
        assert!(keys.replace_if(VK_LMENU, fresh, false));
        assert!(!keys.held(VK_LMENU));
        assert!(!keys.replace_if(256, 0, true), "out of range");
    }

    #[test]
    fn the_watch_never_clears_a_press_that_races_its_probe() {
        for race in [&[true][..], &[false, true][..]] {
            let keys = KeyBits::new();
            let start = Instant::now();
            let mut watch = ModifierWatch::default();
            // A lost key-up left LCONTROL down; the user presses it again
            // (or presses, releases, presses) after the probe said "up".
            keys.set(VK_LCONTROL, true);
            watch.check(&keys, &FakeKeys::new(&[]), start);
            let due = start + MODIFIER_VERIFY_AFTER;
            let probe = RaceOn {
                keys: &keys,
                vk: VK_LCONTROL,
                race,
            };
            assert_eq!(
                watch.check(&keys, &probe, due),
                Some(due + MODIFIER_VERIFY_AFTER),
                "the timer restarts instead of healing ({race:?})"
            );
            assert!(keys.held(VK_CONTROL), "the newer press is kept ({race:?})");
        }
    }

    #[test]
    fn resync_seeds_sided_modifiers_only_so_their_key_up_clears_them() {
        let keys = KeyBits::new();
        // Ctrl held during a resync: the asynchronous state reports both the
        // generic and the sided code.
        keys.resync(&FakeKeys::new(&[VK_CONTROL, VK_LCONTROL]));
        assert!(keys.held(VK_CONTROL));
        // The hook only ever reports the sided key-up.
        keys.set(VK_LCONTROL, false);
        assert!(!keys.held(VK_CONTROL), "no generic bit left behind");
        // A generic bit set by injected input is cleared by the next resync.
        keys.set(VK_SHIFT, true);
        keys.resync(&FakeKeys::new(&[VK_SHIFT]));
        assert!(!keys.held(VK_SHIFT));
    }

    #[test]
    fn the_watch_verifies_a_modifier_only_after_thirty_seconds() {
        let keys = KeyBits::new();
        let start = Instant::now();
        let mut watch = ModifierWatch::default();
        let released = FakeKeys::new(&[]);
        assert_eq!(watch.check(&keys, &released, start), None, "nothing held");
        keys.set(VK_RWIN, true);
        assert_eq!(
            watch.check(&keys, &released, start),
            Some(start + MODIFIER_VERIFY_AFTER)
        );
        let almost = start + MODIFIER_VERIFY_AFTER - Duration::from_millis(1);
        watch.check(&keys, &released, almost);
        assert_eq!(released.reads.get(), 0, "not probed early");
        assert!(keys.held(VK_RWIN));
        let due = start + MODIFIER_VERIFY_AFTER;
        assert_eq!(watch.check(&keys, &released, due), None);
        assert_eq!(released.reads.get(), 1);
        assert!(!keys.held(VK_RWIN), "healed");
    }

    #[test]
    fn a_modifier_really_held_is_kept_and_rechecked_later() {
        let keys = KeyBits::new();
        let start = Instant::now();
        let mut watch = ModifierWatch::default();
        let held = FakeKeys::new(&[VK_LSHIFT]);
        keys.set(VK_LSHIFT, true);
        watch.check(&keys, &held, start);
        let due = start + MODIFIER_VERIFY_AFTER;
        assert_eq!(
            watch.check(&keys, &held, due),
            Some(due + MODIFIER_VERIFY_AFTER)
        );
        assert!(keys.held(VK_LSHIFT));
        // Released normally: the timer is dropped without probing.
        keys.set(VK_LSHIFT, false);
        assert_eq!(watch.check(&keys, &held, due), None);
        assert_eq!(held.reads.get(), 1);
        // A new press starts a fresh 30 s.
        keys.set(VK_LSHIFT, true);
        let later = due + Duration::from_secs(5);
        assert_eq!(
            watch.check(&keys, &held, later),
            Some(later + MODIFIER_VERIFY_AFTER)
        );
    }

    #[test]
    fn readers_use_the_probe_only_until_the_mirror_is_seeded() {
        let keys = KeyBits::new();
        let own = OwnInput::new();
        let probe = FakeKeys::new(&[VK_LWIN, 0x41]);
        assert!(physical_in(&keys, &probe, &own, VK_LWIN));
        assert_eq!(probe.reads.get(), 1);
        // H1: a key we hold reads "down" asynchronously but is not physical.
        own.begin().pressed(0x41);
        assert!(!physical_in(&keys, &probe, &own, 0x41));
        keys.mark_seeded();
        assert!(!physical_in(&keys, &probe, &own, VK_LWIN));
        assert_eq!(probe.reads.get(), 1, "a seeded mirror never probes");
        keys.set(VK_LWIN, true);
        assert!(physical_in(&keys, &probe, &own, VK_LWIN));
    }

    #[test]
    fn injections_are_tracked_per_key_and_while_in_flight() {
        let own = OwnInput::new();
        let seen = own.seq();
        assert!(own.settled(seen));
        {
            let injecting = own.begin();
            assert!(own.active() && !own.settled(seen));
            injecting.pressed(VK_CONTROL);
            injecting.pressed(VK_LBUTTON);
        }
        assert!(!own.active());
        assert!(!own.settled(seen), "an injection ended since `seen`");
        assert!(own.settled(own.seq()));
        // An injected generic Ctrl reads as the left Ctrl, and the reverse.
        assert!(own.holds(VK_LCONTROL) && own.holds(VK_RCONTROL) && own.holds(VK_CONTROL));
        assert!(own.holds(VK_LBUTTON) && !own.holds(VK_SHIFT) && !own.holds(0x41));
        own.begin().released(VK_CONTROL);
        assert!(!own.holds(VK_LCONTROL) && own.holds(VK_LBUTTON));
        own.begin().pressed(256);
        assert!(!own.holds(256), "out of range codes are ignored");
        // The guard ends the injection on unwind too.
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _injecting = own.begin();
            panic!("injector failed");
        }));
        assert!(unwound.is_err() && !own.active());
    }

    #[test]
    fn resync_never_imports_keys_this_process_holds() {
        let keys = KeyBits::new();
        let own = OwnInput::new();
        {
            let injecting = own.begin();
            injecting.pressed(0x41);
            injecting.pressed(VK_CONTROL);
        }
        // The asynchronous state shows our A and our Ctrl (as LCONTROL),
        // plus a Shift the user really holds.
        let probe = FakeKeys::new(&[0x41, VK_CONTROL, VK_LCONTROL, VK_LSHIFT]);
        assert!(keys.resync_unless_injected(&probe, &own, own.seq()));
        assert!(keys.seeded());
        assert!(!keys.held(0x41) && !keys.held(VK_CONTROL));
        assert!(keys.held(VK_SHIFT));
        // A real press of a key we also hold still shows: the mirror keeps
        // the hook's value for owned keys.
        keys.set(0x41, true);
        assert!(keys.resync_unless_injected(&FakeKeys::new(&[]), &own, own.seq()));
        assert!(keys.held(0x41), "owned keys are left to the hook");
    }

    /// Our injection presses and releases A while the scan is half done;
    /// the asynchronous state still shows A down afterwards (the lag).
    struct InjectsWhileProbed<'a> {
        own: &'a OwnInput,
    }
    impl AsyncKeyProbe for InjectsWhileProbed<'_> {
        fn down(&self, vk: u16) -> bool {
            if vk == 0x30 {
                let injecting = self.own.begin();
                injecting.pressed(0x41);
                injecting.released(0x41);
            }
            vk == 0x41
        }
    }

    #[test]
    fn a_sample_that_overlaps_an_injection_is_discarded() {
        let keys = KeyBits::new();
        let own = OwnInput::new();
        let seen = own.seq();
        assert!(!keys.resync_unless_injected(&InjectsWhileProbed { own: &own }, &own, seen));
        assert!(!keys.held(0x41) && !keys.seeded(), "nothing applied");
        // An injection still in flight: not even probed.
        let injecting = own.begin();
        let probe = FakeKeys::new(&[0x41]);
        assert!(!keys.resync_unless_injected(&probe, &own, own.seq()));
        assert_eq!(probe.reads.get(), 0);
        drop(injecting);
        assert!(keys.resync_unless_injected(&FakeKeys::new(&[]), &own, own.seq()));
    }
}
