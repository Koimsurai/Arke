//! Removing Arke from this PC: Settings > About > "Remove Arke from this PC", or
//! `arke --uninstall [--keep-settings] [--quiet] [--dry-run]` for package managers and scripts.
//!
//! Steps, each reported; a failed step doesn't stop the rest:
//! 1. Quit the resident process (it holds the log and index files open).
//! 2. Remove the search service, if installed (administrator approval, `--indexd uninstall`).
//! 3. Hand folders back to File Explorer (`folder_handler.rs`); a kept settings file gets "Open
//!    folders in Arke" turned off.
//! 4. Delete the Start menu and "Start with Windows" shortcuts.
//! 5. Delete saved SFTP/WebDAV passwords (Credential Manager, `Arke:<host>`).
//! 6. Clear the taskbar jump list.
//! 7. Delete `%LOCALAPPDATA%\Arke` (index, log, downloaded updates) and, unless settings are
//!    kept, `%APPDATA%\Arke` (settings, workspaces, trusted host keys).
//! 8. Delete the exe once this process has exited (a hidden `cmd` that retries until the file is
//!    free), unless Scoop or winget installed it: then the package manager owns it. A copy the
//!    installer put in place is left to its uninstaller, which runs this with `--quiet` first and
//!    then removes the files, the Start menu shortcut and the entry in Settings > Apps; "Remove
//!    Arke from this PC" starts that uninstaller instead.
//!
//! - Only Arke's own folders are deleted: a folder must be exactly `<base>\Arke` under the
//!   expected base and never a drive root or a profile folder, otherwise it is refused.
//! - The app doesn't remove itself in-process: it starts `arke --uninstall --wait-pid <pid>` and
//!   quits, so nothing of its own is open while the files go. That process shows the summary.
//! - Without `--quiet`, the command line asks first in a message box (a GUI program can't read
//!   the terminal reliably) and shows the summary the same way; `--quiet` asks nothing and only
//!   prints. The administrator prompt for the search service can't be skipped.
//! - `ARKE_UNINSTALL_SANDBOX` (automated checks with a temporary profile) leaves out the steps that
//!   reach beyond the profile folders: the search service, saved passwords and the jump list.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rust_i18n::t;

use crate::update::PackageManager;

/// `arke.exe --uninstall ...`
pub(crate) const ARG: &str = "--uninstall";
const KEEP_SETTINGS: &str = "--keep-settings";
const QUIET: &str = "--quiet";
const DRY_RUN: &str = "--dry-run";
/// Internal: the app that started the uninstall; waited for before anything is removed.
const WAIT_PID: &str = "--wait-pid";
/// Test-only: leave out steps that reach beyond the (temporary) profile folders.
const SANDBOX_ENV: &str = "ARKE_UNINSTALL_SANDBOX";

/// Command line options of `--uninstall`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) keep_settings: bool,
    pub(crate) quiet: bool,
    pub(crate) dry_run: bool,
    /// Started by the app, which already asked: wait for it to exit first.
    pub(crate) wait_pid: Option<u32>,
}

impl Options {
    /// Parses the arguments after `--uninstall`.
    pub(crate) fn parse(args: &[String]) -> Self {
        let mut options = Self::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                KEEP_SETTINGS => options.keep_settings = true,
                QUIET => options.quiet = true,
                DRY_RUN => options.dry_run = true,
                WAIT_PID => options.wait_pid = it.next().and_then(|p| p.parse().ok()),
                _ => {}
            }
        }
        options
    }
}

/// What the plan depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Machine {
    pub(crate) exe: PathBuf,
    pub(crate) appdata: Option<PathBuf>,
    pub(crate) local_appdata: Option<PathBuf>,
    /// The package manager that owns the exe.
    pub(crate) manager: Option<PackageManager>,
    /// The installer put the exe here (`installer/arke.iss`); its uninstaller removes it.
    pub(crate) setup: bool,
    /// The search service is installed.
    pub(crate) service: bool,
    /// Only touch the profile folders and the exe (see `ARKE_UNINSTALL_SANDBOX`).
    pub(crate) sandbox: bool,
}

