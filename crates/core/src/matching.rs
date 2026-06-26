//! Score candidates against a request and decide auto-download vs. user
//! selection (see DESIGN.md §5).
//!
//! Signals combined into a 0..=1 confidence:
//!   * ISBN exact match → near-1.0 (strongest single signal),
//!   * normalized title similarity (token-set + edit distance),
//!   * normalized author similarity,
//!   * year / publisher / language boosters & tie-breakers,
//!   * format-preference penalty when no preferred format is present.

use crate::model::{BookInput, Candidate, Format, ListSettings, RequestStatus};
use strsim::normalized_levenshtein;

/// Result of evaluating candidates for one request.
#[derive(Debug, Clone)]
pub struct MatchOutcome {
    /// Resulting status: `Matched`, `NeedsSelection`, or `NotFound`.
    pub status: RequestStatus,
    /// Candidates sorted by descending score, each with `score` populated.
    pub ranked: Vec<Candidate>,
}

/// Score and rank `candidates` for `input`, then apply the confidence bands in
/// `settings` (`auto_threshold` / `near_threshold`) to choose an outcome.
pub fn evaluate(
    input: &BookInput,
    candidates: Vec<Candidate>,
    settings: &ListSettings,
) -> MatchOutcome {
    let prefs = effective_format_prefs(input, settings);
    // The language to prefer for THIS request: the request's own language if set,
    // else an explicit list `settings.language` — and when that is `None`/empty (the
    // "Match title language" default) or the legacy `"match-title"` sentinel, the
    // language DETECTED from the request title's script (CJK→Chinese, Cyrillic→
    // Russian, …; Latin→no preference). `None` = no language preference.
    let desired_lang = effective_language(input, settings);

    let mut ranked: Vec<Candidate> = candidates
        .into_iter()
        .map(|mut c| {
            c.score = score_candidate(input, &c, &prefs, desired_lang.as_deref());
            c
        })
        .collect();

    // Sort by score desc, with deterministic tie-breaks:
    // format-preference rank → language match → larger (saner) size → md5.
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| format_rank(&a.extension, &prefs).cmp(&format_rank(&b.extension, &prefs)))
            .then_with(|| {
                language_rank(desired_lang.as_deref(), b)
                    .cmp(&language_rank(desired_lang.as_deref(), a))
            })
            .then_with(|| b.size_bytes.unwrap_or(0).cmp(&a.size_bytes.unwrap_or(0)))
            .then_with(|| a.md5.cmp(&b.md5))
    });

    let mut status = decide(input, &ranked, &prefs, settings);

    // Choose which variations to KEEP for the user to swap between: filter to
    // acceptable formats, guarantee coverage of each format, then keep only
    // size-diverse copies within a format (drop near-duplicates). The decision
    // above used the full ranking, so this never changes the outcome.
    let ranked = select_variations(input, ranked, &prefs, settings.keep_top);

    // Invariant: any status that asks the user to ACT on a candidate (download it
    // or pick among them) MUST have at least one candidate to act on. If the
    // ranking/keep filtering left nothing, there's nothing to offer → NotFound.
    // Without this, a book could surface as "Needs you" / "Matched" with zero
    // variations — an unactionable dead-end the user (rightly) reads as a bug.
    if ranked.is_empty()
        && matches!(
            status,
            RequestStatus::Matched | RequestStatus::NeedsSelection
        )
    {
        status = RequestStatus::NotFound;
    }

    MatchOutcome { status, ranked }
}

/// Relative file-size difference below which two same-format copies are treated
/// as the same copy (one is dropped in favor of keeping diverse sizes).
const SIZE_DIVERSITY: f64 = 0.15;

/// Choose the variations to keep for later swapping:
///   1. filter to acceptable formats (the preferred set; fall back to all if no
///      candidate is a preferred format, so the book stays downloadable),
///   2. guarantee at least one copy per format (coverage), best-ranked first,
///   3. fill the remaining slots (up to `keep_top`) with size-diverse copies,
///      dropping near-duplicate sizes within the same format.
///
/// `keep_top == 0` means "keep everything eligible". Output preserves rank order.
fn select_variations(
    input: &BookInput,
    ranked: Vec<Candidate>,
    prefs: &[Format],
    keep_top: usize,
) -> Vec<Candidate> {
    // 1. Format filter (keep all if none of the candidates is a preferred format).
    let eligible: Vec<Candidate> = if prefs.is_empty() {
        ranked
    } else {
        let pref: Vec<Candidate> = ranked
            .iter()
            .filter(|c| has_preferred_format(c, prefs))
            .cloned()
            .collect();
        if pref.is_empty() {
            ranked
        } else {
            pref
        }
    };
    // 1b. Title gate: only OFFER copies that plausibly name the SAME book the
    // request asked for. A search for "The Secret Garden" can surface an unrelated "A Little Princess"
    // file (it scored low — ~44% — but a different size, so step 3 below would
    // otherwise keep it as a "size-diverse" epub). Drop those different-book copies
    // so they can't be mistakenly selected. Fall back to the unfiltered set if the
    // gate empties it — a book whose search returned ONLY fuzzy titles must still
    // surface something for `decide()`/the user to judge, not silently vanish.
    let eligible = {
        let same_book: Vec<Candidate> = eligible
            .iter()
            .filter(|c| is_same_book_as_request(input, c))
            .cloned()
            .collect();
        if same_book.is_empty() {
            eligible
        } else {
            same_book
        }
    };

    let cap = if keep_top == 0 { usize::MAX } else { keep_top };
    let mut kept: Vec<Candidate> = Vec::new();

    // 2. Coverage: the best (rank-first) candidate of each distinct format.
    for c in &eligible {
        if kept.len() >= cap {
            break;
        }
        if !kept.iter().any(|k| k.extension == c.extension) {
            kept.push(c.clone());
        }
    }
    // 3. Diversity: add size-different copies within already-covered formats.
    for c in &eligible {
        if kept.len() >= cap {
            break;
        }
        if kept.iter().any(|k| k.md5 == c.md5) {
            continue;
        }
        let near_dup = kept
            .iter()
            .any(|k| k.extension == c.extension && sizes_similar(k.size_bytes, c.size_bytes));
        if !near_dup {
            kept.push(c.clone());
        }
    }

    // Preserve rank order in the output.
    let keep: std::collections::HashSet<&str> = kept.iter().map(|c| c.md5.as_str()).collect();
    eligible
        .into_iter()
        .filter(|c| keep.contains(c.md5.as_str()))
        .collect()
}

/// Whether two file sizes are close enough to be considered the same copy.
fn sizes_similar(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let max = a.max(b);
            max != 0 && (a as f64 - b as f64).abs() / (max as f64) < SIZE_DIVERSITY
        }
        // Unknown size: can't tell — treat as distinct so we don't over-merge.
        _ => false,
    }
}

/// The format preferences to use: request-level if present, else list default.
fn effective_format_prefs(input: &BookInput, settings: &ListSettings) -> Vec<Format> {
    if input.format_pref.is_empty() {
        settings.format_pref.clone()
    } else {
        input.format_pref.clone()
    }
}

