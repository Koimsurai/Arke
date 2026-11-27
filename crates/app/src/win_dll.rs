//! Optional system DLLs.
//!
//! A few Windows components are missing on stripped installs (a "debloat" script that deleted
//! `cldapi.dll`, for one). Arke delay-loads them (see `build.rs`), and a delayed load that fails
//! is an exception that kills the process. Every entry point of such a DLL asks [`system_dll`]
//! first and turns its feature off when the answer is no.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::HSTRING;

/// What was found for each DLL asked about, by lower-case name.
static FOUND: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// DLL names this thread's tests pretend are missing.
    static PRETEND_MISSING: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether `name` (`cldapi.dll`) can be loaded from the system folder. Looked up once per DLL and
/// remembered, so later calls cost a lock and a hash lookup. A DLL that is found stays loaded,
/// which is where a delayed load would have put it anyway. A missing one is logged once.
pub(crate) fn system_dll(name: &str) -> bool {
    #[cfg(test)]
    if PRETEND_MISSING.with(|missing| {
        missing
            .borrow()
            .iter()
            .any(|m| m.eq_ignore_ascii_case(name))
    }) {
        return false;
    }
    let key = name.to_ascii_lowercase();
    let mut found = FOUND.get_or_init(Mutex::default).lock();
    if let Some(&known) = found.get(&key) {
        return known;
    }
    // SAFETY: loads a DLL by name from the system folder only; nothing is called in it.
    let present =
        unsafe { LoadLibraryExW(&HSTRING::from(name), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.is_ok();
    if !present {
        log::info!("{name} is missing from this Windows; the features that need it are off");
    }
    found.insert(key, present);
    present
}

/// Makes [`system_dll`] answer no for `names` on this thread until the guard is dropped.
#[cfg(test)]
pub(crate) fn pretend_missing(names: &[&str]) -> impl Drop {
    struct Restore(Vec<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PRETEND_MISSING.with(|missing| *missing.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let before = PRETEND_MISSING.with(|missing| {
        std::mem::replace(
            &mut *missing.borrow_mut(),
            names.iter().map(|n| (*n).to_owned()).collect(),
        )
    });
    Restore(before)
}

/// Whether `name` is loaded in this process right now.
#[cfg(test)]
pub(crate) fn is_loaded(name: &str) -> bool {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    // SAFETY: only looks the module up; nothing is loaded or called.
    unsafe { GetModuleHandleW(&HSTRING::from(name)) }.is_ok()
}

/// For a test that pretends `names` are missing: they must not be loaded yet (else the test
/// would pass by accident; nextest gives each test its own process), and afterwards the
/// gated code must not have loaded them, which is what a delayed load would do.
#[cfg(test)]
pub(crate) fn assert_not_loaded(names: &[&str], when: &str) {
    for name in names {
        assert!(
            !is_loaded(name),
            "{name} is loaded {when}: something in this process loaded it (run the test alone, \
             as nextest does)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_system_dll_and_not_a_made_up_one() {
        assert!(system_dll("kernel32.dll"));
        assert!(system_dll("KERNEL32.DLL"));
        assert!(!system_dll("arke-no-such-component.dll"));
        // The remembered answers don't change.
        assert!(system_dll("kernel32.dll"));
        assert!(!system_dll("arke-no-such-component.dll"));
    }

    #[test]
    fn a_test_can_pretend_a_dll_is_missing() {
        {
            let _guard = pretend_missing(&["kernel32.dll"]);
            assert!(!system_dll("kernel32.dll"));
            assert!(system_dll("user32.dll"));
        }
        assert!(system_dll("kernel32.dll"));
    }
}
