//! Keeps the folder handler registration (`arke_core::folder_handler`) in step with the "Open
//! folders in Arke" setting, and implements `arke --restore-explorer`.
//!
//! The registry and the backup file are reached through traits, so tests drive the whole flow with
//! in-memory fakes and never touch the real registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::folder_handler::{self, BackupStore, FileBackup, Registry};
use gpui::{App, Entity};

use crate::settings::SettingsStore;

/// Registers and unregisters Arke as the folder handler.
pub(crate) struct Handler {
    registry: Arc<dyn Registry>,
    backup: Arc<dyn BackupStore>,
    /// The exe the registered commands start.
    exe: PathBuf,
}

impl Handler {
    /// The real thing: this exe, the current user's registry, `%APPDATA%\Arke`'s backup file.
    /// `None` when the exe or the settings folder is unknown, or when a test environment asks to
    /// keep the shell alone.
    pub(crate) fn real() -> Option<Self> {
        if !allowed_here(|name| std::env::var_os(name).is_some()) {
            log::info!("folder handler: disabled by the environment");
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let dir = settings_dir()?;
        Some(Self::new(
            Arc::new(crate::win_registry::UserRegistry),
            Arc::new(FileBackup(dir.join(folder_handler::BACKUP_FILE))),
            exe,
        ))
    }

    pub(crate) fn new(
        registry: Arc<dyn Registry>,
        backup: Arc<dyn BackupStore>,
        exe: PathBuf,
    ) -> Self {
        Self {
            registry,
            backup,
            exe,
        }
    }

    /// Makes the registry match `wanted`: on registers (or repairs) the handler, off restores the
    /// previous values when Arke registered anything.
    pub(crate) fn sync(&self, wanted: bool) {
        if wanted {
            match folder_handler::apply(&*self.registry, &*self.backup, &self.exe) {
                Ok(done) if done.written == 0 => {
                    log::info!(
                        "folder handler: already registered for {}",
                        self.exe.display()
                    );
                }
                Ok(done) => log::info!(
                    "folder handler: wrote {} values ({} repaired) for {}",
                    done.written,
                    done.repaired,
                    self.exe.display()
                ),
                Err(err) => log::warn!("folder handler: cannot register: {err}"),
            }
        } else if matches!(self.backup.load(), Ok(Some(_))) {
            self.restore();
        }
    }

    /// Puts the registry back and removes the backup.
    pub(crate) fn restore(&self) -> bool {
        match folder_handler::restore(&*self.registry, &*self.backup) {
            Ok(done) => {
                log::info!(
                    "folder handler: restored {} values ({} left as other programs set them)",
                    done.values,
                    done.left_alone
                );
                true
            }
            Err(err) => {
                log::warn!("folder handler: cannot restore: {err}");
                false
            }
        }
    }
}

/// `%APPDATA%\Arke`.
fn settings_dir() -> Option<PathBuf> {
    crate::settings::default_path()?
        .parent()
        .map(Path::to_path_buf)
}

/// Live checks (`ARKE_TAKEOVER_TEST_ROOT`) and `ARKE_NO_FOLDER_HANDLER` never touch the shell
/// registration; `is_set` answers whether an environment variable exists.
fn allowed_here(is_set: impl Fn(&str) -> bool) -> bool {
    !is_set("ARKE_TAKEOVER_TEST_ROOT") && !is_set("ARKE_NO_FOLDER_HANDLER")
}

/// Follows the setting for the rest of the run. The registry work happens on its own thread; only
/// the latest wish counts. At startup with the setting on, the registration is verified and
/// repaired (an exe that moved), with it off a leftover registration is undone.
pub(crate) fn follow_setting(store: &Entity<SettingsStore>, handler: Handler, cx: &mut App) {
    let (tx, rx) = flume::unbounded::<bool>();
    let handler = Arc::new(handler);
    crate::explorer::spawn_thread("folder-handler", move || {
        while let Ok(mut wanted) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                wanted = newer;
            }
            handler.sync(wanted);
        }
    });
    let mut applied: Option<bool> = None;
    let mut follow = move |store: &SettingsStore| {
        // Before settings load the values are defaults.
        if !store.loaded {
            return;
        }
        let wanted = store.settings.open_folders_in_arke;
        if applied.replace(wanted) != Some(wanted) {
            let _ = tx.send(wanted);
        }
    };
    follow(store.read(cx));
    cx.observe(store, move |store, cx| follow(store.read(cx)))
        .detach();
}