/// Decide the outcome based on **how confident we are it's the right book**, not
/// on variation ambiguity (format/size are already auto-ranked, so a clear title
/// match with only format differences should NOT ask the user).
///
/// We compute, per candidate, a "this is the right book" confidence from title +
/// (when given) author. The top candidate auto-matches when that confidence is
/// high; we fall to NeedsSelection only when the best title match is middling
/// (could be a different book) or several DISTINCT-titled candidates are scored
/// comparably (genuinely unclear which book). NotFound when nothing is close.
///
/// Note: the format-preference penalty lives in the *variation* score
/// (`score_candidate`) so format still drives which copy is auto-picked, but it no
/// longer gates the right-book decision — a strong title match in a non-preferred
/// format still auto-matches (the best available variation is taken).
fn decide(
    input: &BookInput,
    ranked: &[Candidate],
    _prefs: &[Format],
    settings: &ListSettings,
) -> RequestStatus {
    let top = match ranked.first() {
        Some(c) => c,
        None => return RequestStatus::NotFound,
    };

    let top_conf = book_confidence(input, top);

    // Nothing close to the request by title → usually not found, BUT don't discard
    // a row that strongly corroborates by AUTHOR (in the author field OR inside the
    // title) and shares the request's distinctive title token. That's the shape of
    // a real-but-low-title-match: a translation ("The Time Machine — Die
    // Zeitmaschine" by H. G. Wells) or a "<book> by <author>" review row. Surface
    // it as NeedsSelection so the user can judge, rather than reporting not-found.
    if top_conf < settings.near_threshold {
        let author_corroborated = !input.authors.is_empty()
            && ranked.iter().any(|c| {
                strong_author_signal(input, c)
                    && shares_distinctive_title_token(&input.title, &c.title)
            });
        if author_corroborated {
            return RequestStatus::NeedsSelection;
        }
        return RequestStatus::NotFound;
    }

    // Strong, unambiguous title (+author) match → auto-pick the best variation,
    // regardless of how many formats/sizes exist.
    if top_conf >= settings.title_match_threshold {
        // Guard against a genuine ambiguity: a DIFFERENT-titled candidate scored
        // essentially as high (two different books, both plausible). If such a rival
        // exists, ask instead of guessing which book.
        if has_close_distinct_title_rival(input, ranked, top_conf) {
            return RequestStatus::NeedsSelection;
        }
        return RequestStatus::Matched;
    }

    // Middling confidence: could be the right book or a near-miss → let the user
    // confirm.
    RequestStatus::NeedsSelection
}

/// Confidence that `cand` is the *same book* the request asks for, on a 0..=1
/// scale, combining title and (when the request supplies authors) author. This is
/// deliberately format/size-agnostic: it answers "is this the right book?", not
/// "is this the best copy?".
///
/// Title dominates. A title that strongly matches (normalized similarity high, or
/// the request title fully contained in the candidate title) is the core signal;
/// author, when present, must be at least reasonable or it caps the confidence so
/// a same-title-different-author book can't silently auto-match.
fn book_confidence(input: &BookInput, cand: &Candidate) -> f32 {
    // ISBN exact match is definitive.
    if let Some(isbn) = &input.isbn {
        if isbn_matches(isbn, cand) {
            return 1.0;
        }
    }

    let title_sim = title_match_strength(
        &input.title,
        &strip_request_author(&cand.title, &input.authors),
    );

    if input.authors.is_empty() {
        return title_sim;
    }
    // Author corroboration: the author FIELD match, OR the requested author
    // appearing IN THE CANDIDATE TITLE ("The Time Machine: … by H. G. Wells", whose author
    // field is the review's author "Spisak"). In-title authorship is a real "this
    // is by the right author" signal, so it counts the same as a field match.
    let author_sim = author_similarity(&input.authors, &cand.authors).max(
        if author_in_text(&input.authors, &cand.title) {
            1.0
        } else {
            0.0
        },
    );
    // Title is the spine; a strong author match nudges confidence up, a poor one
    // pulls it down (so "right title, wrong author" stays out of the auto band).
    let combined = 0.8 * title_sim + 0.2 * author_sim;
    // A clearly wrong author caps confidence below the auto band even with a
    // perfect title (two different books can share a title).
    if author_sim < 0.3 {
        combined.min(0.7)
    } else {
        combined
    }
}

/// Title match strength used for the right-book decision: the request title fully
/// contained in (or equal to) the candidate title is treated as a strong match
/// (handles "The Time Machine" ⊆ "The Time Machine: An Invention"); otherwise the blended
/// token/edit similarity.
fn title_match_strength(want: &str, got: &str) -> f32 {
    let nw = norm(want);
    let ng = norm(got);
    if nw.is_empty() || ng.is_empty() {
        return 0.0;
    }
    // Full containment of the (normalized) request title in the candidate title is
    // a strong signal that it's the same book (subtitle/series suffix differences).
    if ng == nw || ng.contains(&nw) || nw.contains(&ng) {
        return 1.0;
    }
    // Token-subset containment: every SIGNIFICANT word of the request title appears
    // in the candidate title (order-independent). Real mirror titles routinely pad
    // the requested title with a trailing series/volume number, ISBN, or stray
    // tokens ("Cilla Lee-Jenkins 1", "...Wizard of Oz b f 5181969") that defeat a
    // plain substring test even though it is plainly the same book. Requires at
    // least two significant request tokens so a one-word title can't trivially match
    // an unrelated longer one.
    if request_title_tokens_in(&nw, &ng) {
        return 1.0;
    }
    title_similarity(want, got)
}

/// Whether every significant token of the (normalized) request title `nw` appears
/// in the (normalized) candidate title `ng`. "Significant" drops very short tokens
/// and common title stopwords ("the", "of", "and", a trailing "1") so trailing
/// volume numbers / noise on either side don't matter. Requires ≥2 significant
/// request tokens to avoid a single common word matching an unrelated title (so a
/// terse request like "The War" still discriminates between sibling titles rather
/// than swallowing them).
fn request_title_tokens_in(nw: &str, ng: &str) -> bool {
    use std::collections::BTreeSet;
    let want: BTreeSet<&str> = nw
        .split_whitespace()
        .filter(|t| is_significant_title_token(t))
        .collect();
    if want.len() < 2 {
        return false;
    }
    let got: BTreeSet<&str> = ng.split_whitespace().collect();
    want.iter().all(|t| got.contains(t))
}

/// A title token that carries discriminating meaning: length > 2, not a common
/// English title stopword, and not a bare number (a trailing volume/series index).
fn is_significant_title_token(t: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "from", "into", "your", "you", "are", "was",
    ];
    t.len() > 2 && !STOP.contains(&t) && !t.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `cand`'s title plausibly names the SAME book the request asked for.
