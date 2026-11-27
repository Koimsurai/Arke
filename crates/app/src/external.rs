//! "External copy program" and "External preview program": copies and moves, or Space, handed to
//! a program the user names, through one generic command line. There is no code for any one
//! program; `legacy_tools.rs` only converts the TeraCopy and QuickLook switches of older versions.
//!
//! A setting is a command line: the program, then its argument template, for example
//! `"C:\Program Files\TeraCopy\TeraCopy.exe" {op} *{list} {dest}`. The program is the quoted text
//! at the start; unquoted, it runs up to the first `.exe` followed by a space or the end (so a
//! pasted `C:\Program Files\...\x.exe` works), or else up to the first space.
//!
//! Placeholders in the template:
//! - `{file}` (preview): the item Space was pressed on.
//! - `{sources}` (copy): every item, each as its own argument.
//! - `{list}` (copy): a text file listing the items, one path per line (UTF-8 with a byte order
//!   mark, CRLF line ends), written to the temp folder. The program reads it after starting, so it
//!   is left there; list files a day old are removed on a later hand-off.
//! - `{dest}` (copy): the destination folder.
//! - `{op}` (copy): `Copy` or `Move`. Moves go to the program only when the template has `{op}`,
//!   since it couldn't tell them from copies otherwise; without it Arke moves by itself.
//! - `{sid}` (both): the current user's SID as text, for programs that name things after it.
//! - `{{` and `}}`: a literal `{` or `}`.
//!
//! A template without any path placeholder gets them at the end: `{file}` for preview,
//! `{sources} {dest}` for copy. Any other text is passed as written.
//!
//! Every path is quoted so that any Windows command-line parser reads it back the same: in double
//! quotes, except a drive root (`D:\`), which goes bare because `\"` reads as an escaped quote to
//! programs that follow the C runtime's rules; folders lose a trailing backslash. A Windows path
//! can't contain `"`, so nothing inside the quotes needs escaping (a path that has one is
//! refused). The program is started directly with this command line (`CreateProcess`), never
//! through `cmd` or another shell, so `&`, `|`, `%` and `^` in names mean nothing.
//!
//! # Named pipe form (preview only)
//!
//! A program that is already running can be reached faster than by starting another copy of it
//! that only passes the item on. The preview setting may instead be
//!
//! ```text
//! pipe:\\.\pipe\<name> <message> [|| <command line>]
//! ```
//!
//! Arke writes the message as one UTF-8 line (ended with CR LF) to the pipe and closes it. The pipe
//! name comes first, up to the first space, and must be a local pipe (`\\.\pipe\...`); the message
//! is the rest. `{sid}` may be in the name and the message, `{file}` in the message, as the item's
//! path exactly as it is, not quoted (a path that isn't valid Unicode can't be sent). Connecting
//! to a pipe that doesn't exist fails at once; one that is busy is waited for 200 ms at most.
//!
//! `||` with a space on both sides ends the pipe form and starts the fallback, a command line as
//! above, used when the pipe isn't there (the program isn't running), so that it gets started.
//! Paths can't contain `|`, so nothing in a path can be taken for it. Without a fallback, a missing
//! pipe is an error like a command that doesn't start, and Arke shows its own Quick Look.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shell::{FileOp, Shell, ShellError};

/// List files older than this are left over from earlier hand-offs and removed.
const LIST_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(24);
/// Arke's list files in the temp folder: its own, and those of the TeraCopy hand-off before 0.10.
const LIST_PREFIXES: [&str; 2] = ["arke-list-", "arke-teracopy-"];

/// What a command is for, which decides the placeholders it may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// Copies and moves: `{op}`, `{sources}`, `{list}`, `{dest}`.
    Copy,
    /// Space: `{file}`.
    Preview,
}

/// One part of an argument template.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    Op,
    Sources,
    List,
    Dest,
    File,
    Sid,
}

impl Piece {
    /// Whether this stands for one or more paths.
    const fn is_path(&self) -> bool {
        matches!(self, Self::Sources | Self::List | Self::Dest | Self::File)
    }
}

