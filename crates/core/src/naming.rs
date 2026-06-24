//! File naming & foldering (DESIGN.md §10).
//!
//! Given a list/group path, a book's sequence number, and a chosen candidate,
//! produce a sanitized destination path under
//! `<list folder>/<group>/<subgroup>/<NN - Author - Title.ext>`.
//!
//! Everything here is **pure** (no I/O): callers pass in the set of paths that
//! already exist (e.g. siblings written this run) so collisions can be resolved
//! deterministically with a `(2)`, `(3)`, … suffix. This keeps the module
//! trivially unit-testable and side-effect free.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::model::{Candidate, ListSettings};

/// Characters that are illegal (or unwise) in path components on the platforms
/// we target. We strip the DESIGN-listed reserved set plus control characters.
const RESERVED: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Maximum length (in characters) of a single produced filename, extension
/// included. Long titles are truncated to keep within common filesystem limits
/// (255 bytes is the usual ceiling; we cap conservatively on char count).
pub const MAX_FILENAME_CHARS: usize = 180;

/// How many leading md5 hex chars to append to each filename as a uniqueness +
/// determinism tag (`… - 3ddd0b.epub`). 6 hex = 24 bits — collisions among one
/// book's handful of variations are astronomically unlikely.
pub const MD5_TAG_LEN: usize = 6;

/// Inputs needed to render a destination filename for one book.
#[derive(Debug, Clone)]
pub struct NameContext<'a> {
    /// 1-based sequence number for the book within its scope.
    pub seq: u32,
    /// The chosen candidate (provides authors/title/extension fallbacks).
    pub candidate: &'a Candidate,
    /// Request-level title, used when the candidate's title is empty.
    pub title: &'a str,
    /// Request-level authors, used when the candidate has none.
    pub authors: &'a [String],
}

/// Sanitize a single path *component* (folder or the stem of a file): strip
/// reserved/control characters, collapse runs of whitespace to a single space,
/// and trim. A component that sanitizes to empty becomes `"_"` so we never emit
/// an empty path segment.
pub fn sanitize_component(s: &str) -> String {
    let mut cleaned = String::with_capacity(s.len());
    for ch in s.chars() {
        if RESERVED.contains(&ch) || ch.is_control() {
            // Drop reserved/control chars entirely (becomes a word boundary).
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
    }
    // Collapse whitespace.
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    // Trim trailing dots/spaces (Windows dislikes trailing dots).
    let trimmed = collapsed.trim_matches([' ', '.'].as_ref());
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Join authors for display in a filename. Multiple authors are joined with
/// `", "`; an empty list yields `"Unknown"`.
fn join_authors(authors: &[String]) -> String {
    let names: Vec<&str> = authors
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect();
    if names.is_empty() {
        "Unknown".to_string()
    } else {
        names.join(", ")
    }
}

/// Render the filename (stem + extension) from a template, without sanitization
/// or collision handling. Supported placeholders:
///   `{seq}`        — sequence number, no padding
///   `{seq:02}`     — sequence number, zero-padded to 2 (any width `:0N`)
///   `{authors}`    — joined author list
///   `{title}`      — title
///   `{ext}`        — file extension (no dot)
fn render_template(template: &str, ctx: &NameContext, ext: &str) -> String {
    // Prefer the request's CLEAN input metadata; fall back to the candidate's
    // scraped values only when the request didn't supply them. This keeps every
    // variation of a book named consistently (e.g. "Robert Louis Stevenson"), instead of
    // each download inheriting whatever messy author string a mirror returned.
    let title = if ctx.title.trim().is_empty() {
        &ctx.candidate.title
    } else {
        ctx.title
    };
    let authors = if ctx.authors.iter().all(|a| a.trim().is_empty()) {
        join_authors(&ctx.candidate.authors)
    } else {
        join_authors(ctx.authors)
    };

    let mut out = String::with_capacity(template.len() + 16);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close) = template[i..].find('}') {
                let token = &template[i + 1..i + close];
                out.push_str(&expand_token(token, ctx.seq, &authors, title, ext));
                i += close + 1;
                continue;
            }
        }
        // Not a placeholder; copy the char.
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Expand one `{...}` token. Unknown tokens are left bracketed verbatim so a
/// typo'd template is visible rather than silently dropped.
fn expand_token(token: &str, seq: u32, authors: &str, title: &str, ext: &str) -> String {
    let (name, fmt) = match token.split_once(':') {
        Some((n, f)) => (n, Some(f)),
        None => (token, None),
    };
    match name {
        "seq" => match fmt {
            Some(f) if f.starts_with('0') => {
                let width: usize = f[1..].parse().unwrap_or(0);
                format!("{seq:0width$}")
            }
            _ => seq.to_string(),
        },
        "authors" => authors.to_string(),
        "title" => title.to_string(),
        "ext" => ext.to_string(),
        _ => format!("{{{token}}}"),
    }
}

/// The extension to use for a candidate: its declared format, else `"bin"`.
fn extension_of(candidate: &Candidate) -> String {
    candidate
        .extension
        .as_ref()
        .map(|f| f.ext())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "bin".to_string())
}

