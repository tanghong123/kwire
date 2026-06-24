//! Tree-position bridge between the flat front end and the engine's nested model.
//!
//! The orchestrator addresses requests by `(group_path, book_index)` — a path
//! through the nested [`Group`] tree. The UI, however, is happiest with a single
//! stable id per book. We assign each book a flat **depth-first index** (`bk0`,
//! `bk1`, …) that matches the order the UI renders rows in, and translate back to
//! a tree position when issuing engine commands. Indices are stable for a given
//! list shape, which is all the UI needs between a `load_list` and the next one.

use libgen_core::model::{DownloadList, Group};

/// One book's position in the tree, paired with its flat depth-first index.
#[derive(Debug, Clone)]
pub struct Position {
    pub flat_index: usize,
    pub group_path: Vec<usize>,
    pub book_index: usize,
}

/// Walk the list depth-first, collecting a [`Position`] for every book in the
/// exact order the UI renders them (group order, books before subgroups).
pub fn positions(list: &DownloadList) -> Vec<Position> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    let mut flat = 0usize;
    walk(&list.groups, &mut path, &mut flat, &mut out);
    out
}

fn walk(groups: &[Group], path: &mut Vec<usize>, flat: &mut usize, out: &mut Vec<Position>) {
    for (gi, g) in groups.iter().enumerate() {
        path.push(gi);
        for bi in 0..g.books.len() {
            out.push(Position {
                flat_index: *flat,
                group_path: path.clone(),
                book_index: bi,
            });
            *flat += 1;
        }
        walk(&g.subgroups, path, flat, out);
        path.pop();
    }
}

/// Resolve a flat book index to its `(group_path, book_index)` tree position.
pub fn position_of(list: &DownloadList, flat_index: usize) -> Option<Position> {
    positions(list)
        .into_iter()
        .find(|p| p.flat_index == flat_index)
}

/// Parse a UI book id (`"bk12"` or a bare `"12"`) into its flat index.
pub fn parse_book_id(id: &str) -> Option<usize> {
    id.strip_prefix("bk").unwrap_or(id).parse().ok()
}

/// Build the flat UI book id (`"bkN"`) for a `(group_path, book_index)` tree
/// position by replaying the same depth-first ordering [`positions`] uses. Pure
/// (no list needed): a book's flat index is the count of books that precede it in
/// depth-first order, which is fully determined by the path + index *given the
/// tree shape*. Since callers translating engine events don't hold the list here,
/// we derive it from the path alone using the canonical walk — but that needs the
/// tree to know sibling group sizes. So this takes the list to stay exact.
pub fn flat_id_in(list: &DownloadList, group_path: &[usize], book_index: usize) -> Option<String> {
    positions(list)
        .into_iter()
        .find(|p| p.group_path == group_path && p.book_index == book_index)
        .map(|p| format!("bk{}", p.flat_index))
}

/// Addresses one **variation** of a book: its tree position plus the md5 of the
/// specific candidate (distinct file) the action targets.
///
/// The flat UI id (`bk12`) names the book; the md5 names the variation within
/// it. Together they map a UI action onto the orchestrator's
/// `(group_path, book_index, md5)` per-variation API.
#[derive(Debug, Clone)]
pub struct VariationPosition {
    pub group_path: Vec<usize>,
    pub book_index: usize,
    pub md5: String,
}

/// Resolve a `(book_id, md5)` pair to a [`VariationPosition`], confirming the
/// md5 is actually one of that book's kept candidate variations.
pub fn variation_of(list: &DownloadList, book_id: &str, md5: &str) -> Option<VariationPosition> {
    let flat = parse_book_id(book_id)?;
    let pos = position_of(list, flat)?;
    let group = group_at(&list.groups, &pos.group_path)?;
    let req = group.books.get(pos.book_index)?;
    if !req.candidates.iter().any(|c| c.md5 == md5) {
        return None;
    }
    Some(VariationPosition {
        group_path: pos.group_path,
        book_index: pos.book_index,
        md5: md5.to_string(),
    })
}

/// Resolve a group by its `group_path` (mirrors the orchestrator's private
/// helper; kept here so the bridge can validate variation addresses).
fn group_at<'a>(groups: &'a [Group], path: &[usize]) -> Option<&'a Group> {
    let (&first, rest) = path.split_first()?;
    let g = groups.get(first)?;
    if rest.is_empty() {
        Some(g)
    } else {
        group_at(&g.subgroups, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libgen_core::model::{BookInput, BookRequest};

    fn list() -> DownloadList {
        let mut g1 = Group::new("A");
        g1.books.push(BookRequest::new(BookInput {
            title: "a0".into(),
            ..Default::default()
        }));
        g1.books.push(BookRequest::new(BookInput {
            title: "a1".into(),
            ..Default::default()
        }));
        let mut sub = Group::new("A.sub");
        sub.books.push(BookRequest::new(BookInput {
            title: "s0".into(),
            ..Default::default()
        }));
        g1.subgroups.push(sub);
        let mut g2 = Group::new("B");
        g2.books.push(BookRequest::new(BookInput {
            title: "b0".into(),
            ..Default::default()
        }));
        DownloadList {
            title: "t".into(),
            settings: Default::default(),
            groups: vec![g1, g2],
        }
    }

    #[test]
    fn depth_first_indexing_matches_ui_order() {
        let l = list();
        let p = positions(&l);
        // a0, a1 (group A), then s0 (A.sub), then b0 (group B).
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].group_path, vec![0]);
        assert_eq!(p[0].book_index, 0);
        assert_eq!(p[2].group_path, vec![0, 0]); // subgroup
        assert_eq!(p[2].book_index, 0);
        assert_eq!(p[3].group_path, vec![1]);
    }

    #[test]
    fn id_parsing() {
        assert_eq!(parse_book_id("bk7"), Some(7));
        assert_eq!(parse_book_id("7"), Some(7));
        assert_eq!(parse_book_id("nope"), None);
    }

    #[test]
    fn variation_addressing_validates_md5() {
        use libgen_core::model::Candidate;
        let mut l = list();
        // Give book bk0 (group A, index 0) two candidate variations.
        let cand = |md5: &str| Candidate {
            md5: md5.into(),
            title: "a0".into(),
            authors: vec![],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: None,
            size_bytes: None,
            source_host: None,
            cover_url: None,
            score: 1.0,
            job: None,
        };
        l.groups[0].books[0].candidates = vec![cand("aaa"), cand("bbb")];

        let v = variation_of(&l, "bk0", "bbb").expect("known md5 resolves");
        assert_eq!(v.group_path, vec![0]);
        assert_eq!(v.book_index, 0);
        assert_eq!(v.md5, "bbb");

        // Unknown md5 → no match (the action would be rejected).
        assert!(variation_of(&l, "bk0", "zzz").is_none());
        // Unknown book → no match.
        assert!(variation_of(&l, "bk99", "aaa").is_none());
    }
}
