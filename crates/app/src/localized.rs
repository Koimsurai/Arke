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
    let value = name_in(&decode(&bytes, ANSI).0)?;
    let name = resolve(&value);
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Renames the folder `dir` as File Explorer does when its name comes from `desktop.ini`: the
/// folder keeps its own name (programs find Documents by it) and `LocalizedResourceName` becomes
/// `name`; the rest of the file stays, in the encoding it was in. Writes the disk: background
/// thread.
pub(crate) fn write_name(dir: &Path, name: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = dir.join("desktop.ini");
    let (text, encoding) = std::fs::read(&path)
        .map_or_else(|_| (String::new(), Encoding::Utf16), |b| decode(&b, ANSI));
    let bytes = encode(&with_name(&text, name), encoding);
    // Opened for writing, not created anew: Windows refuses to replace a hidden system file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)?;
    file.write_all(&bytes)
}

/// `ini` with `LocalizedResourceName` in `[.ShellClassInfo]` set to `name` (the section added if
/// it isn't there).
fn with_name(ini: &str, name: &str) -> String {
    let line = format!("LocalizedResourceName={name}");
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut section_seen = false;
    let mut done = false;
    for raw in ini.lines() {
        let trimmed = raw.trim();
        if let Some(section) = trimmed.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if in_section && !done {
                out.push(line.clone());
                done = true;
            }
            in_section = section.trim().eq_ignore_ascii_case(".ShellClassInfo");
            section_seen |= in_section;
            out.push(raw.to_owned());
            continue;
        }
        if in_section
            && let Some((key, _)) = trimmed.split_once('=')
            && key.trim().eq_ignore_ascii_case("LocalizedResourceName")
        {
            if !done {
                out.push(line.clone());
                done = true;
            }
            continue;
        }
        out.push(raw.to_owned());
    }
    if !done {
        if !section_seen {
            out.push("[.ShellClassInfo]".to_owned());
        }
        out.push(line);
    }
    let mut text = out.join("\r\n");
    text.push_str("\r\n");
    text
}

/// The ANSI code page: the system's (`CP_ACP`), which is how Windows reads a `desktop.ini` with no
/// byte order mark. Tests pass a fixed one.
const ANSI: u32 = 0;

/// How a `desktop.ini` was written, so a rename writes it back the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    /// UTF-16 with a byte order mark: what Windows writes.
    Utf16,
    /// UTF-8 with a byte order mark.
    Utf8Bom,
    /// UTF-8 with no byte order mark (some tools write it).
    Utf8,
    /// No byte order mark and not UTF-8: an ANSI code page (older files, other programs': GBK,
    /// Big5, Shift-JIS). Plain ASCII is this too.
    Ansi(u32),
}

/// `desktop.ini` as text, and how it was encoded. With no byte order mark it is UTF-8 when the
/// bytes are valid UTF-8 (and not plain ASCII), otherwise code page `ansi`.
fn decode(bytes: &[u8], ansi: u32) -> (String, Encoding) {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair))
                .collect();
            (String::from_utf16_lossy(&units), Encoding::Utf16)
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => (
            String::from_utf8_lossy(rest).into_owned(),
            Encoding::Utf8Bom,
        ),
        _ if !bytes.is_ascii() && std::str::from_utf8(bytes).is_ok() => {
            (String::from_utf8_lossy(bytes).into_owned(), Encoding::Utf8)
        }
        _ => (ansi::decode(bytes, ansi), Encoding::Ansi(ansi)),
    }
}

/// `text` written as `encoding`. A name the ANSI code page can't hold makes the file UTF-16 (what
/// Windows itself writes) rather than turning those characters into question marks.
fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    let utf16 = || {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    };
    match encoding {
        Encoding::Utf16 => utf16(),
        Encoding::Utf8Bom => [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat(),
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Ansi(page) => ansi::encode(text, page).unwrap_or_else(utf16),
    }
}

#[cfg(windows)]
mod ansi {
    use windows::Win32::Globalization::{
        MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar, WideCharToMultiByte,
    };
    use windows::core::PCSTR;

    /// `bytes` in code page `page`.
    pub(super) fn decode(bytes: &[u8], page: u32) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
        // SAFETY: pure conversion; the first call gets the length, the second fills a buffer of it.
        let wide = unsafe {
            let len = MultiByteToWideChar(page, flags, bytes, None);
            let Ok(len) = usize::try_from(len) else {
                return String::from_utf8_lossy(bytes).into_owned();
            };
            let mut buf = vec![0u16; len];
            let n = MultiByteToWideChar(page, flags, bytes, Some(&mut buf));
            buf.truncate(usize::try_from(n).unwrap_or(0));
            buf
        };
        String::from_utf16_lossy(&wide)
    }

    /// `text` in code page `page`; `None` if a character has no place in it.
    pub(super) fn encode(text: &str, page: u32) -> Option<Vec<u8>> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        if wide.is_empty() {
            return Some(Vec::new());
        }
        let mut lossy = windows::core::BOOL(0);
        // SAFETY: pure conversion; the first call gets the length, the second fills a buffer of
        // it. The flag pointer is valid for the call.
        unsafe {
            let len = WideCharToMultiByte(page, 0, &wide, None, PCSTR::null(), None);
            let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
            let mut out = vec![0u8; len];
            let n = WideCharToMultiByte(
                page,
                0,
                &wide,
                Some(&mut out),
                PCSTR::null(),
                Some(&raw mut lossy),
            );
            out.truncate(usize::try_from(n).ok()?);
            (!lossy.as_bool()).then_some(out)
        }
    }
}

