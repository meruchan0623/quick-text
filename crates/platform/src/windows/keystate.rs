//! Physical key state mirrored by the low-level hooks.
//!
//! B-05: the mirror is a 256-bit atomic bitmap, so the keyboard and mouse
//! hooks update it without taking a lock and readers never observe a
//! "contended" mirror. Until the mirror is seeded, readers fall back to the
//! asynchronous key state through [`AsyncKeyProbe`]; tests only use fakes.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;

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

/// Virtual-key codes 0..=255 as four atomic words. Codes outside that range
/// are ignored.
pub(super) struct KeyBits {
    words: [AtomicU64; 4],
    seeded: AtomicBool,
}
impl KeyBits {
    pub(super) const fn new() -> Self {
        Self {
            words: [const { AtomicU64::new(0) }; 4],
            seeded: AtomicBool::new(false),
        }
    }
    fn slot(&self, vk: u16) -> Option<(&AtomicU64, u64)> {
        self.words
            .get(usize::from(vk >> 6))
            .map(|word| (word, 1u64 << (vk & 63)))
    }
    /// Records a key transition. Lock-free; safe on the hook thread.
    pub(super) fn set(&self, vk: u16, down: bool) {
        if let Some((word, bit)) = self.slot(vk) {
            if down {
                word.fetch_or(bit, Ordering::SeqCst);
            } else {
                word.fetch_and(!bit, Ordering::SeqCst);
            }
        }
    }
    fn get(&self, vk: u16) -> bool {
        self.slot(vk)
            .is_some_and(|(word, bit)| word.load(Ordering::SeqCst) & bit != 0)
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
    /// Re-reads every code from `probe` and marks the mirror seeded.
    ///
    /// A bit the hooks changed while its word was being probed keeps the
    /// hook's value: that transition is newer than the probe sample.
    pub(super) fn resync(&self, probe: &impl AsyncKeyProbe) {
        for (base, word) in (0u16..).step_by(64).zip(&self.words) {
            let before = word.load(Ordering::SeqCst);
            let probed = (0..64u16)
                .filter(|bit| base + bit != 0 && probe.down(base + bit))
                .fold(0u64, |acc, bit| acc | 1 << bit);
            // The closure never returns None, so the update cannot fail.
            let _ = word.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                let changed = before ^ current;
                Some((probed & !changed) | (current & changed))
            });
        }
        self.seeded.store(true, Ordering::SeqCst);
    }
    /// Test rigs: treat the (empty) mirror as authoritative so readers never
    /// fall back to the real asynchronous key state.
    #[cfg(test)]
    pub(super) fn mark_seeded(&self) {
        self.seeded.store(true, Ordering::SeqCst);
    }
}

pub(super) static PHYSICAL: KeyBits = KeyBits::new();

pub(super) fn seed_physical() {
    PHYSICAL.resync(&SystemAsyncKeys);
}
fn physical_in(keys: &KeyBits, probe: &impl AsyncKeyProbe, v: u16) -> bool {
    if keys.seeded() {
        keys.held(v)
    } else {
        probe.down(v)
    }
}
pub(super) fn physical(v: u16) -> bool {
    physical_in(&PHYSICAL, &SystemAsyncKeys, v)
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

    #[test]
    fn readers_use_the_probe_only_until_the_mirror_is_seeded() {
        let keys = KeyBits::new();
        let probe = FakeKeys::new(&[VK_LWIN]);
        assert!(physical_in(&keys, &probe, VK_LWIN));
        assert_eq!(probe.reads.get(), 1);
        keys.mark_seeded();
        assert!(!physical_in(&keys, &probe, VK_LWIN));
        assert_eq!(probe.reads.get(), 1, "a seeded mirror never probes");
        keys.set(VK_LWIN, true);
        assert!(physical_in(&keys, &probe, VK_LWIN));
    }
}
