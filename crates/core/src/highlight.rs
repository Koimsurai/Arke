//! Simple syntax coloring for previews: comments, strings, numbers and keywords,
//! language picked by extension.
//!
//! Not a real parser (no nesting, string interpolation, regex literals); enough to see the
//! structure at a glance, and fast: a few ms for 64 KB. Computed in the background.

use std::ops::Range;

/// Kind of a highlighted span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// Line or block comment.
    Comment,
    /// Quoted string or character literal.
    String,
    /// Numeric literal.
    Number,
    /// Language keyword.
    Keyword,
}

/// Highlighting rules for one language; get one with [`lang_for`].
pub struct Lang {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    keywords: &'static [&'static str],
    /// Case-insensitive keywords (SQL, PowerShell, batch).
    ignore_case: bool,
}

const C_LIKE_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "defer",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "fn",
    "for",
    "func",
    "function",
    "go",
    "goto",
    "if",
    "impl",
    "implements",
    "import",
    "in",
    "interface",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "namespace",
    "new",
    "nil",
    "null",
    "override",
    "package",
    "private",
    "protected",
    "pub",
    "public",
    "readonly",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "switch",
    "this",
    "throw",
    "throws",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "unsafe",
    "use",
    "using",
    "var",
    "virtual",
    "void",
    "where",
    "while",
    "yield",
    "bool",
    "char",
    "int",
    "float",
    "double",
    "long",
    "short",
    "unsigned",
    "string",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "isize",
    "f32",
    "f64",
    "str",
    "from",
    "of",
    "instanceof",
    "sealed",
    "internal",
    "get",
    "set",
    "dyn",
    "crate",
    "select",
    "chan",
    "map",
    "range",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "self", "True", "try",
    "while", "with", "yield",
];

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "foreach", "while", "do", "done", "case", "esac",
    "in", "function", "return", "param", "begin", "process", "end", "try", "catch", "finally",
    "switch", "break", "continue", "exit", "echo", "set", "call", "goto", "rem", "export", "local",
];

const SQL_KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "drop",
    "alter",
    "index",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "on",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "as",
    "null",
    "is",
    "in",
    "like",
    "primary",
    "key",
    "foreign",
    "references",
    "union",
    "distinct",
    "case",
    "when",
    "then",
    "else",
    "end",
    "with",
    "returning",
    "default",
    "begin",
    "commit",
    "rollback",
];

const LITERALS: &[&str] = &["true", "false", "null"];

const C_LIKE: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    keywords: C_LIKE_KEYWORDS,
    ignore_case: false,
};
const RUST: Lang = Lang {
    // In Rust ' also starts a lifetime, so it is not a string quote.
    quotes: &['"'],
    ..C_LIKE
};
const PYTHON: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    keywords: PYTHON_KEYWORDS,
    ignore_case: false,
};
const SHELL: Lang = Lang {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    keywords: SHELL_KEYWORDS,
    ignore_case: false,
};
const POWERSHELL: Lang = Lang {
    block_comment: Some(("<#", "#>")),
    ignore_case: true,
    ..SHELL
};
const BATCH: Lang = Lang {
    line_comments: &["::", "rem ", "REM "],
    block_comment: None,
    quotes: &['"'],
    keywords: SHELL_KEYWORDS,
    ignore_case: true,
};
const SQL: Lang = Lang {
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    quotes: &['\''],
    keywords: SQL_KEYWORDS,
    ignore_case: true,
};
const JSON: Lang = Lang {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"'],
    keywords: LITERALS,
    ignore_case: false,
};
const CONFIG: Lang = Lang {
    line_comments: &["#", ";"],
    block_comment: None,
    quotes: &['"', '\''],
    keywords: LITERALS,
    ignore_case: false,
};
const MARKUP: Lang = Lang {
    line_comments: &[],
    block_comment: Some(("<!--", "-->")),
    quotes: &['"', '\''],
    keywords: &[],
    ignore_case: false,
};
const CSS: Lang = Lang {
    line_comments: &[],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    keywords: &[],
    ignore_case: false,
};
const LUA: Lang = Lang {
    line_comments: &["--"],
    block_comment: Some(("--[[", "]]")),
    quotes: &['"', '\''],
    keywords: C_LIKE_KEYWORDS,
    ignore_case: false,
};

