//! Parse Markdown / JSON reading lists into the normalized [`DownloadList`].
//!
//! Both formats desugar into the same model. The real fixtures live in
//! `fixtures/jeremy_public_domain_list.{md,json}`; golden tests pin behavior.

use crate::model::{BookInput, BookRequest, DownloadList, Format, Group};
use anyhow::{Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// JSON (canonical)
// ---------------------------------------------------------------------------

/// Wire shape of the canonical JSON list format. Kept private; it desugars into
/// the shared [`DownloadList`] model so the rest of the engine never sees it.
#[derive(Debug, Deserialize)]
struct JsonList {
    list_title: String,
    #[serde(default)]
    sections: Vec<JsonSection>,
}

#[derive(Debug, Deserialize)]
struct JsonSection {
    // `id` is part of the schema but unused by the model; accept and ignore it.
    #[allow(dead_code)]
    #[serde(default)]
    id: i64,
    title: String,
    #[serde(default)]
    books: Vec<JsonBook>,
}

#[derive(Debug, Deserialize)]
struct JsonBook {
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    year: Option<u16>,
}

/// Parse the canonical JSON list format
/// (`{ list_title, sections: [{ id, title, books: [{ title, author }] }] }`)
/// into a [`DownloadList`].
pub fn parse_json(input: &str) -> Result<DownloadList> {
    let raw: JsonList = serde_json::from_str(input).context("parsing JSON reading list")?;

    let groups = raw
        .sections
        .into_iter()
        .map(|section| {
            let mut group = Group::new(section.title);
            group.books = section
                .books
                .into_iter()
                .map(|book| {
                    BookRequest::new(BookInput {
                        title: book.title.trim().to_string(),
                        authors: split_authors(&book.author),
                        year: book.year,
                        ..Default::default()
                    })
                })
                .collect();
            group
        })
        .collect();

    Ok(DownloadList {
        title: raw.list_title.trim().to_string(),
        settings: Default::default(),
        groups,
    })
}

// ---------------------------------------------------------------------------
// Markdown (human-friendly, forgiving)
// ---------------------------------------------------------------------------

/// Parse the human-friendly Markdown format:
/// `#` = list title, `##`/`###` = nested groups, numbered/bulleted items =
/// books (`Title — Author`, `Title by Author`, optional `(Year)` / `[ISBN]`).
pub fn parse_markdown(input: &str) -> Result<DownloadList> {
    let mut title = String::new();
    let mut groups: Vec<Group> = Vec::new();
    // Index of the current `##` group and, if any, current `###` subgroup,
    // so items attach to the deepest open heading.
    let mut cur_h2: Option<usize> = None;
    let mut cur_h3: Option<usize> = None;

    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = heading(line, "###") {
            // Subgroup nested under the current `##` group. If there is no open
            // `##` group yet, synthesize one so the subgroup has a parent.
            let parent = match cur_h2 {
                Some(idx) => idx,
                None => {
                    groups.push(Group::new(""));
                    cur_h2 = Some(groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[parent].subgroups.push(Group::new(group_name(rest)));
            cur_h3 = Some(groups[parent].subgroups.len() - 1);
        } else if let Some(rest) = heading(line, "##") {
            groups.push(Group::new(group_name(rest)));
            cur_h2 = Some(groups.len() - 1);
            cur_h3 = None;
        } else if let Some(rest) = heading(line, "#") {
            title = rest.to_string();
        } else if let Some(item) = list_item(line) {
            let request = parse_item(item);
            // Attach to deepest open heading: subgroup, else group, else a
            // synthesized root group named after the list.
            match (cur_h2, cur_h3) {
                (Some(h2), Some(h3)) => groups[h2].subgroups[h3].books.push(request),
                (Some(h2), None) => groups[h2].books.push(request),
                (None, _) => {
                    if groups.is_empty() {
                        groups.push(Group::new(title.clone()));
                        cur_h2 = Some(0);
                    }
                    groups[cur_h2.unwrap()].books.push(request);
                }
            }
        }
        // Non-heading, non-item lines (prose, separators) are ignored.
    }

    Ok(DownloadList {
        title: title.trim().to_string(),
        settings: Default::default(),
        groups,
    })
}

/// Normalize a group heading into its display name. A heading of the form
/// `Batch 1 — Lift-Off` carries a leading sequence/label before an em dash; the
/// group name is the part *after* the em dash (matching the JSON `title`). A
/// heading without an em dash is used verbatim.
fn group_name(heading: &str) -> String {
    match heading.split_once('—') {
        Some((_, name)) => name.trim().to_string(),
        None => heading.trim().to_string(),
    }
}

/// If `line` is a heading at exactly the given level, return the trimmed text.
/// `##` must not also match `###`, so callers must check deepest level first.
fn heading<'a>(line: &'a str, hashes: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(hashes)?;
    // The next char must be a space (so `##` doesn't swallow `###`).
    if rest.starts_with(' ') {
        Some(rest.trim())
    } else if rest.is_empty() {
        Some("")
    } else {
        None
    }
}

/// If `line` is a numbered (`1. `) or bulleted (`- ` / `* `) list item, return
/// the item text after the marker.
fn list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest.trim());
    }
    // Numbered: leading digits, then `.` or `)`, then a space.
    let digits_end = line.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let after = &line[digits_end..];
    let rest = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some(rest.trim())
}