/// Why a command line can't be used.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TemplateError {
    /// Only arguments, or an empty pair of quotes.
    #[error("the command has no program")]
    NoProgram,
    /// `{name}` that this setting doesn't have.
    #[error("unknown placeholder {{{0}}}")]
    Unknown(String),
    /// A `{` that is never closed.
    #[error("a {{ without its }}")]
    Unclosed,
    /// A single `}` with no `{` before it.
    #[error("a }} without its {{")]
    Unopened,
    /// A path with a `"` in it, which no quoting would carry safely.
    #[error("a path with a quote in it: {0}")]
    Quote(String),
    /// `pipe:` and then nothing.
    #[error("the pipe form has no pipe name")]
    NoPipe,
    /// A pipe name that isn't `\\.\pipe\` and a name.
    #[error("not a local pipe name: {0}")]
    BadPipe(String),
    /// A pipe and no message to write to it.
    #[error("the pipe form has no message")]
    NoMessage,
    /// `{sid}` when the user's SID can't be read.
    #[error("the user's SID can't be read")]
    NoSid,
    /// A path that can't be written as UTF-8 text.
    #[error("a path that isn't valid Unicode: {0}")]
    NotUnicode(String),
    /// The pipe form in the external copy program.
    #[error("a named pipe is only for the preview program")]
    PipeForCopy,
}

impl From<TemplateError> for ShellError {
    fn from(err: TemplateError) -> Self {
        Self::Io(io::Error::new(io::ErrorKind::InvalidInput, err))
    }
}

/// A parsed setting: the program and its argument template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    /// The exe, as written (a bare name is looked up on `PATH`).
    pub(crate) program: PathBuf,
    pieces: Vec<Piece>,
}

/// The paths a template is filled with.
#[derive(Default)]
struct Paths<'a> {
    op: &'a str,
    sources: &'a [PathBuf],
    list: Option<&'a Path>,
    dest: Option<&'a Path>,
    file: Option<&'a Path>,
    sid: Option<&'a str>,
}

impl Command {
    /// Reads a setting. `None` when it is empty: the setting is off.
    pub(crate) fn parse(line: &str, usage: Use) -> Option<Result<Self, TemplateError>> {
        let line = line.trim();
        (!line.is_empty()).then(|| Self::parse_line(line, usage))
    }

    fn parse_line(line: &str, usage: Use) -> Result<Self, TemplateError> {
        if usage == Use::Copy && starts_with_pipe(line) {
            return Err(TemplateError::PipeForCopy);
        }
        let (program, args) = split_program(line);
        let program = program.trim();
        if program.is_empty() {
            return Err(TemplateError::NoProgram);
        }
        let mut pieces = template(args.trim(), usage)?;
        if !pieces.iter().any(Piece::is_path) {
            let space = || Piece::Text(" ".to_owned());
            if !pieces.is_empty() {
                pieces.push(space());
            }
            match usage {
                Use::Preview => pieces.push(Piece::File),
                Use::Copy => pieces.extend([Piece::Sources, space(), Piece::Dest]),
            }
        }
        Ok(Self {
            program: PathBuf::from(program),
            pieces,
        })
    }

    /// Whether moves go to the program too: the template says which it is with `{op}`.
    fn takes_moves(&self) -> bool {
        self.pieces.contains(&Piece::Op)
    }

    /// Whether this copy or move goes to the program (other operations never do).
    pub(crate) fn takes(&self, op: &FileOp) -> bool {
        match op {
            FileOp::Copy { sources, .. } => !sources.is_empty(),
            FileOp::Move { sources, .. } => self.takes_moves() && !sources.is_empty(),
            _ => false,
        }
    }

    /// The arguments that preview `file`.
    pub(crate) fn preview_args(&self, file: &Path) -> Result<OsString, TemplateError> {
        self.expand(&Paths {
            file: Some(file),
            sid: self.sid(),
            ..Paths::default()
        })
    }

    /// Starts the program for a preview of `item`. Background thread.
    fn start_preview(&self, shell: &dyn Shell, item: &Path) -> Result<(), ShellError> {
        let args = self.preview_args(item)?;
        shell.run_program(&self.program, &args)
    }

