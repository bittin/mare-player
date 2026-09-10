// SPDX-License-Identifier: GPL-3.0-only

//! Turso-backed cache database.
//!
//! A single embedded SQLite-compatible database (pure-Rust `turso`) that backs:
//!
//! * **view-state cache** — the last-seen row model of each view, so navigation
//!   paints instantly from cache and then upserts when the network responds
//!   (stale-while-revalidate);
//! * **image cache** — artwork blobs (replacing the on-disk image partition);
//! * **play history** — one row per played track (dedup + move-to-front by
//!   track id, ordered by play time); the only *non-disposable* table, since
//!   TIDAL exposes no "recently played" endpoint to rebuild it from.
//!
//! Songs are deliberately **not** stored here — large audio stays on the
//! filesystem so it can be streamed and seeked while still downloading. Videos
//! are never cached at all.
//!
//! ## Concurrency
//!
//! All access goes through a single [`turso::Connection`] behind a
//! `tokio::Mutex`. The cache is not a high-throughput hot path (a handful of
//! ops per navigation / image), so serialising keeps the model simple and
//! avoids relying on the young engine's concurrent-writer behaviour.
//!
//! The price of that choice is that every operation queues behind every other
//! one: a screen of album art landing at once is enough to delay the view-cache
//! read that gates the next navigation's paint. So each operation here stays
//! O(1)-ish — no full-table scans on the write path — and callers whose result
//! drives a paint hand their cache writes to a detached task rather than
//! awaiting them.
//!
//! ## Disposability
//!
//! Everything here is a cache: it can always be rebuilt from TIDAL. So instead
//! of migrations we stamp `PRAGMA user_version` and **drop + recreate** the
//! tables whenever [`SCHEMA_VERSION`] changes. That also de-risks running a
//! beta database engine — a corrupt or incompatible file just triggers a cold
//! refetch, never data loss.
//!
//! The lone exception is `play_history`: it has no server-side source, so it is
//! never in the drop list and must survive schema bumps. New non-disposable
//! tables are added with `CREATE TABLE IF NOT EXISTS` (no version bump) and any
//! data reshaping is done with an explicit one-time migration.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use turso::Builder;

/// Bump to invalidate (drop + recreate) all cached tables.
const SCHEMA_VERSION: i64 = 1;

/// Sentinel for "this table's live byte total hasn't been measured yet".
const UNKNOWN_TOTAL: i64 = -1;

/// A cache table kept under a byte budget by evicting its
/// least-recently-accessed rows. (`play_history` has no budget: it is the one
/// non-disposable table.)
#[derive(Clone, Copy)]
enum Evicted {
    Image,
    ViewCache,
}

impl Evicted {
    /// SQL table name.
    fn table(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::ViewCache => "view_cache",
        }
    }

    /// Primary-key column, used to size the row a write replaces.
    fn key_column(self) -> &'static str {
        match self {
            Self::Image => "url",
            Self::ViewCache => "key",
        }
    }
}

/// Live `SUM(bytes)` of each evicted table, maintained in memory so the byte
/// budget can be enforced without scanning the table on every write.
///
/// Both cells start at [`UNKNOWN_TOTAL`] and are measured once, on the first
/// write of the session. They are only ever touched with the connection mutex
/// held, which is what orders the updates — the atomics exist so that the
/// shared handle stays `Sync` without a second lock.
struct Totals {
    image: AtomicI64,
    view_cache: AtomicI64,
}

impl Totals {
    fn new() -> Self {
        Self { image: AtomicI64::new(UNKNOWN_TOTAL), view_cache: AtomicI64::new(UNKNOWN_TOTAL) }
    }

    fn cell(&self, table: Evicted) -> &AtomicI64 {
        match table {
            Evicted::Image => &self.image,
            Evicted::ViewCache => &self.view_cache,
        }
    }
}

/// Handle to the cache database. Cheap to clone (shared connection).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<turso::Connection>>,
    totals: Arc<Totals>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `turso::Connection` isn't `Debug`; the handle has no useful fields to
        // print anyway. This impl exists so `Db` can ride inside `Message`.
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

