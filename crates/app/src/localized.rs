//! Folders' localized names (#370): what File Explorer shows for a folder whose `desktop.ini`
//! names it, such as the user folder's Desktop, Documents and Downloads in a language other than
//! English (`LocalizedResourceName=@%SystemRoot%\system32\shell32.dll,-21769`). Windows reads
//! `desktop.ini` only for folders with the read-only or system attribute, and so does Arke.

use std::path::Path;

/// Windows `FILE_ATTRIBUTE_READONLY` and `FILE_ATTRIBUTE_SYSTEM`: a folder with either may have a
/// `desktop.ini` Windows reads.
const MARKED: u32 = 0x1 | 0x4;

/// Whether Windows would read the `desktop.ini` of a folder with these attributes.
pub(crate) const fn may_have_name(attributes: u32) -> bool {
    attributes & MARKED != 0
}

/// A `desktop.ini` is a few hundred bytes; anything far larger isn't one Windows wrote.
const MAX_INI: u64 = 64 * 1024;

/// The localized name of the folder `dir`, from its `desktop.ini`; `None` without one. Reads the
/// disk: background thread.
pub(crate) fn read(fs: &dyn arke_core::Fs, dir: &Path) -> Option<String> {
    let bytes = fs.read(&dir.join("desktop.ini"), MAX_INI).ok()?;
    let value = name_in(&decode(&bytes))?;
    let name = resolve(&value);
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// `desktop.ini` as text: UTF-16 with a byte order mark (what Windows writes), UTF-8, or the
/// ANSI code page read leniently.
fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            String::from_utf16_lossy(&units)
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// `LocalizedResourceName` in the `[.ShellClassInfo]` section.
fn name_in(ini: &str) -> Option<String> {
    let mut in_section = false;
    for line in ini.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = section.trim().eq_ignore_ascii_case(".ShellClassInfo");
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("LocalizedResourceName")
        {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
    }
    None
}

/// `@file.dll,-123` read from the file's string table (environment variables expanded); any
/// other value is the name itself.
#[cfg(windows)]
fn resolve(value: &str) -> String {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows::Win32::UI::Shell::SHLoadIndirectString;
    use windows::core::{HSTRING, PCWSTR};
    if !value.starts_with('@') {
        return value.to_owned();
    }
    let source = HSTRING::from(value);
    let mut expanded = vec![0u16; 1024];
    // SAFETY: source is NUL-terminated; the buffer and its length are passed together.
    let len = unsafe { ExpandEnvironmentStringsW(&source, Some(&mut expanded)) } as usize;
    let expanded = if len == 0 || len > expanded.len() {
        source.to_vec()
    } else {
        expanded[..len.saturating_sub(1)].to_vec()
    };
    let mut wide = expanded;
    wide.push(0);
    let mut out = vec![0u16; 512];
    // SAFETY: `wide` is NUL-terminated; the output buffer is ours.
    match unsafe { SHLoadIndirectString(PCWSTR(wide.as_ptr()), &mut out, None) } {
        Ok(()) => {
            let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
            String::from_utf16_lossy(&out[..end])
        }
        Err(_) => String::new(),
    }
}

#[cfg(not(windows))]
fn resolve(value: &str) -> String {
    if value.starts_with('@') {
        String::new()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_read_from_the_shell_class_info_section() {
        let ini = "[ViewState]\r\nLocalizedResourceName=no\r\n[.ShellClassInfo]\r\nIconResource=x\r\n localizedresourcename = 文件 \r\n";
        assert_eq!(name_in(ini).as_deref(), Some("文件"));
        assert_eq!(name_in("[.ShellClassInfo]\r\nIconResource=x\r\n"), None);
        assert_eq!(
            name_in("[.ShellClassInfo]\r\nLocalizedResourceName=\r\n"),
            None
        );
    }

    #[test]
    fn utf16_and_utf8_files_are_read() {
        let text = "[.ShellClassInfo]\r\nLocalizedResourceName=下载\r\n";
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(name_in(&decode(&utf16)).as_deref(), Some("下载"));
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend(text.as_bytes());
        assert_eq!(name_in(&decode(&utf8)).as_deref(), Some("下载"));
    }

    #[test]
    fn only_marked_folders_are_read() {
        assert!(may_have_name(0x1 | 0x10));
        assert!(may_have_name(0x4 | 0x10));
        assert!(!may_have_name(0x10));
    }

    /// A real `desktop.ini` with a plain name and one with a resource string Windows has
    /// (shell32's "Documents"), both read from a temporary folder.
    #[cfg(windows)]
    #[test]
    fn a_folder_s_name_is_read_from_its_desktop_ini() {
        let root = std::env::temp_dir().join(format!("arke-test-localized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |name: &str, value: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let text = format!("[.ShellClassInfo]\r\nLocalizedResourceName={value}\r\n");
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            std::fs::write(dir.join("desktop.ini"), bytes).unwrap();
            dir
        };
        let plain = write("Plain", "我的資料");
        let docs = write("Docs", r"@%SystemRoot%\system32\shell32.dll,-21770");
        let fs = arke_core::RealFs;
        assert_eq!(read(&fs, &plain).as_deref(), Some("我的資料"));
        let docs_name = read(&fs, &docs);
        assert!(
            docs_name.as_deref().is_some_and(|n| !n.starts_with('@')),
            "{docs_name:?}"
        );
        assert_eq!(read(&fs, &root), None, "no desktop.ini");
        let _ = std::fs::remove_dir_all(&root);
    }
}
