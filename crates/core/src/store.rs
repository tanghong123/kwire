//! SQLite persistence for lists, groups, book requests, candidates, and jobs
//! (DESIGN.md §3 `store`). Backed by `rusqlite` with the `bundled` feature so no
//! system SQLite is required.
//!
//! The store owns the on-disk representation of a [`DownloadList`]; the rest of
//! the engine works with the in-memory model and round-trips through here. A
//! freshly opened DB runs schema migration; reopening an existing DB leaves data
//! intact so the queue can resume after a quit/crash.
//!
//! ## Identity & ordering
//! Groups and books carry no IDs in the model, so the store assigns stable
//! integer ids and preserves declaration order via an `ord` column. Reload
//! reconstructs the exact tree (groups, nested subgroups, books) in order, so a
//! parsed list round-trips to an equal [`DownloadList`].

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::model::{
    BookInput, BookRequest, Candidate, DownloadJob, DownloadList, Goal, Group, ListSettings,
    RequestStatus, TrashPending,
};

/// Current schema version. Bump + add a migration arm when the schema changes.
const SCHEMA_VERSION: i64 = 8;

/// A handle to the persistence layer.
pub struct Store {
    conn: Connection,
}

/// A persisted list plus its assigned row id.
#[derive(Debug, Clone)]
pub struct StoredList {
    pub id: i64,
    pub list: DownloadList,
}

/// Which failover chain a [`SiteQuality`] row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteRole {
    /// A search mirror (`mirrors.toml` `[[search_mirror]]`).
    Search,
    /// A download resolver site (`download::ALL_SITES`).
    Download,
}

impl SiteRole {
    /// The string stored in the `site_quality.role` column.
    pub fn as_str(self) -> &'static str {
        match self {
            SiteRole::Search => "search",
            SiteRole::Download => "download",
        }
    }
}

/// Per-host measured reliability + latency for one [`SiteRole`], used to
/// auto-order mirror failover toward sites that actually work. Persisted in the
/// `site_quality` table (schema v6); global, not per-list.
#[derive(Debug, Clone, PartialEq)]
pub struct SiteQuality {
    pub host: String,
    pub successes: u64,
    pub failures: u64,
    /// Exponentially-weighted moving average of successful-request latency in ms
    /// (`None` until the first timed success).
    pub ewma_ms: Option<f64>,
    /// SQLite timestamps of the last success / failure (`None` if never).
    pub last_ok: Option<String>,
    pub last_fail: Option<String>,
}

impl SiteQuality {
    /// Laplace-smoothed success rate in `[0, 1]` (`(s + 1) / (s + f + 2)`), so a
    /// host with no samples scores a neutral `0.5` rather than 0 or a divide-by-
    /// zero. Higher is better.
    pub fn success_rate(&self) -> f64 {
        (self.successes as f64 + 1.0) / (self.successes as f64 + self.failures as f64 + 2.0)
    }
}