impl Machine {
    /// This PC, as seen from the environment. `service`: whether the search service is installed
    /// (the app already knows; the command line asks Windows).
    pub(crate) fn detect(service: bool) -> Option<Self> {
        let env = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let sandbox = std::env::var_os(SANDBOX_ENV).is_some();
        Some(Self {
            exe: std::env::current_exe().ok()?,
            appdata: env("APPDATA"),
            local_appdata: env("LOCALAPPDATA"),
            manager: crate::update::package_manager(),
            setup: crate::update::installed_by_setup(),
            service: service && !sandbox,
            sandbox,
        })
    }
}

/// Which shortcut a step deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shortcut {
    StartMenu,
    Startup,
}

/// One thing the uninstall does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    QuitArke,
    RemoveService,
    /// Hand folders back to File Explorer; with kept settings, also turn the setting off in this
    /// file.
    RestoreFolderHandler {
        settings: Option<PathBuf>,
    },
    DeleteShortcut(Shortcut, PathBuf),
    ForgetPasswords,
    ClearJumpList,
    /// `%LOCALAPPDATA%\Arke`: rebuilt by Arke, nothing of the user's.
    DeleteData(PathBuf),
    /// `%APPDATA%\Arke`: settings, workspaces, trusted host keys.
    DeleteSettings(PathBuf),
    KeepSettings(PathBuf),
    DeleteExe(PathBuf),
    KeepExe(PackageManager),
    /// The installer's uninstaller removes the files and the Start menu shortcut.
    LeaveToSetup,
}

/// The steps for this PC, in order.
pub(crate) fn plan(m: &Machine, keep_settings: bool) -> Vec<Step> {
    let arke = |base: &PathBuf| base.join("Arke");
    let mut steps = vec![Step::QuitArke];
    if m.service {
        steps.push(Step::RemoveService);
    }
    steps.push(Step::RestoreFolderHandler {
        settings: m
            .appdata
            .as_ref()
            .filter(|_| keep_settings)
            .map(|a| arke(a).join("settings.json")),
    });
    if let Some(appdata) = &m.appdata {
        // The installer's own Start menu shortcut goes with its uninstaller.
        if !m.setup {
            steps.push(Step::DeleteShortcut(
                Shortcut::StartMenu,
                crate::win::start_menu_shortcut(appdata),
            ));
        }
        steps.push(Step::DeleteShortcut(
            Shortcut::Startup,
            crate::win::autostart_shortcut(appdata),
        ));
    }
    if !m.sandbox {
        steps.push(Step::ForgetPasswords);
        steps.push(Step::ClearJumpList);
    }
    if let Some(local) = &m.local_appdata {
        steps.push(Step::DeleteData(arke(local)));
    }
    if let Some(appdata) = &m.appdata {
        steps.push(if keep_settings {
            Step::KeepSettings(arke(appdata))
        } else {
            Step::DeleteSettings(arke(appdata))
        });
    }
    steps.push(match m.manager {
        Some(manager) => Step::KeepExe(manager),
        None if m.setup => Step::LeaveToSetup,
        None => Step::DeleteExe(m.exe.clone()),
    });
    steps
}

/// Why a path was not deleted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refused {
    /// Not Arke's own folder (or file), so it is never deleted.
    #[error("{}", t!("uninstall.refused", path = .0.display().to_string()))]
    NotArke(PathBuf),
}

/// Checks that `folder` is Arke's own: `<base>\Arke`, where `base` is an absolute folder below a
/// drive root and not the folder holding user profiles (`C:\Users`).
pub(crate) fn check_folder(folder: &Path) -> Result<(), Refused> {
    let refuse = || Refused::NotArke(folder.to_path_buf());
    let named_arke = folder
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Arke"));
    let base = folder.parent().ok_or_else(refuse)?;
    let base_is_root = base.parent().is_none();
    let base_holds_profiles = base
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Users"));
    let is_profile = std::env::var_os("USERPROFILE")
        .is_some_and(|p| crate::global_search::same_path(Path::new(&p), folder));
    if !folder.is_absolute() || !named_arke || base_is_root || base_holds_profiles || is_profile {
        return Err(refuse());
    }
    Ok(())
}

