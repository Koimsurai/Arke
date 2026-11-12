//! Takes personal details out of text the user is about to share (Settings > About > Report a
//! problem): the profile folder, the account and computer names, server names, and optionally the
//! names of files and folders.
//!
//! - The profile folder becomes `%USERPROFILE%` wherever it appears (any case, `\` or `/`, after a
//!   `\\?\` prefix too); other accounts' folders under `X:\Users\` become `X:\Users\<user>`.
//! - The account and computer names become `<user>` and `<pc>` as whole words.
//! - Network servers (`\\server\share`, `sftp://user@host`, WebDAV `https://host`) become
//!   `<server>`. GitHub hosts stay, since update checks log them.
//! - With `mask_names`, names below the profile folder and on servers become `***` (an extension
//!   is kept: `***.pdf`), except well-known folders (Documents, AppData...) and Arke's own files,
//!   so the structure still helps to diagnose.
//!
//! Best effort on free text: a path in a log line has no clear end, so the user always sees the
//! exact result before it is sent.

/// Who to hide: values from the environment (`USERPROFILE`, `USERNAME`, `COMPUTERNAME`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// Profile folder, e.g. `C:\Users\alice`.
    pub profile: Option<String>,
    /// Account name, e.g. `alice`.
    pub user: Option<String>,
    /// Computer name, e.g. `DESKTOP-1234`.
    pub computer: Option<String>,
}

/// What the profile folder becomes.
pub const PROFILE: &str = "%USERPROFILE%";
/// What an account name becomes.
pub const USER: &str = "<user>";
/// What the computer name becomes.
pub const COMPUTER: &str = "<pc>";
/// What a server name becomes.
pub const SERVER: &str = "<server>";
/// What a hidden file or folder name becomes.
pub const HIDDEN: &str = "***";

/// Folder names kept when names are masked: Windows' own and Arke's, never personal.
const KNOWN: &[&str] = &[
    "3D Objects",
    "AppData",
    "Arke",
    "Contacts",
    "Desktop",
    "Documents",
    "Downloads",
    "Favorites",
    "Links",
    "Local",
    "LocalLow",
    "Microsoft",
    "Music",
    "OneDrive",
    "Packages",
    "Pictures",
    "Programs",
    "Recent",
    "Roaming",
    "Saved Games",
    "Searches",
    "Start Menu",
    "Startup",
    "Temp",
    "Videos",
    "WinGet",
    "Windows",
    "apps",
    "current",
    "scoop",
    "shims",
    "updates",
    // Arke's own files
    "arke.exe",
    "arke.log",
    "folder_views.json",
    "index.bin",
    "known_hosts",
    "search_history.json",
    "settings.json",
    "shelf.json",
    "workspaces.json",
];

/// Account folders under `X:\Users\` that belong to nobody.
const SHARED_PROFILES: &[&str] = &["All Users", "Default", "Default User", "Public"];

/// Hosts that are not private (update checks).
const PUBLIC_HOSTS: &[&str] = &["github.com", "githubusercontent.com"];

/// URL schemes whose host is a server the user connected to.
const SERVER_SCHEMES: &[&str] = &[
    "sftp", "ssh", "ftp", "dav", "davs", "webdav", "webdavs", "http", "https",
];

/// Returns `text` without the details of `who`, see the module docs.
#[must_use]
pub fn scrub(text: &str, who: &Identity, mask_names: bool) -> String {
    let mut out = text.to_owned();
    if let Some(profile) = who
        .profile
        .as_deref()
        .map(|p| p.trim_end_matches(['\\', '/']))
        && profile.chars().count() >= 3
    {
        out = replace_path(&out, profile, PROFILE);
        // As `{:?}` prints paths in log lines: backslashes doubled.
        out = replace_path(&out, &profile.replace('\\', r"\\"), PROFILE);
    }
    out = other_profiles(&out);
    out = servers(&out, mask_names);
    if mask_names {
        out = mask_below(&out, PROFILE);
    }
    for (name, token) in [(&who.user, USER), (&who.computer, COMPUTER)] {
        if let Some(name) = name.as_deref().map(str::trim)
            && name.chars().count() >= 2
        {
            out = replace_word(&out, name, token);
        }
    }
    out
}