    /// The user's SID, when the template has `{sid}`.
    fn sid(&self) -> Option<&'static str> {
        self.pieces
            .contains(&Piece::Sid)
            .then(current_sid)
            .flatten()
    }

    /// The arguments for a copy or move (`list`: the list file, when the template has `{list}`).
    /// `None` for anything [`Self::takes`] refuses.
    fn copy_args(
        &self,
        op: &FileOp,
        list: Option<&Path>,
    ) -> Option<Result<OsString, TemplateError>> {
        if !self.takes(op) {
            return None;
        }
        let (op, sources, dest) = match op {
            FileOp::Copy { sources, dest } => ("Copy", sources, dest),
            FileOp::Move { sources, dest } => ("Move", sources, dest),
            _ => return None,
        };
        Some(self.expand(&Paths {
            op,
            sources,
            list,
            dest: Some(dest),
            file: None,
            sid: self.sid(),
        }))
    }

    fn expand(&self, paths: &Paths<'_>) -> Result<OsString, TemplateError> {
        let mut out = OsString::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(text) => out.push(text),
                Piece::Op => out.push(paths.op),
                Piece::Sources => {
                    for (ix, source) in paths.sources.iter().enumerate() {
                        if ix > 0 {
                            out.push(" ");
                        }
                        out.push(quote(source)?);
                    }
                }
                Piece::List => push_quoted(&mut out, paths.list)?,
                Piece::Dest => push_quoted(&mut out, paths.dest)?,
                Piece::File => push_quoted(&mut out, paths.file)?,
                Piece::Sid => out.push(paths.sid.ok_or(TemplateError::NoSid)?),
            }
        }
        Ok(out)
    }

    /// Starts the program for a copy or move: writes the list file first if the template has
    /// `{list}`. Returns once it has started, not when it finishes; an error means it could not
    /// start, and the caller runs the job itself. Background thread.
    pub(crate) fn hand_off(&self, shell: &dyn Shell, op: &FileOp) -> Result<(), ShellError> {
        let list = match op {
            FileOp::Copy { sources, .. } | FileOp::Move { sources, .. }
                if self.pieces.contains(&Piece::List) =>
            {
                Some(write_list(sources)?)
            }
            _ => None,
        };
        let args = self
            .copy_args(op, list.as_deref())
            .ok_or(ShellError::Unsupported)??;
        shell.run_program(&self.program, &args)
    }
}

/// The pipe form starts with this (any letter case).
const PIPE_FORM: &str = "pipe:";
/// Every pipe name starts with this: a pipe on this computer.
const PIPE_ROOT: &str = r"\\.\pipe\";

/// `text` after `prefix`, if it starts with it in any ASCII letter case.
fn strip_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Whether a setting is written in the pipe form.
fn starts_with_pipe(line: &str) -> bool {
    strip_ignore_case(line.trim_start(), PIPE_FORM).is_some()
}

/// The current user's SID as text; read once.
fn current_sid() -> Option<&'static str> {
    static SID: OnceLock<Option<String>> = OnceLock::new();
    SID.get_or_init(arke_indexd::sec::current_user_sid)
        .as_deref()
}

/// Splits `pipe form || command line` at the first `||` that has a space (or the end) on both
/// sides, so a `||` inside the message is left alone.
fn split_fallback(text: &str) -> (&str, Option<&str>) {
    let mut from = 0;
    while let Some(found) = text[from..].find("||") {
        let at = from + found;
        let after = at + "||".len();
        let space_before = text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let space_after = text[after..].chars().next().is_none_or(char::is_whitespace);
        if space_before && space_after {
            return (&text[..at], Some(&text[after..]));
        }
        from = after;
    }
    (text, None)
}

/// A template as plain text: no quoting, for a message that isn't a command line.
fn plain(pieces: &[Piece], paths: &Paths<'_>) -> Result<String, TemplateError> {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Sid => out.push_str(paths.sid.ok_or(TemplateError::NoSid)?),
            Piece::File => {
                let file = paths.file.unwrap_or_else(|| Path::new(""));
                let text = file
                    .to_str()
                    .ok_or_else(|| TemplateError::NotUnicode(file.display().to_string()))?;
                out.push_str(text);
            }
            // The preview setting has none of the copy placeholders.
            Piece::Op | Piece::Sources | Piece::List | Piece::Dest => {}
        }
    }
    Ok(out)
}

/// The pipe part of a preview setting: where to write, and what.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PipeLine {
    name: Vec<Piece>,
    message: Vec<Piece>,
}

impl PipeLine {
    /// Reads `\\.\pipe\<name> <message>` (what follows `pipe:`, up to any fallback).
    fn parse(text: &str) -> Result<Self, TemplateError> {
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        let (name, message) = text.split_at(end);
        if name.is_empty() {
            return Err(TemplateError::NoPipe);
        }
        if strip_ignore_case(name, PIPE_ROOT).is_none_or(str::is_empty) {
            return Err(TemplateError::BadPipe(name.to_owned()));
        }
        let message = message.trim();
        if message.is_empty() {
            return Err(TemplateError::NoMessage);
        }
        let name = template(name, Use::Preview)?;
        if name.iter().any(Piece::is_path) {
            return Err(TemplateError::Unknown("file".to_owned()));
        }
        Ok(Self {
            name,
            message: template(message, Use::Preview)?,
        })
    }