/// Render a sanitized **filename** (no directory) for a book, applying the
/// list's `naming_template`, sanitization, and length cap. Collision handling is
/// applied separately by [`destination`].
pub fn filename(settings: &ListSettings, ctx: &NameContext) -> String {
    let ext = extension_of(ctx.candidate);
    let rendered = render_template(&settings.naming_template, ctx, &ext);

    // Split into stem + extension so we sanitize/truncate the stem but keep a
    // clean extension. We split on the *last* '.' that the template's `{ext}`
    // produced; if the template has no extension, treat the whole thing as stem.
    let (stem_raw, ext_raw) = match rendered.rfind('.') {
        Some(idx) if idx + 1 < rendered.len() => (&rendered[..idx], Some(&rendered[idx + 1..])),
        _ => (rendered.as_str(), None),
    };

    let mut stem = sanitize_component(stem_raw);
    let ext_clean = ext_raw.map(sanitize_component);

    // Append a short md5 tag so every VARIATION of a book gets a UNIQUE,
    // DETERMINISTIC filename. Two copies of one book (or two books that resolve to
    // one file) can never collide on a path — no order-dependent " (2)" suffix that
    // could silently overwrite a sibling — and re-planning/reorganize is stable
    // (the name depends only on seq/author/title/md5, not on candidate order).
    let md5_tag: String = ctx.candidate.md5.chars().take(MD5_TAG_LEN).collect();
    let suffix = if md5_tag.is_empty() {
        String::new()
    } else {
        format!(" - {md5_tag}")
    };

    // Length cap: reserve room for the md5 tag + extension + dot, truncate the stem
    // (so the tag is never the part that gets cut off).
    if let Some(ext) = &ext_clean {
        let budget =
            MAX_FILENAME_CHARS.saturating_sub(ext.chars().count() + 1 + suffix.chars().count());
        truncate_chars(&mut stem, budget.max(1));
        if stem.is_empty() {
            stem.push('_');
        }
        format!("{stem}{suffix}.{ext}")
    } else {
        let budget = MAX_FILENAME_CHARS.saturating_sub(suffix.chars().count());
        truncate_chars(&mut stem, budget.max(1));
        if stem.is_empty() {
            stem.push('_');
        }
        format!("{stem}{suffix}")
    }
}

/// Truncate `s` in place to at most `max` characters (not bytes), trimming any
/// trailing whitespace the cut leaves behind.
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() <= max {
        return;
    }
    let truncated: String = s.chars().take(max).collect();
    *s = truncated.trim_end().to_string();
}