/// Checks that `exe` can go: an absolute `.exe` path the hidden `cmd` can quote safely.
fn check_exe(exe: &Path) -> Result<(), Refused> {
    let text = exe.to_string_lossy();
    let is_exe = exe
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if !exe.is_absolute() || !is_exe || exe.parent().is_none() || text.contains(['%', '"']) {
        return Err(Refused::NotArke(exe.to_path_buf()));
    }
    Ok(())
}

/// The `cmd` command line that deletes `exe` (and `exe.old`, left by an update) once nothing runs
/// it: a running exe can't be deleted, so it retries every second for a minute.
fn delete_exe_command(exe: &Path) -> String {
    let exe = exe.display();
    format!(
        "/d /q /c for /l %i in (1,1,60) do @(del /f /q \"{exe}.old\" 2>nul & \
         if exist \"{exe}\" (del /f /q \"{exe}\" 2>nul & \
         if exist \"{exe}\" ping -n 2 127.0.0.1 >nul))"
    )
}

/// Starts the hidden `cmd` that deletes the exe after this process exits.
fn schedule_exe_deletion(exe: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let system =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    std::process::Command::new(system.join("System32").join("cmd.exe"))
        .raw_arg(delete_exe_command(exe))
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

/// How a step went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Done, with an optional note (how many passwords, when the exe goes).
    Done(Option<String>),
    /// Nothing to do, and why.
    Skipped(String),
    Failed(String),
}

/// Short description of a step, for the confirmation list and the summary.
pub(crate) fn label(step: &Step) -> String {
    let path = |p: &Path| p.display().to_string();
    match step {
        Step::QuitArke => t!("uninstall.step_quit"),
        Step::RemoveService => t!("uninstall.step_service"),
        Step::RestoreFolderHandler { .. } => t!("uninstall.step_takeover"),
        Step::DeleteShortcut(Shortcut::StartMenu, _) => t!("uninstall.step_start_menu"),
        Step::DeleteShortcut(Shortcut::Startup, _) => t!("uninstall.step_startup"),
        Step::ForgetPasswords => t!("uninstall.step_passwords"),
        Step::ClearJumpList => t!("uninstall.step_jump_list"),
        Step::DeleteData(p) => t!("uninstall.step_data", path = path(p)),
        Step::DeleteSettings(p) => t!("uninstall.step_settings", path = path(p)),
        Step::KeepSettings(p) => t!("uninstall.step_keep_settings", path = path(p)),
        Step::DeleteExe(p) => t!("uninstall.step_exe", path = path(p)),
        Step::KeepExe(m) => t!(
            "uninstall.step_keep_exe",
            manager = m.name(),
            command = m.uninstall_command()
        ),
        Step::LeaveToSetup => t!("uninstall.step_leave_to_setup"),
    }
    .into_owned()
}

fn io_error(err: &std::io::Error) -> Outcome {
    Outcome::Failed(err.to_string())
}