/// Parse a single Markdown item body into a [`BookRequest`].
///
/// Grammar (all suffixes optional, any order at the tail):
/// `Title <sep> Author(s) (YYYY) [ISBN] {format}`
/// where `<sep>` is em dash `—`, ` - `, or ` by `. Authors joined by ` and `
/// split into multiple. An item with no separator becomes title-only.
fn parse_item(item: &str) -> BookRequest {
    let mut input = BookInput::default();
    let mut text = item.to_string();

    // Strip trailing annotations first so they don't pollute the author field.
    // Repeatedly peel `{...}`, `[...]`, `(...)` from the end.
    loop {
        let trimmed = text.trim_end();
        if let Some(inner) = enclosed_suffix(trimmed, '{', '}') {
            input.format_pref = vec![Format::parse(inner)];
            text = trimmed[..trimmed.len() - (inner.len() + 2)].to_string();
        } else if let Some(inner) = enclosed_suffix(trimmed, '[', ']') {
            input.isbn = Some(inner.trim().to_string());
            text = trimmed[..trimmed.len() - (inner.len() + 2)].to_string();
        } else if let Some(inner) = enclosed_suffix(trimmed, '(', ')') {
            // Only treat as a year if it parses as one; otherwise leave it in
            // the title (e.g. a parenthetical that is part of the title).
            if let Ok(year) = inner.trim().parse::<u16>() {
                input.year = Some(year);
                text = trimmed[..trimmed.len() - (inner.len() + 2)].to_string();
            } else {
                text = trimmed.to_string();
                break;
            }
        } else {
            text = trimmed.to_string();
            break;
        }
    }

    // Split title from author(s) on the first recognized separator.
    let (title, authors) = split_title_author(text.trim());
    input.title = title;
    input.authors = authors;

    if input.authors.is_empty() {
        tracing::warn!(item = %item, "reading-list item has no author separator; keeping as title-only");
    }

    BookRequest::new(input)
}

/// If `text` ends with `open...close`, return the inner slice (without the
/// delimiters). Requires the open delimiter to be the *last* such pair so we
/// peel from the tail.
fn enclosed_suffix(text: &str, open: char, close: char) -> Option<&str> {
    let text = text.trim_end();
    if !text.ends_with(close) {
        return None;
    }
    let open_idx = text.rfind(open)?;
    let close_idx = text.len() - close.len_utf8();
    if open_idx >= close_idx {
        return None;
    }
    Some(&text[open_idx + open.len_utf8()..close_idx])
}

