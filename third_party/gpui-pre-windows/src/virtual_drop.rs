//! Arke: drops of virtual files.
//!
//! Some sources don't list files on disk (`CF_HDROP`). They describe each item with
//! `FileGroupDescriptorW` (or the ANSI `FileGroupDescriptor`) and hand over its bytes through
//! `FileContents` when asked: Outlook attachments, images dragged out of a browser, files inside a
//! zip folder, a phone's files. GPUI's drop events only carry paths, so such a drop lists one
//! marker path, [`VIRTUAL_DROP_ROOT`], and while the drop is dispatched the data object can be
//! taken with [`take_dropped_data_object`]. A data object that also lists files on disk is
//! dropped as those files, as before.
//!
//! When a drag offers neither, its formats are logged, so a report says what the source offered.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use windows::{
    Win32::{
        Foundation::S_OK,
        System::{
            Com::{
                CoTaskMemFree, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL,
            },
            DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW},
        },
    },
    core::w,
};

/// The one path a drop of virtual files lists.
pub const VIRTUAL_DROP_ROOT: &str = r"\\?\virtual-drop";

thread_local! {
    /// The data object of the virtual drop being dispatched.
    static DROPPED: RefCell<Option<IDataObject>> = const { RefCell::new(None) };
}

/// Whether these dropped paths are a drop of virtual files (see the module docs).
pub fn is_virtual_drop(paths: &[PathBuf]) -> bool {
    matches!(paths, [only] if only.as_path() == Path::new(VIRTUAL_DROP_ROOT))
}

/// The data object of a drop of virtual files, while that drop is being dispatched (inside the
/// `on_drop` handler, on the thread that received it). `None` afterwards or once taken.
pub fn take_dropped_data_object() -> Option<IDataObject> {
    DROPPED.with(|dropped| dropped.borrow_mut().take())
}

/// What a drop of `data` lists: its files on disk, else the marker of virtual files, else `None`.
pub fn drop_paths(data: &IDataObject) -> Option<Vec<PathBuf>> {
    // SAFETY: `data` is a live data object; the helper only queries and reads it.
    unsafe { crate::window::dropped_paths(data) }.map(|paths| paths.into_vec())
}

/// Runs `dispatch` with `data` available to [`take_dropped_data_object`].
pub(crate) fn offered_while<R>(data: &IDataObject, dispatch: impl FnOnce() -> R) -> R {
    DROPPED.with(|dropped| *dropped.borrow_mut() = Some(data.clone()));
    let result = dispatch();
    DROPPED.with(|dropped| dropped.borrow_mut().take());
    result
}

/// `FileGroupDescriptorW` and `FileGroupDescriptor`.
fn descriptor_formats() -> [u16; 2] {
    static FORMATS: OnceLock<[u16; 2]> = OnceLock::new();
    *FORMATS.get_or_init(|| {
        // SAFETY: registering a format name has no preconditions.
        unsafe {
            [
                RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16,
                RegisterClipboardFormatW(w!("FileGroupDescriptor")) as u16,
            ]
        }
    })
}

/// Whether `data` describes virtual files.
pub(crate) fn offers_virtual_files(data: &IDataObject) -> bool {
    descriptor_formats().into_iter().any(|format| {
        let query = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        // SAFETY: `query` lives for the call.
        format != 0 && unsafe { data.QueryGetData(&query) } == S_OK
    })
}

/// The formats `data` offers, by name, at most 64 (for a log line).
fn format_names(data: &IDataObject) -> Vec<String> {
    // SAFETY: enumerating is read-only; each FORMATETC's target device is freed as documented.
    unsafe {
        let Ok(list) = data.EnumFormatEtc(DATADIR_GET.0 as u32) else {
            return Vec::new();
        };
        let mut formats = Vec::new();
        loop {
            let mut one = [FORMATETC::default()];
            let mut fetched = 0;
            if list.Next(&mut one, Some(&mut fetched)) != S_OK || fetched == 0 {
                break;
            }
            let [format] = one;
            if !format.ptd.is_null() {
                CoTaskMemFree(Some(format.ptd as _));
            }
            let mut name = [0u16; 128];
            let len = GetClipboardFormatNameW(u32::from(format.cfFormat), &mut name);
            let name = if len > 0 {
                String::from_utf16_lossy(&name[..len as usize])
            } else {
                standard_format_name(format.cfFormat)
            };
            formats.push(format!("{name} (tymed {})", format.tymed));
            if formats.len() >= 64 {
                break;
            }
        }
        formats
    }
}

/// The name of a predefined clipboard format, which `GetClipboardFormatNameW` doesn't know.
fn standard_format_name(format: u16) -> String {
    match format {
        1 => "CF_TEXT".to_owned(),
        2 => "CF_BITMAP".to_owned(),
        8 => "CF_DIB".to_owned(),
        13 => "CF_UNICODETEXT".to_owned(),
        15 => "CF_HDROP".to_owned(),
        16 => "CF_LOCALE".to_owned(),
        17 => "CF_DIBV5".to_owned(),
        other => other.to_string(),
    }
}

/// Logs the formats `data` offers (a drag nothing here can take).
pub(crate) fn log_formats(data: &IDataObject) {
    log::info!(
        "drag offers no files; formats: {}",
        format_names(data).join(", ")
    );
}

/// What a drop takes from a data object, for the log: `paths` is what the drop lists.
fn describe_choice(offers_virtual: bool, paths: &[PathBuf]) -> String {
    if is_virtual_drop(paths) {
        "virtual files (FileGroupDescriptor + FileContents)".to_owned()
    } else if offers_virtual {
        format!(
            "CF_HDROP, {} path(s); virtual files are offered too and left unused",
            paths.len()
        )
    } else {
        format!("CF_HDROP, {} path(s)", paths.len())
    }
}

/// Logs the formats a drop offers and which of them it takes, so a report says what the source
/// handed over (a source can offer real files and virtual ones at once, and the two end up with
/// different attributes).
pub(crate) fn log_drop(data: &IDataObject, paths: &[PathBuf]) {
    log::info!(
        "drop offers: {}; taking: {}",
        format_names(data).join(", "),
        describe_choice(offers_virtual_files(data), paths)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_which_format_a_drop_takes() {
        let marker = [PathBuf::from(VIRTUAL_DROP_ROOT)];
        let files = [PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\b.txt")];
        assert!(describe_choice(true, &marker).starts_with("virtual files"));
        assert_eq!(describe_choice(false, &files), "CF_HDROP, 2 path(s)");
        assert!(describe_choice(true, &files).contains("left unused"));
    }

    #[test]
    fn names_the_predefined_formats() {
        assert_eq!(standard_format_name(15), "CF_HDROP");
        assert_eq!(standard_format_name(999), "999");
    }
}