impl Store {
    /// Open (or create) a database at `path`, running migrations as needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .with_context(|| format!("opening sqlite db at {}", path.as_ref().display()))?;
        Self::init(conn)
    }

    /// Open an in-memory database (handy for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite db")?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("setting WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign keys")?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Apply schema migrations from the DB's current `user_version` up to
    /// [`SCHEMA_VERSION`]. On a fresh DB `user_version` is 0.
    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .context("reading user_version")?;

        if current < 1 {
            self.conn
                .execute_batch(SCHEMA_V1)
                .context("applying schema v1")?;
        }

        // Book-table columns added across v2..v8. Each is applied IDEMPOTENTLY
        // (added only if missing) rather than gated on `user_version`, so a launch
        // killed mid-migration — leaving a column present but `user_version`
        // un-bumped — SELF-HEALS instead of failing with "duplicate column name".
        // (`ALTER TABLE ADD COLUMN` and the version bump are separate, non-atomic
        // statements, so a crash between them used to brick every later launch.)
        for (name, decl) in [
            ("seq", "INTEGER"),                       // v2: stable per-book sequence no.
            ("review", "INTEGER NOT NULL DEFAULT 0"), // v3: a better match exists
            ("trash_on_replace_json", "TEXT"),        // v3: old file to trash on replace
            ("goal_json", "TEXT"),                    // v4: per-book execution goal
            ("dismissed_json", "TEXT"),               // v5: md5s the user removed
            ("review_dismissed_md5", "TEXT"),         // v7: declined recommendation
            ("history_json", "TEXT"),                 // v8: event chronicle
        ] {
            self.ensure_book_column(name, decl)?;
        }

        // v6: per-host site-quality stats — CREATE TABLE IF NOT EXISTS is already
        // idempotent.
        self.conn
            .execute_batch(SCHEMA_V6)
            .context("applying site_quality table")?;

        if current != SCHEMA_VERSION {
            self.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .context("setting user_version")?;
        }
        Ok(())
    }

    /// Add a column to the `book` table only if it isn't already present —
    /// idempotent, so re-running a migration (or recovering a DB left with the
    /// column present but `user_version` stale by a killed launch) never errors
    /// with "duplicate column name".
    fn ensure_book_column(&self, name: &str, decl: &str) -> Result<()> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('book') WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .context("checking book column existence")?;
        if count == 0 {
            self.conn
                .execute_batch(&format!("ALTER TABLE book ADD COLUMN {name} {decl};"))
                .with_context(|| format!("adding book column {name}"))?;
        }
        Ok(())
    }

    /// Insert a parsed [`DownloadList`] (and its full tree) as a new row,
    /// returning the assigned list id. Use [`Store::upsert_list`] to replace an
    /// existing list by id.
    pub fn insert_list(&mut self, list: &DownloadList) -> Result<i64> {
        let tx = self.conn.transaction().context("begin insert_list")?;
        let id = insert_list_tx(&tx, list)?;
        tx.commit().context("commit insert_list")?;
        Ok(id)
    }

    /// Replace the list at `id` (and its whole tree) with `list`. Existing
    /// groups/books/candidates/jobs for that list are deleted and rewritten, so
    /// the persisted tree matches `list` exactly. Errors if `id` is unknown.
    pub fn upsert_list(&mut self, id: i64, list: &DownloadList) -> Result<()> {
        let tx = self.conn.transaction().context("begin upsert_list")?;
        let exists: bool = tx
            .query_row("SELECT 1 FROM list WHERE id = ?1", params![id], |_| Ok(()))
            .optional()
            .context("checking list existence")?
            .is_some();
        anyhow::ensure!(exists, "no list with id {id}");
        // ON DELETE CASCADE on the child tables clears the old tree.
        tx.execute("DELETE FROM \"group\" WHERE list_id = ?1", params![id])
            .context("clearing old groups")?;
        update_list_row(&tx, id, list)?;
        for (ord, g) in list.groups.iter().enumerate() {
            insert_group_tx(&tx, id, None, ord as i64, g)?;
        }
        tx.commit().context("commit upsert_list")?;
        Ok(())
    }

    /// Delete a list and its entire tree (groups → books → candidates → jobs) by
    /// id. `foreign_keys` is ON so `DELETE FROM list` cascades; we also drop the
    /// groups explicitly (belt-and-suspenders) so no rows are orphaned.
    pub fn delete_list(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction().context("begin delete_list")?;
        tx.execute("DELETE FROM \"group\" WHERE list_id = ?1", params![id])
            .context("deleting groups")?;
        tx.execute("DELETE FROM list WHERE id = ?1", params![id])
            .context("deleting list")?;
        tx.commit().context("commit delete_list")?;
        Ok(())
    }

    /// Update just the list-level settings (e.g. the format-preference order),
    /// without rewriting the request tree.
    pub fn update_settings(&mut self, id: i64, settings: &ListSettings) -> Result<()> {
        let settings_json = serde_json::to_string(settings).context("encoding settings")?;
        let n = self
            .conn
            .execute(
                "UPDATE list SET settings_json = ?2 WHERE id = ?1",
                params![id, settings_json],
            )
            .context("updating settings")?;
        anyhow::ensure!(n == 1, "no list with id {id}");
        Ok(())
    }

    /// The id of the (oldest) persisted list with this exact title, if any.
    /// Used to de-dupe re-imports: importing a list whose title already exists
    /// replaces it instead of creating a duplicate.
    pub fn list_id_by_title(&self, title: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM list WHERE title = ?1 ORDER BY id LIMIT 1",
                params![title],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .context("looking up list by title")
    }

    /// Load a list (full tree) by id, reconstructing an equivalent
    /// [`DownloadList`]. Returns `None` if no such list exists.
    pub fn load_list(&self, id: i64) -> Result<Option<DownloadList>> {
        let row = self
            .conn
            .query_row(
                "SELECT title, settings_json FROM list WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .context("loading list row")?;
        let (title, settings_json) = match row {
            Some(v) => v,
            None => return Ok(None),
        };
        let settings: ListSettings =
            serde_json::from_str(&settings_json).context("decoding list settings")?;
        let groups = self.load_groups(id, None)?;
        Ok(Some(DownloadList {
            title,
            settings,
            groups,
        }))
    }

    /// List all persisted lists (id + reconstructed model).
    pub fn all_lists(&self) -> Result<Vec<StoredList>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM list ORDER BY id")
            .context("preparing all_lists")?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .context("querying list ids")?
            .collect::<rusqlite::Result<_>>()
            .context("collecting list ids")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(list) = self.load_list(id)? {
                out.push(StoredList { id, list });
            }
        }
        Ok(out)
    }

    /// Recursively load the groups for a list under `parent_group_id`
    /// (`None` = top level), each with its books and (recursively) subgroups.
    fn load_groups(&self, list_id: i64, parent: Option<i64>) -> Result<Vec<Group>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, name FROM \"group\"
                 WHERE list_id = ?1 AND parent_id IS ?2
                 ORDER BY ord, id",
            )
            .context("preparing group load")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(params![list_id, parent], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .context("querying groups")?
            .collect::<rusqlite::Result<_>>()
            .context("collecting groups")?;

        let mut groups = Vec::with_capacity(rows.len());
        for (gid, name) in rows {
            let books = self.load_books(gid)?;
            let subgroups = self.load_groups(list_id, Some(gid))?;
            groups.push(Group {
                name,
                books,
                subgroups,
            });
        }
        Ok(groups)
    }

    /// Load all books for a group in declaration order, each with candidates and
    /// (optional) job reconstructed.
    fn load_books(&self, group_id: i64) -> Result<Vec<BookRequest>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, input_json, status_json, selected, job_json, seq,
                        review, trash_on_replace_json, goal_json, dismissed_json,
                        review_dismissed_md5, history_json
                 FROM book WHERE group_id = ?1 ORDER BY ord, id",
            )
            .context("preparing book load")?;
        struct Row {
            id: i64,
            input_json: String,
            status_json: String,
            selected: Option<String>,
            job_json: Option<String>,
            seq: Option<i64>,
            review: i64,
            trash_on_replace_json: Option<String>,
            goal_json: Option<String>,
            dismissed_json: Option<String>,
            review_dismissed_md5: Option<String>,
            history_json: Option<String>,
        }
        let rows: Vec<Row> = stmt
            .query_map(params![group_id], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    input_json: r.get(1)?,
                    status_json: r.get(2)?,
                    selected: r.get(3)?,
                    job_json: r.get(4)?,
                    seq: r.get(5)?,
                    review: r.get(6)?,
                    trash_on_replace_json: r.get(7)?,
                    goal_json: r.get(8)?,
                    dismissed_json: r.get(9)?,
                    review_dismissed_md5: r.get(10)?,
                    history_json: r.get(11)?,
                })
            })
            .context("querying books")?
            .collect::<rusqlite::Result<_>>()
            .context("collecting books")?;

        let mut books = Vec::with_capacity(rows.len());
        for row in rows {
            let input: BookInput =
                serde_json::from_str(&row.input_json).context("decoding book input")?;
            let status: RequestStatus =
                serde_json::from_str(&row.status_json).context("decoding book status")?;
            let job: Option<DownloadJob> = match row.job_json {
                Some(j) => Some(serde_json::from_str(&j).context("decoding job")?),
                None => None,
            };
            let trash_on_replace: Option<TrashPending> = match row.trash_on_replace_json {
                Some(j) => Some(serde_json::from_str(&j).context("decoding trash_on_replace")?),
                None => None,
            };
            let goal: Goal = match row.goal_json {
                Some(j) => serde_json::from_str(&j).context("decoding goal")?,
                None => Goal::default(),
            };
            let dismissed: Vec<String> = match row.dismissed_json {
                Some(j) => serde_json::from_str(&j).context("decoding dismissed")?,
                None => Vec::new(),
            };
            let history: Vec<crate::model::BookEvent> = match row.history_json {
                Some(j) => serde_json::from_str(&j).context("decoding history")?,
                None => Vec::new(),
            };
            let candidates = self.load_candidates(row.id)?;
            books.push(BookRequest {
                input,
                status,
                candidates,
                selected: row.selected,
                job,
                seq: row.seq.map(|s| s as u32),
                review: row.review != 0,
                review_dismissed: row.review_dismissed_md5,
                trash_on_replace,
                goal,
                dismissed,
                history,
            });
        }
        Ok(books)
    }

    fn load_candidates(&self, book_id: i64) -> Result<Vec<Candidate>> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM candidate WHERE book_id = ?1 ORDER BY ord, id")
            .context("preparing candidate load")?;
        let jsons: Vec<String> = stmt
            .query_map(params![book_id], |r| r.get(0))
            .context("querying candidates")?
            .collect::<rusqlite::Result<_>>()
            .context("collecting candidates")?;
        jsons
            .into_iter()
            .map(|j| serde_json::from_str::<Candidate>(&j).context("decoding candidate"))
            .collect()
    }

    /// Cross-list dedup cache: find an already-downloaded, md5-verified file for
    /// `md5` ANYWHERE in the library (any list/book), returning the path of an
    /// existing file so a fresh request can reuse it instead of re-downloading the
    /// identical bytes. Scans candidate rows — cheap next to a network fetch.
    pub fn find_downloaded_md5(&self, md5: &str) -> Result<Option<std::path::PathBuf>> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM candidate")
            .context("preparing dedup scan")?;
        let jsons: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .context("dedup scan")?
            .collect::<rusqlite::Result<_>>()
            .context("collecting dedup scan")?;
        for j in jsons {
            let c: Candidate = match serde_json::from_str(&j) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if c.md5 != md5 {
                continue;
            }
            if let Some(job) = &c.job {
                if matches!(job.state, crate::model::JobState::Done) && job.md5_verified {
                    if let Some(p) = &job.output_path {
                        let path = std::path::PathBuf::from(p);
                        if path.exists() {
                            return Ok(Some(path));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Update a single book request (status, candidates, selected, job) in place.
    /// Identified by its position: `(list_id, group_path, book_index)` where
    /// `group_path` is the chain of declaration indices from the top-level group
    /// down to the book's group. This mirrors the model tree without needing the
    /// caller to track DB ids.
    pub fn update_request(
        &mut self,
        list_id: i64,
        group_path: &[usize],
        book_index: usize,
        request: &BookRequest,
    ) -> Result<()> {
        let book_id = self
            .book_id_at(list_id, group_path, book_index)?
            .with_context(|| {
                format!("no book at list {list_id} group_path {group_path:?} index {book_index}")
            })?;
        let tx = self.conn.transaction().context("begin update_request")?;
        write_book_fields(&tx, book_id, request)?;
        tx.execute("DELETE FROM candidate WHERE book_id = ?1", params![book_id])
            .context("clearing candidates")?;
        for (ord, c) in request.candidates.iter().enumerate() {
            insert_candidate_tx(&tx, book_id, ord as i64, c)?;
        }
        tx.commit().context("commit update_request")?;
        Ok(())
    }

    /// Append one new [`BookRequest`] to the END of the group at `group_path`
    /// (chain of declaration indices), assigning it the next `ord`. Used by the
    /// mutable **Manual** list to add a book without rewriting the whole tree (no
    /// other book's persisted state is touched). Errors if the group is unknown.
    pub fn append_book(
        &mut self,
        list_id: i64,
        group_path: &[usize],
        book: &BookRequest,
    ) -> Result<()> {
        let group_id = self
            .group_id_at(list_id, group_path)?
            .with_context(|| format!("no group at list {list_id} group_path {group_path:?}"))?;
        let next_ord: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(ord) + 1, 0) FROM book WHERE group_id = ?1",
                params![group_id],
                |r| r.get(0),
            )
            .context("computing next book ord")?;
        let tx = self.conn.transaction().context("begin append_book")?;
        insert_book_tx(&tx, group_id, next_ord, book)?;
        tx.commit().context("commit append_book")?;
        Ok(())
    }

    /// Remove the book at the given tree position (its candidates/jobs cascade via
    /// `ON DELETE CASCADE`). Used by the mutable **Manual** list. Other books in
    /// the group keep their `ord` (gaps are harmless — loads order by `ord, id`).
    /// Errors if no book exists at that position.
    pub fn remove_book(
        &mut self,
        list_id: i64,
        group_path: &[usize],
        book_index: usize,
    ) -> Result<()> {
        let book_id = self
            .book_id_at(list_id, group_path, book_index)?
            .with_context(|| {
                format!("no book at list {list_id} group_path {group_path:?} index {book_index}")
            })?;
        self.conn
            .execute("DELETE FROM book WHERE id = ?1", params![book_id])
            .context("deleting book")?;
        Ok(())
    }

    /// Resolve the DB id of the book at the given tree position.
    fn book_id_at(
        &self,
        list_id: i64,
        group_path: &[usize],
        book_index: usize,
    ) -> Result<Option<i64>> {
        let group_id = match self.group_id_at(list_id, group_path)? {
            Some(g) => g,
            None => return Ok(None),
        };
        let id = self
            .conn
            .query_row(
                "SELECT id FROM book WHERE group_id = ?1 ORDER BY ord, id LIMIT 1 OFFSET ?2",
                params![group_id, book_index as i64],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .context("resolving book id")?;
        Ok(id)
    }

    /// Resolve the DB id of the group at `group_path` (chain of declaration
    /// indices from top level down).
    fn group_id_at(&self, list_id: i64, group_path: &[usize]) -> Result<Option<i64>> {
        let mut parent: Option<i64> = None;
        for &idx in group_path {
            let gid = self
                .conn
                .query_row(
                    "SELECT id FROM \"group\"
                     WHERE list_id = ?1 AND parent_id IS ?2
                     ORDER BY ord, id LIMIT 1 OFFSET ?3",
                    params![list_id, parent, idx as i64],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
                .context("resolving group id")?;
            match gid {
                Some(g) => parent = Some(g),
                None => return Ok(None),
            }
        }
        Ok(parent)
    }

    /// Query the requests that are `ready` to download (status `Matched` or
    /// `Ready`, with a selected/auto-chosen candidate). Returns, for each, the
    /// tree position and the chosen md5 — enough for the orchestrator to build
    /// download requests and persist results back. Used for resume.
    pub fn ready_requests(&self, list_id: i64) -> Result<Vec<ReadyRequest>> {
        let list = match self.load_list(list_id)? {
            Some(l) => l,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        collect_ready(&list.groups, &mut Vec::new(), list_id, &mut out);
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Site quality (schema v6): per-host search/download reliability + latency
    // -----------------------------------------------------------------------

    /// Record one request outcome against a `(host, role)` pair: bump the
    /// success or failure counter and stamp the time. On a success with a
    /// measured `latency_ms`, fold it into the EWMA (α = 0.3). Upserts the row.
    pub fn record_site_outcome(
        &mut self,
        host: &str,
        role: SiteRole,
        ok: bool,
        latency_ms: Option<u64>,
    ) -> Result<()> {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return Ok(());
        }
        let role = role.as_str();
        if ok {
            let lat = latency_ms.map(|m| m as f64);
            self.conn
                .execute(
                    r#"
                    INSERT INTO site_quality (host, role, successes, failures, ewma_ms, last_ok)
                    VALUES (?1, ?2, 1, 0, ?3, datetime('now'))
                    ON CONFLICT(host, role) DO UPDATE SET
                        successes = successes + 1,
                        ewma_ms = CASE
                            WHEN ?3 IS NULL THEN ewma_ms
                            WHEN ewma_ms IS NULL THEN ?3
                            ELSE 0.7 * ewma_ms + 0.3 * ?3
                        END,
                        last_ok = datetime('now')
                    "#,
                    params![host, role, lat],
                )
                .context("recording site success")?;
        } else {
            self.conn
                .execute(
                    r#"
                    INSERT INTO site_quality (host, role, successes, failures, last_fail)
                    VALUES (?1, ?2, 0, 1, datetime('now'))
                    ON CONFLICT(host, role) DO UPDATE SET
                        failures = failures + 1,
                        last_fail = datetime('now')
                    "#,
                    params![host, role],
                )
                .context("recording site failure")?;
        }
        Ok(())
    }

    /// All recorded [`SiteQuality`] rows for a role, keyed by host. The caller
    /// blends these measured stats with live availability (SLUM) to order the
    /// failover chain.
    pub fn site_quality(&self, role: SiteRole) -> Result<Vec<SiteQuality>> {
        let mut stmt = self
            .conn
            .prepare(
                r#"SELECT host, successes, failures, ewma_ms, last_ok, last_fail
                   FROM site_quality WHERE role = ?1 ORDER BY host"#,
            )
            .context("preparing site_quality query")?;
        let rows = stmt
            .query_map(params![role.as_str()], |r| {
                Ok(SiteQuality {
                    host: r.get(0)?,
                    successes: r.get::<_, i64>(1)?.max(0) as u64,
                    failures: r.get::<_, i64>(2)?.max(0) as u64,
                    ewma_ms: r.get(3)?,
                    last_ok: r.get(4)?,
                    last_fail: r.get(5)?,
                })
            })
            .context("querying site_quality")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("decoding site_quality row")?);
        }
        Ok(out)
    }
}

/// A downloadable unit identified by its position in the persisted tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyRequest {
    pub list_id: i64,
    pub group_path: Vec<usize>,
    pub book_index: usize,
    pub md5: String,
}

fn collect_ready(
    groups: &[Group],
    path: &mut Vec<usize>,
    list_id: i64,
    out: &mut Vec<ReadyRequest>,
) {
    for (gi, g) in groups.iter().enumerate() {
        path.push(gi);
        for (bi, b) in g.books.iter().enumerate() {
            let ready = matches!(b.status, RequestStatus::Matched | RequestStatus::Ready);
            if let (true, Some(md5)) = (ready, chosen_md5(b)) {
                out.push(ReadyRequest {
                    list_id,
                    group_path: path.clone(),
                    book_index: bi,
                    md5,
                });
            }
        }
        collect_ready(&g.subgroups, path, list_id, out);
        path.pop();
    }
}

/// The md5 a request would download: the explicit `selected`, else the top
/// (highest-ranked) candidate's md5.
fn chosen_md5(b: &BookRequest) -> Option<String> {
    if let Some(sel) = &b.selected {
        return Some(sel.clone());
    }
    b.candidates.first().map(|c| c.md5.clone())
}

// ---------------------------------------------------------------------------
// Transaction-level write helpers
// ---------------------------------------------------------------------------

fn insert_list_tx(tx: &Transaction, list: &DownloadList) -> Result<i64> {
    let settings_json = serde_json::to_string(&list.settings).context("encoding settings")?;
    tx.execute(
        "INSERT INTO list (title, settings_json) VALUES (?1, ?2)",
        params![list.title, settings_json],
    )
    .context("inserting list")?;
    let id = tx.last_insert_rowid();
    for (ord, g) in list.groups.iter().enumerate() {
        insert_group_tx(tx, id, None, ord as i64, g)?;
    }
    Ok(id)
}

fn update_list_row(tx: &Transaction, id: i64, list: &DownloadList) -> Result<()> {
    let settings_json = serde_json::to_string(&list.settings).context("encoding settings")?;
    tx.execute(
        "UPDATE list SET title = ?2, settings_json = ?3 WHERE id = ?1",
        params![id, list.title, settings_json],
    )
    .context("updating list row")?;
    Ok(())
}

fn insert_group_tx(
    tx: &Transaction,
    list_id: i64,
    parent: Option<i64>,
    ord: i64,
    group: &Group,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO \"group\" (list_id, parent_id, ord, name) VALUES (?1, ?2, ?3, ?4)",
        params![list_id, parent, ord, group.name],
    )
    .context("inserting group")?;
    let gid = tx.last_insert_rowid();
    for (ord, b) in group.books.iter().enumerate() {
        insert_book_tx(tx, gid, ord as i64, b)?;
    }
    for (ord, sub) in group.subgroups.iter().enumerate() {
        insert_group_tx(tx, list_id, Some(gid), ord as i64, sub)?;
    }
    Ok(gid)
}

fn insert_book_tx(tx: &Transaction, group_id: i64, ord: i64, book: &BookRequest) -> Result<i64> {
    let input_json = serde_json::to_string(&book.input).context("encoding input")?;
    let status_json = serde_json::to_string(&book.status).context("encoding status")?;
    let job_json = match &book.job {
        Some(j) => Some(serde_json::to_string(j).context("encoding job")?),
        None => None,
    };
    let trash_json = match &book.trash_on_replace {
        Some(t) => Some(serde_json::to_string(t).context("encoding trash_on_replace")?),
        None => None,
    };
    let goal_json = serde_json::to_string(&book.goal).context("encoding goal")?;
    let dismissed_json = serde_json::to_string(&book.dismissed).context("encoding dismissed")?;
    let history_json: Option<String> = if book.history.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&book.history).context("encoding history")?)
    };
    tx.execute(
        "INSERT INTO book (group_id, ord, input_json, status_json, selected, job_json, seq,
                           review, trash_on_replace_json, goal_json, dismissed_json,
                           review_dismissed_md5, history_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            group_id,
            ord,
            input_json,
            status_json,
            book.selected,
            job_json,
            book.seq.map(|s| s as i64),
            book.review as i64,
            trash_json,
            goal_json,
            dismissed_json,
            book.review_dismissed,
            history_json
        ],
    )
    .context("inserting book")?;
    let bid = tx.last_insert_rowid();
    for (ord, c) in book.candidates.iter().enumerate() {
        insert_candidate_tx(tx, bid, ord as i64, c)?;
    }
    Ok(bid)
}

