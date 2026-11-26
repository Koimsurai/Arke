//! Display formatting: file sizes, counts and type labels.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use crate::entry::{Entry, EntryKind};
use parking_lot::Mutex;
use rust_i18n::t;

/// Use base-1000 sizes (disk vendors) instead of base-1024 (Windows). Process-wide.
static DECIMAL_SIZES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switches the base; returns whether it changed.
pub fn set_decimal_sizes(decimal: bool) -> bool {
    DECIMAL_SIZES.swap(decimal, std::sync::atomic::Ordering::Relaxed) != decimal
}

/// File size with one decimal below 10. Base follows the setting (default 1024).
pub fn format_size(bytes: u64) -> String {
    format_size_base(
        bytes,
        DECIMAL_SIZES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// File size in base 1000 if `decimal`, otherwise 1024.
pub fn format_size_base(bytes: u64, decimal: bool) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let step = if decimal { 1000.0 } else { 1024.0 };
    if (bytes as f64) < step {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= step && unit < UNITS.len() - 1 {
        value /= step;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Integer with thousands separators, for the item count in the status bar.
pub fn format_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Text for the Type column: the name Windows has for the type (as File Explorer shows it, so
/// sorting agrees with it and names the user customised appear), once the background lookup
/// ([`start_type_names`]) has answered. Until then, and for extensions Windows has no name for,
/// the built-in name. When Arke's language is not Windows' display language ([`set_windows_language`])
/// extensions in the built-in table keep the built-in, translated name.
pub fn type_label(entry: &Entry) -> String {
    match entry.kind {
        EntryKind::Dir => return t!("type.folder").into_owned(),
        EntryKind::Link { is_dir: true } => return t!("type.folder_link").into_owned(),
        _ => {}
    }
    let ext = entry.extension();
    if ext.is_empty() {
        return t!("type.file").into_owned();
    }
    let known = known_type(ext);
    if (known.is_none() || windows_names_apply())
        && let Some(name) = system_type_name(ext)
    {
        return name.to_string();
    }
    if let Some(key) = known {
        return t!(key).into_owned();
    }
    t!("type.ext_file", ext = ext.to_uppercase()).into_owned()
}

/// Type names Windows gave, per extension (`None`: it has no usable one), for one UI language.
#[derive(Default)]
struct SystemNames {
    locale: String,
    names: HashMap<String, Option<Arc<str>>>,
    /// Extensions already sent to the lookup thread.
    asked: HashSet<String>,
}

impl SystemNames {
    /// Forgets the names when the UI language changed since they were stored.
    fn follow_language(&mut self) {
        let locale = rust_i18n::locale();
        if *locale != *self.locale {
            self.locale = locale.to_string();
            self.names.clear();
            self.asked.clear();
            NAMES_VERSION.fetch_add(1, Ordering::Relaxed);
            self.ask_known();
        }
    }

    /// Asks the lookup thread for every extension of the built-in table, so the names are there
    /// before the first list needs them. Only when Windows' names are used for them.
    fn ask_known(&mut self) {
        let Some(ask) = ASK.get() else { return };
        let windows = WINDOWS_LANGUAGE.lock().clone();
        self.ask_known_to(ask, windows.as_deref());
    }

    /// [`Self::ask_known`] with the channel and Windows' language given.
    fn ask_known_to(&mut self, ask: &flume::Sender<String>, windows: Option<&str>) {
        if !names_apply(&self.locale, windows) {
            return;
        }
        for ext in KNOWN_EXTENSIONS {
            if self.asked.insert((*ext).to_owned()) {
                let _ = ask.send((*ext).to_owned());
            }
        }
    }
}

/// The language Windows shows its names in, as one of Arke's locales; `None` when it is none of
/// them (Arke then keeps its own names for the built-in table).
static WINDOWS_LANGUAGE: Mutex<Option<String>> = Mutex::new(None);

/// Tells the type names which language Windows' own names are in. Windows' names replace the
/// built-in ones only when Arke's UI language is the same, so a Traditional Chinese UI on an
/// English Windows keeps reading Chinese.
pub fn set_windows_language(language: Option<&str>) {
    *WINDOWS_LANGUAGE.lock() = language.map(str::to_owned);
    let mut state = SYSTEM_NAMES.lock();
    state.follow_language();
    state.ask_known();
    drop(state);
    NAMES_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Whether Windows' names are used for the built-in table: the UI language is Windows' language.
fn names_apply(ui: &str, windows: Option<&str>) -> bool {
    windows == Some(ui)
}

fn windows_names_apply() -> bool {
    names_apply(&rust_i18n::locale(), WINDOWS_LANGUAGE.lock().as_deref())
}

static SYSTEM_NAMES: LazyLock<Mutex<SystemNames>> = LazyLock::new(Mutex::default);
/// Requests to the lookup thread; unset until [`start_type_names`].
static ASK: OnceLock<flume::Sender<String>> = OnceLock::new();
/// Bumped whenever a name arrives or the language changes; texts cached under an older one are
/// stale.
static NAMES_VERSION: AtomicU32 = AtomicU32::new(0);

/// Version of the known system type names (see [`type_label`]).
pub fn type_names_version() -> u32 {
    NAMES_VERSION.load(Ordering::Relaxed)
}

/// Tells the type names the UI language changed, so [`type_names_version`] moves now and views
/// sorted by type are ordered again at once, not when a type text is next asked for.
pub fn language_changed() {
    SYSTEM_NAMES.lock().follow_language();
}

/// The name for `ext` if Windows gave one; otherwise asks the lookup thread (once) and `None`.
fn system_type_name(ext: &str) -> Option<Arc<str>> {
    let mut state = SYSTEM_NAMES.lock();
    state.follow_language();
    if let Some(known) = state.names.get(ext) {
        return known.clone();
    }
    if let Some(ask) = ASK.get()
        && state.asked.insert(ext.to_owned())
    {
        let _ = ask.send(ext.to_owned());
    }
    drop(state);
    None
}

/// Records what Windows said the type name of `ext` is (`raw`, `None`: nothing). Names that are
/// not friendly (see [`friendly_type_name`]) count as none. Public for the tests of other crates.
pub fn set_type_name(ext: &str, raw: Option<&str>) {
    let name = raw.and_then(|raw| friendly_type_name(ext, raw));
    let mut state = SYSTEM_NAMES.lock();
    state.follow_language();
    state.names.insert(ext.to_owned(), name.map(Arc::from));
    drop(state);
    NAMES_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// `raw` as a type name, unless it is no name: Windows answers with the registered ProgID
/// ("Word.Document.12", no spaces and a dot) when the program is not installed, or with the
/// extension itself.
pub fn friendly_type_name(ext: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    let progid_like = !raw.contains(char::is_whitespace) && raw.contains('.');
    let is_ext = raw.eq_ignore_ascii_case(ext) || raw.eq_ignore_ascii_case(&format!(".{ext}"));
    (!raw.is_empty() && !progid_like && !is_ext).then(|| raw.to_owned())
}

/// Starts the thread that asks `source` for the type name of each extension [`type_label`] needs
/// (Windows' friendly name; it never touches a file), so the UI thread never waits for it.
/// `changed` runs after names arrived.
pub fn start_type_names(
    source: impl Fn(&str) -> Option<String> + Send + 'static,
    changed: impl Fn() + Send + 'static,
) {
    let (tx, rx) = flume::unbounded::<String>();
    if ASK.set(tx).is_err() {
        return;
    }
    {
        let mut state = SYSTEM_NAMES.lock();
        state.follow_language();
        state.ask_known();
    }
    let spawned = std::thread::Builder::new()
        .name("type-names".into())
        .spawn(move || {
            while let Ok(ext) = rx.recv() {
                set_type_name(&ext, source(&ext).as_deref());
                while let Ok(ext) = rx.try_recv() {
                    set_type_name(&ext, source(&ext).as_deref());
                }
                changed();
            }
        });
    if let Err(e) = spawned {
        log::warn!("type name thread: {e}");
    }
}

/// Every extension [`known_type`] has a name for (a test keeps the two in step); these are looked
/// up in the background at startup.
const KNOWN_EXTENSIONS: &[&str] = &[
    "txt", "log", "md", "exe", "msi", "dll", "lnk", "url", "bat", "cmd", "ps1", "zip", "7z", "rar",
    "jpg", "jpeg", "png", "gif", "webp", "svg", "ico", "mp3", "wav", "flac", "mp4", "mkv", "mov",
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "json", "xml", "html", "htm", "rs",
    "py", "js", "ts", "toml", "yaml", "yml", "iso",
];

/// Translation key for an extension (`type.*` in `locales/core.yml`).
fn known_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" | "log" => "type.text",
        "md" => "type.markdown",
        "exe" => "type.exe",
        "msi" => "type.msi",
        "dll" => "type.dll",
        "lnk" => "type.lnk",
        "url" => "type.url",
        "bat" | "cmd" => "type.batch",
        "ps1" => "type.ps1",
        "zip" => "type.zip",
        "7z" => "type.7z",
        "rar" => "type.rar",
        "jpg" | "jpeg" => "type.jpeg",
        "png" => "type.png",
        "gif" => "type.gif",
        "webp" => "type.webp",
        "svg" => "type.svg",
        "ico" => "type.ico",
        "mp3" => "type.mp3",
        "wav" => "type.wav",
        "flac" => "type.flac",
        "mp4" => "type.mp4",
        "mkv" => "type.mkv",
        "mov" => "type.mov",
        "pdf" => "type.pdf",
        "doc" | "docx" => "type.word",
        "xls" | "xlsx" => "type.excel",
        "ppt" | "pptx" => "type.powerpoint",
        "csv" => "type.csv",
        "json" => "type.json",
        "xml" => "type.xml",
        "html" | "htm" => "type.html",
        "rs" => "type.rust",
        "py" => "type.python",
        "js" => "type.javascript",
        "ts" => "type.typescript",
        "toml" => "type.toml",
        "yaml" | "yml" => "type.yaml",
        "iso" => "type.iso",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry::new(name, EntryKind::File, 1, None, 0)
    }

    /// Switching language moves the version at once (views re-sort by the new texts), not only
    /// when a text is next asked for.
    #[test]
    fn language_switch_moves_the_names_version() {
        rust_i18n::set_locale("en");
        language_changed();
        let before = type_names_version();
        language_changed();
        assert_eq!(
            type_names_version(),
            before,
            "same language: nothing changes"
        );
        rust_i18n::set_locale("zh-TW");
        language_changed();
        assert_ne!(type_names_version(), before);
        rust_i18n::set_locale("en");
    }

    #[test]
    fn friendly_names_reject_progids_and_the_extension() {
        let ok = |raw| friendly_type_name("dwg", raw);
        assert_eq!(ok("AutoCAD Drawing").as_deref(), Some("AutoCAD Drawing"));
        assert_eq!(
            ok(" Word 97-2003.doc ").as_deref(),
            Some("Word 97-2003.doc")
        );
        assert_eq!(ok("Word.Document.12"), None, "a ProgID");
        assert_eq!(ok("DWG"), None);
        assert_eq!(ok(".dwg"), None);
        assert_eq!(ok(""), None);
    }

    /// Built-in names win; others show Windows' name once known, else the built-in "XYZ File".
    #[test]
    fn type_label_decides_between_table_windows_and_fallback() {
        rust_i18n::set_locale("en");
        assert_eq!(type_label(&file("a.dwg")), "DWG File");
        set_type_name("dwg", Some("AutoCAD Drawing"));
        set_type_name("txt", Some("Something Else"));
        assert_eq!(type_label(&file("a.dwg")), "AutoCAD Drawing");
        assert_eq!(
            type_label(&file("a.txt")),
            "Text Document",
            "built-in table while Windows' language is unknown"
        );
        set_type_name("qqq", Some("Qqq.Document.3"));
        assert_eq!(type_label(&file("a.qqq")), "QQQ File", "ProgID rejected");
        set_type_name("rrr", None);
        assert_eq!(type_label(&file("a.rrr")), "RRR File");
    }

    /// Windows' name wins for the built-in table only when the UI language is Windows' language.
    #[test]
    fn windows_names_replace_built_in_ones_only_in_its_language() {
        rust_i18n::set_locale("en");
        language_changed();
        set_type_name("iso", Some("Disc Image File"));
        set_windows_language(Some("en"));
        assert_eq!(type_label(&file("a.iso")), "Disc Image File");
        set_windows_language(Some("zh-TW"));
        assert_eq!(type_label(&file("a.iso")), "Disc Image", "other language");
        set_windows_language(None);
        assert_eq!(type_label(&file("a.iso")), "Disc Image", "unknown language");
        assert!(names_apply("en", Some("en")));
        assert!(!names_apply("zh-TW", Some("en")));
        assert!(!names_apply("en", None));
    }

    /// Asking for the built-in table happens once per language, only when Windows' names apply,
    /// and a language change starts over (the cache is per UI language).
    #[test]
    fn known_extensions_are_asked_once_per_language() {
        let (tx, rx) = flume::unbounded::<String>();
        let mut state = SystemNames {
            locale: "en".into(),
            ..SystemNames::default()
        };
        state.ask_known_to(&tx, Some("zh-TW"));
        assert_eq!(rx.try_iter().count(), 0, "languages differ: nothing asked");
        state.ask_known_to(&tx, Some("en"));
        assert_eq!(rx.try_iter().count(), KNOWN_EXTENSIONS.len());
        state.ask_known_to(&tx, Some("en"));
        assert_eq!(rx.try_iter().count(), 0, "already asked");
        state.asked.clear();
        state.ask_known_to(&tx, Some("en"));
        assert_eq!(
            rx.try_iter().count(),
            KNOWN_EXTENSIONS.len(),
            "new language"
        );
    }

    #[test]
    fn known_extension_list_matches_the_table() {
        for ext in KNOWN_EXTENSIONS {
            assert!(known_type(ext).is_some(), "{ext}");
        }
        let unique: HashSet<_> = KNOWN_EXTENSIONS.iter().collect();
        assert_eq!(unique.len(), KNOWN_EXTENSIONS.len());
    }

    /// The lookup runs on its own thread, once per extension; the label shows the built-in name
    /// until the answer arrives.
    #[test]
    fn names_are_looked_up_off_the_calling_thread() {
        rust_i18n::set_locale("en");
        let caller = std::thread::current().id();
        let (asked_tx, asked_rx) = flume::unbounded::<(String, bool)>();
        let (changed_tx, changed_rx) = flume::unbounded::<()>();
        start_type_names(
            move |ext| {
                let _ = asked_tx.send((ext.to_owned(), std::thread::current().id() != caller));
                Some("AutoCAD Drawing".to_owned())
            },
            move || {
                let _ = changed_tx.send(());
            },
        );
        let before = type_names_version();
        assert_eq!(type_label(&file("a.dwg")), "DWG File");
        changed_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(type_label(&file("c.dwg")), "AutoCAD Drawing");
        assert_ne!(type_names_version(), before);
        // Other tests may have made the built-in table asked too; only dwg is of interest.
        let asked: Vec<_> = asked_rx
            .try_iter()
            .filter(|(ext, _)| ext == "dwg")
            .collect();
        assert_eq!(asked, [("dwg".to_owned(), true)]);
    }

    #[test]
    fn sizes() {
        // Base 1000: the same byte count gives a larger number.
        assert_eq!(format_size_base(999, true), "999 B");
        assert_eq!(format_size_base(1000, true), "1.0 KB");
        assert_eq!(format_size_base(1536, true), "1.5 KB");
        assert_eq!(format_size_base(50 * 1024 * 1024, true), "52 MB");
        assert_eq!(format_size_base(1536, false), "1.5 KB");
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(50 * 1024 * 1024), "50 MB");
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(5), "5");
        assert_eq!(format_count(50000), "50,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }
}