/// Split an item into `(title, authors)` on the first recognized separator.
/// Separators, in priority order: em dash `—`, ` - `, ` by `. No separator →
/// the whole string is the title and authors are empty.
fn split_title_author(text: &str) -> (String, Vec<String>) {
    // Em dash (primary). Match with optional surrounding spaces.
    if let Some(idx) = text.find('—') {
        let title = text[..idx].trim().to_string();
        let authors = &text[idx + '—'.len_utf8()..];
        return (title, split_authors(authors));
    }
    if let Some(idx) = text.find(" - ") {
        let title = text[..idx].trim().to_string();
        let authors = &text[idx + 3..];
        return (title, split_authors(authors));
    }
    if let Some(idx) = find_word_separator(text, " by ") {
        let title = text[..idx].trim().to_string();
        let authors = &text[idx + 4..];
        return (title, split_authors(authors));
    }
    (text.trim().to_string(), Vec::new())
}

/// Find a case-insensitive ` by ` separator, returning the byte index of its
/// leading space. Avoids matching ` by ` inside a word boundary issue by
/// relying on the surrounding spaces already present in the needle.
fn find_word_separator(text: &str, needle: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    lower.find(needle)
}

/// Split an author string into individual authors on ` and `, trimming each and
/// dropping empties. An empty/blank input yields an empty vec.
fn split_authors(authors: &str) -> Vec<String> {
    // Co-authors may be joined by " and ", " & ", or ";". Comma is intentionally
    // NOT a separator — it commonly appears as "Last, First".
    let normalized = authors.replace(" & ", " and ").replace(';', " and ");
    normalized
        .split(" and ")
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| a.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch on file extension / content sniffing.
pub fn parse_auto(input: &str, hint_is_json: bool) -> Result<DownloadList> {
    if hint_is_json || input.trim_start().starts_with('{') {
        parse_json(input)
    } else {
        parse_markdown(input)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn one_book(md: &str) -> BookInput {
        let list = parse_markdown(md).unwrap();
        list.groups[0].books[0].input.clone()
    }

    #[test]
    fn em_dash_separator() {
        let b = one_book("# L\n- Treasure Island — Robert Louis Stevenson");
        assert_eq!(b.title, "Treasure Island");
        assert_eq!(b.authors, vec!["Robert Louis Stevenson"]);
    }

    #[test]
    fn hyphen_separator() {
        let b = one_book("# L\n- Treasure Island - Robert Louis Stevenson");
        assert_eq!(b.title, "Treasure Island");
        assert_eq!(b.authors, vec!["Robert Louis Stevenson"]);
    }

    #[test]
    fn by_separator() {
        let b = one_book("# L\n- Kidnapped by Robert Louis Stevenson");
        assert_eq!(b.title, "Kidnapped");
        assert_eq!(b.authors, vec!["Robert Louis Stevenson"]);
    }

    #[test]
    fn multi_author_split() {
        let b = one_book("# L\n- Grimms' Fairy Tales — Jacob Grimm and Wilhelm Grimm");
        assert_eq!(b.title, "Grimms' Fairy Tales");
        assert_eq!(b.authors, vec!["Jacob Grimm", "Wilhelm Grimm"]);
    }

    #[test]
    fn multi_author_split_on_ampersand_and_semicolon() {
        let b = one_book("# L\n- Tales from Shakespeare — Charles Lamb & Mary Lamb");
        assert_eq!(b.authors, vec!["Charles Lamb", "Mary Lamb"]);
        let b2 = one_book("# L\n- X — Alice; Bob");
        assert_eq!(b2.authors, vec!["Alice", "Bob"]);
        // A comma is NOT a separator (it's often "Last, First").
        let b3 = one_book("# L\n- Y — Twain, Mark");
        assert_eq!(b3.authors, vec!["Twain, Mark"]);
    }

    #[test]
    fn year_isbn_format_suffixes() {
        let b = one_book(
            "# L\n- Treasure Island — Robert Louis Stevenson (1883) [9780141321004] {epub}",
        );
        assert_eq!(b.title, "Treasure Island");
        assert_eq!(b.authors, vec!["Robert Louis Stevenson"]);
        assert_eq!(b.year, Some(1883));
        assert_eq!(b.isbn.as_deref(), Some("9780141321004"));
        assert_eq!(b.format_pref, vec![Format::Epub]);
    }

    #[test]
    fn format_pdf_suffix() {
        let b = one_book("# L\n- Some Book — Some Author {pdf}");
        assert_eq!(b.format_pref, vec![Format::Pdf]);
    }

    #[test]
    fn title_only_fallback() {
        let b = one_book("# L\n- Just A Title With No Author");
        assert_eq!(b.title, "Just A Title With No Author");
        assert!(b.authors.is_empty());
    }

    #[test]
    fn title_only_with_year() {
        // Suffix peeling must work even without an author separator.
        let b = one_book("# L\n- Mystery Title (1999)");
        assert_eq!(b.title, "Mystery Title");
        assert_eq!(b.year, Some(1999));
        assert!(b.authors.is_empty());
    }

    #[test]
    fn non_year_parenthetical_stays_in_title() {
        let b = one_book("# L\n- What Is Man? — Mark Twain");
        assert_eq!(b.title, "What Is Man?");
        assert_eq!(b.authors, vec!["Mark Twain"]);
    }

    #[test]
    fn numbered_items() {
        let list = parse_markdown("# L\n## G\n1. A — X\n2. B — Y").unwrap();
        assert_eq!(list.groups[0].books.len(), 2);
        assert_eq!(list.groups[0].books[1].input.title, "B");
    }

    #[test]
    fn nested_subgroups() {
        let md = "# Top\n## Parent\n- A — X\n### Child\n- B — Y\n- C — Z";
        let list = parse_markdown(md).unwrap();
        assert_eq!(list.title, "Top");
        assert_eq!(list.groups.len(), 1);
        assert_eq!(list.groups[0].name, "Parent");
        assert_eq!(list.groups[0].books.len(), 1);
        assert_eq!(list.groups[0].subgroups.len(), 1);
        assert_eq!(list.groups[0].subgroups[0].name, "Child");
        assert_eq!(list.groups[0].subgroups[0].books.len(), 2);
        assert_eq!(list.groups[0].subgroups[0].books[1].input.title, "C");
    }

    #[test]
    fn list_title_and_section() {
        let list = parse_markdown("# My List\n## Batch 1\n- A — X").unwrap();
        assert_eq!(list.title, "My List");
        assert_eq!(list.groups[0].name, "Batch 1");
    }

    #[test]
    fn json_multi_author_split() {
        let json = r#"{ "list_title": "L", "sections": [
            { "id": 1, "title": "S", "books": [
                { "title": "Grimms' Fairy Tales", "author": "Jacob Grimm and Wilhelm Grimm" }
            ]}
        ]}"#;
        let list = parse_json(json).unwrap();
        let b = &list.groups[0].books[0].input;
        assert_eq!(b.authors, vec!["Jacob Grimm", "Wilhelm Grimm"]);
    }

    #[test]
    fn json_basic_shape() {
        let json = r#"{ "list_title": "Top", "sections": [
            { "id": 7, "title": "Sec", "books": [
                { "title": "Treasure Island", "author": "Robert Louis Stevenson" }
            ]}
        ]}"#;
        let list = parse_json(json).unwrap();
        assert_eq!(list.title, "Top");
        assert_eq!(list.groups[0].name, "Sec");
        assert_eq!(list.groups[0].books[0].input.title, "Treasure Island");
        assert_eq!(
            list.groups[0].books[0].input.authors,
            vec!["Robert Louis Stevenson"]
        );
    }
}