    fn needs_sid(&self) -> bool {
        self.name.contains(&Piece::Sid) || self.message.contains(&Piece::Sid)
    }

    /// The pipe's name and the line to write to it for `item`: the message and a CR LF.
    fn render(&self, item: &Path, sid: Option<&str>) -> Result<(String, String), TemplateError> {
        let paths = Paths {
            file: Some(item),
            sid,
            ..Paths::default()
        };
        let name = plain(&self.name, &paths)?;
        let mut line = plain(&self.message, &paths)?;
        line.push_str("\r\n");
        Ok((name, line))
    }

    /// Writes the line for `item` to the pipe. Background thread.
    fn write(&self, shell: &dyn Shell, item: &Path) -> Result<(), ShellError> {
        let sid = self.needs_sid().then(current_sid).flatten();
        let (name, line) = self.render(item, sid)?;
        shell.write_pipe(&name, &line)
    }
}

/// Whether an error from the pipe means that the fallback should be tried: the pipe isn't there,
/// or the message can't be made for this item. Anything else (a busy pipe that never freed up)
/// means the program is running but not answering, and a second copy would only wait on it.
fn is_miss(err: &ShellError) -> bool {
    matches!(err, ShellError::Io(err)
        if matches!(err.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput))
}

/// The external preview program setting: a command line, or the pipe form with an optional
/// fallback command line (see the module notes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preview {
    pipe: Option<PipeLine>,
    launch: Option<Command>,
}

impl Preview {
    /// Reads the setting. `None` when it is empty: the setting is off.
    pub(crate) fn parse(line: &str) -> Option<Result<Self, TemplateError>> {
        let line = line.trim();
        (!line.is_empty()).then(|| Self::parse_line(line))
    }

    fn parse_line(line: &str) -> Result<Self, TemplateError> {
        let Some(rest) = strip_ignore_case(line, PIPE_FORM) else {
            return Command::parse_line(line, Use::Preview).map(|launch| Self {
                pipe: None,
                launch: Some(launch),
            });
        };
        let (pipe, fallback) = split_fallback(rest);
        let pipe = PipeLine::parse(pipe.trim())?;
        let launch = fallback
            .map(|line| Command::parse(line, Use::Preview).ok_or(TemplateError::NoProgram)?)
            .transpose()?;
        Ok(Self {
            pipe: Some(pipe),
            launch,
        })
    }

    /// Previews `item`: writes to the pipe if there is one, and starts the program if there is no
    /// pipe to write to (and a fallback command). Returns once the line is written or the program
    /// has started; an error means neither happened, and the caller shows its own Quick Look.
    /// Background thread.
    pub(crate) fn open(&self, shell: &dyn Shell, item: &Path) -> Result<(), ShellError> {
        if let Some(pipe) = &self.pipe {
            match pipe.write(shell, item) {
                Ok(()) => return Ok(()),
                Err(err) if self.launch.is_some() && is_miss(&err) => {
                    log::debug!("no pipe for the preview program, starting it: {err}");
                }
                Err(err) => return Err(err),
            }
        }
        match &self.launch {
            Some(launch) => launch.start_preview(shell, item),
            None => Err(io::Error::from(io::ErrorKind::NotFound).into()),
        }
    }
}

/// A command line for `program` with `args`, as the setting stores it.
pub(crate) fn command_line(program: &Path, args: &str) -> String {
    format!("\"{}\" {args}", program.display())
}

/// Splits a command line into the program and the rest (see the module notes).
fn split_program(line: &str) -> (&str, &str) {
    if let Some(rest) = line.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&rest[..end], &rest[end + 1..]),
            None => (rest, ""),
        };
    }
    // Lowercasing ASCII keeps every byte where it was, so offsets match `line`.
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(".exe") {
        let end = from + at + ".exe".len();
        if lower[end..].chars().next().is_none_or(char::is_whitespace) {
            return line.split_at(end);
        }
        from = end;
    }
    line.split_once(char::is_whitespace).unwrap_or((line, ""))
}

/// Reads an argument template into its pieces.
fn template(text: &str, usage: Use) -> Result<Vec<Piece>, TemplateError> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '}' => return Err(TemplateError::Unopened),
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(TemplateError::Unclosed),
                    }
                }
                let Some(piece) = placeholder(&name, usage) else {
                    return Err(TemplateError::Unknown(name));
                };
                if !literal.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut literal)));
                }
                pieces.push(piece);
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