#[cfg(not(windows))]
mod ansi {
    pub(super) fn decode(bytes: &[u8], _: u32) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    pub(super) fn encode(text: &str, _: u32) -> Option<Vec<u8>> {
        text.is_ascii().then(|| text.as_bytes().to_vec())
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
        assert_eq!(name_in(&decode(&utf16, ANSI).0).as_deref(), Some("下载"));
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend(text.as_bytes());
        assert_eq!(name_in(&decode(&utf8, ANSI).0).as_deref(), Some("下载"));
        assert_eq!(
            decode(text.as_bytes(), ANSI),
            (text.to_owned(), Encoding::Utf8),
            "UTF-8 with no byte order mark"
        );
    }

    /// An older or another program's `desktop.ini` in an ANSI code page (GBK here, code page 936
    /// whatever the system's is) reads right, and a rename writes it back in that code page; a
    /// name the code page can't hold makes the file UTF-16 instead of question marks.
    #[cfg(windows)]
    #[test]
    fn ansi_files_are_read_and_written_in_their_code_page() {
        const GBK: u32 = 936;
        // 下载 in GBK.
        let ini = b"[.ShellClassInfo]\r\nLocalizedResourceName=\xCF\xC2\xD4\xD8\r\n";
        let (text, encoding) = decode(ini, GBK);
        assert_eq!(encoding, Encoding::Ansi(GBK));
        assert_eq!(name_in(&text).as_deref(), Some("下载"));
        // 文件 in GBK.
        assert_eq!(
            encode(&with_name(&text, "文件"), encoding),
            b"[.ShellClassInfo]\r\nLocalizedResourceName=\xCE\xC4\xBC\xFE\r\n"
        );
        let emoji = encode(&with_name(&text, "文件\u{1F642}"), encoding);
        assert_eq!(emoji[..2], [0xFF, 0xFE]);
        assert_eq!(
            name_in(&decode(&emoji, GBK).0).as_deref(),
            Some("文件\u{1F642}")
        );
        // Plain ASCII counts as ANSI: a rename keeps it a byte per character.
        let (ascii, encoding) = decode(b"[.ShellClassInfo]\r\n", GBK);
        assert_eq!(encoding, Encoding::Ansi(GBK));
        assert_eq!(
            encode(&with_name(&ascii, "Docs"), encoding),
            b"[.ShellClassInfo]\r\nLocalizedResourceName=Docs\r\n"
        );
    }

    #[test]
    fn a_new_name_replaces_the_old_one_and_keeps_the_rest() {
        let ini = "[.ShellClassInfo]\r\nLocalizedResourceName=@shell32.dll,-21770\r\nIconResource=x,1\r\n[ViewState]\r\nMode=\r\n";
        assert_eq!(
            with_name(ini, "報告"),
            "[.ShellClassInfo]\r\nLocalizedResourceName=報告\r\nIconResource=x,1\r\n[ViewState]\r\nMode=\r\n"
        );
        assert_eq!(
            with_name("[.ShellClassInfo]\r\nIconResource=x,1\r\n", "A"),
            "[.ShellClassInfo]\r\nIconResource=x,1\r\nLocalizedResourceName=A\r\n"
        );
        assert_eq!(
            with_name("", "A"),
            "[.ShellClassInfo]\r\nLocalizedResourceName=A\r\n"
        );
        assert_eq!(name_in(&with_name(ini, "報告")).as_deref(), Some("報告"));
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
        let tmp_dir = tempfile::tempdir().unwrap();
        let root = tmp_dir.path().to_path_buf();
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
        // Renamed: hidden and system as Windows keeps it, rewritten in place.
        let ini = plain.join("desktop.ini");
        let hide = |on: bool| {
            let flag = if on { "+" } else { "-" };
            std::process::Command::new("attrib")
                .args([
                    format!("{flag}h"),
                    format!("{flag}s"),
                    ini.display().to_string(),
                ])
                .status()
                .unwrap();
        };
        hide(true);
        write_name(&plain, "新名字").unwrap();
        assert_eq!(read(&fs, &plain).as_deref(), Some("新名字"));
        assert!(plain.exists(), "the folder keeps its own name");
        hide(false);
    }
}