/// Seconds since the Unix epoch, used for LRU `accessed_at` / `updated_at`.
fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Db {
    /// Open (or create) the cache database at `path`.
    ///
    /// On a [`SCHEMA_VERSION`] mismatch the cached tables are dropped and
    /// recreated. Pass `:memory:` for an ephemeral database (used in tests).
    pub async fn open(path: &Path) -> turso::Result<Self> {
        let path_str = path.to_str().unwrap_or(":memory:");
        let db = Builder::new_local(path_str).build().await?;
        let conn = db.connect()?;
        let me = Self { conn: Arc::new(Mutex::new(conn)), totals: Arc::new(Totals::new()) };
        me.init_schema().await?;
        Ok(me)
    }

    async fn init_schema(&self) -> turso::Result<()> {
        let conn = self.conn.lock().await;

        let mut ver: i64 = 0;
        {
            let mut rows = conn.query("PRAGMA user_version", ()).await?;
            if let Some(row) = rows.next().await? {
                ver = row.get_value(0)?.as_integer().copied().unwrap_or(0);
            }
        }

        if ver != SCHEMA_VERSION {
            // `play_history` is intentionally absent: it has no server-side
            // source and must never be dropped on a schema bump.
            for t in ["view_cache", "image"] {
                let _ = conn.execute(&format!("DROP TABLE IF EXISTS {t}"), ()).await;
            }
        }

        conn.execute(
            "CREATE TABLE IF NOT EXISTS view_cache (
                key         TEXT PRIMARY KEY,
                payload     BLOB NOT NULL,
                etag        TEXT,
                bytes       INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                accessed_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS image (
                url         TEXT PRIMARY KEY,
                data        BLOB NOT NULL,
                bytes       INTEGER NOT NULL,
                accessed_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        // Eviction picks the least-recently-accessed row; without these it is a
        // full table scan plus a sort of every cached blob, on every write.
        conn.execute("CREATE INDEX IF NOT EXISTS idx_view_cache_accessed_at ON view_cache (accessed_at)", ()).await?;
        conn.execute("CREATE INDEX IF NOT EXISTS idx_image_accessed_at ON image (accessed_at)", ()).await?;
        // Non-disposable: one row per played track, deduped by `track_id`
        // (move-to-front on replay), ordered by `played_at` (epoch millis).
        // `entry` is the JSON-serialised `HistoryEntry`. Created unconditionally
        // (no version bump) so existing databases gain it without losing data.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS play_history (
                track_id   TEXT PRIMARY KEY,
                played_at  INTEGER NOT NULL,
                entry      BLOB NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_play_history_played_at
                ON play_history (played_at DESC)",
            (),
        )
        .await?;

        conn.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), ()).await?;
        Ok(())
    }

    // ── view-state cache ────────────────────────────────────────────────

    /// Fetch a cached view payload by key, bumping its LRU timestamp.
    pub async fn get_view(&self, key: &str) -> Option<Vec<u8>> {
        let conn = self.conn.lock().await;
        let mut rows = conn.query("SELECT payload FROM view_cache WHERE key = ?1", [key]).await.ok()?;
        let row = rows.next().await.ok()??;
        let data = row.get_value(0).ok()?.as_blob().cloned()?;
        let _ = conn.execute("UPDATE view_cache SET accessed_at = ?1 WHERE key = ?2", (now_secs(), key)).await;
        Some(data)
    }

    /// Upsert a view payload, then evict oldest entries past `budget_bytes`.
    pub async fn put_view(&self, key: &str, payload: &[u8], etag: Option<&str>, budget_bytes: i64) {
        let now = now_secs();
        let conn = self.conn.lock().await;
        let etag = etag.map(|s| s.to_string());
        // Sized before the write: the row this replaces (if any) stops
        // counting toward the table's live total.
        let replaced = Self::row_bytes(&conn, Evicted::ViewCache, key).await;
        let res = conn
            .execute(
                "INSERT INTO view_cache (key, payload, etag, bytes, updated_at, accessed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(key) DO UPDATE SET
                    payload = excluded.payload,
                    etag = excluded.etag,
                    bytes = excluded.bytes,
                    updated_at = excluded.updated_at,
                    accessed_at = excluded.accessed_at",
                (key, payload.to_vec(), etag, payload.len() as i64, now),
            )
            .await;
        if let Err(e) = res {
            tracing::warn!("cache put_view failed: {e}");
            return;
        }
        self.enforce_budget(&conn, Evicted::ViewCache, payload.len() as i64 - replaced, budget_bytes).await;
    }

    // ── image cache ─────────────────────────────────────────────────────

    /// Fetch a cached image by URL, bumping its LRU timestamp.
    pub async fn get_image(&self, url: &str) -> Option<Vec<u8>> {
        let conn = self.conn.lock().await;
        let mut rows = conn.query("SELECT data FROM image WHERE url = ?1", [url]).await.ok()?;
        let row = rows.next().await.ok()??;
        let data = row.get_value(0).ok()?.as_blob().cloned()?;
        let _ = conn.execute("UPDATE image SET accessed_at = ?1 WHERE url = ?2", (now_secs(), url)).await;
        Some(data)
    }

    /// Upsert an image blob, then evict oldest entries past `budget_bytes`.
    pub async fn put_image(&self, url: &str, data: &[u8], budget_bytes: i64) {
        let now = now_secs();
        let conn = self.conn.lock().await;
        let replaced = Self::row_bytes(&conn, Evicted::Image, url).await;
        let res = conn
            .execute(
                "INSERT INTO image (url, data, bytes, accessed_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(url) DO UPDATE SET
                    data = excluded.data,
                    bytes = excluded.bytes,
                    accessed_at = excluded.accessed_at",
                (url, data.to_vec(), data.len() as i64, now),
            )
            .await;
        if let Err(e) = res {
            tracing::warn!("cache put_image failed: {e}");
            return;
        }
        self.enforce_budget(&conn, Evicted::Image, data.len() as i64 - replaced, budget_bytes).await;
    }

    // ── play history ────────────────────────────────────────

    /// Load every play-history entry blob, most-recent first. Each blob is a
    /// JSON-serialised `HistoryEntry`; the caller deserialises (the cache layer
    /// stays free of model types).
    pub async fn get_play_history(&self) -> Vec<Vec<u8>> {
        let conn = self.conn.lock().await;
        let mut out = Vec::new();
        let mut rows = match conn.query("SELECT entry FROM play_history ORDER BY played_at DESC", ()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("cache get_play_history failed: {e}");
                return out;
            }
        };
        while let Ok(Some(row)) = rows.next().await {
            if let Some(blob) = row.get_value(0).ok().and_then(|v| v.as_blob().cloned()) {
                out.push(blob);
            }
        }
        out
    }

    /// Upsert a single play-history entry, deduping and moving-to-front by
    /// `track_id`: a replay updates the row's `played_at` and `entry` in place
    /// rather than appending a duplicate. O(1)-ish — no full-history rewrite.
    pub async fn put_play_history(&self, track_id: &str, played_at_ms: i64, entry: &[u8]) {
        let conn = self.conn.lock().await;
        let res = conn
            .execute(
                "INSERT INTO play_history (track_id, played_at, entry)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(track_id) DO UPDATE SET
                    played_at = excluded.played_at,
                    entry = excluded.entry",
                (track_id, played_at_ms, entry.to_vec()),
            )
            .await;
        if let Err(e) = res {
            tracing::warn!("cache put_play_history failed: {e}");
        }
    }

    /// Delete all play-history rows.
    pub async fn clear_play_history(&self) {
        let conn = self.conn.lock().await;
        if let Err(e) = conn.execute("DELETE FROM play_history", ()).await {
            tracing::warn!("cache clear_play_history failed: {e}");
        }
    }

    // ── eviction ────────────────────────────────────────────────────────

    /// Fold a write of `delta` bytes into `table`'s live total, then evict the
    /// oldest rows (by `accessed_at`) until it fits `budget_bytes`.
    ///
    /// The total is tracked in memory rather than re-derived per write: a
    /// `SUM(bytes)` over a full image table costs ~50 ms, and paying that on
    /// every artwork write, with the single connection mutex held, holds up
    /// every other cache user for seconds — including the view-cache read and
    /// write that gate a navigation's paint. It is measured once per session
    /// and maintained from there, so the common under-budget write does no
    /// reads at all.
    ///
    /// Called with the connection mutex held; that is what serialises the
    /// read-modify-write of the total.
    async fn enforce_budget(&self, conn: &turso::Connection, table: Evicted, delta: i64, budget_bytes: i64) {
        if budget_bytes <= 0 {
            return;
        }
        let cell = self.totals.cell(table);
        let mut total = match cell.load(Ordering::Relaxed) {
            // First write of the session: measure. The scan runs after the
            // insert, so it already accounts for `delta`.
            UNKNOWN_TOTAL => match Self::sum_bytes(conn, table).await {
                Some(total) => total,
                None => return,
            },
            known => known + delta,
        };

        // Window-function-free so it works on the current engine: drop the
        // single oldest row, repeat. Each pass is an index lookup and a delete.
        while total > budget_bytes {
            let Some((rowid, bytes)) = Self::oldest_row(conn, table).await else {
                break;
            };
            let sql = format!("DELETE FROM {} WHERE rowid = ?1", table.table());
            if conn.execute(&sql, [rowid]).await.unwrap_or(0) == 0 {
                break;
            }
            total -= bytes;
        }

        cell.store(total, Ordering::Relaxed);
    }

    /// Total live bytes in `table`. A full scan — see [`Db::enforce_budget`].
    async fn sum_bytes(conn: &turso::Connection, table: Evicted) -> Option<i64> {
        let sql = format!("SELECT COALESCE(SUM(bytes), 0) FROM {}", table.table());
        let mut rows = conn.query(&sql, ()).await.ok()?;
        let row = rows.next().await.ok()??;
        row.get_value(0).ok()?.as_integer().copied()
    }

    /// `(rowid, bytes)` of the least-recently-accessed row, found through the
    /// `accessed_at` index.
    async fn oldest_row(conn: &turso::Connection, table: Evicted) -> Option<(i64, i64)> {
        let sql = format!("SELECT rowid, bytes FROM {} ORDER BY accessed_at ASC LIMIT 1", table.table());
        let mut rows = conn.query(&sql, ()).await.ok()?;
        let row = rows.next().await.ok()??;
        let rowid = row.get_value(0).ok()?.as_integer().copied()?;
        let bytes = row.get_value(1).ok()?.as_integer().copied()?;
        Some((rowid, bytes))
    }

    /// Size of the row currently stored under `key`, or 0 if there is none. A
    /// primary-key lookup, used to keep the live total exact across upserts.
    async fn row_bytes(conn: &turso::Connection, table: Evicted, key: &str) -> i64 {
        let sql = format!("SELECT bytes FROM {} WHERE {} = ?1", table.table(), table.key_column());
        let Ok(mut rows) = conn.query(&sql, [key]).await else {
            return 0;
        };
        rows.next().await.ok().flatten().and_then(|row| row.get_value(0).ok()?.as_integer().copied()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_db() -> Db {
        Db::open(Path::new(":memory:")).await.expect("open db")
    }

    #[tokio::test]
    async fn view_blob_round_trips() {
        let db = mem_db().await;
        let payload = vec![0u8, 1, 2, 3, 250, 251, 252, 253];
        db.put_view("album:42", &payload, Some("etag-1"), 1024 * 1024).await;
        let got = db.get_view("album:42").await;
        assert_eq!(got.as_deref(), Some(payload.as_slice()));
        assert!(db.get_view("album:nope").await.is_none());
    }

    #[tokio::test]
    async fn view_upsert_replaces() {
        let db = mem_db().await;
        db.put_view("k", b"first", None, 1024 * 1024).await;
        db.put_view("k", b"second", None, 1024 * 1024).await;
        assert_eq!(db.get_view("k").await.as_deref(), Some(&b"second"[..]));
    }

    #[tokio::test]
    async fn image_round_trips() {
        let db = mem_db().await;
        db.put_image("https://x/y.jpg", &[9u8; 64], 1024 * 1024).await;
        assert_eq!(db.get_image("https://x/y.jpg").await.map(|d| d.len()), Some(64));
    }

    #[tokio::test]
    async fn image_budget_evicts_until_it_fits() {
        let db = mem_db().await;
        let urls = ["https://x/1.jpg", "https://x/2.jpg", "https://x/3.jpg"];
        // Budget for two of the three 100-byte images.
        for url in urls {
            db.put_image(url, &[0u8; 100], 250).await;
        }
        let mut kept = 0;
        for url in urls {
            kept += usize::from(db.get_image(url).await.is_some());
        }
        // Which rows survive depends on `accessed_at`, which has one-second
        // resolution, so only the count is pinned down here.
        assert_eq!(kept, 2);
    }

    #[tokio::test]
    async fn rewriting_an_image_does_not_inflate_the_budget() {
        let db = mem_db().await;
        // The live total tracks replacement, not accumulation: ten writes of
        // the same 100-byte key stay 100 bytes, so a second image still fits
        // under a 250-byte budget.
        for _ in 0..10 {
            db.put_image("https://x/1.jpg", &[0u8; 100], 250).await;
        }
        db.put_image("https://x/2.jpg", &[0u8; 100], 250).await;
        assert!(db.get_image("https://x/1.jpg").await.is_some());
        assert!(db.get_image("https://x/2.jpg").await.is_some());
    }

    #[tokio::test]
    async fn play_history_orders_by_played_at_desc() {
        let db = mem_db().await;
        db.put_play_history("a", 100, b"entry-a").await;
        db.put_play_history("b", 300, b"entry-b").await;
        db.put_play_history("c", 200, b"entry-c").await;

        let got = db.get_play_history().await;
        // Most-recent first: b (300), c (200), a (100).
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_slice(), b"entry-b");
        assert_eq!(got[1].as_slice(), b"entry-c");
        assert_eq!(got[2].as_slice(), b"entry-a");
    }

    #[tokio::test]
    async fn play_history_upsert_dedups_and_moves_to_front() {
        let db = mem_db().await;
        db.put_play_history("a", 100, b"entry-a").await;
        db.put_play_history("b", 200, b"entry-b").await;
        // Replay "a" with a newer timestamp and refreshed payload.
        db.put_play_history("a", 300, b"entry-a-v2").await;

        let got = db.get_play_history().await;
        // Still two rows (deduped by track_id), "a" now at the front.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].as_slice(), b"entry-a-v2");
        assert_eq!(got[1].as_slice(), b"entry-b");
    }

    #[tokio::test]
    async fn play_history_clear_empties_the_table() {
        let db = mem_db().await;
        db.put_play_history("a", 100, b"entry-a").await;
        db.put_play_history("b", 200, b"entry-b").await;
        db.clear_play_history().await;
        assert!(db.get_play_history().await.is_empty());
    }
}