/// Uses the same lenient test that `has_close_distinct_title_rival` uses to tell a
/// genuine "which book?" rival from a mere format/size variant — anchored on the
/// REQUEST title here (what the user typed). Lenient on purpose: it errs toward
/// keeping a copy (a subtitle expansion "The Secret Garden: Illustrated", a series sibling
/// padded with the request's tokens), and only rejects a clearly different book
/// (no shared containment, not all significant request tokens present). An empty
/// request or candidate title can't discriminate, so it's treated as same-book.
fn is_same_book_as_request(input: &BookInput, cand: &Candidate) -> bool {
    let req_title = norm(&input.title);
    if req_title.is_empty() {
        return true;
    }
    let ct = norm(&cand.title);
    ct == req_title
        || ct.contains(&req_title)
        || req_title.contains(&ct)
        || request_title_tokens_in(&req_title, &ct)
}

/// Is there a candidate with a *materially different* title scored close to the
/// top one? That signals genuine "which book?" ambiguity (vs. mere format/size
/// variation of the same book, which is fine to auto-resolve).
fn has_close_distinct_title_rival(input: &BookInput, ranked: &[Candidate], top_conf: f32) -> bool {
    let top_title = norm(&ranked[0].title);
    let req_title = norm(&input.title);
    for c in ranked.iter().skip(1) {
        // Same book is not a rival — it's another variation/copy. Treat a candidate
        // as the same book when its title is a substring of (or contains) the top's,
        // OR when it carries all the REQUEST title's significant tokens. The latter
        // is what stops a series' sibling volumes ("…Society 1" vs "…Society 2",
        // both padding the requested "Twenty Thousand Leagues Under the Sea") from reading
        // as different books and forcing a needless choice: every such copy is a hit
        // for the title the user actually typed.
        let ct = norm(&c.title);
        let same_book = ct == top_title
            || ct.contains(&top_title)
            || top_title.contains(&ct)
            || request_title_tokens_in(&req_title, &ct);
        if same_book {
            continue;
        }
        let conf = book_confidence(input, c);
        // A distinct-titled candidate within a small margin of the top → ambiguous.
        if conf >= top_conf - 0.07 && conf >= input_near(input) {
            return true;
        }
    }
    false
}

/// Floor a rival must clear to count as a genuine competitor: a clearly-relevant
/// title. Kept simple (constant) — the margin check above does the heavy lifting.
fn input_near(_input: &BookInput) -> f32 {
    0.6
}