/// The directory (relative to the list root) for a chain of nested group names.
/// Each name is sanitized into one path component. Empty group names (e.g. a
/// synthesized root group) are skipped so they don't create `_` folders.
pub fn group_dir(root: &Path, groups: &[&str]) -> PathBuf {
    let mut dir = root.to_path_buf();
    for g in groups {
        let trimmed = g.trim();
        if trimmed.is_empty() {
            continue;
        }
        dir.push(sanitize_component(trimmed));
    }
    dir
}

/// Compute the full destination path for a book, resolving collisions against
/// `taken` (a set of already-claimed absolute paths). On collision, a ` (2)`,
/// ` (3)`, … suffix is inserted before the extension. The returned path is also
/// inserted into `taken` so subsequent calls see it.
///
/// `root` is the list's destination folder; `groups` is the nested group-name
/// chain (outermost first) leading to this book's subfolder.
pub fn destination(
    root: &Path,
    groups: &[&str],
    settings: &ListSettings,
    ctx: &NameContext,
    taken: &mut HashSet<PathBuf>,
) -> PathBuf {
    let dir = group_dir(root, groups);
    let base = filename(settings, ctx);

    let (stem, ext) = match base.rfind('.') {
        Some(idx) if idx + 1 < base.len() => {
            (base[..idx].to_string(), Some(base[idx + 1..].to_string()))
        }
        _ => (base.clone(), None),
    };

    let mut n = 1u32;
    loop {
        let candidate_name = if n == 1 {
            base.clone()
        } else {
            match &ext {
                Some(e) => format!("{stem} ({n}).{e}"),
                None => format!("{stem} ({n})"),
            }
        };
        let path = dir.join(&candidate_name);
        if !taken.contains(&path) {
            taken.insert(path.clone());
            return path;
        }
        n += 1;
    }
}