/// Language for an extension (no dot, any case); `None` means no coloring.
pub fn lang_for(ext: &str) -> Option<&'static Lang> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => &RUST,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts" | "go"
        | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "swift" | "dart" | "scala" | "php"
        | "zig" | "m" | "gradle" | "groovy" => &C_LIKE,
        "py" | "pyw" | "pyi" => &PYTHON,
        "sh" | "bash" | "zsh" | "fish" | "rb" | "pl" | "r" | "dockerfile" | "mk" | "cmake" => {
            &SHELL
        }
        "ps1" | "psm1" | "psd1" => &POWERSHELL,
        "bat" | "cmd" => &BATCH,
        "sql" => &SQL,
        "json" | "jsonc" | "json5" => &JSON,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties" | "env"
        | "editorconfig" | "gitignore" => &CONFIG,
        "html" | "htm" | "xml" | "svg" | "xaml" | "csproj" | "props" | "targets" | "vue"
        | "resx" | "plist" => &MARKUP,
        "css" | "scss" | "less" => &CSS,
        "lua" => &LUA,
        _ => return None,
    })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where a piece of text starts, for highlighting a file piece by piece: inside a block comment
/// or a string that may run over lines (`` ` ``), left open by the text before it, or in code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Carry {
    /// Plain code
    #[default]
    Code,
    /// Inside a block comment
    Comment,
    /// Inside a string quoted with this character
    String(char),
}

/// Ranges to color as byte offsets, in order and non-overlapping.
pub fn highlight(text: &str, lang: &Lang) -> Vec<(Range<usize>, Token)> {
    highlight_from(text, lang, Carry::Code).0
}

/// [`highlight`] for text that goes on from other text: `carry` is where the text before it left
/// off. Also returns where this text leaves off, for the text after it.
pub fn highlight_from(
    text: &str,
    lang: &Lang,
    carry: Carry,
) -> (Vec<(Range<usize>, Token)>, Carry) {
    let mut out: Vec<(Range<usize>, Token)> = Vec::new();
    let bytes = text.as_bytes();
    // Finish what the text before left open.
    let (mut i, open) = match (carry, lang.block_comment) {
        (Carry::Comment, Some((_, close))) => {
            let found = text.find(close);
            let end = found.map_or(text.len(), |p| p + close.len());
            out.push((0..end, Token::Comment));
            (end, found.is_none().then_some(Carry::Comment))
        }
        (Carry::String(quote), _) => {
            let (end, open) = string_end(text, 0, quote);
            out.push((0..end, Token::String));
            (end, open.then_some(carry))
        }
        _ => (0, None),
    };
    out.retain(|(r, _)| !r.is_empty());
    if let Some(open) = open {
        return (out, open);
    }
    // What the last token left open: only a token that runs to the end of the text does.
    let mut left = Carry::Code;
    while i < text.len() {
        let rest = &text[i..];
        // Block comments first (Lua's --[[ must match before the -- line comment).
        if let Some((open, close)) = lang.block_comment
            && rest.starts_with(open)
        {
            let found = rest[open.len()..].find(close);
            let end = found.map_or(text.len(), |p| i + open.len() + p + close.len());
            if found.is_none() {
                left = Carry::Comment;
            }
            out.push((i..end, Token::Comment));
            i = end;
            continue;
        }
        if lang
            .line_comments
            .iter()
            .any(|c| starts_with(rest, c, lang.ignore_case))
            && (i == 0 || !is_ident(text[..i].chars().next_back().unwrap_or(' ')))
        {
            let end = rest.find('\n').map_or(text.len(), |p| i + p);
            out.push((i..end, Token::Comment));
            i = end;
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        if lang.quotes.contains(&c) {
            let (end, open) = string_end(text, i + c.len_utf8(), c);
            if open {
                left = Carry::String(c);
            }
            out.push((i..end, Token::String));
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < text.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'.' || bytes[j] == b'_')
            {
                j += 1;
            }
            out.push((i..j, Token::Number));
            i = j;
            continue;
        }
        if is_ident(c) {
            let end = rest
                .char_indices()
                .find(|&(_, ch)| !is_ident(ch))
                .map_or(text.len(), |(p, _)| i + p);
            let word = &text[i..end];
            let keyword = if lang.ignore_case {
                lang.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
            } else {
                lang.keywords.contains(&word)
            };
            if keyword {
                out.push((i..end, Token::Keyword));
            }
            i = end;
            continue;
        }
        i += c.len_utf8();
    }
    (out, left)
}