/// Core scorer: combine signals into 0..=1. `desired_lang` is the resolved
/// language preference for this request (see [`effective_language`]); a candidate
/// whose language matches gets a small boost.
/// Remove the request author's name (and an optional leading "by") from a
/// candidate title. "Alice's Adventures in Wonderland by Lewis Carroll" → "Alice's
/// Unicorn". The author is scored separately, so leaving it in the title would
/// shrink the symmetric title overlap and can rank a different series volume
/// above the exact match. Conservative: only strips the FULL requested author
/// string, so it can't accidentally delete a real title word.
fn strip_request_author(title: &str, authors: &[String]) -> String {
    let mut t = norm(title);
    for a in authors {
        let na = norm(a);
        if na.len() < 3 {
            continue;
        }
        t = t.replace(&format!("by {na}"), " ");
        t = t.replace(&na, " ");
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn score_candidate(
    input: &BookInput,
    cand: &Candidate,
    prefs: &[Format],
    desired_lang: Option<&str>,
) -> f32 {
    // ISBN exact match dominates everything else.
    if let Some(isbn) = &input.isbn {
        if isbn_matches(isbn, cand) {
            // Still nudge slightly by format so an ISBN match in a preferred
            // format outranks the same ISBN in a non-preferred one.
            let fmt_bonus: f32 = if has_preferred_format(cand, prefs) {
                0.02
            } else {
                0.0
            };
            return (0.97_f32 + fmt_bonus).min(1.0);
        }
    }

    // Compare against the candidate title with the request author's name removed:
    // a "<Title> by <Author>" row should score its TITLE as "<Title>", not be
    // penalized for the extra author tokens (which shrink the symmetric overlap and
    // can rank a different series volume above the exact match). The author is
    // scored separately below.
    let cand_title = strip_request_author(&cand.title, &input.authors);
    let title_sim = title_similarity(&input.title, &cand_title);
    // Author field match OR the author appearing in the candidate title (many rows
    // put authorship in the title — "… by Lewis Carroll" — and leave the author
    // field sparse). Treat in-title authorship as a full author match so the exact
    // book isn't out-ranked by a different volume that merely has a tidier field.
    let author_sim = author_similarity(&input.authors, &cand.authors).max(
        if author_in_text(&input.authors, &cand.title) {
            1.0
        } else {
            0.0
        },
    );

    // Title carries most weight; author is a strong secondary signal. When the
    // request has no authors, redistribute that weight onto the title.
    let (w_title, w_author) = if input.authors.is_empty() {
        (1.0_f32, 0.0_f32)
    } else {
        (0.65_f32, 0.35_f32)
    };
    let mut score = w_title * title_sim + w_author * author_sim;

    // Boosters (small, additive, capped below 1.0).
    if let (Some(want), Some(got)) = (input.year, cand.year) {
        let diff = (want as i32 - got as i32).abs();
        if diff == 0 {
            score += 0.05;
        } else if diff <= 2 {
            score += 0.02;
        }
    }
    if let (Some(want), Some(got)) = (&input.publisher, &cand.publisher) {
        if contains_either(&norm(want), &norm(got)) {
            score += 0.03;
        }
    }
    if let (Some(want), Some(got)) = (desired_lang, &cand.language) {
        if language_eq(want, got) {
            score += 0.03;
        }
    }

    // Format penalty: a candidate lacking any preferred format is demoted so it
    // can't auto-match, but stays above the not-found floor if otherwise good.
    if !prefs.is_empty() && !has_preferred_format(cand, prefs) {
        score -= 0.20;
    }

    score.clamp(0.0, 1.0)
}

// --- title / author similarity --------------------------------------------

/// Symmetric title similarity (0..=1) between a request title and a candidate
/// title — Jaccard token overlap blended with edit distance. SYMMETRIC, unlike
/// `title_match_strength` (which rewards containment): "Heidi" vs "Heidi:
/// Twenty Thousand Alpine Goats … (Heidi #11)" scores LOW (the candidate adds a
/// whole distinct volume), while "The Jungle Book: Mowgli's Story" vs "The Jungle Book #1" scores
/// high. Re-verify uses this to tell a good downloaded copy from a wrong one.
pub fn request_title_match(request_title: &str, candidate_title: &str) -> f32 {
    title_similarity(request_title, candidate_title)
}

/// Score a free-form "title + author" query against a candidate by comparing it
/// to the candidate's title and authors COMBINED into one string. Used by the CLI
/// when the user types `kwire search "Steve Jobs, Walter Isaacson"` — a single
/// blob that means title + author, not "title only".
///
/// Unlike [`title_similarity`] (whose `0.5*token + 0.5*max(edit,token)` blend lets
/// the `max` mask trailing padding, so a duplicated-author entry ties the clean
/// one), this uses a LENGTH-SENSITIVE blend: `0.5*token_set_ratio + 0.5*edit`. The
/// raw edit distance penalizes the extra padding, so a clean "Steve Jobs Walter
/// Isaacson" outranks "Steve Jobs Walter Isaacson Walter Isaacson".
pub fn freeform_query_match(query: &str, candidate: &Candidate) -> f32 {
    // Combined candidate text: title followed by each non-empty author.
    let mut combined = candidate.title.clone();
    for a in &candidate.authors {
        if !a.trim().is_empty() {
            combined.push(' ');
            combined.push_str(a);
        }
    }
    let na = norm(query);
    let nb = norm(&combined);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    let token = token_set_ratio(&na, &nb);
    let edit = normalized_levenshtein(&na, &nb) as f32;
    (0.5 * token + 0.5 * edit).clamp(0.0, 1.0)
}

fn title_similarity(a: &str, b: &str) -> f32 {
    let na = norm(a);
    let nb = norm(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    let edit = normalized_levenshtein(&na, &nb) as f32;
    let token = token_set_ratio(&na, &nb);
    // Token-set ratio handles subtitle/word-order differences; edit distance
    // catches typos. Take the stronger of the two, blended.
    0.5 * token + 0.5 * edit.max(token)
}

/// A strong signal that `cand` is by the requested author — either the author
/// FIELD matches well, or a distinctive author token (≥4 chars, e.g. a surname)
/// appears in the candidate TITLE ("… by H. G. Wells" rows whose author column is
/// something else).
fn strong_author_signal(input: &BookInput, cand: &Candidate) -> bool {
    if input.authors.is_empty() {
        return false;
    }
    if author_similarity(&input.authors, &cand.authors) >= 0.85 {
        return true;
    }
    author_in_text(&input.authors, &cand.title)
}

/// Any distinctive token (≥4 chars) of a requested author appears in `text`.
fn author_in_text(authors: &[String], text: &str) -> bool {
    let nt = norm(text);
    let toks: std::collections::HashSet<&str> = nt.split_whitespace().collect();
    authors.iter().any(|a| {
        norm(a)
            .split_whitespace()
            .filter(|t| t.len() >= 4)
            .any(|t| toks.contains(t))
    })
}

/// The request title's distinctive token (significant + ≥4 chars, e.g. "wonderland")
/// appears in the candidate title — a shared content word, not just stopwords.
fn shares_distinctive_title_token(want: &str, got: &str) -> bool {
    let ng = norm(got);
    let gtoks: std::collections::HashSet<&str> = ng.split_whitespace().collect();
    norm(want)
        .split_whitespace()
        .filter(|t| is_significant_title_token(t) && t.len() >= 4)
        .any(|t| gtoks.contains(t))
}

fn author_similarity(want: &[String], got: &[String]) -> f32 {
    if want.is_empty() {
        return 1.0; // no constraint
    }
    if got.is_empty() {
        return 0.0;
    }
    // Best pairwise match across requested vs. candidate authors.
    let mut best = 0.0_f32;
    for w in want {
        for g in got {
            let s = token_set_ratio(&norm(w), &norm(g))
                .max(normalized_levenshtein(&norm(w), &norm(g)) as f32);
            if s > best {
                best = s;
            }
        }
    }
    best
}

/// Token-set ratio: |intersection| / |union| over whitespace tokens.
fn token_set_ratio(a: &str, b: &str) -> f32 {
    use std::collections::BTreeSet;
    let sa: BTreeSet<&str> = a.split_whitespace().collect();
    let sb: BTreeSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    inter / union
}

// --- format helpers --------------------------------------------------------

fn has_preferred_format(cand: &Candidate, prefs: &[Format]) -> bool {
    match &cand.extension {
        Some(ext) => prefs.iter().any(|p| p == ext),
        None => false,
    }
}

/// Lower is better. Position in the preference list; non-preferred sorts last.
fn format_rank(ext: &Option<Format>, prefs: &[Format]) -> usize {
    match ext {
        Some(e) => prefs.iter().position(|p| p == e).unwrap_or(usize::MAX - 1),
        None => usize::MAX,
    }
}

fn language_rank(desired_lang: Option<&str>, cand: &Candidate) -> u8 {
    match (desired_lang, &cand.language) {
        (Some(w), Some(g)) if language_eq(w, g) => 1,
        _ => 0,
    }
}

/// Loose language-name equality: normalized exact match, or one being a prefix of
/// the other (so "English" matches "en"/"eng" and "Chinese" matches "zh"/"chi").
fn language_eq(a: &str, b: &str) -> bool {
    let na = norm(a);
    let nb = norm(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.starts_with(&nb) || nb.starts_with(&na)
}

/// The language preference to apply to a request:
///   * the request's own `language` if set (most specific),
///   * else an EXPLICIT `settings.language` (e.g. "English", "Chinese"),
///   * else — when `settings.language` is `None`/empty (the "Match title language"
///     default) or the legacy `"match-title"` sentinel — the language DETECTED from
///     the request title's script (see [`detect_title_language`]). Detection returns
///     `None` for Latin/unknown scripts (English/Spanish/French can't be told apart),
///     in which case there is no language preference — exactly the prior behavior.
///
/// Returns `None` when there is no preference to apply.
///
/// However the preference is applied, it is a SOFT signal: a small additive score
/// boost in [`score_candidate`] plus a tie-break in [`language_rank`]. It is never a
/// hard filter, so an inferred (or explicit) language can never drop all results
/// when no matching-language copy exists — it only re-orders ties toward the wanted
/// language. This matches how an explicit `language` already behaves.
fn effective_language(input: &BookInput, settings: &ListSettings) -> Option<String> {
    if let Some(l) = &input.language {
        let l = l.trim();
        if !l.is_empty() {
            return Some(l.to_string());
        }
    }
    match settings.language.as_deref() {
        // Explicit language (anything other than the legacy match-title sentinel).
        Some(s) if !s.trim().is_empty() && !s.trim().eq_ignore_ascii_case(MATCH_TITLE_SENTINEL) => {
            Some(s.trim().to_string())
        }
        // `None`/empty (the "Match title language" default, which the UI saves as an
        // empty language) OR the legacy `"match-title"` sentinel → detect from title.
        _ => detect_title_language(&input.title),
    }
}

/// Sentinel value for `ListSettings.language` meaning "infer the desired language
/// per book from its title's script".
pub const MATCH_TITLE_SENTINEL: &str = "match-title";

/// Detect a desired language from a title's dominant script. A pragmatic heuristic
/// (no external dep): scan the title's ALPHABETIC chars (digits/punctuation/spaces
/// are ignored), bucket each by Unicode block, and return the language for the
/// script holding the majority of letters.
///
/// Script → language:
///   * Han (CJK ideographs) → "Chinese" (Han could be Japanese, but default to
///     Chinese; if any Hiragana/Katakana is present the title is "Japanese"),
///   * Hiragana/Katakana → "Japanese",
///   * Hangul → "Korean",
///   * Cyrillic → "Russian",
///   * Greek → "Greek",
///   * Arabic → "Arabic",
///   * Hebrew → "Hebrew",
///   * Devanagari → "Hindi",
///   * Thai → "Thai",
///   * Latin (or no strong signal) → `None` — Latin script can't disambiguate
///     English/Spanish/French/…, so we DON'T guess (no language preference).
///
/// Returned names match how candidate `language` fields are labeled (full English
/// names: "English", "Chinese", "Russian", …).
pub fn detect_title_language(title: &str) -> Option<String> {
    let mut latin = 0usize;
    let mut han = 0usize;
    let mut kana = 0usize;
    let mut hangul = 0usize;
    let mut cyrillic = 0usize;
    let mut arabic = 0usize;
    let mut greek = 0usize;
    let mut hebrew = 0usize;
    let mut devanagari = 0usize;
    let mut thai = 0usize;

    for ch in title.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        let c = ch as u32;
        match c {
            // Hiragana + Katakana (incl. half-width katakana).
            0x3040..=0x30FF | 0xFF66..=0xFF9D => kana += 1,
            // CJK Unified Ideographs (+ Ext A) → Han.
            0x4E00..=0x9FFF | 0x3400..=0x4DBF => han += 1,
            // Hangul syllables + Jamo.
            0xAC00..=0xD7AF | 0x1100..=0x11FF => hangul += 1,
            // Cyrillic.
            0x0400..=0x04FF => cyrillic += 1,
            // Arabic.
            0x0600..=0x06FF | 0x0750..=0x077F => arabic += 1,
            // Greek.
            0x0370..=0x03FF => greek += 1,
            // Hebrew.
            0x0590..=0x05FF => hebrew += 1,
            // Devanagari.
            0x0900..=0x097F => devanagari += 1,
            // Thai.
            0x0E00..=0x0E7F => thai += 1,
            // Basic Latin / Latin-1 / Latin Extended.
            0x0041..=0x024F => latin += 1,
            _ => {}
        }
    }

    // Japanese commonly mixes Han + Kana; if ANY kana is present the title is
    // Japanese, regardless of Han count.
    if kana > 0 {
        return Some("Japanese".to_string());
    }

    // Decide by the script holding the MAJORITY of alphabetic chars. Latin is in the
    // running so a predominantly-Latin title (e.g. "Wonderland 中文版" if Latin wins)
    // resolves to no preference rather than a stray CJK char flipping the language.
    let buckets = [
        (latin, None),
        (han, Some("Chinese")),
        (hangul, Some("Korean")),
        (cyrillic, Some("Russian")),
        (greek, Some("Greek")),
        (arabic, Some("Arabic")),
        (hebrew, Some("Hebrew")),
        (devanagari, Some("Hindi")),
        (thai, Some("Thai")),
    ];
    // `max_by_key` keeps the FIRST max on ties; Latin is listed first so a tie
    // between Latin and a non-Latin script stays Latin (no preference).
    let (best_count, best_lang) = buckets
        .iter()
        .copied()
        .max_by_key(|(n, _)| *n)
        .unwrap_or((0, None));

    // No alphabetic chars at all, or Latin won → no language preference (Latin
    // script can't disambiguate the language).
    if best_count == 0 {
        return None;
    }
    best_lang.map(|l| l.to_string())
}

/// Legacy helper: like [`detect_title_language`] but falls back to "English" for
/// Latin/unknown titles (instead of `None`). Retained for the `"match-title"`
/// sentinel path and any caller that wants a concrete language name.
pub fn infer_language_from_title(title: &str) -> String {
    detect_title_language(title).unwrap_or_else(|| "English".to_string())
}

// --- isbn ------------------------------------------------------------------

fn isbn_matches(want: &str, cand: &Candidate) -> bool {
    let want = normalize_isbn(want);
    if want.is_empty() {
        return false;
    }
    // libgen.li does not expose ISBN in the parsed Candidate fields directly,
    // but when it is captured (e.g. via JSON or future parsing) it can live in
    // the publisher/title text. We match against any ISBN-like run in title +
    // publisher as a pragmatic signal.
    let haystack = format!("{} {}", cand.title, cand.publisher.as_deref().unwrap_or(""));
    extract_isbns(&haystack).iter().any(|c| isbn_eq(&want, c))
}

fn isbn_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Treat ISBN-10 and ISBN-13 of the same book as equal via the 978 core.
    let core = |s: &str| -> String {
        if s.len() == 13 && s.starts_with("978") {
            s[3..12].to_string()
        } else if s.len() == 10 {
            s[..9].to_string()
        } else {
            s.to_string()
        }
    };
    core(a) == core(b)
}

fn extract_isbns(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == 'x' || ch == 'X' || ch == '-' {
            cur.push(ch);
        } else {
            push_isbn(&cur, &mut out);
            cur.clear();
        }
    }
    push_isbn(&cur, &mut out);
    out
}

fn push_isbn(raw: &str, out: &mut Vec<String>) {
    let n = normalize_isbn(raw);
    if n.len() == 10 || n.len() == 13 {
        out.push(n);
    }
}

fn normalize_isbn(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_digit() || *c == 'x' || *c == 'X')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

// --- text normalization ----------------------------------------------------

fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_space = false;
        } else if !last_space && !out.is_empty() {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn contains_either(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.contains(b) || b.contains(a))
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn cand(title: &str, authors: &[&str], ext: Option<Format>) -> Candidate {
        Candidate {
            md5: format!("{:032x}", title.len() as u128 + authors.len() as u128),
            title: title.into(),
            authors: authors.iter().map(|s| s.to_string()).collect(),
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: ext,
            size_bytes: Some(2 * 1024 * 1024),
            source_host: Some("libgen.li".into()),
            cover_url: None,
            score: 0.0,
            job: None,
        }
    }

    fn input(title: &str, authors: &[&str]) -> BookInput {
        BookInput {
            title: title.into(),
            authors: authors.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn settings() -> ListSettings {
        ListSettings::default()
    }

    #[test]
    fn exact_match_auto() {
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        // Two copies of the SAME book (epub + pdf): both are offered, and the
        // unrelated-title filler that used to be here is now correctly dropped by
        // the same-book variation gate (see does_not_offer_a_different_book_...).
        let cands = vec![
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Epub),
            ),
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Pdf),
            ),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
        assert_eq!(out.ranked[0].title, "Treasure Island");
        assert!(out.ranked[0].score >= settings().auto_threshold);
        // sorted descending
        assert!(out.ranked[0].score >= out.ranked[1].score);
    }

    #[test]
    fn ambiguous_needs_selection() {
        // Partial title overlap, wrong-ish author → moderate confidence.
        let inp = input("The Secret Garden", &["Frances Hodgson Burnett"]);
        let cands = vec![cand(
            "The Secret Garth",
            &["Frances Hodgson Burnett"],
            Some(Format::Epub),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::NeedsSelection);
        let s = out.ranked[0].score;
        assert!(
            s >= settings().near_threshold && s < settings().auto_threshold,
            "score {s} should be in the near band"
        );
    }

    #[test]
    fn garbage_not_found() {
        let inp = input("The Adventures of Tom Sawyer", &["Mark Twain"]);
        let cands = vec![cand(
            "Quantum Field Theory",
            &["Anonymous Physicist"],
            Some(Format::Pdf),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::NotFound);
        assert!(out.ranked[0].score < settings().near_threshold);
    }

    #[test]
    fn author_match_with_low_title_surfaces_needs_selection() {
        // German translation: title differs a lot, but the author matches and the
        // distinctive "Time Machine" token is shared → surface for the user, not not-found.
        let inp = input("The Time Machine: An Invention", &["H. G. Wells"]);
        let cands = vec![cand(
            "The Time Machine - Die Zeitmaschine",
            &["H. G. Wells"],
            Some(Format::Pdf),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::NeedsSelection);
    }

    #[test]
    fn author_in_title_does_not_sink_the_exact_match_in_ranking() {
        // Ranking regression: the EXACT base title "<request> by <author>" must
        // rank #1, not be sunk below a different series volume just because its
        // title embeds the author ("by Lewis Carroll"), which shrinks raw overlap.
        let inp = input("Alice's Adventures in Wonderland", &["Lewis Carroll"]);
        let cands = vec![
            cand(
                "Wonderland Revisited: Another Alice's Adventures in Wonderland",
                &["Lewis Carroll"],
                Some(Format::Pdf),
            ),
            cand(
                "Alice's Adventures in Wonderland by Lewis Carroll",
                &["Lewis Carroll"],
                Some(Format::Pdf),
            ),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(
            out.ranked[0].title,
            "Alice's Adventures in Wonderland by Lewis Carroll",
            "the exact base title must rank #1, got {:?}",
            out.ranked.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn author_in_title_auto_matches() {
        // The request title is fully contained in the candidate title AND the
        // requested author appears IN that title ("… by H. G. Wells"), even though
        // the author FIELD is the review's author. Title + author-in-title is a
        // confident match → auto-match (no needless "Needs you" confirm).
        let inp = input("The Time Machine: An Invention", &["H. G. Wells"]);
        let cands = vec![cand(
            "Boletim da Sociedade // The Time Machine: An Invention by H. G. Wells",
            &["Spisak, April"],
            Some(Format::Pdf),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn low_title_without_author_corroboration_stays_not_found() {
        // No author match (field or title) AND no shared distinctive title token →
        // still not-found (the surface rule must not turn noise into Needs-you).
        let inp = input("The Time Machine: An Invention", &["H. G. Wells"]);
        let cands = vec![cand(
            "Boletim da Sociedade Brasileira vol. 70 iss. 5",
            &["Spisak, April"],
            Some(Format::Pdf),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::NotFound);
    }

    #[test]
    fn clear_title_match_auto_matches_even_non_preferred_format() {
        // Task 2 redesign: the decision keys on "is this the right book?", not on
        // format. A perfect title+author match in a non-preferred format (djvu)
        // still auto-matches — the best available variation is taken rather than
        // bugging the user about a choice that's only about format.
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let cands = vec![cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Djvu),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
        // The djvu is still demoted in the *variation* score (format penalty), but
        // it's the only copy, so it's what gets picked.
        assert_eq!(out.ranked[0].extension, Some(Format::Djvu));
    }

    #[test]
    fn clear_title_match_with_only_format_size_variation_auto_matches() {
        // Several variations of the SAME book (different formats + sizes). The title
        // clearly matches, so we auto-pick the best variation without asking.
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let cands = vec![
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Pdf),
            ),
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Epub),
            ),
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Mobi),
            ),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
        // epub preferred over pdf/mobi for the auto-pick.
        assert_eq!(out.ranked[0].extension, Some(Format::Epub));
    }

    #[test]
    fn ambiguous_distinct_titles_still_ask() {
        // Two DIFFERENT books, both plausible for the request → genuinely unclear
        // which book, so ask (NeedsSelection) even though each title is decent.
        let inp = input("The War", &["A. Writer"]);
        let cands = vec![
            cand("The War of the Worlds", &["A. Writer"], Some(Format::Epub)),
            cand("The War at Home", &["A. Writer"], Some(Format::Epub)),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::NeedsSelection);
    }

    #[test]
    fn right_title_wrong_author_does_not_auto_match() {
        // Same title, clearly different author → not confidently the same book.
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let cands = vec![cand(
            "Treasure Island",
            &["Completely Different Person"],
            Some(Format::Epub),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_ne!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn preferred_format_outranks_nonpreferred() {
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let cands = vec![
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Djvu),
            ),
            cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Epub),
            ),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.ranked[0].extension, Some(Format::Epub));
        assert_eq!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn isbn_dominates() {
        let mut inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        inp.isbn = Some("978-1-4027-1467-2".into());
        // A candidate whose title is wrong but whose metadata carries the ISBN
        // should still win on the ISBN signal.
        let mut c = cand(
            "treasure island (cassell) 9781402714672",
            &["R. L. S."],
            Some(Format::Epub),
        );
        c.publisher = Some("Cassell 9781402714672".into());
        let weak = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Pdf),
        );
        let out = evaluate(&inp, vec![weak, c], &settings());
        assert!(
            out.ranked[0].score >= 0.97,
            "isbn match should score near 1"
        );
        assert_eq!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn isbn_10_13_equivalence() {
        assert!(isbn_eq("140271467X", "9781402714672"));
        assert!(!isbn_eq("140271467X", "9780307798367"));
    }

    #[test]
    fn tie_break_by_format_then_size() {
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        // Two identical-text candidates, same preferred format → larger file
        // wins the size tie-break (sanity: avoid tiny truncated uploads).
        let mut big = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        big.size_bytes = Some(5 * 1024 * 1024);
        let mut small = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        small.size_bytes = Some(100 * 1024);
        let out = evaluate(&inp, vec![small, big], &settings());
        assert_eq!(out.ranked[0].size_bytes, Some(5 * 1024 * 1024));
    }

    #[test]
    fn empty_candidates_not_found() {
        let inp = input("Anything", &["Someone"]);
        let out = evaluate(&inp, vec![], &settings());
        assert_eq!(out.status, RequestStatus::NotFound);
        assert!(out.ranked.is_empty());
    }

    #[test]
    fn year_and_language_boost_breaks_near_tie() {
        let mut inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        inp.year = Some(2000);
        inp.language = Some("English".into());
        let mut matching_meta = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        matching_meta.year = Some(2000);
        matching_meta.language = Some("English".into());
        let mut other = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        other.year = Some(1850);
        other.language = Some("German".into());
        let out = evaluate(&inp, vec![other, matching_meta], &settings());
        assert_eq!(out.ranked[0].year, Some(2000));
    }

    fn variation(fmt: Format, size: u64, tag: u8) -> Candidate {
        let mut c = cand("Treasure Island", &["Robert Louis Stevenson"], Some(fmt));
        c.md5 = format!("{tag:032x}");
        c.size_bytes = Some(size);
        c
    }

    #[test]
    fn keeps_format_filtered_size_diverse_variations() {
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let mut s = settings();
        s.format_pref = vec![Format::Epub, Format::Pdf, Format::Mobi]; // explicit, default-independent
        s.keep_top = 5;
        let cands = vec![
            variation(Format::Epub, 10_100_000, 1),
            variation(Format::Epub, 20_000_000, 2),
            variation(Format::Epub, 10_500_000, 3), // ~dup of the 10.1MB epub
            variation(Format::Pdf, 9_000_000, 4),
            variation(Format::Mobi, 5_000_000, 5),
            variation(Format::Djvu, 7_000_000, 6), // not a preferred format
        ];
        let kept = evaluate(&inp, cands, &s).ranked;

        // Non-preferred format dropped; near-duplicate epub dropped; each
        // preferred format covered.
        assert!(kept.iter().all(|c| c.extension != Some(Format::Djvu)));
        let epubs: Vec<u64> = kept
            .iter()
            .filter(|c| c.extension == Some(Format::Epub))
            .filter_map(|c| c.size_bytes)
            .collect();
        assert_eq!(
            epubs.len(),
            2,
            "keep two size-diverse epubs, not the near-dup"
        );
        let (lo, hi) = (*epubs.iter().min().unwrap(), *epubs.iter().max().unwrap());
        assert!(
            (hi - lo) as f64 / hi as f64 >= 0.15,
            "kept epubs are size-diverse"
        );
        assert!(kept.iter().any(|c| c.extension == Some(Format::Pdf)));
        assert!(kept.iter().any(|c| c.extension == Some(Format::Mobi)));
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn does_not_offer_a_different_book_as_a_size_diverse_variation() {
        // A search for "The Secret Garden" surfaced an "A Little Princess" epub (different book, ~44% match)
        // at a different size. Without the title gate, step 3 (size diversity) would
        // keep it as a second epub for "The Secret Garden" — the bug that let it be selected by
        // mistake. It must NOT be offered.
        let inp = input("The Secret Garden", &["Frances Hodgson Burnett"]);
        let mut s = settings();
        s.format_pref = vec![Format::Epub, Format::Pdf];
        s.keep_top = 5;
        let mut garden = cand(
            "The Secret Garden",
            &["Frances Hodgson Burnett"],
            Some(Format::Epub),
        );
        garden.md5 = format!("{:032x}", 0x5117e_u128);
        garden.size_bytes = Some(10_000_000);
        let mut princess = cand(
            "A Little Princess",
            &["Frances Hodgson Burnett"],
            Some(Format::Epub),
        );
        princess.md5 = format!("{:032x}", 0x515737_u128);
        princess.size_bytes = Some(78_000_000); // very different size — would pass diversity
        let kept = evaluate(&inp, vec![garden, princess], &s).ranked;
        assert!(
            kept.iter().any(|c| c.title == "The Secret Garden"),
            "the real Secret Garden epub must be kept"
        );
        assert!(
            !kept.iter().any(|c| c.title == "A Little Princess"),
            "a different-titled book must not be offered as a variation: {:?}",
            kept.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn keeps_subtitle_and_series_variations_of_the_same_book() {
        // The gate is lenient: a subtitle expansion and a token-padded series sibling
        // are the SAME requested book and must still be offered.
        let inp = input("Twenty Thousand Leagues Under the Sea", &["Jules Verne"]);
        let mut s = settings();
        s.format_pref = vec![Format::Epub];
        s.keep_top = 5;
        let mut a = cand(
            "Twenty Thousand Leagues Under the Sea",
            &["Jules Verne"],
            Some(Format::Epub),
        );
        a.md5 = format!("{:032x}", 0xa_u128);
        a.size_bytes = Some(5_000_000);
        let mut b = cand(
            "Twenty Thousand Leagues Under the Sea (Book 1)",
            &["Jules Verne"],
            Some(Format::Epub),
        );
        b.md5 = format!("{:032x}", 0xb_u128);
        b.size_bytes = Some(9_000_000);
        let kept = evaluate(&inp, vec![a, b], &s).ranked;
        assert_eq!(
            kept.len(),
            2,
            "both same-book variations kept: {:?}",
            kept.iter().map(|c| &c.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tight_cap_prefers_format_coverage_over_within_format_diversity() {
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let mut s = settings();
        s.format_pref = vec![Format::Epub, Format::Pdf];
        s.keep_top = 2;
        let cands = vec![
            variation(Format::Epub, 10_100_000, 1),
            variation(Format::Epub, 20_000_000, 2),
            variation(Format::Pdf, 9_000_000, 3),
        ];
        let kept = evaluate(&inp, cands, &s).ranked;
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().any(|c| c.extension == Some(Format::Epub)));
        assert!(
            kept.iter().any(|c| c.extension == Some(Format::Pdf)),
            "coverage of pdf beats a second epub"
        );
    }

    #[test]
    fn infers_language_from_title_script() {
        // Legacy wrapper: Latin/unknown falls back to "English".
        assert_eq!(infer_language_from_title("Treasure Island"), "English");
        assert_eq!(infer_language_from_title("活着"), "Chinese"); // To Live (Yu Hua)
        assert_eq!(infer_language_from_title("ノルウェイの森"), "Japanese"); // mixed kana+han
        assert_eq!(infer_language_from_title("Война и мир"), "Russian");
        assert_eq!(infer_language_from_title("ألف ليلة وليلة"), "Arabic");
        // A Latin title with a stray non-letter symbol stays English.
        assert_eq!(infer_language_from_title("C++ Primer"), "English");
    }

    #[test]
    fn detect_title_language_by_script() {
        // Latin / unknown → None (can't disambiguate English/Spanish/French/…).
        assert_eq!(detect_title_language("Treasure Island"), None);
        assert_eq!(detect_title_language("1984"), None);
        assert_eq!(detect_title_language("C++ Primer"), None);
        assert_eq!(detect_title_language(""), None);
        // Non-Latin scripts → full English language names matching candidate labels.
        assert_eq!(detect_title_language("三体").as_deref(), Some("Chinese")); // Han → Chinese
        assert_eq!(detect_title_language("活着").as_deref(), Some("Chinese"));
        assert_eq!(
            detect_title_language("進撃の巨人").as_deref(),
            Some("Japanese")
        ); // han+kana → Japanese
        assert_eq!(
            detect_title_language("ノルウェイの森").as_deref(),
            Some("Japanese")
        );
        assert_eq!(
            detect_title_language("운수 좋은 날").as_deref(),
            Some("Korean")
        );
        assert_eq!(
            detect_title_language("Преступление и наказание").as_deref(),
            Some("Russian")
        );
        assert_eq!(
            detect_title_language("ألف ليلة وليلة").as_deref(),
            Some("Arabic")
        );
        assert_eq!(detect_title_language("Ἰλιάς").as_deref(), Some("Greek"));
        assert_eq!(detect_title_language("בראשית").as_deref(), Some("Hebrew"));
        assert_eq!(detect_title_language("महाभारत").as_deref(), Some("Hindi"));
        assert_eq!(detect_title_language("ความสุข").as_deref(), Some("Thai"));
    }

    #[test]
    fn detect_title_language_picks_majority_script() {
        // Mixed Latin + Han where Latin dominates → no preference (Latin majority).
        assert_eq!(detect_title_language("Wonderland Book"), None);
        // Mixed where the CJK script holds the majority of letters → that language.
        assert_eq!(
            detect_title_language("活着 活着 x").as_deref(),
            Some("Chinese")
        );
    }

    #[test]
    fn match_title_prefers_inferred_language_chinese() {
        // `language = None` is the "Match title language" default (what the UI saves).
        // A Chinese-titled input must detect "Chinese" and prefer the Chinese copy.
        let mut s = settings();
        s.language = None;
        let inp = input("活着", &["余华"]);
        let mut zh = cand("活着", &["余华"], Some(Format::Epub));
        zh.language = Some("Chinese".into());
        zh.md5 = "1".repeat(32);
        let mut en = cand("活着", &["余华"], Some(Format::Epub));
        en.language = Some("English".into());
        en.md5 = "2".repeat(32);
        let out = evaluate(&inp, vec![en, zh], &s);
        assert_eq!(
            out.ranked[0].language.as_deref(),
            Some("Chinese"),
            "match-title (None) should prefer the Chinese copy for a Chinese title"
        );
    }

    #[test]
    fn match_title_legacy_sentinel_still_detects_chinese() {
        // Back-compat: the legacy `"match-title"` sentinel value is treated the same
        // as `None` (detect from the title).
        let mut s = settings();
        s.language = Some(super::MATCH_TITLE_SENTINEL.to_string());
        let inp = input("活着", &["余华"]);
        let mut zh = cand("活着", &["余华"], Some(Format::Epub));
        zh.language = Some("Chinese".into());
        zh.md5 = "1".repeat(32);
        let mut en = cand("活着", &["余华"], Some(Format::Epub));
        en.language = Some("English".into());
        en.md5 = "2".repeat(32);
        let out = evaluate(&inp, vec![en, zh], &s);
        assert_eq!(out.ranked[0].language.as_deref(), Some("Chinese"));
    }

    #[test]
    fn match_title_latin_title_applies_no_language_preference() {
        // New semantics: a Latin title can't be disambiguated, so match-title (None)
        // applies NO language preference — behaving exactly as before (no filter).
        // With everything else equal, the language tie-break does NOT fire, so the
        // pre-existing order is preserved (German copy passed first stays first).
        let mut s = settings();
        s.language = None;
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let mut en = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        en.language = Some("English".into());
        en.md5 = "1".repeat(32);
        let mut de = cand(
            "Treasure Island",
            &["Robert Louis Stevenson"],
            Some(Format::Epub),
        );
        de.language = Some("German".into());
        de.md5 = "2".repeat(32);
        let out = evaluate(&inp, vec![de, en], &s);
        // No language boost/tie-break → the two copies score equally and fall to the
        // md5 tie-break ("1…" < "2…"), so the English copy (md5 "1…") wins — NOT
        // because of any language preference.
        assert_eq!(out.ranked[0].language.as_deref(), Some("English"));
        // An EXPLICIT "German" preference, by contrast, would re-order toward German.
        s.language = Some("German".into());
        let de2 = {
            let mut c = cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Epub),
            );
            c.language = Some("German".into());
            c.md5 = "2".repeat(32);
            c
        };
        let en2 = {
            let mut c = cand(
                "Treasure Island",
                &["Robert Louis Stevenson"],
                Some(Format::Epub),
            );
            c.language = Some("English".into());
            c.md5 = "1".repeat(32);
            c
        };
        let out = evaluate(&inp, vec![en2, de2], &s);
        assert_eq!(out.ranked[0].language.as_deref(), Some("German"));
    }

    #[test]
    fn series_volume_siblings_do_not_force_a_choice() {
        // Real libgen returns the requested book padded with a trailing series
        // index ("…Society 1") alongside its sibling volumes ("…Society 2", "3").
        // Each carries the full requested title, so they are the SAME requested
        // book's copies/volumes — not different books — and must auto-match the
        // best one rather than asking. (multi-token: "Twenty Thousand Leagues Under the Sea".)
        let inp = input("Twenty Thousand Leagues Under the Sea", &["Jules Verne"]);
        let cands = vec![
            cand(
                "Twenty Thousand Leagues Under the Sea 1",
                &["Jules Verne"],
                Some(Format::Epub),
            ),
            cand(
                "Twenty Thousand Leagues Under the Sea 2",
                &["Jules Verne"],
                Some(Format::Epub),
            ),
            cand(
                "Twenty Thousand Leagues Under the Sea 3",
                &["Jules Verne"],
                Some(Format::Epub),
            ),
        ];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn trailing_noise_title_auto_matches_with_good_author() {
        // The requested title padded with stray ISBN/barcode tokens still reads as
        // the same book when the author matches ("The Wonderful Wizard of Oz" →
        // "…Wizard of Oz b f 5181969").
        let inp = input("The Wonderful Wizard of Oz", &["L. Frank Baum"]);
        let cands = vec![cand(
            "The Wonderful Wizard of Oz b f 5181969",
            &["Baum", "L. Frank"],
            Some(Format::Epub),
        )];
        let out = evaluate(&inp, cands, &settings());
        assert_eq!(out.status, RequestStatus::Matched);
    }

    #[test]
    fn token_subset_requires_two_significant_tokens() {
        // A terse one-significant-token request must NOT swallow an unrelated longer
        // title via token-subset matching (guards the rival check from over-merging).
        assert!(!request_title_tokens_in("the war", "the war of the worlds"));
        // Two significant tokens, all present → same book.
        assert!(request_title_tokens_in(
            "twenty thousand leagues",
            "the twenty thousand leagues under the sea 2"
        ));
        // A missing significant token → not a subset.
        assert!(!request_title_tokens_in(
            "the secret garden",
            "the secret garth song"
        ));
    }

    #[test]
    fn freeform_query_ranks_clean_combined_above_duplicated() {
        // The CLI bug: a free-form "title + author" query must rank the cleanly
        // catalogued copy (title="Steve Jobs", author="Walter Isaacson") above a
        // malformed entry whose title duplicates the author
        // ("Steve Jobs Walter Isaacson Walter Isaacson").
        let query = "Steve Jobs, Walter Isaacson";
        let clean = cand("Steve Jobs", &["Walter Isaacson"], Some(Format::Epub));
        let dup = cand(
            "Steve Jobs Walter Isaacson Walter Isaacson",
            &[],
            Some(Format::Epub),
        );
        let s_clean = freeform_query_match(query, &clean);
        let s_dup = freeform_query_match(query, &dup);
        assert!(
            s_clean > s_dup,
            "clean ({s_clean}) must outrank duplicated ({s_dup})"
        );
    }

    #[test]
    fn non_preferred_formats_filtered_by_default() {
        // Default prefs (epub, pdf) exclude mobi-only results.
        let inp = input("Treasure Island", &["Robert Louis Stevenson"]);
        let cands = vec![
            variation(Format::Mobi, 5_000_000, 1),
            variation(Format::Epub, 6_000_000, 2),
        ];
        let kept = evaluate(&inp, cands, &settings()).ranked;
        assert!(kept.iter().all(|c| c.extension == Some(Format::Epub)));
    }
}
