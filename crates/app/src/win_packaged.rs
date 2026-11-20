//! Which items a packaged app's context menu command is for (#278), read from its manifest:
//! `desktop5:ItemType Type="..."` (or `desktop10:`) around `Verb Clsid="..."`. Windows keeps
//! these in a private store with no API to list them, so Arke learns a command's class id from
//! the whole Windows menu (its canonical verb) and then reads that one app's manifest: the
//! package that registered the class (`HKCR\PackagedCom\ClassIndex`), its folder, and
//! `AppxManifest.xml` there, or inside the `.appx` an app installed as a sparse package keeps
//! in its own folder (VS Code, Zed). Files and the registry are only read; nothing of the app
//! runs. Background thread.

use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use windows::Win32::System::Registry::HKEY_CLASSES_ROOT;

/// Item types (`Directory`, `Directory\Background`, `Drive`, `*`, `.txt`...) the command with
/// class id `clsid` (`{...}`) is registered for; empty when the manifest can't be read.
pub(crate) fn item_types(clsid: &str) -> Vec<String> {
    manifest_of(clsid).map_or_default(|xml| manifest_item_types(&xml, clsid))
}

fn manifest_of(clsid: &str) -> Option<String> {
    let index = format!(r"PackagedCom\ClassIndex\{clsid}");
    let packages = crate::win::reg_subkeys(HKEY_CLASSES_ROOT, &index);
    packages.iter().find_map(|package| {
        let root = crate::win::read_reg_string(
            HKEY_CLASSES_ROOT,
            &format!(
                r"Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages\{package}"
            ),
            "PackageRootFolder",
        )?;
        read_manifest(Path::new(&root))
    })
}

/// Largest manifest read (they are a few KB).
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;

fn read_manifest(root: &Path) -> Option<String> {
    let plain = root.join("AppxManifest.xml");
    if let Ok(meta) = std::fs::metadata(&plain)
        && meta.len() <= MAX_MANIFEST
        && let Ok(xml) = std::fs::read_to_string(&plain)
    {
        return Some(xml);
    }
    // A sparse package: the app's folder holds the package it registered.
    let packages: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("appx") || e.eq_ignore_ascii_case("msix"))
        })
        .collect();
    packages.iter().find_map(|p| manifest_in_package(p))
}

fn manifest_in_package(package: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(package).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let entry = zip.by_name("AppxManifest.xml").ok()?;
    if entry.size() > MAX_MANIFEST {
        return None;
    }
    let mut xml = String::new();
    entry.take(MAX_MANIFEST).read_to_string(&mut xml).ok()?;
    Some(xml)
}

/// The `Type` of each `ItemType` holding a `Verb` with class id `clsid` (any namespace prefix,
/// braces and case ignored), each once.
pub(crate) fn manifest_item_types(xml: &str, clsid: &str) -> Vec<String> {
    let want = bare(clsid);
    let mut reader = Reader::from_str(xml);
    let mut current: Option<String> = None;
    let mut out: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "ItemType" => {
                current = attr(&e, "Type");
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "ItemType" => current = None,
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "Verb" => {
                if let Some(kind) = &current
                    && attr(&e, "Clsid").is_some_and(|c| bare(&c) == want)
                    && !out.iter().any(|k| k.eq_ignore_ascii_case(kind))
                {
                    out.push(kind.clone());
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    out
}

/// A class id without braces, lowercased.
fn bare(clsid: &str) -> String {
    clsid
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .to_ascii_lowercase()
}

/// An attribute by local name (the prefix is ignored).
fn attr(e: &BytesStart, name: &str) -> Option<String> {
    let mut attrs = e.attributes();
    attrs.with_checks(false);
    attrs
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(std::borrow::Cow::into_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As Windows Terminal and Zed register their "Open in" commands.
    const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
         xmlns:desktop5="http://schemas.microsoft.com/appx/manifest/desktop/windows10/5">
  <Applications><Application><Extensions>
    <desktop4:Extension Category="windows.fileExplorerContextMenus">
      <desktop4:FileExplorerContextMenus>
        <desktop5:ItemType Type="Directory">
          <desktop5:Verb Id="Open" Clsid="9f156763-7844-4dc4-b2b1-901f640f5155" />
        </desktop5:ItemType>
        <desktop5:ItemType Type="Directory\Background">
          <desktop5:Verb Id="Open" Clsid="9F156763-7844-4DC4-B2B1-901F640F5155" />
        </desktop5:ItemType>
        <desktop5:ItemType Type="*">
          <desktop5:Verb Id="Other" Clsid="11111111-2222-3333-4444-555555555555" />
        </desktop5:ItemType>
      </desktop4:FileExplorerContextMenus>
    </desktop4:Extension>
  </Extensions></Application></Applications>
</Package>"#;

    #[test]
    fn item_types_come_from_the_command_s_own_verbs() {
        assert_eq!(
            manifest_item_types(MANIFEST, "{9F156763-7844-4DC4-B2B1-901F640F5155}"),
            ["Directory", r"Directory\Background"]
        );
        assert_eq!(
            manifest_item_types(MANIFEST, "{11111111-2222-3333-4444-555555555555}"),
            ["*"]
        );
        assert!(manifest_item_types(MANIFEST, "{00000000-0000-0000-0000-000000000000}").is_empty());
        assert!(manifest_item_types("not xml <", "{x}").is_empty());
    }
}