/// Compute destinations for several *variations* (distinct candidates) of the
/// SAME book sharing one sequence number. Each candidate keys its extension off
/// its own format, so an epub and a pdf land at different filenames naturally;
/// two variations of the SAME format would collide on the base name, so the
/// shared `taken` set disambiguates the second with a ` (2)`, ` (3)`, … suffix.
///
/// `candidates` is the ordered list of requested variations to place. Returns a
/// `(md5, destination)` pair per input candidate, in the same order.
#[allow(clippy::too_many_arguments)]
pub fn destinations_for_variations(
    root: &Path,
    groups: &[&str],
    settings: &ListSettings,
    seq: u32,
    title: &str,
    authors: &[String],
    candidates: &[&Candidate],
    taken: &mut HashSet<PathBuf>,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::with_capacity(candidates.len());
    for cand in candidates {
        let ctx = NameContext {
            seq,
            candidate: cand,
            title,
            authors,
        };
        let dest = destination(root, groups, settings, &ctx, taken);
        out.push((cand.md5.clone(), dest));
    }
    out
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Format;

    fn cand(title: &str, authors: &[&str], ext: Option<Format>) -> Candidate {
        Candidate {
            md5: "0".repeat(32),
            title: title.into(),
            authors: authors.iter().map(|s| s.to_string()).collect(),
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: ext,
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 0.0,
            job: None,
        }
    }

    fn ctx<'a>(seq: u32, c: &'a Candidate) -> NameContext<'a> {
        NameContext {
            seq,
            candidate: c,
            title: &c.title,
            authors: &c.authors,
        }
    }

    fn settings() -> ListSettings {
        ListSettings::default()
    }

    #[test]
    fn default_template_basic() {
        let c = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        let name = filename(&settings(), &ctx(1, &c));
        assert_eq!(
            name,
            "01 - Robert Louis Stevenson - Treasure Island - 000000.epub"
        );
    }

    #[test]
    fn prefers_request_input_over_messy_candidate_metadata() {
        // A mirror returned a messy author/title for this variation; the
        // request's clean input must win so every variation of the book is
        // named consistently.
        let c = cand(
            "Treasure Island №1 (retail scan)",
            &["Stevenson, Robert Louis (Author), Стивенсон, Роберт Луис (Author)"],
            Some(Format::Pdf),
        );
        let clean = vec!["Robert Louis Stevenson".to_string()];
        let nc = NameContext {
            seq: 1,
            candidate: &c,
            title: "Treasure Island",
            authors: &clean,
        };
        assert_eq!(
            filename(&settings(), &nc),
            "01 - Robert Louis Stevenson - Treasure Island - 000000.pdf"
        );
    }

    #[test]
    fn seq_padding_two_digits() {
        let c = cand("X", &["Y"], Some(Format::Pdf));
        assert_eq!(
            filename(&settings(), &ctx(7, &c)),
            "07 - Y - X - 000000.pdf"
        );
        assert_eq!(
            filename(&settings(), &ctx(42, &c)),
            "42 - Y - X - 000000.pdf"
        );
        // Three-digit seq overflows the pad width gracefully.
        assert_eq!(
            filename(&settings(), &ctx(123, &c)),
            "123 - Y - X - 000000.pdf"
        );
    }

    #[test]
    fn sanitization_strips_reserved() {
        let c = cand("A/B: C?* \"<>|D", &["E/F"], Some(Format::Epub));
        let name = filename(&settings(), &ctx(1, &c));
        // No reserved chars survive; whitespace collapsed.
        for r in RESERVED {
            assert!(
                !name.contains(*r),
                "reserved {r} should be stripped: {name}"
            );
        }
        assert_eq!(name, "01 - E F - A B C D - 000000.epub");
    }

    #[test]
    fn collapses_whitespace() {
        let c = cand("Hello    World\t\tBook", &["John   Doe"], Some(Format::Pdf));
        let name = filename(&settings(), &ctx(3, &c));
        assert_eq!(name, "03 - John Doe - Hello World Book - 000000.pdf");
    }

    #[test]
    fn missing_extension_defaults_bin() {
        let c = cand("No Ext", &["Author"], None);
        let name = filename(&settings(), &ctx(1, &c));
        assert!(name.ends_with(".bin"), "got {name}");
    }

    #[test]
    fn empty_authors_become_unknown() {
        let c = cand("Orphan Title", &[], Some(Format::Epub));
        let name = filename(&settings(), &ctx(2, &c));
        assert_eq!(name, "02 - Unknown - Orphan Title - 000000.epub");
    }

    #[test]
    fn multi_author_joined() {
        let c = cand("Two", &["Mark Twain", "Bret Harte"], Some(Format::Epub));
        let name = filename(&settings(), &ctx(1, &c));
        assert_eq!(name, "01 - Mark Twain, Bret Harte - Two - 000000.epub");
    }

    #[test]
    fn unicode_title_preserved() {
        let c = cand(
            "Les Misérables — Tomé Première",
            &["Victor Hugo"],
            Some(Format::Epub),
        );
        let name = filename(&settings(), &ctx(5, &c));
        // Unicode letters survive; only reserved/control chars are stripped.
        assert!(name.contains("Misérables"));
        assert!(name.contains("Première"));
        assert!(name.ends_with(".epub"));
    }

    #[test]
    fn long_title_is_capped() {
        let long = "Z".repeat(500);
        let c = cand(&long, &["A"], Some(Format::Epub));
        let name = filename(&settings(), &ctx(1, &c));
        assert!(
            name.chars().count() <= MAX_FILENAME_CHARS,
            "len {}",
            name.chars().count()
        );
        assert!(name.ends_with(".epub"));
    }

    #[test]
    fn collision_suffix_increments() {
        let root = Path::new("/out");
        let mut taken = HashSet::new();
        let c = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        let p1 = destination(root, &["Batch 1"], &settings(), &ctx(1, &c), &mut taken);
        let p2 = destination(root, &["Batch 1"], &settings(), &ctx(1, &c), &mut taken);
        let p3 = destination(root, &["Batch 1"], &settings(), &ctx(1, &c), &mut taken);
        assert_eq!(
            p1,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - 000000.epub"
            )
        );
        assert_eq!(
            p2,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - 000000 (2).epub"
            )
        );
        assert_eq!(
            p3,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - 000000 (3).epub"
            )
        );
    }

    #[test]
    fn nested_subgroups_become_subfolders() {
        let root = Path::new("/out");
        let mut taken = HashSet::new();
        let c = cand("Child Book", &["Author"], Some(Format::Pdf));
        let p = destination(
            root,
            &["Parent", "Child"],
            &settings(),
            &ctx(1, &c),
            &mut taken,
        );
        assert_eq!(
            p,
            PathBuf::from("/out/Parent/Child/01 - Author - Child Book - 000000.pdf")
        );
    }

    #[test]
    fn empty_group_name_skipped() {
        let root = Path::new("/out");
        let mut taken = HashSet::new();
        let c = cand("Root Book", &["Author"], Some(Format::Epub));
        let p = destination(root, &["", "Real"], &settings(), &ctx(1, &c), &mut taken);
        assert_eq!(
            p,
            PathBuf::from("/out/Real/01 - Author - Root Book - 000000.epub")
        );
    }

    #[test]
    fn group_dir_sanitizes_components() {
        let dir = group_dir(Path::new("/out"), &["Batch 1: Lift/Off"]);
        assert_eq!(dir, PathBuf::from("/out/Batch 1 Lift Off"));
    }

    #[test]
    fn custom_template_per_list() {
        let mut s = settings();
        s.naming_template = "{title} ({seq}).{ext}".to_string();
        let c = cand("Heidi", &["Spyri"], Some(Format::Epub));
        assert_eq!(filename(&s, &ctx(4, &c)), "Heidi (4) - 000000.epub");
    }

    #[test]
    fn two_same_format_variations_get_distinct_filenames() {
        let root = Path::new("/out");
        let mut taken = HashSet::new();
        // Two epub variations of the same book (distinct md5s).
        let mut c1 = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        c1.md5 = "a".repeat(32);
        let mut c2 = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        c2.md5 = "b".repeat(32);
        let out = destinations_for_variations(
            root,
            &["Batch 1"],
            &settings(),
            1,
            "Treasure Island",
            &["Robert Louis Stevenson".to_string()],
            &[&c1, &c2],
            &mut taken,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].1,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - aaaaaa.epub"
            )
        );
        // Distinct md5 tags → unique names; no order-dependent " (2)" needed.
        assert_eq!(
            out[1].1,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - bbbbbb.epub"
            )
        );
        assert_ne!(
            out[0].1, out[1].1,
            "same-format variations must not collide"
        );
    }

    #[test]
    fn epub_and_pdf_variations_coexist() {
        let root = Path::new("/out");
        let mut taken = HashSet::new();
        let mut e = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        e.md5 = "a".repeat(32);
        let mut p = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Pdf),
        );
        p.md5 = "b".repeat(32);
        let out = destinations_for_variations(
            root,
            &["Batch 1"],
            &settings(),
            1,
            "Treasure Island",
            &["Robert Louis Stevenson".to_string()],
            &[&e, &p],
            &mut taken,
        );
        // Different extensions + distinct md5 tags → distinct names.
        assert_eq!(
            out[0].1,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - aaaaaa.epub"
            )
        );
        assert_eq!(
            out[1].1,
            PathBuf::from(
                "/out/Batch 1/01 - Robert Louis Stevenson - Treasure Island - bbbbbb.pdf"
            )
        );
    }

    #[test]
    fn unknown_token_left_verbatim() {
        let mut s = settings();
        s.naming_template = "{title}-{bogus}.{ext}".to_string();
        let c = cand("T", &["A"], Some(Format::Pdf));
        // The reserved-char sanitizer keeps braces (not reserved), so the typo
        // is visible in the output rather than silently dropped.
        let name = filename(&s, &ctx(1, &c));
        assert!(name.contains("{bogus}"), "got {name}");
    }
}
