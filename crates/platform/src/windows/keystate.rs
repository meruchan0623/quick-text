//! Physical key state mirrored by the hooks (falls back to GetAsyncKeyState).
use super::*;
pub(super) static PHYSICAL: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
pub(super) fn seed_physical() {
    let initial = (1u16..=255)
        .filter(|v| unsafe { GetAsyncKeyState(*v as i32) & i16::MIN != 0 })
        .collect();
    PHYSICAL.set(Mutex::new(initial)).ok();
}
pub(super) fn physical(v: u16) -> bool {
    if let Some(set) = PHYSICAL.get() {
        let Ok(set) = set.try_lock() else {
            return true;
        };
        match v {
            VK_CONTROL => {
                set.contains(&VK_LCONTROL) || set.contains(&VK_RCONTROL) || set.contains(&v)
            }
            VK_SHIFT => set.contains(&VK_LSHIFT) || set.contains(&VK_RSHIFT) || set.contains(&v),
            VK_MENU => set.contains(&VK_LMENU) || set.contains(&VK_RMENU) || set.contains(&v),
            _ => set.contains(&v),
        }
    } else {
        unsafe { GetAsyncKeyState(v as i32) & i16::MIN != 0 }
    }
}
pub(super) fn modifiers_held() -> bool {
    physical(VK_CONTROL)
        || physical(VK_MENU)
        || physical(VK_SHIFT)
        || physical(VK_LWIN)
        || physical(VK_RWIN)
}
