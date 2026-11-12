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
/// `tn!("clip.copied", n)` reads `clip.copied_one` or `clip.copied_other`.
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
    use rust_i18n::t;

    use super::plural_key;

    #[test]
    fn one_is_singular_and_everything_else_plural() {
        assert_eq!(plural_key("clip.copied", 1), "clip.copied_one");
        assert_eq!(plural_key("clip.copied", 0), "clip.copied_other");
        assert_eq!(plural_key("clip.copied", 2), "clip.copied_other");
        assert_eq!(plural_key("clip.copied", 21), "clip.copied_other");
    }

    #[test]
    fn english_reads_naturally_and_chinese_is_unchanged() {
        let en = |n: usize| tn!("clip.copied", n, locale = "en").into_owned();
        assert_eq!(en(1), "Copied 1 item");
        assert_eq!(en(2), "Copied 2 items");
        assert_eq!(en(0), "Copied 0 items");
        assert_eq!(en(1234), "Copied 1,234 items");
        let zh = |n: usize| tn!("clip.copied", n, locale = "zh-TW").into_owned();
        assert_eq!(zh(1), "已複製 1 個項目");
        assert_eq!(zh(3), "已複製 3 個項目");
        // Extra arguments are passed through.
        let undid = tn!(
            "undo.done",
            1,
            op = t!("op.copy", locale = "en"),
            locale = "en"
        );
        assert_eq!(undid, "Undid Copy (1 item)");
    }
}
