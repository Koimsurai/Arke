//! Which items a packaged app's context menu command is for (#278), read from its manifest:
//! `desktop5:ItemType Type="..."` (or `desktop10:`) around `Verb Clsid="..."`. Windows keeps
//! these in a private store with no API to list them, so Arke learns a command's class id from
//! the whole Windows menu (its canonical verb) and then reads that one app's manifest: the
//! package that registered the class (`HKCR\PackagedCom\ClassIndex`), its folder, and
//! `AppxManifest.xml` there, or inside the `.appx` an app installed as a sparse package keeps
//! in its own folder (VS Code, Zed). Files and the registry are only read; nothing of the app
//! runs. Background thread.
//!
//! The commands under a packaged command's own submenu (NanaZip's, WinRAR's) have no canonical
//! verb in the Windows menu, so for those every packaged command is listed instead ([`commands`]:
//! the manifests of the packages that registered COM classes) and read until one holds it (#283).

use std::collections::{HashMap, HashSet};
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
    packages
        .iter()
        .find_map(|package| package_manifest(package))
}

/// A package's manifest, by its full name.
fn package_manifest(package: &str) -> Option<String> {
    let root = crate::win::read_reg_string(
        HKEY_CLASSES_ROOT,
        &format!(
            r"Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages\{package}"
        ),
        "PackageRootFolder",
    )?;
    read_manifest(Path::new(&root))
}

/// Every packaged app's context menu command (`{...}`) with the item types it is for, from the
/// manifests of the packages that registered COM classes (a few dozen on a typical PC).
pub(crate) fn commands() -> Vec<(String, Vec<String>)> {
    let mut seen_packages: HashSet<String> = HashSet::new();
    let mut packages: Vec<String> = Vec::new();
    for class in crate::win::reg_subkeys(HKEY_CLASSES_ROOT, r"PackagedCom\ClassIndex") {
        let index = format!(r"PackagedCom\ClassIndex\{class}");
        for package in crate::win::reg_subkeys(HKEY_CLASSES_ROOT, &index) {
            if seen_packages.insert(package.clone()) {
                packages.push(package);
            }
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for xml in packages.iter().filter_map(|p| package_manifest(p)) {
        for (clsid, types) in manifest_commands(&xml) {
            if seen.insert(bare(&clsid)) {
                out.push((clsid, types));
            }
        }
    }
    out
}

/// The context menu commands in a manifest (`{...}`, as written there plus braces), each with
/// the `Type` of every `ItemType` it is under.
pub(crate) fn manifest_commands(xml: &str) -> Vec<(String, Vec<String>)> {
    if !xml.contains("FileExplorerContextMenus") {
        return Vec::new();
    }
    let mut reader = Reader::from_str(xml);
    let mut current: Option<String> = None;
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    // Place in `out` by bare class id.
    let mut at: HashMap<String, usize> = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "ItemType" => {
                current = attr(&e, "Type");
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "ItemType" => current = None,
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "Verb" => {
                if let (Some(kind), Some(clsid)) = (&current, attr(&e, "Clsid")) {
                    let clsid = format!("{{{}}}", clsid.trim_matches(['{', '}']));
                    if let Some(&i) = at.get(&bare(&clsid)) {
                        let types = &mut out[i].1;
                        if !types.iter().any(|t| t.eq_ignore_ascii_case(kind)) {
                            types.push(kind.clone());
                        }
                    } else {
                        at.insert(bare(&clsid), out.len());
                        out.push((clsid, vec![kind.clone()]));
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    out
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
    use std::fmt::Write as _;

    use super::*;

    /// Every command a manifest registers, with its item types, as NanaZip registers its own
    /// (one class for files, folders and drives, under three namespaces) (#283).
    #[test]
    fn lists_every_command_in_a_manifest() {
        let xml = r#"<Package xmlns:desktop4="d4" xmlns:desktop5="d5" xmlns:desktop10="d10">
          <desktop4:Extension Category="windows.fileExplorerContextMenus">
            <desktop4:FileExplorerContextMenus>
              <desktop4:ItemType Type="*">
                <desktop4:Verb Id="Nana" Clsid="CAE3F1D4-7765-4D98-A060-52CD14D56EAB" />
              </desktop4:ItemType>
              <desktop5:ItemType Type="Directory">
                <desktop5:Verb Id="Nana" Clsid="CAE3F1D4-7765-4D98-A060-52CD14D56EAB" />
                <desktop5:Verb Id="Other" Clsid="{11111111-2222-3333-4444-555555555555}" />
              </desktop5:ItemType>
              <desktop10:ItemType Type="Drive">
                <desktop10:Verb Id="Nana" Clsid="CAE3F1D4-7765-4D98-A060-52CD14D56EAB" />
              </desktop10:ItemType>
            </desktop4:FileExplorerContextMenus>
          </desktop4:Extension>
        </Package>"#;
        assert_eq!(
            manifest_commands(xml),
            [
                (
                    "{CAE3F1D4-7765-4D98-A060-52CD14D56EAB}".to_owned(),
                    vec!["*".to_owned(), "Directory".to_owned(), "Drive".to_owned()]
                ),
                (
                    "{11111111-2222-3333-4444-555555555555}".to_owned(),
                    vec!["Directory".to_owned()]
                ),
            ]
        );
        assert!(manifest_commands("<Package/>").is_empty());
    }

    /// Many commands (and repeats of them) keep their first-seen order, one entry per class id.
    #[test]
    fn a_big_manifest_keeps_order_and_merges_repeats() {
        let mut xml = String::from(
            "<Package xmlns:d=\"d\"><d:FileExplorerContextMenus><d:ItemType Type=\"*\">",
        );
        for i in 0..2000 {
            let _ = write!(
                xml,
                "<d:Verb Clsid=\"{i:08X}-0000-0000-0000-000000000000\"/>"
            );
        }
        xml += "</d:ItemType><d:ItemType Type=\"Drive\">";
        for i in (0..2000).rev() {
            let _ = write!(
                xml,
                "<d:Verb Clsid=\"{{{i:08x}-0000-0000-0000-000000000000}}\"/>"
            );
        }
        xml += "</d:ItemType></d:FileExplorerContextMenus></Package>";
        let commands = manifest_commands(&xml);
        assert_eq!(commands.len(), 2000);
        assert_eq!(commands[0].0, "{00000000-0000-0000-0000-000000000000}");
        assert_eq!(commands[1999].0, "{000007CF-0000-0000-0000-000000000000}");
        assert!(commands.iter().all(|(_, t)| t == &["*", "Drive"]));
    }

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
