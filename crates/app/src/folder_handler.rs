//! Keeps the folder handler registration (`arke_core::folder_handler`) in step with the "Open
//! folders in Arke" setting, and implements `arke --restore-explorer`.
//!
//! The registry and the backup file are reached through traits, so tests drive the whole flow with
//! in-memory fakes and never touch the real registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arke_core::folder_handler::{self, BackupStore, FileBackup, Registry, VerbMode};
use gpui::{App, Entity};

use crate::settings::SettingsStore;

/// Whether the shell reaches Arke's folder-open COM class (`win_folder_com::works`).
pub(crate) type ComCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Registers and unregisters Arke as the folder handler.
pub(crate) struct Handler {
    registry: Arc<dyn Registry>,
    backup: Arc<dyn BackupStore>,
    /// The exe the registered commands start.
    exe: PathBuf,
    /// Decides whether the folder verbs call Arke over COM (#340); without it (an Arke that is not
    /// resident, which doesn't answer COM) the verbs keep the way they are registered.
    com_check: Option<ComCheck>,
}

impl Handler {
    /// The real thing: this exe, the current user's registry, `%APPDATA%\Arke`'s backup file.
    /// `None` when the exe or the settings folder is unknown, or when a test environment asks to
    /// keep the shell alone.
    pub(crate) fn real() -> Option<Self> {
        if !shell_allowed() {
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
            com_check: None,
        }
    }

    /// Lets `check` decide whether the folder verbs use COM each time the handler is registered.
    pub(crate) fn with_com_check(mut self, check: ComCheck) -> Self {
        self.com_check = Some(check);
        self
    }

    /// Registers the handler. Without a check the verbs keep reaching Arke the way they do now.
    /// With one, only the COM class (and its backup entry) goes in first, since the check needs
    /// it; the folder verbs are written once, after the check, straight in the mode it found. So
    /// when COM works they never hold an empty `DelegateExecute` on the way, not even while
    /// repairing a registration (#540). A check that passes again changes nothing.
    fn register(&self, win_e: bool) -> Result<folder_handler::Applied, folder_handler::Error> {
        let now = if folder_handler::uses_com(&*self.registry) {
            VerbMode::Com
        } else {
            verbs_without_com()
        };
        let Some(check) = &self.com_check else {
            return folder_handler::apply_mode(
                &*self.registry,
                &*self.backup,
                &self.exe,
                win_e,
                now,
            );
        };
        let class = folder_handler::register_com_class(&*self.registry, &*self.backup, &self.exe)?;
        let mode = if check() {
            VerbMode::Com
        } else {
            verbs_without_com()
        };
        if mode != now {
            log::info!(
                "folder handler: folder verbs {}",
                match mode {
                    VerbMode::Com => "call Arke over COM",
                    VerbMode::Command => "run the command (COM does not reach Arke)",
                }
            );
        }
        let verbs =
            folder_handler::apply_mode(&*self.registry, &*self.backup, &self.exe, win_e, mode)?;
        Ok(folder_handler::Applied {
            written: class.written + verbs.written,
            repaired: class.repaired + verbs.repaired,
            dropped: verbs.dropped,
        })
    }

