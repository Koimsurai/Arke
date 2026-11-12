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

/// Ranges to color as byte offsets, in order and non-overlapping.
pub fn highlight(text: &str, lang: &Lang) -> Vec<(Range<usize>, Token)> {
    let mut out: Vec<(Range<usize>, Token)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        // Block comments first (Lua's --[[ must match before the -- line comment).
        if let Some((open, close)) = lang.block_comment
            && rest.starts_with(open)
        {
            let end = rest[open.len()..]
                .find(close)
                .map_or(text.len(), |p| i + open.len() + p + close.len());
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
            // Up to the same quote (backslash escapes the next char); unterminated runs to end of
            // line.
            let mut j = i + c.len_utf8();
            let mut escaped = false;
            let mut end = None;
            for (k, ch) in text[j..].char_indices() {
                if ch == '\n' && c != '`' {
                    end = Some(j + k);
                    break;
                }
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == c {
                    end = Some(j + k + ch.len_utf8());
                    break;
                }
            }
            j = end.unwrap_or(text.len());
            out.push((i..j, Token::String));
            i = j;
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
    out
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

    #[test]
    fn identifiers_with_digits_are_not_numbers() {
        assert_eq!(tokens("x1 = u8", "rs"), [("u8", Token::Keyword)]);
    }
}