/// The placeholder `{name}` stands for, if `usage` has it.
fn placeholder(name: &str, usage: Use) -> Option<Piece> {
    Some(match (usage, name) {
        (_, "sid") => Piece::Sid,
        (Use::Preview, "file") => Piece::File,
        (Use::Copy, "op") => Piece::Op,
        (Use::Copy, "sources") => Piece::Sources,
        (Use::Copy, "list") => Piece::List,
        (Use::Copy, "dest") => Piece::Dest,
        _ => return None,
    })
}

fn push_quoted(out: &mut OsString, path: Option<&Path>) -> Result<(), TemplateError> {
    if let Some(path) = path {
        out.push(quote(path)?);
    }
    Ok(())
}

/// One path as an argument (see the module notes): quoted, a drive root bare, no trailing
/// backslash. Works on the UTF-16 name, so any name Windows allows comes through unchanged.
fn quote(path: &Path) -> Result<OsString, TemplateError> {
    const QUOTE: u16 = 0x22;
    const BACKSLASH: u16 = 0x5C;
    const COLON: u16 = 0x3A;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&QUOTE) {
        return Err(TemplateError::Quote(path.display().to_string()));
    }
    // `D:\` (and `\\?\D:\`): no spaces in it, and a quote after its backslash would be escaped.
    if wide.ends_with(&[COLON, BACKSLASH]) {
        return Ok(OsString::from_wide(&wide));
    }
    while wide.len() > 1 && wide.last() == Some(&BACKSLASH) {
        wide.pop();
    }
    let mut out = OsString::from("\"");
    out.push(OsString::from_wide(&wide));
    out.push("\"");
    Ok(out)
}

/// One path per line, UTF-8 with a byte order mark so non-ASCII names read correctly.
fn list_bytes(sources: &[PathBuf]) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    for source in sources {
        bytes.extend_from_slice(source.to_string_lossy().as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

/// Writes the list file in the temp folder, removing Arke's list files from earlier hand-offs.
fn write_list(sources: &[PathBuf]) -> io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    remove_old_lists(&dir);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{}{}-{n}.txt",
        LIST_PREFIXES[0],
        std::process::id()
    ));
    let mut file = std::fs::File::create(&path)?;
    file.write_all(&list_bytes(sources))?;
    Ok(path)
}

/// Whether `name` is one of Arke's list files.
fn is_list_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    LIST_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        && Path::new(&*name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
}