/// Case-insensitive char equality that treats `/` and `\` alike.
fn same(a: char, b: char) -> bool {
    let sep = |c| c == '\\' || c == '/';
    (sep(a) && sep(b)) || a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Byte length of `needle` at the start of `hay` if it matches (see [`same`]).
fn match_len(hay: &str, needle: &str) -> Option<usize> {
    let mut hay_chars = hay.char_indices();
    for n in needle.chars() {
        let (_, h) = hay_chars.next()?;
        if !same(h, n) {
            return None;
        }
    }
    Some(hay_chars.next().map_or(hay.len(), |(i, _)| i))
}

/// A character that can be part of a name (so a match ending before it is only a prefix).
fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '$')
}

/// Ends a path segment in free text: separators, characters Windows forbids in names, and line
/// breaks.
const fn ends_segment(c: char) -> bool {
    matches!(
        c,
        '\\' | '/' | '"' | '\'' | '<' | '>' | '|' | '*' | '?' | ':' | ';' | '\n' | '\r' | '\t'
    )
}

/// Replaces every occurrence of the path `from` that is not a prefix of a longer name.
fn replace_path(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if let Some(len) = match_len(rest, from)
            && !rest[len..].chars().next().is_some_and(name_char)
        {
            out.push_str(to);
            i += len;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Replaces `word` where it stands alone (no name characters on either side).
fn replace_word(text: &str, word: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < text.len() {
        let rest = &text[i..];
        if !prev.is_some_and(word_char)
            && let Some(len) = match_len(rest, word)
            && !rest[len..].chars().next().is_some_and(word_char)
        {
            out.push_str(to);
            i += len;
            prev = to.chars().last();
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
        prev = Some(c);
    }
    out
}

/// Part of a word for whole-word matching of account and computer names.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The segment at the start of `text` (up to a separator or a character that ends it) and its
/// byte length. When nothing follows on the path, a space after `name.ext` ends it too.
fn segment(text: &str) -> (&str, usize) {
    let end = text
        .char_indices()
        .find(|&(_, c)| ends_segment(c))
        .map_or(text.len(), |(i, _)| i);
    let seg = &text[..end];
    let is_last = !text[end..].starts_with(['\\', '/']);
    if is_last
        && let Some(space) = seg.find(' ')
        && seg[..space].contains('.')
    {
        return (&seg[..space], space);
    }
    (seg, end)
}

/// `X:\Users\<name>` for accounts other than the shared ones -> `X:\Users\<user>`.
fn other_profiles(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if let Some(len) = match_len(rest, ":\\Users\\") {
            out.push_str(&rest[..len]);
            i += len;
            let (name, name_len) = segment(&text[i..]);
            let keep = name.is_empty()
                || name == PROFILE
                || name == USER
                || SHARED_PROFILES.iter().any(|s| s.eq_ignore_ascii_case(name));
            out.push_str(if keep { name } else { USER });
            i += name_len;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A masked name: `***`, with the extension of a file name kept (`***.pdf`).
fn masked(name: &str) -> String {
    let ext = name
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map(|dot| &name[dot + 1..])
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
    ext.map_or_else(|| HIDDEN.to_owned(), |ext| format!("{HIDDEN}.{ext}"))
}

/// Whether a name stays when names are masked.
fn known(name: &str) -> bool {
    name.is_empty()
        || name == HIDDEN
        || name == USER
        || name == SERVER
        || KNOWN.iter().any(|k| k.eq_ignore_ascii_case(name))
}

/// Masks the path segments at the start of `text` (each after a separator), writing to `out`.
/// Returns the bytes consumed.
fn mask_segments(text: &str, out: &mut String) -> usize {
    let mut i = 0;
    while let Some(sep) = text[i..].chars().next().filter(|c| matches!(c, '\\' | '/')) {
        out.push(sep);
        i += 1;
        let (name, len) = segment(&text[i..]);
        if known(name) {
            out.push_str(name);
        } else {
            out.push_str(&masked(name));
        }
        i += len;
    }
    i
}

/// Masks the names below every `root` token (the profile folder).
fn mask_below(text: &str, root: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(root) {
        let after = at + root.len();
        out.push_str(&rest[..after]);
        let used = mask_segments(&rest[after..], &mut out);
        rest = &rest[after + used..];
    }
    out.push_str(rest);
    out
}

/// Whether a path can start after this character in free text (start of a word, after a quote or
/// bracket). A `\\` elsewhere is part of something else, like an escaped path.
const fn starts_a_path(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '(' | '[' | '=' | ',')
}

/// Whether a URL host is one of the public ones that stay.
fn public_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    PUBLIC_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Server names in UNC paths and URLs -> `<server>`; with `mask_names`, the names on them too.
fn servers(text: &str, mask_names: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        // `\\server\share`, `\\?\UNC\server\share`; not `\\?\C:` or `\\.\pipe`.
        let unc = match_len(rest, "\\\\?\\UNC\\").or_else(|| {
            (rest.starts_with("\\\\") && !rest[2..].starts_with(['?', '.', '\\'])).then_some(2)
        });
        if let Some(lead) = unc
            && out.chars().last().is_none_or(starts_a_path)
        {
            let (host, len) = segment(&rest[lead..]);
            if !host.is_empty() && !host.contains(' ') {
                out.push_str(&rest[..lead]);
                out.push_str(SERVER);
                i += lead + len;
                if mask_names {
                    i += mask_segments(&text[i..], &mut out);
                }
                continue;
            }
        }
        if let Some((scheme_len, sep_len)) = url_start(rest, &out) {
            let host_start = scheme_len + sep_len;
            let host_end = rest[host_start..]
                .char_indices()
                .find(|&(_, c)| c == '/' || c == '\\' || c.is_whitespace() || c == '"' || c == ')')
                .map_or(rest.len(), |(at, _)| host_start + at);
            let authority = &rest[host_start..host_end];
            let host = authority.rsplit('@').next().unwrap_or_default();
            let host = host.split(':').next().unwrap_or_default();
            if !host.is_empty() && !public_host(host) {
                out.push_str(&rest[..host_start]);
                out.push_str(SERVER);
                i += host_end;
                if mask_names {
                    i += mask_segments(&text[i..], &mut out);
                }
                continue;
            }
            out.push_str(&rest[..host_end]);
            i += host_end;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A server URL at the start of `rest`: the scheme's byte length and the separator's (`://`, or
/// `:\\` for a URL that went through a path).
fn url_start(rest: &str, before: &str) -> Option<(usize, usize)> {
    if before.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    // Schemes are short; looking further would make the scan quadratic.
    let colon = rest.bytes().take(9).position(|b| b == b':')?;
    let scheme = &rest[..colon];
    if !SERVER_SCHEMES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(scheme))
    {
        return None;
    }
    let after = &rest[colon..];
    if after.starts_with("://") || after.starts_with(":\\\\") {
        Some((colon, 3))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> Identity {
        Identity {
            profile: Some(r"C:\Users\alice".into()),
            user: Some("alice".into()),
            computer: Some("DESKTOP-42".into()),
        }
    }

    #[test]
    fn replaces_the_profile_in_every_spelling() {
        let who = alice();
        assert_eq!(
            scrub(r"open C:\Users\alice\Documents", &who, false),
            r"open %USERPROFILE%\Documents"
        );
        assert_eq!(
            scrub(r"\\?\C:\USERS\Alice\x.txt", &who, false),
            r"\\?\%USERPROFILE%\x.txt"
        );
        assert_eq!(
            scrub("c:/users/alice/AppData", &who, false),
            "%USERPROFILE%/AppData"
        );
        assert_eq!(scrub(r"C:\Users\alice", &who, false), "%USERPROFILE%");
        // Debug-printed paths double their backslashes.
        assert_eq!(
            scrub(r#"path: "C:\\Users\\alice\\x""#, &who, false),
            r#"path: "%USERPROFILE%\\x""#
        );
    }

    #[test]
    fn a_longer_account_folder_is_not_the_profile() {
        let who = alice();
        // alicex is someone else: their folder name is hidden, but not as the profile.
        assert_eq!(
            scrub(r"C:\Users\alicex\a", &who, false),
            r"C:\Users\<user>\a"
        );
        assert_eq!(
            scrub(r"C:\Users\Public\x", &who, false),
            r"C:\Users\Public\x"
        );
        assert_eq!(scrub(r"D:\Users\bob\x", &who, false), r"D:\Users\<user>\x");
    }

    #[test]
    fn replaces_account_and_computer_names_as_words() {
        let who = alice();
        assert_eq!(
            scrub("signed in as ALICE on desktop-42.", &who, false),
            "signed in as <user> on <pc>."
        );
        // Inside another word it stays.
        assert_eq!(scrub("malice aliceberg", &who, false), "malice aliceberg");
    }

    #[test]
    fn handles_names_that_are_not_ascii() {
        let who = Identity {
            profile: Some(r"C:\Users\王小明".into()),
            user: Some("王小明".into()),
            computer: None,
        };
        assert_eq!(
            scrub(
                r"王小明 opened C:\Users\王小明\Desktop\報告.docx",
                &who,
                true
            ),
            r"<user> opened %USERPROFILE%\Desktop\***.docx"
        );
    }

    #[test]
    fn masks_names_below_the_profile_but_keeps_the_structure() {
        let who = alice();
        assert_eq!(
            scrub(
                r"cannot read C:\Users\alice\Documents\Taxes 2025\invoice.pdf: denied",
                &who,
                true
            ),
            r"cannot read %USERPROFILE%\Documents\***\***.pdf: denied"
        );
        assert_eq!(
            scrub(
                r"C:\Users\alice\AppData\Roaming\Arke\settings.json",
                &who,
                true
            ),
            r"%USERPROFILE%\AppData\Roaming\Arke\settings.json"
        );
        // A path at the end of a sentence: the words after `name.ext` stay.
        assert_eq!(
            scrub(r"C:\Users\alice\Desktop\notes.txt was moved", &who, true),
            r"%USERPROFILE%\Desktop\***.txt was moved"
        );
        // Without masking, names stay.
        assert_eq!(
            scrub(r"C:\Users\alice\Desktop\notes.txt", &who, false),
            r"%USERPROFILE%\Desktop\notes.txt"
        );
    }

    #[test]
    fn hides_servers() {
        let who = Identity::default();
        assert_eq!(
            scrub(r"listing \\nas\photos\2024 failed", &who, false),
            r"listing \\<server>\photos\2024 failed"
        );
        assert_eq!(
            scrub(r"listing \\nas\photos\2024", &who, true),
            r"listing \\<server>\***\***"
        );
        assert_eq!(
            scrub(r"\\?\UNC\nas\share\x", &who, false),
            r"\\?\UNC\<server>\share\x"
        );
        assert_eq!(
            scrub(
                "connect sftp://bob@files.example.com:2222/home/bob",
                &who,
                true
            ),
            "connect sftp://<server>/***/***"
        );
        assert_eq!(
            scrub("dav https://dav.example.com/files", &who, false),
            "dav https://<server>/files"
        );
        // Update checks: GitHub stays.
        assert_eq!(
            scrub("GET https://api.github.com/repos/x/y/releases", &who, true),
            "GET https://api.github.com/repos/x/y/releases"
        );
        // Device paths and pipes are not servers.
        assert_eq!(
            scrub(r"\\.\pipe\arke.1.x", &who, false),
            r"\\.\pipe\arke.1.x"
        );
        assert_eq!(scrub(r"\\?\C:\x", &who, false), r"\\?\C:\x");
    }

    #[test]
    fn missing_or_tiny_values_change_nothing() {
        let who = Identity {
            profile: Some(r"C:\".into()),
            user: Some("a".into()),
            computer: Some(String::new()),
        };
        assert_eq!(scrub(r"a C:\x", &who, false), r"a C:\x");
        assert_eq!(scrub("", &alice(), true), "");
    }
}