/// End of a string quoted with `quote` whose content starts at `from`: after the same quote
/// (backslash escapes the next char). Unterminated, it runs to the end of the line, or for `` ` ``
/// (which may span lines) to the end of the text; the flag is set when it is still open there.
fn string_end(text: &str, from: usize, quote: char) -> (usize, bool) {
    let mut escaped = false;
    for (k, ch) in text[from..].char_indices() {
        if ch == '\n' && quote != '`' {
            return (from + k, false);
        }
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return (from + k + ch.len_utf8(), false);
        }
    }
    (text.len(), quote == '`')
}

fn starts_with(text: &str, prefix: &str, ignore_case: bool) -> bool {
    if ignore_case {
        text.get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    } else {
        text.starts_with(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens<'a>(text: &'a str, ext: &str) -> Vec<(&'a str, Token)> {
        let lang = lang_for(ext).expect("lang");
        highlight(text, lang)
            .into_iter()
            .map(|(r, t)| (&text[r], t))
            .collect()
    }

    #[test]
    fn rust_basics() {
        assert_eq!(
            tokens("fn main() { let s = \"a\\\"b\"; // hi\n 42 }", "rs"),
            [
                ("fn", Token::Keyword),
                ("let", Token::Keyword),
                ("\"a\\\"b\"", Token::String),
                ("// hi", Token::Comment),
                ("42", Token::Number),
            ]
        );
        // A lifetime ' is not a string.
        assert_eq!(tokens("&'a str", "RS"), [("str", Token::Keyword)]);
    }

    #[test]
    fn other_languages() {
        // Multi-byte text inside a token: ranges must stay on char boundaries.
        assert_eq!(
            tokens("x = 1 # note\nprint('中文')", "py"),
            [
                ("1", Token::Number),
                ("# note", Token::Comment),
                ("'中文'", Token::String),
            ]
        );
        assert_eq!(
            tokens("SELECT * FROM t -- c", "sql"),
            [
                ("SELECT", Token::Keyword),
                ("FROM", Token::Keyword),
                ("-- c", Token::Comment),
            ]
        );
        assert_eq!(
            tokens("<a href=\"x\"><!-- c --></a>", "html"),
            [("\"x\"", Token::String), ("<!-- c -->", Token::Comment)]
        );
        // An unterminated block comment runs to the end of the file.
        assert_eq!(tokens("/* open", "js"), [("/* open", Token::Comment)]);
        assert!(lang_for("txt").is_none());
    }

    /// Highlighting piece by piece, each piece starting where the one before left off, colors the
    /// same as highlighting the whole text at once.
    #[test]
    fn pieces_carry_open_comments_and_strings() {
        let lang = lang_for("js").expect("lang");
        let pieces = [
            "let a = 1; /* opens",
            "still comment let",
            "ends */ let s = `multi",
            "line ${x}",
            "done` + 2; // tail",
        ];
        let whole = pieces.join("\n");
        let mut joined = Vec::new();
        let mut carry = Carry::Code;
        let mut at = 0;
        for piece in pieces {
            let (marks, next) = highlight_from(piece, lang, carry);
            joined.extend(
                marks
                    .into_iter()
                    .map(|(r, t)| (r.start + at..r.end + at, t)),
            );
            carry = next;
            at += piece.len() + 1;
        }
        // The whole text joins a comment or string cut by a piece's end into one range: compare
        // byte by byte, without the line breaks between the pieces.
        let bytes = whole.as_bytes();
        let flat = |marks: Vec<(Range<usize>, Token)>| -> Vec<(usize, Token)> {
            marks
                .into_iter()
                .flat_map(|(r, t)| r.map(move |b| (b, t)))
                .filter(|&(b, _)| bytes[b] != b'\n')
                .collect()
        };
        assert_eq!(flat(joined), flat(highlight(&whole, lang)));
        assert_eq!(carry, Carry::Code);
        // Left open at the end.
        assert_eq!(highlight_from("/* x", lang, Carry::Code).1, Carry::Comment);
        assert_eq!(
            highlight_from("`x", lang, Carry::Code).1,
            Carry::String('`')
        );
        assert_eq!(highlight_from("'x", lang, Carry::Code).1, Carry::Code);
        assert_eq!(
            highlight_from("x */ y", lang, Carry::Comment).0,
            [(0..4, Token::Comment)]
        );
    }

    #[test]
    fn identifiers_with_digits_are_not_numbers() {
        assert_eq!(tokens("x1 = u8", "rs"), [("u8", Token::Keyword)]);
    }
}
