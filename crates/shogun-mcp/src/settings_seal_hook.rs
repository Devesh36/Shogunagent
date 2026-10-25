//! Where the sealed settings files this crate owns meet `shogun_core::settings_seal` (audit
//! 2026-10-20 M6, extended the same day to `web_fetch.json`, `export_folders.json` and
//! `gmail-policy.json`).
//!
//! The sealing code lives in `shogun-core`, which depends on this crate, so it installs itself
//! into a [`SealSlot`] per file at process start. Each file's loader passes what it read through
//! [`SealSlot::trusted`] (a change made outside the app that would widen the setting is not
//! followed) and each writer calls [`SealSlot::seal`] after the file is in place. Without an
//! installed slot (tests, Linux dev, a process whose Keychain read failed) the file is read and
//! written as it is.

use std::path::Path;
use std::sync::OnceLock;

/// The two calls a sealed setting needs.
pub struct SealFns<T: 'static> {
    /// The value in force for what the file says.
    pub trusted: fn(&Path, T) -> T,
    /// Seal a value just written to the path.
    pub seal: fn(&Path, &T),
}

/// One file's installed seal.
pub struct SealSlot<T: 'static>(OnceLock<SealFns<T>>);

impl<T: 'static> SealSlot<T> {
    pub const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Install the seal for this process. `false` when one is already installed.
    pub fn install(&self, fns: SealFns<T>) -> bool {
        self.0.set(fns).is_ok()
    }

    /// Whether a seal is installed.
    pub fn installed(&self) -> bool {
        self.0.get().is_some()
    }

    /// The value in force for `file`, read from `path`.
    pub fn trusted(&self, path: &Path, file: T) -> T {
        match self.0.get() {
            Some(fns) => (fns.trusted)(path, file),
            None => file,
        }
    }

    /// Seal `value`, just written to `path`.
    pub fn seal(&self, path: &Path, value: &T) {
        if let Some(fns) = self.0.get() {
            (fns.seal)(path, value);
        }
    }
}

impl<T: 'static> Default for SealSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_slot_passes_the_file_through_and_an_installed_one_decides() {
        static SLOT: SealSlot<u32> = SealSlot::new();
        assert!(!SLOT.installed());
        assert_eq!(SLOT.trusted(Path::new("x.json"), 7), 7);
        SLOT.seal(Path::new("x.json"), &7);
        assert!(SLOT.install(SealFns { trusted: |_, v| v.min(3), seal: |_, _| {} }));
        assert!(!SLOT.install(SealFns { trusted: |_, v| v, seal: |_, _| {} }), "installed once");
        assert_eq!(SLOT.trusted(Path::new("x.json"), 7), 3);
    }
}