/// Runs one step. Blocking.
fn run_step(step: &Step, exe: &Path) -> Outcome {
    match step {
        Step::QuitArke => {
            if crate::instance::quit_resident(Duration::from_secs(15)) {
                Outcome::Done(None)
            } else {
                Outcome::Failed(t!("uninstall.still_running").into_owned())
            }
        }
        Step::RemoveService => match crate::win::run_elevated(exe, "--indexd uninstall") {
            Ok(0) => Outcome::Done(None),
            Ok(code) => Outcome::Failed(t!("uninstall.exit_code", code = code).into_owned()),
            Err(err) => Outcome::Failed(err.to_string()),
        },
        Step::RestoreFolderHandler { settings } => {
            if let Err(err) = crate::folder_handler::restore_folder_handler() {
                return Outcome::Failed(err.to_string());
            }
            if settings
                .as_deref()
                .is_some_and(|path| !crate::settings::turn_off_takeover(path))
            {
                return Outcome::Failed(t!("uninstall.settings_unreadable").into_owned());
            }
            Outcome::Done(None)
        }
        Step::DeleteShortcut(_, path) => {
            if !path.exists() {
                return Outcome::Skipped(t!("uninstall.none").into_owned());
            }
            crate::win::remove_shortcut(path)
                .map_or_else(|e| io_error(&e), |()| Outcome::Done(None))
        }
        Step::ForgetPasswords => match arke_remote::creds::forget_all() {
            Ok(0) => Outcome::Skipped(t!("uninstall.none").into_owned()),
            Ok(n) => Outcome::Done(Some(tn!("uninstall.passwords_removed", n).into_owned())),
            Err(err) => Outcome::Failed(err.to_string()),
        },
        Step::ClearJumpList => crate::win_jumplist::clear()
            .map_or_else(|e| Outcome::Failed(e.to_string()), |()| Outcome::Done(None)),
        Step::DeleteData(folder) | Step::DeleteSettings(folder) => delete_folder(folder),
        Step::KeepSettings(_) | Step::KeepExe(_) | Step::LeaveToSetup => Outcome::Done(None),
        Step::DeleteExe(path) => match check_exe(path) {
            Err(refused) => Outcome::Failed(refused.to_string()),
            Ok(()) => schedule_exe_deletion(path).map_or_else(
                |e| io_error(&e),
                |()| Outcome::Done(Some(t!("uninstall.exe_later").into_owned())),
            ),
        },
    }
}

/// Deletes one of Arke's folders after [`check_folder`].
fn delete_folder(folder: &Path) -> Outcome {
    if let Err(refused) = check_folder(folder) {
        return Outcome::Failed(refused.to_string());
    }
    match std::fs::symlink_metadata(folder) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Outcome::Skipped(t!("uninstall.none").into_owned())
        }
        Err(e) => io_error(&e),
        // A link (junction) is removed itself; what it points to stays.
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_dir(folder)
            .or_else(|_| std::fs::remove_file(folder))
            .map_or_else(|e| io_error(&e), |()| Outcome::Done(None)),
        Ok(_) => {
            std::fs::remove_dir_all(folder).map_or_else(|e| io_error(&e), |()| Outcome::Done(None))
        }
    }
}

/// One line of the summary.
pub(crate) fn report_line(step: &Step, outcome: &Outcome) -> String {
    let label = label(step);
    match outcome {
        Outcome::Done(None) => format!("✓ {label}"),
        Outcome::Done(Some(note)) => format!("✓ {label} ({note})"),
        Outcome::Skipped(why) => format!("– {label} ({why})"),
        Outcome::Failed(err) => format!("✗ {label}: {err}"),
    }
}

/// The whole summary, and whether everything went through.
fn summary(results: &[(Step, Outcome)]) -> (String, bool) {
    let ok = results
        .iter()
        .all(|(_, o)| !matches!(o, Outcome::Failed(_)));
    let head = if ok {
        t!("uninstall.summary_ok")
    } else {
        t!("uninstall.summary_failed")
    };
    let lines: Vec<String> = results.iter().map(|(s, o)| report_line(s, o)).collect();
    (format!("{head}\n\n{}", lines.join("\n")), ok)
}

/// Waits up to 30 s for the app that started the uninstall to exit.
fn wait_for_exit(pid: u32) {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: only requests wait access; the handle is closed after use.
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(handle, 30_000);
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
    }
}