/// `arke --restore-explorer`: undoes the registration and turns the setting off so it is not
/// registered again at the next start. Returns whether everything worked.
pub(crate) fn restore_cli() -> bool {
    let restored = match Handler::real() {
        Some(handler) => handler.restore(),
        None => false,
    };
    let setting = crate::settings::default_path()
        .is_none_or(|path| crate::settings::turn_off_takeover(&path));
    restored && setting
}

/// Why File Explorer could not be made the folder handler again.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RestoreError {
    /// The registry could not be put back; the log says how.
    #[error("the previous folder handler could not be restored")]
    Failed,
}

/// Makes File Explorer open folders again (the uninstaller's step). Blocking; call from a
/// background thread or the uninstaller's own process. Nothing to undo when the environment keeps
/// the shell alone or Arke never registered.
pub(crate) fn restore_folder_handler() -> Result<(), RestoreError> {
    let Some(handler) = Handler::real() else {
        return Ok(());
    };
    if handler.restore() {
        Ok(())
    } else {
        Err(RestoreError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arke_core::folder_handler::{MemoryBackup, MemoryRegistry};
    use gpui::TestAppContext;
    use std::time::{Duration, Instant};

    const DIR_CMD: &str = r"Software\Classes\Folder\shell\open\command";

    fn wait_for(cx: &mut TestAppContext, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            cx.run_until_parked();
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_environment_can_keep_the_shell_alone() {
        assert!(allowed_here(|_| false));
        assert!(!allowed_here(|n| n == "ARKE_TAKEOVER_TEST_ROOT"));
        assert!(!allowed_here(|n| n == "ARKE_NO_FOLDER_HANDLER"));
    }

    #[gpui::test]
    fn toggling_the_setting_registers_and_restores_through_the_registry(cx: &mut TestAppContext) {
        let reg = Arc::new(MemoryRegistry::default());
        let backup = Arc::new(MemoryBackup::default());
        reg.put(DIR_CMD, "", r#""D:\Other\fm.exe" "%1""#);
        let before = reg.snapshot();
        let exe = PathBuf::from(r"C:\Apps\Arke\arke.exe");
        let store = cx.update(|cx| {
            let store = SettingsStore::global(cx);
            store.update(cx, SettingsStore::mark_loaded);
            follow_setting(
                &store,
                Handler::new(reg.clone(), backup.clone(), exe.clone()),
                cx,
            );
            store
        });

        // Off at the start and nothing registered: the registry is not touched.
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(reg.writes(), 0);

        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.update(|st| st.open_folders_in_arke = true, cx);
            });
        });
        wait_for(cx, "registered", || {
            reg.text(DIR_CMD, "")
                .is_some_and(|t| t.contains(r"C:\Apps\Arke\arke.exe"))
        });
        assert!(backup.load().unwrap().is_some(), "backup saved first");

        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.update(|st| st.open_folders_in_arke = false, cx);
            });
        });
        wait_for(cx, "restored", || backup.load().unwrap().is_none());
        assert_eq!(
            reg.snapshot(),
            before,
            "the other program's command is back"
        );
    }

    #[gpui::test]
    fn starting_with_the_setting_on_repairs_a_moved_exe(cx: &mut TestAppContext) {
        let reg = Arc::new(MemoryRegistry::default());
        let backup = Arc::new(MemoryBackup::default());
        folder_handler::apply(&*reg, &*backup, Path::new(r"C:\Old\arke.exe")).unwrap();
        cx.update(|cx| {
            let store = SettingsStore::global(cx);
            store.update(cx, |s, cx| {
                s.update(|st| st.open_folders_in_arke = true, cx);
                s.mark_loaded(cx);
            });
            follow_setting(
                &store,
                Handler::new(
                    reg.clone(),
                    backup.clone(),
                    PathBuf::from(r"E:\New\arke.exe"),
                ),
                cx,
            );
        });
        wait_for(cx, "repaired", || {
            reg.text(DIR_CMD, "")
                .is_some_and(|t| t.contains(r"E:\New\arke.exe"))
        });
    }

    #[gpui::test]
    fn starting_with_the_setting_off_undoes_a_leftover_registration(cx: &mut TestAppContext) {
        let reg = Arc::new(MemoryRegistry::default());
        let backup = Arc::new(MemoryBackup::default());
        folder_handler::apply(&*reg, &*backup, Path::new(r"C:\Old\arke.exe")).unwrap();
        cx.update(|cx| {
            let store = SettingsStore::global(cx);
            store.update(cx, SettingsStore::mark_loaded);
            follow_setting(
                &store,
                Handler::new(
                    reg.clone(),
                    backup.clone(),
                    PathBuf::from(r"C:\Old\arke.exe"),
                ),
                cx,
            );
        });
        wait_for(cx, "restored", || reg.snapshot().is_empty());
    }
}