    /// Makes the registry match `wanted`: on registers (or repairs) the handler, Win+E included
    /// only with `win_e`; off restores the previous values when Arke registered anything.
    pub(crate) fn sync(&self, wanted: bool, win_e: bool) {
        if wanted {
            match self.register(win_e) {
                Ok(done) if done.dropped > 0 => log::info!(
                    "folder handler: Win+E back to File Explorer ({} values)",
                    done.dropped
                ),
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

/// How the folder verbs reach Arke when COM is not known to: the check failed (DCOM refuses a
/// per-user class whose exe is in a folder an administrator created, such as a per-machine install
/// under Program Files; see `win_folder_com`), or this Arke is not resident and cannot check while
/// the verbs aren't using COM yet. For now they run the command, which takes an empty
/// `DelegateExecute`: the shape some antivirus programs flag (#540). This is the one place to
/// change once per-machine installs have been tried on a real machine.
const fn verbs_without_com() -> VerbMode {
    VerbMode::Command
}

/// `%APPDATA%\Arke`.
fn settings_dir() -> Option<PathBuf> {
    crate::settings::default_path()?
        .parent()
        .map(Path::to_path_buf)
}

/// Whether this process may change how the shell opens folders (the registration, the Win+E
/// hook); live checks and test runs turn it off through the environment.
pub(crate) fn shell_allowed() -> bool {
    allowed_here(|name| std::env::var_os(name).is_some())
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
    let (tx, rx) = flume::unbounded::<(bool, bool)>();
    let handler = Arc::new(handler);
    crate::explorer::spawn_thread("folder-handler", move || {
        while let Ok(mut wanted) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                wanted = newer;
            }
            handler.sync(wanted.0, wanted.1);
        }
    });
    let mut applied: Option<(bool, bool)> = None;
    let mut follow = move |store: &SettingsStore| {
        // Before settings load the values are defaults.
        if !store.loaded {
            return;
        }
        let wanted = (
            store.settings.open_folders_in_arke,
            store.settings.win_e_opens_arke,
        );
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

/// Whether folders or Win+E are registered to this exe (and not to another copy of Arke).
pub(crate) fn handled_by_this_exe() -> bool {
    Handler::real().is_some_and(|h| folder_handler::handled_by(&*h.registry, &h.exe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arke_core::folder_handler::{MemoryBackup, MemoryRegistry, RegValue};
    use gpui::TestAppContext;
    use std::time::{Duration, Instant};

    const DIR_CMD: &str = r"Software\Classes\Folder\shell\open\command";
    const EXPLORE_CMD: &str = r"Software\Classes\Folder\shell\explore\command";
    const COM_SERVER: &str =
        r"Software\Classes\CLSID\{8F3B6C21-4E7A-4D2B-9C5E-A1D04F7E3340}\LocalServer32";
    const STOCK_DELEGATE: &str = "{11dbb47c-a525-400b-9e80-a54615a090c0}";

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
                Handler::new(
                    Arc::clone(&reg) as Arc<dyn Registry>,
                    Arc::clone(&backup) as Arc<dyn BackupStore>,
                    exe.clone(),
                ),
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

    /// The resident Arke's check decides whether the folder verbs call it over COM, both ways;
    /// without a check the verbs stay as they are (#340).
    #[test]
    fn the_com_check_decides_how_the_folder_verbs_reach_arke() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let reg = Arc::new(MemoryRegistry::default());
        let backup = Arc::new(MemoryBackup::default());
        let reaches = Arc::new(AtomicBool::new(true));
        let answer = Arc::clone(&reaches);
        let handler = |check: bool| {
            let h = Handler::new(
                Arc::clone(&reg) as Arc<dyn Registry>,
                Arc::clone(&backup) as Arc<dyn BackupStore>,
                PathBuf::from(r"C:\Apps\Arke\arke.exe"),
            );
            if check {
                let answer = Arc::clone(&answer);
                h.with_com_check(Arc::new(move || answer.load(Ordering::SeqCst)))
            } else {
                h
            }
        };
        let delegate = || reg.text(DIR_CMD, "DelegateExecute");
        handler(true).sync(true, false);
        assert_eq!(delegate().as_deref(), Some(folder_handler::ARKE_CLSID));
        handler(false).sync(true, false);
        assert_eq!(delegate().as_deref(), Some(folder_handler::ARKE_CLSID));
        reaches.store(false, Ordering::SeqCst);
        handler(true).sync(true, false);
        assert_eq!(delegate().as_deref(), Some(""));
        handler(true).sync(false, false);
        assert!(
            reg.snapshot().is_empty(),
            "turning off removes the class too"
        );
    }

    /// A [`MemoryRegistry`] that keeps the whole state after every change, so a test sees each
    /// state the shell could have read in between.
    #[derive(Default)]
    struct Recording {
        reg: MemoryRegistry,
        states: parking_lot::Mutex<Vec<Vec<(String, String, String)>>>,
    }

    impl Recording {
        fn record(&self) {
            let state = self.reg.snapshot();
            self.states.lock().push(state);
        }

        /// How many recorded states had a folder verb with an empty `DelegateExecute`.
        fn empty_delegates(&self) -> usize {
            let verbs = [DIR_CMD.to_lowercase(), EXPLORE_CMD.to_lowercase()];
            self.states
                .lock()
                .iter()
                .filter(|state| {
                    state.iter().any(|(key, name, text)| {
                        verbs.contains(key) && name == "delegateexecute" && text.is_empty()
                    })
                })
                .count()
        }
    }

    impl Registry for Recording {
        fn read(&self, key: &str, value: &str) -> Result<Option<RegValue>, folder_handler::Error> {
            self.reg.read(key, value)
        }

        fn key_exists(&self, key: &str) -> Result<bool, folder_handler::Error> {
            self.reg.key_exists(key)
        }

        fn write(
            &self,
            key: &str,
            value: &str,
            data: &RegValue,
        ) -> Result<(), folder_handler::Error> {
            self.reg.write(key, value, data)?;
            self.record();
            Ok(())
        }

        fn delete_value(&self, key: &str, value: &str) -> Result<(), folder_handler::Error> {
            self.reg.delete_value(key, value)?;
            self.record();
            Ok(())
        }

        fn delete_key_if_empty(&self, key: &str) -> Result<bool, folder_handler::Error> {
            let gone = self.reg.delete_key_if_empty(key)?;
            self.record();
            Ok(gone)
        }
    }

    /// When the check passes, the folder verbs go straight to Arke's class id: turning the setting
    /// on, repairing a moved exe and repairing a half-missing registration never leave an empty
    /// `DelegateExecute` on them, the check runs with the class registered and the verbs untouched,
    /// and turning off still restores exactly (#540).
    #[test]
    fn a_passing_check_never_leaves_an_empty_delegate_on_the_folder_verbs() {
        let rec = Arc::new(Recording::default());
        let backup = Arc::new(MemoryBackup::default());
        rec.reg.put(DIR_CMD, "DelegateExecute", STOCK_DELEGATE);
        let before = rec.reg.snapshot();
        let handler = |exe: &str| {
            let seen = Arc::clone(&rec);
            let exe_text = exe.to_owned();
            Handler::new(
                Arc::clone(&rec) as Arc<dyn Registry>,
                Arc::clone(&backup) as Arc<dyn BackupStore>,
                PathBuf::from(exe),
            )
            .with_com_check(Arc::new(move || {
                assert_eq!(
                    seen.reg.text(COM_SERVER, "").as_deref(),
                    Some(format!(r#""{exe_text}""#).as_str()),
                    "the class is registered before the check"
                );
                true
            }))
        };

        // Turned on: the verbs are not touched before the check.
        let fresh = Handler::new(
            Arc::clone(&rec) as Arc<dyn Registry>,
            Arc::clone(&backup) as Arc<dyn BackupStore>,
            PathBuf::from(r"C:\Apps\Arke\arke.exe"),
        )
        .with_com_check({
            let seen = Arc::clone(&rec);
            Arc::new(move || {
                assert_eq!(
                    seen.reg.text(DIR_CMD, ""),
                    None,
                    "no command before the check"
                );
                assert_eq!(
                    seen.reg.text(DIR_CMD, "DelegateExecute").as_deref(),
                    Some(STOCK_DELEGATE),
                    "the verbs are untouched before the check"
                );
                assert!(seen.reg.text(COM_SERVER, "").is_some());
                true
            })
        });
        fresh.sync(true, false);
        for key in [DIR_CMD, EXPLORE_CMD] {
            assert_eq!(
                rec.reg.text(key, "DelegateExecute").as_deref(),
                Some(folder_handler::ARKE_CLSID)
            );
        }
        assert!(
            backup
                .load()
                .unwrap()
                .is_some_and(|b| b.entries.iter().any(|e| e.key == COM_SERVER)),
            "the class is in the backup"
        );

        // The exe moved.
        handler(r"E:\New\arke.exe").sync(true, false);
        assert!(
            rec.reg
                .text(DIR_CMD, "")
                .is_some_and(|t| t.contains(r"E:\New\arke.exe"))
        );

        // Something removed one of the values: the repair goes straight back to COM.
        rec.reg.delete_value(DIR_CMD, "DelegateExecute").unwrap();
        handler(r"E:\New\arke.exe").sync(true, false);
        assert_eq!(
            rec.reg.text(DIR_CMD, "DelegateExecute").as_deref(),
            Some(folder_handler::ARKE_CLSID)
        );

        assert!(!rec.states.lock().is_empty(), "writes were recorded");
        assert_eq!(rec.empty_delegates(), 0, "{:?}", rec.states.lock());

        handler(r"E:\New\arke.exe").sync(false, false);
        assert_eq!(rec.reg.snapshot(), before, "turning off restores exactly");
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
                    Arc::clone(&reg) as Arc<dyn Registry>,
                    Arc::clone(&backup) as Arc<dyn BackupStore>,
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
                    Arc::clone(&reg) as Arc<dyn Registry>,
                    Arc::clone(&backup) as Arc<dyn BackupStore>,
                    PathBuf::from(r"C:\Old\arke.exe"),
                ),
                cx,
            );
        });
        wait_for(cx, "restored", || reg.snapshot().is_empty());
    }
}