fn write_book_fields(tx: &Transaction, book_id: i64, book: &BookRequest) -> Result<()> {
    let input_json = serde_json::to_string(&book.input).context("encoding input")?;
    let status_json = serde_json::to_string(&book.status).context("encoding status")?;
    let job_json = match &book.job {
        Some(j) => Some(serde_json::to_string(j).context("encoding job")?),
        None => None,
    };
    let trash_json = match &book.trash_on_replace {
        Some(t) => Some(serde_json::to_string(t).context("encoding trash_on_replace")?),
        None => None,
    };
    let goal_json = serde_json::to_string(&book.goal).context("encoding goal")?;
    let dismissed_json = serde_json::to_string(&book.dismissed).context("encoding dismissed")?;
    let history_json: Option<String> = if book.history.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&book.history).context("encoding history")?)
    };
    tx.execute(
        "UPDATE book SET input_json = ?2, status_json = ?3, selected = ?4, job_json = ?5,
         seq = ?6, review = ?7, trash_on_replace_json = ?8, goal_json = ?9,
         dismissed_json = ?10, review_dismissed_md5 = ?11, history_json = ?12 WHERE id = ?1",
        params![
            book_id,
            input_json,
            status_json,
            book.selected,
            job_json,
            book.seq.map(|s| s as i64),
            book.review as i64,
            trash_json,
            goal_json,
            dismissed_json,
            book.review_dismissed,
            history_json
        ],
    )
    .context("updating book fields")?;
    Ok(())
}