/// Removes Arke's own list files that are a day old (only `arke-list-*.txt` and the older
/// `arke-teracopy-*.txt`).
fn remove_old_lists(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_list_name(&entry.file_name()) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LIST_MAX_AGE);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(line: &str) -> Command {
        Command::parse(line, Use::Copy)
            .expect("not empty")
            .expect("valid")
    }

    fn preview(line: &str) -> Command {
        Command::parse(line, Use::Preview)
            .expect("not empty")
            .expect("valid")
    }

    fn args(cmd: &Command, op: &FileOp, list: Option<&Path>) -> Option<String> {
        cmd.copy_args(op, list)
            .map(|args| args.expect("expands").to_string_lossy().into_owned())
    }

    fn copy_op(sources: &[&str], dest: &str) -> FileOp {
        FileOp::Copy {
            sources: sources.iter().map(PathBuf::from).collect(),
            dest: PathBuf::from(dest),
        }
    }

    fn move_op(sources: &[&str], dest: &str) -> FileOp {
        FileOp::Move {
            sources: sources.iter().map(PathBuf::from).collect(),
            dest: PathBuf::from(dest),
        }
    }

    #[test]
    fn an_empty_setting_is_off() {
        assert_eq!(Command::parse("", Use::Copy), None);
        assert_eq!(Command::parse("   ", Use::Preview), None);
    }

    #[test]
    fn the_program_is_quoted_unquoted_up_to_its_exe_or_the_first_word() {
        let quoted = preview(r#""C:\Program Files\Tool\tool.exe" --view {file}"#);
        assert_eq!(
            quoted.program,
            PathBuf::from(r"C:\Program Files\Tool\tool.exe")
        );
        let pasted = preview(r"C:\Program Files\Tool\Tool.EXE --view {file}");
        assert_eq!(
            pasted.program,
            PathBuf::from(r"C:\Program Files\Tool\Tool.EXE")
        );
        // `.exe` inside a folder name doesn't end the program.
        let odd = preview(r"C:\tools.exe.d\v.exe {file}");
        assert_eq!(odd.program, PathBuf::from(r"C:\tools.exe.d\v.exe"));
        let bare = preview("viewer {file}");
        assert_eq!(bare.program, PathBuf::from("viewer"));
        assert_eq!(
            Command::parse(r#""" {file}"#, Use::Preview),
            Some(Err(TemplateError::NoProgram))
        );
    }

    #[test]
    fn placeholders_are_filled_and_paths_quoted() {
        let cmd = preview(r#""C:\v\v.exe" /open {file} /x"#);
        assert_eq!(
            cmd.preview_args(Path::new(r"C:\Users\u\資料\a b.txt"))
                .unwrap(),
            OsString::from(r#"/open "C:\Users\u\資料\a b.txt" /x"#)
        );
        // A drive root goes bare: `"D:\"` would read as `D:"` to most programs.
        assert_eq!(
            cmd.preview_args(Path::new(r"D:\")).unwrap(),
            OsString::from(r"/open D:\ /x")
        );
        // A folder loses its trailing backslash for the same reason.
        assert_eq!(
            cmd.preview_args(Path::new(r"C:\a folder\")).unwrap(),
            OsString::from(r#"/open "C:\a folder" /x"#)
        );
        // Shell characters are only text: nothing runs through a shell.
        assert_eq!(
            cmd.preview_args(Path::new(r"C:\a & b %PATH% ^ | c.txt"))
                .unwrap(),
            OsString::from(r#"/open "C:\a & b %PATH% ^ | c.txt" /x"#)
        );
        assert_eq!(
            cmd.preview_args(Path::new(r#"C:\a"b"#)),
            Err(TemplateError::Quote(r#"C:\a"b"#.to_owned()))
        );
    }

    #[test]
    fn names_that_are_not_unicode_come_through_unchanged() {
        let odd = PathBuf::from(OsString::from_wide(&[
            u16::from(b'C'),
            u16::from(b':'),
            u16::from(b'\\'),
            0xD800,
        ]));
        let got = preview("v.exe {file}").preview_args(&odd).unwrap();
        let want: Vec<u16> = [u16::from(b'"')]
            .into_iter()
            .chain(odd.as_os_str().encode_wide())
            .chain([u16::from(b'"')])
            .collect();
        assert_eq!(got.encode_wide().collect::<Vec<_>>(), want);
    }

    #[test]
    fn sources_list_dest_and_op() {
        let cmd = copy(r#""C:\c\c.exe" {op} {sources} -> {dest}"#);
        let op = copy_op(&[r"C:\a\1.txt", r"C:\a\two words"], r"E:\backup");
        assert_eq!(
            args(&cmd, &op, None).as_deref(),
            Some(r#"Copy "C:\a\1.txt" "C:\a\two words" -> "E:\backup""#)
        );
        let op = move_op(&[r"C:\a\1.txt"], r"E:\");
        assert_eq!(
            args(&cmd, &op, None).as_deref(),
            Some(r#"Move "C:\a\1.txt" -> E:\"#)
        );
        let list = copy(r"c.exe {op} *{list} {dest}");
        let op = copy_op(&[r"C:\a\1", r"C:\a\2"], r"E:\b");
        assert_eq!(
            args(&list, &op, Some(Path::new(r"C:\Temp\list.txt"))).as_deref(),
            Some(r#"Copy *"C:\Temp\list.txt" "E:\b""#)
        );
    }

    #[test]
    fn moves_need_op_and_other_operations_never_go() {
        let cmd = copy(r"c.exe {sources} {dest}");
        assert!(cmd.takes(&copy_op(&[r"C:\a"], r"D:\b")));
        assert!(
            !cmd.takes(&move_op(&[r"C:\a"], r"D:\b")),
            "without {{op}} a move would run as a copy"
        );
        assert!(copy(r"c.exe {op} {sources} {dest}").takes(&move_op(&[r"C:\a"], r"D:\b")));
        assert!(!cmd.takes(&copy_op(&[], r"D:\b")), "nothing to copy");
        let rename = FileOp::Rename {
            path: PathBuf::from(r"C:\a"),
            new_name: "b".into(),
        };
        assert!(!cmd.takes(&rename));
        assert_eq!(args(&cmd, &rename, None), None);
    }

    #[test]
    fn without_path_placeholders_the_paths_go_at_the_end() {
        assert_eq!(
            preview(r"C:\v\v.exe")
                .preview_args(Path::new(r"C:\a.txt"))
                .unwrap(),
            OsString::from(r#""C:\a.txt""#)
        );
        assert_eq!(
            preview(r"C:\v\v.exe /s")
                .preview_args(Path::new(r"C:\a.txt"))
                .unwrap(),
            OsString::from(r#"/s "C:\a.txt""#)
        );
        let cmd = copy(r"C:\c\c.exe /y");
        assert_eq!(
            args(&cmd, &copy_op(&[r"C:\a", r"C:\b"], r"D:\x"), None).as_deref(),
            Some(r#"/y "C:\a" "C:\b" "D:\x""#)
        );
    }

    #[test]
    fn braces_escape_and_bad_placeholders_are_refused() {
        assert_eq!(
            preview(r"v.exe {{x}} {file}")
                .preview_args(Path::new(r"C:\a"))
                .unwrap(),
            OsString::from(r#"{x} "C:\a""#)
        );
        let parse = |line: &str, usage| Command::parse(line, usage).expect("not empty");
        assert_eq!(
            parse("v.exe {path}", Use::Preview),
            Err(TemplateError::Unknown("path".into()))
        );
        assert_eq!(
            parse("c.exe {file}", Use::Copy),
            Err(TemplateError::Unknown("file".into())),
            "each setting has its own placeholders"
        );
        assert_eq!(
            parse("c.exe {dest}", Use::Preview),
            Err(TemplateError::Unknown("dest".into()))
        );
        assert_eq!(
            parse("v.exe {file", Use::Preview),
            Err(TemplateError::Unclosed)
        );
        assert_eq!(
            parse("v.exe file}", Use::Preview),
            Err(TemplateError::Unopened)
        );
    }

    #[test]
    fn the_stored_command_line_reads_back() {
        let line = command_line(
            Path::new(r"C:\Program Files\T\T.exe"),
            "{op} *{list} {dest}",
        );
        assert_eq!(line, r#""C:\Program Files\T\T.exe" {op} *{list} {dest}"#);
        let cmd = copy(&line);
        assert_eq!(cmd.program, PathBuf::from(r"C:\Program Files\T\T.exe"));
        assert!(cmd.takes_moves());
    }

    #[test]
    fn list_files_are_utf8_with_a_bom_one_path_per_line() {
        let bytes = list_bytes(&[PathBuf::from(r"C:\相片\a.jpg"), PathBuf::from(r"D:\b")]);
        assert_eq!(&bytes[..3], [0xEF, 0xBB, 0xBF]);
        assert_eq!(
            String::from_utf8(bytes[3..].to_vec()).unwrap(),
            "C:\\相片\\a.jpg\r\nD:\\b\r\n"
        );
        assert!(is_list_name(OsStr::new("arke-list-12-0.txt")));
        assert!(is_list_name(OsStr::new("arke-teracopy-12-0.TXT")));
        assert!(!is_list_name(OsStr::new("arke-list-12-0.log")));
        assert!(!is_list_name(OsStr::new("notes.txt")));
    }

    fn look(line: &str) -> Preview {
        Preview::parse(line).expect("not empty").expect("valid")
    }

    fn look_err(line: &str) -> TemplateError {
        Preview::parse(line)
            .expect("not empty")
            .expect_err("can't be used")
    }

    const PIPE: &str = r"pipe:\\.\pipe\Tool.Pipe.{sid} Toggle|{file}";

    #[test]
    fn a_command_line_is_a_preview_without_a_pipe() {
        assert_eq!(Preview::parse("  "), None);
        let cmd = look(r#""C:\v\v.exe" {file}"#);
        assert_eq!(cmd.pipe, None);
        assert_eq!(cmd.launch, Some(preview(r#""C:\v\v.exe" {file}"#)));
    }

    #[test]
    fn the_pipe_form_has_a_pipe_a_message_and_maybe_a_fallback() {
        let alone = look(PIPE);
        assert!(alone.launch.is_none());
        let pipe = alone.pipe.expect("a pipe");
        assert_eq!(
            pipe.render(Path::new(r"C:\q\a b.txt"), Some("S-1-5-21-7")),
            Ok((
                r"\\.\pipe\Tool.Pipe.S-1-5-21-7".to_owned(),
                "Toggle|C:\\q\\a b.txt\r\n".to_owned()
            ))
        );

        let both = look(&format!(r#"{PIPE} || "C:\Program Files\T\T.exe" {{file}}"#));
        assert!(both.pipe.is_some());
        assert_eq!(
            both.launch,
            Some(preview(r#""C:\Program Files\T\T.exe" {file}"#))
        );
        // The prefix may be in any case, and the spaces around `||` any whitespace.
        let loose = look("PIPE:\\\\.\\PIPE\\x  msg {file}\t||\tv.exe");
        assert_eq!(loose.launch, Some(preview("v.exe")));
        // A `||` that isn't set apart belongs to the message.
        let inside = look(r"pipe:\\.\pipe\x a||b {file}");
        assert!(inside.launch.is_none());
        let pipe = inside.pipe.expect("a pipe");
        assert_eq!(
            pipe.render(Path::new(r"C:\a"), None).map(|(_, line)| line),
            Ok("a||b C:\\a\r\n".to_owned())
        );
    }

    #[test]
    fn the_message_carries_the_path_as_it_is_and_the_user_sid() {
        let pipe = look(r"pipe:\\.\pipe\x {sid}|{{{file}}}|{sid}")
            .pipe
            .expect("a pipe");
        assert_eq!(
            pipe.render(Path::new(r"D:\"), Some("S-1")).unwrap().1,
            "S-1|{D:\\}|S-1\r\n"
        );
        // No SID to put in, or a path that isn't text: refused, not sent half-made.
        assert_eq!(
            pipe.render(Path::new(r"D:\"), None),
            Err(TemplateError::NoSid)
        );
        let odd = PathBuf::from(OsString::from_wide(&[u16::from(b'C'), 0xD800]));
        let plain = look(r"pipe:\\.\pipe\x {file}").pipe.expect("a pipe");
        assert!(matches!(
            plain.render(&odd, None),
            Err(TemplateError::NotUnicode(_))
        ));
        assert!(pipe.needs_sid() && !plain.needs_sid());
    }

    #[test]
    fn sid_fills_a_command_line_too() {
        let cmd = preview(r"v.exe --user {sid} {file}");
        let paths = Paths {
            file: Some(Path::new(r"C:\a")),
            sid: Some("S-1-5-7"),
            ..Paths::default()
        };
        assert_eq!(
            cmd.expand(&paths).unwrap(),
            OsString::from(r#"--user S-1-5-7 "C:\a""#)
        );
        let none = Paths { sid: None, ..paths };
        assert_eq!(cmd.expand(&none), Err(TemplateError::NoSid));
        assert!(
            copy(r"c.exe {sid} {sources} {dest}")
                .pieces
                .contains(&Piece::Sid)
        );
    }

    #[test]
    fn bad_pipe_settings_say_what_is_wrong() {
        assert_eq!(look_err("pipe:"), TemplateError::NoPipe);
        assert_eq!(look_err("pipe:   || v.exe"), TemplateError::NoPipe);
        assert_eq!(
            look_err(r"pipe:\\server\pipe\x msg"),
            TemplateError::BadPipe(r"\\server\pipe\x".to_owned())
        );
        assert_eq!(
            look_err("pipe:Tool msg {file}"),
            TemplateError::BadPipe("Tool".to_owned())
        );
        assert_eq!(
            look_err(r"pipe:\\.\pipe\ msg"),
            TemplateError::BadPipe(r"\\.\pipe\".to_owned())
        );
        assert_eq!(look_err(r"pipe:\\.\pipe\x"), TemplateError::NoMessage);
        assert_eq!(look_err(r"pipe:\\.\pipe\x  || v"), TemplateError::NoMessage);
        assert_eq!(
            look_err(r"pipe:\\.\pipe\x {file}  ||  "),
            TemplateError::NoProgram
        );
        assert_eq!(
            look_err(r"pipe:\\.\pipe\x msg {path}"),
            TemplateError::Unknown("path".to_owned())
        );
        assert_eq!(
            look_err(r"pipe:\\.\pipe\{file} msg"),
            TemplateError::Unknown("file".to_owned())
        );
        assert_eq!(
            look_err(r"pipe:\\.\pipe\x {file} || v.exe {dest}"),
            TemplateError::Unknown("dest".to_owned())
        );
        // A bad command line is still one.
        assert_eq!(look_err("v.exe {file"), TemplateError::Unclosed);
    }

    #[test]
    fn the_copy_program_has_no_pipe_form() {
        assert_eq!(
            Command::parse(r"pipe:\\.\pipe\x {sources}", Use::Copy),
            Some(Err(TemplateError::PipeForCopy))
        );
    }
}