/// A message box from this windowless process. Returns whether the user chose Yes (or OK).
fn message_box(text: &str, ask: bool, warning: bool) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDOK, IDYES, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
        MB_YESNO, MessageBoxW,
    };
    use windows::core::PCWSTR;
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "Arke".encode_utf16().chain(std::iter::once(0)).collect();
    let mut style = MB_SETFOREGROUND;
    style |= if ask { MB_YESNO | MB_DEFBUTTON2 } else { MB_OK };
    style |= if ask || warning {
        MB_ICONWARNING
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let answer = unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style) };
    answer == IDYES || answer == IDOK
}

/// `arke --uninstall ...`: runs everything and returns the exit code (1 if a step failed).
pub(crate) fn run_cli(args: &[String]) -> std::process::ExitCode {
    use crate::bench_index::console;
    let options = Options::parse(args);
    let attached = console::attach();
    let lead = if attached { "\n" } else { "" };
    if let Some(pid) = options.wait_pid {
        wait_for_exit(pid);
    }
    // Asks the service manager only; nothing is changed.
    let Some(machine) = Machine::detect(arke_indexd::client::installed()) else {
        console::stderr("arke: cannot find its own exe\n");
        return std::process::ExitCode::FAILURE;
    };
    let steps = plan(&machine, options.keep_settings);
    let list: Vec<String> = steps
        .iter()
        .filter(|s| !matches!(s, Step::QuitArke))
        .map(|s| format!("• {}", label(s)))
        .collect();
    let list = list.join("\n");
    if options.dry_run {
        console::stdout(&format!("{lead}{}\n{list}\n", t!("uninstall.intro")));
        return std::process::ExitCode::SUCCESS;
    }
    // From the app the user already confirmed; from a terminal, ask (unless quiet).
    let ask = options.wait_pid.is_none() && !options.quiet;
    if ask
        && !message_box(
            &format!(
                "{}\n\n{}\n{list}",
                t!("uninstall.confirm_title"),
                t!("uninstall.intro")
            ),
            true,
            true,
        )
    {
        return std::process::ExitCode::FAILURE;
    }
    console::stdout(lead);
    let results: Vec<(Step, Outcome)> = steps
        .into_iter()
        .map(|step| {
            let outcome = run_step(&step, &machine.exe);
            console::stdout(&format!("{}\n", report_line(&step, &outcome)));
            (step, outcome)
        })
        .collect();
    let (text, ok) = summary(&results);
    if !options.quiet {
        message_box(&text, false, !ok);
    }
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// From the app: starts `arke --uninstall --wait-pid <this> [--keep-settings]`, which finishes
/// after the app has quit. The caller quits right after this returns `Ok`.
pub(crate) fn start_from_app(keep_settings: bool) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe()?;
    if crate::update::installed_by_setup()
        && let Some(dir) = exe.parent()
    {
        // The installer's uninstaller: it runs `--uninstall --quiet` for Arke's own changes, then
        // removes the files and the entry in Settings > Apps. The user already confirmed here.
        return std::process::Command::new(dir.join(crate::update::SETUP_UNINSTALLER))
            .arg("/SILENT")
            .arg(if keep_settings {
                "/KEEPSETTINGS"
            } else {
                "/REMOVESETTINGS"
            })
            .current_dir(std::env::temp_dir())
            .spawn()
            .map(drop);
    }
    let mut command = std::process::Command::new(exe);
    command
        .arg(ARG)
        .arg(WAIT_PID)
        .arg(std::process::id().to_string())
        // The summary in the language the app shows now (the settings may be gone by then).
        .env("ARKE_LANG", &*rust_i18n::locale())
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW);
    if keep_settings {
        command.arg(KEEP_SETTINGS);
    }
    command.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(root: &Path) -> Machine {
        Machine {
            exe: root.join("Tools").join("arke.exe"),
            appdata: Some(root.join("Roaming")),
            local_appdata: Some(root.join("Local")),
            manager: None,
            setup: false,
            service: false,
            sandbox: false,
        }
    }

    #[test]
    fn parses_the_command_line() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(Options::parse(&[]), Options::default());
        let all = Options::parse(&args(&[
            "--keep-settings",
            "--quiet",
            "--dry-run",
            "--wait-pid",
            "42",
        ]));
        assert!(all.keep_settings && all.quiet && all.dry_run);
        assert_eq!(all.wait_pid, Some(42));
        assert_eq!(Options::parse(&args(&["--wait-pid", "x"])).wait_pid, None);
    }

    #[test]
    fn plans_every_step_in_order() {
        let root = Path::new(r"C:\Users\me\AppData");
        let mut m = machine(root);
        m.service = true;
        let steps = plan(&m, false);
        assert_eq!(
            steps,
            vec![
                Step::QuitArke,
                Step::RemoveService,
                Step::RestoreFolderHandler { settings: None },
                Step::DeleteShortcut(
                    Shortcut::StartMenu,
                    PathBuf::from(
                        r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Arke.lnk"
                    )
                ),
                Step::DeleteShortcut(
                    Shortcut::Startup,
                    PathBuf::from(
                        r"C:\Users\me\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\Arke.lnk"
                    )
                ),
                Step::ForgetPasswords,
                Step::ClearJumpList,
                Step::DeleteData(PathBuf::from(r"C:\Users\me\AppData\Local\Arke")),
                Step::DeleteSettings(PathBuf::from(r"C:\Users\me\AppData\Roaming\Arke")),
                Step::DeleteExe(PathBuf::from(r"C:\Users\me\AppData\Tools\arke.exe")),
            ]
        );
    }

    #[test]
    fn leaves_an_installed_copy_to_its_uninstaller() {
        let root = Path::new(r"C:\Users\me\AppData");
        let mut m = machine(root);
        m.exe = PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Arke\arke.exe");
        m.setup = true;
        let steps = plan(&m, false);
        assert_eq!(steps.last(), Some(&Step::LeaveToSetup));
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, Step::DeleteShortcut(Shortcut::StartMenu, _))),
            "the installer's shortcut goes with its uninstaller"
        );
        assert!(steps.contains(&Step::DeleteSettings(root.join("Roaming").join("Arke"))));
        assert!(
            steps
                .iter()
                .any(|s| matches!(s, Step::DeleteShortcut(Shortcut::Startup, _)))
        );
    }

    #[test]
    fn keeps_settings_and_package_managed_exes() {
        let root = Path::new(r"C:\Users\me\AppData");
        let mut m = machine(root);
        m.exe = PathBuf::from(r"C:\Users\me\scoop\apps\arke\current\arke.exe");
        m.manager = crate::update::package_manager_for(&m.exe, &[]);
        let steps = plan(&m, true);
        assert!(
            !steps.contains(&Step::RemoveService),
            "no service installed"
        );
        assert!(steps.contains(&Step::RestoreFolderHandler {
            settings: Some(PathBuf::from(
                r"C:\Users\me\AppData\Roaming\Arke\settings.json"
            ))
        }));
        assert!(steps.contains(&Step::KeepSettings(PathBuf::from(
            r"C:\Users\me\AppData\Roaming\Arke"
        ))));
        assert!(
            !steps.iter().any(|s| matches!(s, Step::DeleteSettings(_))),
            "{steps:?}"
        );
        // Passwords go even when settings stay.
        assert!(steps.contains(&Step::ForgetPasswords));
        assert_eq!(steps.last(), Some(&Step::KeepExe(PackageManager::Scoop)));
        // winget's portable folder too; a plain folder is Arke's to delete.
        let winget = PathBuf::from(
            r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\Koimsurai.Arke_x\arke.exe",
        );
        m.manager = crate::update::package_manager_for(&winget, &[]);
        assert_eq!(
            plan(&m, false).last(),
            Some(&Step::KeepExe(PackageManager::Winget))
        );
        m.exe = PathBuf::from(r"D:\Apps\Arke\arke.exe");
        m.manager = crate::update::package_manager_for(&m.exe, &[]);
        assert_eq!(
            plan(&m, false).last(),
            Some(&Step::DeleteExe(m.exe.clone()))
        );
    }

    #[test]
    fn sandbox_and_missing_folders_leave_steps_out() {
        let mut m = machine(Path::new(r"C:\Temp\arke-test"));
        m.sandbox = true;
        m.appdata = None;
        let steps = plan(&m, false);
        assert!(!steps.contains(&Step::ForgetPasswords));
        assert!(!steps.contains(&Step::ClearJumpList));
        assert!(
            !steps
                .iter()
                .any(|s| matches!(s, Step::DeleteShortcut(..) | Step::DeleteSettings(_)))
        );
        assert!(steps.contains(&Step::DeleteData(PathBuf::from(
            r"C:\Temp\arke-test\Local\Arke"
        ))));
    }

    #[test]
    fn refuses_folders_that_are_not_arkes_own() {
        assert!(check_folder(Path::new(r"C:\Users\me\AppData\Roaming\Arke")).is_ok());
        assert!(check_folder(Path::new(r"C:\Users\me\AppData\Local\arke")).is_ok());
        for bad in [
            r"C:\Arke",
            r"C:\",
            r"C:\Users\me\AppData\Roaming",
            r"C:\Users\Arke",
            r"Arke",
            r"AppData\Arke",
            r"C:\Users\me\AppData\Roaming\Arke\..",
            r"C:\Users\me\AppData\Roaming\Arkey",
        ] {
            assert!(check_folder(Path::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn deletes_only_the_arke_folder_in_a_temp_profile() {
        let root = std::env::temp_dir().join(format!("arke-test-uninstall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let roaming = root.join("Roaming");
        let arke = roaming.join("Arke");
        std::fs::create_dir_all(arke.join("sub")).unwrap();
        std::fs::write(arke.join("settings.json"), b"{}").unwrap();
        std::fs::write(arke.join("sub").join("x.json"), b"{}").unwrap();
        let neighbour = roaming.join("Other");
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("keep.txt"), b"mine").unwrap();

        assert_eq!(delete_folder(&arke), Outcome::Done(None));
        assert!(!arke.exists());
        assert!(neighbour.join("keep.txt").exists(), "neighbours stay");
        assert!(roaming.exists(), "the parent stays");
        assert!(matches!(delete_folder(&arke), Outcome::Skipped(_)));
        assert!(matches!(delete_folder(&neighbour), Outcome::Failed(_)));
        assert!(neighbour.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn exe_deletion_quotes_the_path_and_retries() {
        let exe = Path::new(r"C:\Program Files (x86)\My Tools & Co\arke.exe");
        assert!(check_exe(exe).is_ok());
        let command = delete_exe_command(exe);
        assert!(command.contains(r#"del /f /q "C:\Program Files (x86)\My Tools & Co\arke.exe""#));
        assert!(command.contains(r#""C:\Program Files (x86)\My Tools & Co\arke.exe.old""#));
        assert!(command.contains("for /l %i in (1,1,60)"));
        assert!(check_exe(Path::new(r"C:\x\100%\arke.exe")).is_err());
        assert!(check_exe(Path::new(r"C:\x\arke.dll")).is_err());
        assert!(check_exe(Path::new(r"arke.exe")).is_err());
    }

    #[test]
    fn summary_lists_each_step() {
        let results = vec![
            (Step::ClearJumpList, Outcome::Done(None)),
            (
                Step::ForgetPasswords,
                Outcome::Skipped("nothing to remove".into()),
            ),
            (Step::RemoveService, Outcome::Failed("declined".into())),
        ];
        let (text, ok) = summary(&results);
        assert!(!ok);
        assert!(text.contains(&format!("✓ {}", label(&Step::ClearJumpList))));
        assert!(text.contains(&format!(
            "– {} (nothing to remove)",
            label(&Step::ForgetPasswords)
        )));
        assert!(text.contains(&format!("✗ {}: declined", label(&Step::RemoveService))));
        assert!(summary(&results[..2]).1);
    }
}