fn insert_candidate_tx(tx: &Transaction, book_id: i64, ord: i64, c: &Candidate) -> Result<()> {
    let json = serde_json::to_string(c).context("encoding candidate")?;
    tx.execute(
        "INSERT INTO candidate (book_id, ord, md5, json) VALUES (?1, ?2, ?3, ?4)",
        params![book_id, ord, c.md5, json],
    )
    .context("inserting candidate")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS list (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    title        TEXT NOT NULL,
    settings_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "group" (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    list_id   INTEGER NOT NULL REFERENCES list(id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES "group"(id) ON DELETE CASCADE,
    ord       INTEGER NOT NULL,
    name      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_group_list   ON "group"(list_id);
CREATE INDEX IF NOT EXISTS idx_group_parent ON "group"(parent_id);

CREATE TABLE IF NOT EXISTS book (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id    INTEGER NOT NULL REFERENCES "group"(id) ON DELETE CASCADE,
    ord         INTEGER NOT NULL,
    input_json  TEXT NOT NULL,
    status_json TEXT NOT NULL,
    selected    TEXT,
    job_json    TEXT
);
CREATE INDEX IF NOT EXISTS idx_book_group ON book(group_id);

CREATE TABLE IF NOT EXISTS candidate (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id INTEGER NOT NULL REFERENCES book(id) ON DELETE CASCADE,
    ord     INTEGER NOT NULL,
    md5     TEXT NOT NULL,
    json    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_candidate_book ON candidate(book_id);
"#;

// Book-table columns (v2–v8) are now added idempotently by `ensure_book_column`
// in `migrate()` (a list of name/decl pairs), so their old per-version ALTER
// constants were removed — re-running a column add is a no-op instead of an error.

/// v6: per-host site-quality stats for auto-ordering mirror failover. Keyed by
/// `(host, role)` where role is `'search'` or `'download'`. `ewma_ms` is an
/// exponentially-weighted moving average of successful-request latency (NULL
/// until the first timed success). Timestamps are SQLite `datetime('now')`.
const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS site_quality (
    host       TEXT NOT NULL,
    role       TEXT NOT NULL,
    successes  INTEGER NOT NULL DEFAULT 0,
    failures   INTEGER NOT NULL DEFAULT 0,
    ewma_ms    REAL,
    last_ok    TEXT,
    last_fail  TEXT,
    PRIMARY KEY (host, role)
);
CREATE INDEX IF NOT EXISTS idx_site_quality_role ON site_quality(role);
"#;

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Candidate, Format};

    fn sample_list() -> DownloadList {
        let mut g1 = Group::new("Batch 1");
        g1.books.push(BookRequest::new(BookInput {
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            ..Default::default()
        }));
        g1.books.push(BookRequest::new(BookInput {
            title: "Anne of Green Gables".into(),
            authors: vec!["L. M. Montgomery".into()],
            ..Default::default()
        }));
        let mut child = Group::new("Child");
        child.books.push(BookRequest::new(BookInput {
            title: "Nested Book".into(),
            authors: vec!["Someone".into()],
            ..Default::default()
        }));
        g1.subgroups.push(child);

        DownloadList {
            title: "Test List".into(),
            settings: ListSettings::default(),
            groups: vec![g1, Group::new("Batch 2")],
        }
    }

    #[test]
    fn round_trip_equal() {
        let mut store = Store::open_in_memory().unwrap();
        let list = sample_list();
        let id = store.insert_list(&list).unwrap();
        let loaded = store.load_list(id).unwrap().expect("list exists");
        assert_eq!(loaded, list);
    }

    #[test]
    fn mutate_then_reload() {
        let mut store = Store::open_in_memory().unwrap();
        let list = sample_list();
        let id = store.insert_list(&list).unwrap();

        // Mutate the first book in Batch 1: give it candidates + Matched status.
        let mut req = list.groups[0].books[0].clone();
        req.status = RequestStatus::Matched;
        req.candidates = vec![Candidate {
            md5: "a".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: Some(2000),
            publisher: None,
            language: Some("English".into()),
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: Some(1024),
            source_host: Some("libgen.li".into()),
            cover_url: None,
            score: 0.95,
            job: None,
        }];
        req.selected = Some("a".repeat(32));
        store.update_request(id, &[0], 0, &req).unwrap();

        let loaded = store.load_list(id).unwrap().unwrap();
        let b = &loaded.groups[0].books[0];
        assert_eq!(b.status, RequestStatus::Matched);
        assert_eq!(b.candidates.len(), 1);
        assert_eq!(b.candidates[0].score, 0.95);
        assert_eq!(b.selected.as_deref(), Some(&"a".repeat(32)[..]));
        // Other books untouched.
        assert_eq!(loaded.groups[0].books[1].status, RequestStatus::Queued);
    }

    #[test]
    fn per_candidate_jobs_round_trip() {
        use crate::model::{DownloadJob, JobState};
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_list(&sample_list()).unwrap();

        // Give Treasure Island two variations: an epub requested (Pending) + a pdf not yet
        // requested (no job).
        let mut req = sample_list().groups[0].books[0].clone();
        req.status = RequestStatus::Matched;
        let mut epub = Candidate {
            md5: "e".repeat(32),
            title: "Treasure Island".into(),
            authors: vec!["Robert Louis Stevenson".into()],
            year: None,
            publisher: None,
            language: None,
            pages: None,
            extension: Some(Format::Epub),
            size_bytes: Some(2048),
            source_host: Some("libgen.li".into()),
            cover_url: None,
            score: 0.95,
            job: Some(DownloadJob {
                state: JobState::Pending,
                ..Default::default()
            }),
        };
        let pdf = Candidate {
            md5: "p".repeat(32),
            extension: Some(Format::Pdf),
            job: None,
            ..epub.clone()
        };
        epub.md5 = "e".repeat(32);
        req.candidates = vec![epub, pdf];
        store.update_request(id, &[0], 0, &req).unwrap();

        // Reload: the requested epub keeps its Pending job; the pdf has none.
        let loaded = store.load_list(id).unwrap().unwrap();
        let cands = &loaded.groups[0].books[0].candidates;
        assert_eq!(cands[0].job.as_ref().unwrap().state, JobState::Pending);
        assert!(cands[1].job.is_none());

        // Mark the epub Done with a path, reload, still Done + path survives.
        let mut req = loaded.groups[0].books[0].clone();
        let job = req.candidates[0].job.as_mut().unwrap();
        job.state = JobState::Done;
        job.md5_verified = true;
        job.output_path =
            Some("/books/Batch 1/01 - Robert Louis Stevenson - Treasure Island.epub".into());
        store.update_request(id, &[0], 0, &req).unwrap();

        let loaded = store.load_list(id).unwrap().unwrap();
        let job = loaded.groups[0].books[0].candidates[0]
            .job
            .as_ref()
            .unwrap();
        assert_eq!(job.state, JobState::Done);
        assert!(job.md5_verified);
        assert_eq!(
            job.output_path.as_deref(),
            Some("/books/Batch 1/01 - Robert Louis Stevenson - Treasure Island.epub")
        );
        // Untouched pdf variation still has no job.
        assert!(loaded.groups[0].books[0].candidates[1].job.is_none());
    }

    #[test]
    fn nested_group_update() {
        let mut store = Store::open_in_memory().unwrap();
        let list = sample_list();
        let id = store.insert_list(&list).unwrap();

        let mut req = list.groups[0].subgroups[0].books[0].clone();
        req.status = RequestStatus::NotFound;
        // group_path [0, 0] = first group -> its first subgroup.
        store.update_request(id, &[0, 0], 0, &req).unwrap();

        let loaded = store.load_list(id).unwrap().unwrap();
        assert_eq!(
            loaded.groups[0].subgroups[0].books[0].status,
            RequestStatus::NotFound
        );
    }

    #[test]
    fn resume_after_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");

        let list = sample_list();
        let id;
        {
            let mut store = Store::open(&db).unwrap();
            id = store.insert_list(&list).unwrap();
            let mut req = list.groups[0].books[1].clone();
            req.status = RequestStatus::Ready;
            req.selected = Some("b".repeat(32));
            req.candidates = vec![Candidate {
                md5: "b".repeat(32),
                title: "Anne of Green Gables".into(),
                authors: vec!["L. M. Montgomery".into()],
                year: None,
                publisher: None,
                language: None,
                pages: None,
                extension: Some(Format::Epub),
                size_bytes: None,
                source_host: None,
                cover_url: None,
                score: 0.9,
                job: None,
            }];
            store.update_request(id, &[0], 1, &req).unwrap();
        }
        // Reopen: state must survive.
        let store = Store::open(&db).unwrap();
        let loaded = store.load_list(id).unwrap().unwrap();
        assert_eq!(loaded.groups[0].books[1].status, RequestStatus::Ready);

        let ready = store.ready_requests(id).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].md5, "b".repeat(32));
        assert_eq!(ready[0].group_path, vec![0]);
        assert_eq!(ready[0].book_index, 1);
    }

    #[test]
    fn upsert_replaces_tree() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_list(&sample_list()).unwrap();

        let mut replacement = DownloadList {
            title: "Replaced".into(),
            settings: ListSettings::default(),
            groups: vec![Group::new("Only Group")],
        };
        replacement.groups[0]
            .books
            .push(BookRequest::new(BookInput {
                title: "Solo".into(),
                ..Default::default()
            }));
        store.upsert_list(id, &replacement).unwrap();

        let loaded = store.load_list(id).unwrap().unwrap();
        assert_eq!(loaded, replacement);
    }

    #[test]
    fn goal_round_trips() {
        let mut store = Store::open_in_memory().unwrap();
        let id = store.insert_list(&sample_list()).unwrap();
        let mut req = sample_list().groups[0].books[0].clone();
        req.goal = Goal::Complete;
        store.update_request(id, &[0], 0, &req).unwrap();
        let loaded = store.load_list(id).unwrap().unwrap();
        assert_eq!(loaded.groups[0].books[0].goal, Goal::Complete);
        // Untouched book keeps the default goal.
        assert_eq!(loaded.groups[0].books[1].goal, Goal::Idle);
    }

    #[test]
    fn v3_db_without_goal_column_migrates_and_loads_as_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("v3.db");
        // Build a v3-era schema by hand (base + seq/review/trash columns, no
        // goal_json), insert a book row, set user_version=3, then open via Store
        // (which migrates forward).
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(
                "ALTER TABLE book ADD COLUMN seq INTEGER;
                 ALTER TABLE book ADD COLUMN review INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE book ADD COLUMN trash_on_replace_json TEXT;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO list (title, settings_json) VALUES ('L', ?1)",
                params![serde_json::to_string(&ListSettings::default()).unwrap()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO \"group\" (list_id, parent_id, ord, name) VALUES (1, NULL, 0, 'G')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO book (group_id, ord, input_json, status_json) VALUES (1, 0, ?1, ?2)",
                params![
                    serde_json::to_string(&BookInput {
                        title: "Old".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                    serde_json::to_string(&RequestStatus::Done).unwrap()
                ],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3i64).unwrap();
        }
        let store = Store::open(&db).unwrap();
        let loaded = store.load_list(1).unwrap().unwrap();
        let b = &loaded.groups[0].books[0];
        assert_eq!(b.input.title, "Old");
        assert_eq!(b.goal, Goal::Idle, "old rows decode to the default goal");
        assert_eq!(b.status, RequestStatus::Done, "existing data intact");
    }

    /// Regression: a launch killed mid-migration can leave a column PRESENT while
    /// `user_version` is stale. Re-opening must SELF-HEAL (idempotent column add),
    /// not fail with "duplicate column name" — the bug that made the app show no
    /// books after a relaunch.
    #[test]
    fn open_self_heals_when_column_present_but_user_version_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("stuck.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            // A column from a later migration is already present...
            conn.execute_batch("ALTER TABLE book ADD COLUMN review_dismissed_md5 TEXT;")
                .unwrap();
            // ...but user_version was never bumped past 2 (killed before the bump).
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }
        // Previously this errored ("duplicate column name: review_dismissed_md5").
        let store = Store::open(&db).unwrap();
        // Healed: user_version is current and a brand-new DB opens too.
        let v: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(
            v, SCHEMA_VERSION,
            "user_version bumped to current after heal"
        );
    }

    #[test]
    fn all_lists_returns_inserted() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.insert_list(&sample_list()).unwrap();
        let b = store.insert_list(&sample_list()).unwrap();
        let all = store.all_lists().unwrap();
        let ids: Vec<i64> = all.iter().map(|s| s.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
    }

    #[test]
    fn site_quality_accumulates_outcomes_and_ewma() {
        let mut store = Store::open_in_memory().unwrap();
        // First timed success seeds the EWMA exactly.
        store
            .record_site_outcome("libgen.li", SiteRole::Download, true, Some(100))
            .unwrap();
        // Second success blends: 0.7*100 + 0.3*200 = 130.
        store
            .record_site_outcome("libgen.li", SiteRole::Download, true, Some(200))
            .unwrap();
        // A failure bumps only the failure counter.
        store
            .record_site_outcome("libgen.li", SiteRole::Download, false, None)
            .unwrap();
        // A success with no latency leaves the EWMA unchanged.
        store
            .record_site_outcome("libgen.li", SiteRole::Download, true, None)
            .unwrap();

        let rows = store.site_quality(SiteRole::Download).unwrap();
        assert_eq!(rows.len(), 1);
        let q = &rows[0];
        assert_eq!(q.host, "libgen.li");
        assert_eq!(q.successes, 3);
        assert_eq!(q.failures, 1);
        assert!((q.ewma_ms.unwrap() - 130.0).abs() < 1e-6, "{:?}", q.ewma_ms);
        assert!(q.last_ok.is_some() && q.last_fail.is_some());
        // 3 ok / 1 fail → Laplace-smoothed (3+1)/(3+1+2) = 4/6.
        assert!((q.success_rate() - 4.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn site_quality_is_keyed_by_host_and_role() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .record_site_outcome("libgen.li", SiteRole::Search, true, Some(50))
            .unwrap();
        store
            .record_site_outcome("libgen.li", SiteRole::Download, false, None)
            .unwrap();
        store
            .record_site_outcome("annas-archive.gl", SiteRole::Search, true, None)
            .unwrap();
        // Host is normalized to lowercase; a mixed-case write hits the same row.
        store
            .record_site_outcome("LibGen.li", SiteRole::Search, true, Some(70))
            .unwrap();

        let search = store.site_quality(SiteRole::Search).unwrap();
        // Two distinct search hosts; the download row is a separate role.
        assert_eq!(search.len(), 2);
        let li = search.iter().find(|q| q.host == "libgen.li").unwrap();
        assert_eq!(li.successes, 2); // both case variants counted together
        assert_eq!(store.site_quality(SiteRole::Download).unwrap().len(), 1);
    }

    #[test]
    fn append_book_adds_to_end_and_remove_book_drops_one() {
        let mut store = Store::open_in_memory().unwrap();
        // A single-group list (like the Manual list).
        let list = DownloadList {
            title: "Manual".into(),
            settings: ListSettings::default(),
            groups: vec![Group::new("Manual")],
        };
        let id = store.insert_list(&list).unwrap();

        // Append two books to the (empty) root group.
        for t in ["First", "Second"] {
            store
                .append_book(
                    id,
                    &[0],
                    &BookRequest::new(BookInput {
                        title: t.into(),
                        ..Default::default()
                    }),
                )
                .unwrap();
        }
        let loaded = store.load_list(id).unwrap().unwrap();
        let titles: Vec<&str> = loaded.groups[0]
            .books
            .iter()
            .map(|b| b.input.title.as_str())
            .collect();
        assert_eq!(titles, vec!["First", "Second"], "appended in order");

        // Remove the first; the second survives (other books untouched).
        store.remove_book(id, &[0], 0).unwrap();
        let loaded = store.load_list(id).unwrap().unwrap();
        let titles: Vec<&str> = loaded.groups[0]
            .books
            .iter()
            .map(|b| b.input.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Second"]);

        // Removing a non-existent position errors.
        assert!(store.remove_book(id, &[0], 5).is_err());
    }

    #[test]
    fn is_manual_round_trips_and_defaults_false_for_old_settings() {
        let mut store = Store::open_in_memory().unwrap();
        let mut list = sample_list();
        list.settings.is_manual = true;
        let id = store.insert_list(&list).unwrap();
        assert!(store.load_list(id).unwrap().unwrap().settings.is_manual);

        // A settings blob WITHOUT the field (old list) decodes to is_manual=false.
        let s: ListSettings = serde_json::from_str(
            r#"{"format_pref":["epub"],"naming_template":"{seq:02} - {title}.{ext}",
                "auto_threshold":0.85,"near_threshold":0.45,"title_match_threshold":0.9,
                "seq_per_group":true,"keep_top":5}"#,
        )
        .unwrap();
        assert!(!s.is_manual, "absent field defaults to false");
    }

    #[test]
    fn site_quality_empty_host_is_ignored() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .record_site_outcome("   ", SiteRole::Search, true, Some(10))
            .unwrap();
        assert!(store.site_quality(SiteRole::Search).unwrap().is_empty());
    }
}
