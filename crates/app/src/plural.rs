//! Singular and plural forms for counted messages ("1 item", "2 items").
//!
//! A counted message has two locale keys, `<key>_one` and `<key>_other`. English needs both;
//! Chinese has no plural forms, so both keys carry the same text there. Use [`tn!`] instead of
//! `t!` for these keys so the form always follows the number shown.

/// Locale key for `count`: `<key>_one` for exactly one, `<key>_other` otherwise (including zero,
/// as in "0 items").
pub(crate) fn plural_key(key: &str, count: usize) -> String {
    let form = if count == 1 { "one" } else { "other" };
    format!("{key}_{form}")
}

/// `t!` for a counted message: picks `<key>_one` or `<key>_other` by `count` (a `usize`) and passes
/// it, with digit grouping, as `%{count}`. Other `name = value` arguments are passed through.
///
/// `tn!("extract.done", n)` reads `extract.done_one` or `extract.done_other`.
macro_rules! tn {
    ($key:literal, $count:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let count: usize = $count;
        rust_i18n::t!(
            crate::plural::plural_key($key, count),
            count = arke_core::format::format_count(count)
            $(, $name = $value)*
        )
    }};
}

#[cfg(test)]
mod tests {

    use super::plural_key;

    #[test]
    fn one_is_singular_and_everything_else_plural() {
        assert_eq!(plural_key("extract.done", 1), "extract.done_one");
        assert_eq!(plural_key("extract.done", 0), "extract.done_other");
        assert_eq!(plural_key("extract.done", 2), "extract.done_other");
        assert_eq!(plural_key("extract.done", 21), "extract.done_other");
    }

    #[test]
    fn english_reads_naturally_and_chinese_is_unchanged() {
        let en = |n: usize| tn!("extract.done", n, locale = "en").into_owned();
        assert_eq!(en(1), "Extracted 1 file");
        assert_eq!(en(2), "Extracted 2 files");
        assert_eq!(en(0), "Extracted 0 files");
        assert_eq!(en(1234), "Extracted 1,234 files");
        let zh = |n: usize| tn!("extract.done", n, locale = "zh-TW").into_owned();
        assert_eq!(zh(1), "已解壓縮 1 個檔案");
        assert_eq!(zh(3), "已解壓縮 3 個檔案");
        // Extra arguments are passed through.
        let compressed = tn!("compress.done", 1, name = "a.zip", locale = "en");
        assert_eq!(compressed, "Compressed 1 file into “a.zip”");
    }
}
