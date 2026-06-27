//! Shared UI-string catalog for the TUI.
//!
//! The catalog is the SAME data the desktop frontend uses. `app/ui/i18n.json`
//! is the single canonical source; the desktop currently mirrors it inline in
//! `app/ui/index.html` (a classic file://-openable page that can't `fetch()` a
//! sidecar), so that copy carries a sync note pointing here. The TUI embeds the
//! JSON at build time, so TUI chrome labels and decoded backend `ui_msg` tokens
//! render the same English strings as the desktop.
//!
//! Backend status/history strings are encoded as `ui_msg` tokens
//! (`"key\u{1f}k=v\u{1f}…"`, see `libgen_core::model::ui_msg`). [`decode`] turns
//! such a token into a resolved, interpolated string; non-token input (old
//! already-English rows, free-form messages) passes through unchanged.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The canonical catalog JSON, embedded at build time. Shared source of truth
/// with the desktop (see the module docs + the sync note in `index.html`).
const CATALOG_JSON: &str = include_str!("../../../app/ui/i18n.json");

/// `locale -> (key -> template)`.
type Catalog = HashMap<String, HashMap<String, String>>;

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_JSON).expect("app/ui/i18n.json must be valid JSON")
    })
}

/// Default UI locale. (Locale selection is a deferred follow-up; the TUI ships
/// English to match the desktop's default.)
pub const DEFAULT_LOCALE: &str = "en";

/// Resolve a template for `key`: `locale`, then English fallback.
fn template<'a>(cat: &'a Catalog, key: &str, locale: &str) -> Option<&'a str> {
    cat.get(locale)
        .and_then(|m| m.get(key))
        .or_else(|| cat.get("en").and_then(|m| m.get(key)))
        .map(String::as_str)
}

/// Replace `{name}` placeholders with `args[name]`; unknown placeholders are
/// left verbatim (mirrors the desktop `t()`'s `{name}` fallback).
fn interpolate(tpl: &str, args: &[(&str, &str)]) -> String {
    if !tpl.contains('{') {
        return tpl.to_string();
    }
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        if let Some(rel) = tail.find('}') {
            let name = &tail[1..rel];
            match args.iter().find(|(k, _)| *k == name) {
                Some((_, v)) => out.push_str(v),
                None => out.push_str(&tail[..=rel]),
            }
            rest = &tail[rel + 1..];
        } else {
            // Unterminated `{` — emit the remainder verbatim.
            out.push_str(tail);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Translate a catalog `key` with `{name}` interpolation. Falls back `locale →
/// en → the raw key`, matching the desktop's `t()`.
pub fn t(key: &str, args: &[(&str, &str)], locale: &str) -> String {
    let cat = catalog();
    let tpl = template(cat, key, locale).unwrap_or(key);
    interpolate(tpl, args)
}

/// Translate a catalog `key` in the default locale.
pub fn tr(key: &str) -> String {
    t(key, &[], DEFAULT_LOCALE)
}

/// Decode a backend `ui_msg` token in the default locale. See [`decode_locale`].
pub fn decode(packed: &str) -> String {
    decode_locale(packed, DEFAULT_LOCALE)
}

/// Decode a backend `ui_msg` token (`"key\u{1f}k=v\u{1f}…"`). If the leading key
/// is in the catalog, resolve + interpolate it; otherwise return `packed`
/// unchanged so already-English / free-form strings pass through untouched.
pub fn decode_locale(packed: &str, locale: &str) -> String {
    let mut parts = packed.split('\u{1f}');
    let key = parts.next().unwrap_or("");
    let cat = catalog();
    // Catalog membership is keyed off English (the canonical key set), like the
    // desktop's `if (!(key in I18N.en)) return s;`.
    if template(cat, key, "en").is_none() {
        return packed.to_string();
    }
    let args: Vec<(&str, &str)> = parts.filter_map(|p| p.split_once('=')).collect();
    t(key, &args, locale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libgen_core::model::ui_msg;

    #[test]
    fn catalog_parses_and_has_en_and_zh() {
        let cat = catalog();
        assert!(cat.contains_key("en"));
        assert!(cat.contains_key("zh"));
        assert_eq!(cat["en"].len(), cat["zh"].len());
        // A representative chrome key resolves.
        assert_eq!(tr("filter.cantdl"), "Cannot download");
    }

    #[test]
    fn decode_resolves_token_with_interpolation() {
        // event.matched: "{n} candidate(s) → matched (auto-selected {ext})"
        let packed = ui_msg("event.matched", &[("n", "3"), ("ext", "epub")]);
        assert_eq!(
            decode(&packed),
            "3 candidate(s) → matched (auto-selected epub)"
        );
    }

    #[test]
    fn decode_key_only_token() {
        let packed = ui_msg("event.paused", &[]);
        assert_eq!(decode(&packed), "paused");
    }

    #[test]
    fn decode_passes_through_non_catalog_input() {
        // An old already-English history row, or any free-form string.
        assert_eq!(
            decode("completed on cdn1 (4 MB)"),
            "completed on cdn1 (4 MB)"
        );
        assert_eq!(decode("Paused"), "Paused");
        assert_eq!(decode(""), "");
    }

    #[test]
    fn t_falls_back_to_english_then_key() {
        // Unknown locale → English template.
        assert_eq!(t("filter.review", &[], "de"), "Check download");
        // Unknown key → the key itself (both locales).
        assert_eq!(t("nope.not_a_key", &[], "en"), "nope.not_a_key");
        assert_eq!(t("nope.not_a_key", &[], "zh"), "nope.not_a_key");
    }

    #[test]
    fn t_zh_locale_resolves() {
        assert_eq!(t("filter.cantdl", &[], "zh"), "无法下载");
    }

    #[test]
    fn interpolate_leaves_unknown_placeholder_verbatim() {
        // event.done template uses {host} and {mb}; omit one arg.
        let packed = ui_msg("event.done", &[("host", "cdn2")]);
        assert_eq!(decode(&packed), "completed on cdn2 ({mb} MB)");
    }
}
