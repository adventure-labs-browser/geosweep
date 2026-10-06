//! Single-database persistence: discovery queue + cache mirror.
//!
//! Same resumability contract as labsweep: queue.next_skip checkpoints
//! every page, claims are reaped on startup, inserts dedup by guid so
//! re-walking is idempotent. Never-delete: field changes append to
//! cache_versions; removals are future work (absence from a re-walked
//! box is not conclusive proof — boxes overlap).

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tokio::task::spawn_blocking;

use crate::geo::Cell;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS queue (
    id TEXT PRIMARY KEY,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    radius REAL NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending|in_progress|done|subdivided|failed
    attempts INTEGER NOT NULL DEFAULT 0,
    total_count INTEGER,
    next_skip INTEGER NOT NULL DEFAULT 0,    -- pagination checkpoint for resume
    inserted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS queue_status_idx ON queue(status);

CREATE TABLE IF NOT EXISTS caches (
    guid TEXT PRIMARY KEY,                -- GC code, e.g. GC7H626
    name TEXT,
    cache_type INTEGER,
    latitude REAL,                        -- NULL for premium-only (basic accounts)
    longitude REAL,
    difficulty REAL,
    terrain REAL,
    size INTEGER,
    premium INTEGER,                      -- premiumOnly flag
    placed_utc TEXT,
    favorite_points INTEGER,
    owner TEXT,
    region TEXT,
    country TEXT,
    raw_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',   -- active (removals: future work)
    content_hash TEXT,                       -- FNV-1a(raw_json); change detection
    removed_at TEXT,
    version_seq INTEGER NOT NULL DEFAULT 1,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    next_retry_at TEXT,
    first_seen TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    fetched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_caches_status ON caches(status);

CREATE TABLE IF NOT EXISTS cache_versions (
    guid TEXT NOT NULL,
    version_seq INTEGER NOT NULL,
    superseded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    change TEXT NOT NULL,              -- updated
    status TEXT,
    raw_json TEXT,
    PRIMARY KEY (guid, version_seq)
);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
"#;

/// A claimed queue row: the cell geometry plus its pagination checkpoint.
#[derive(Debug)]
pub struct ClaimedCell {
    pub cell: Cell,
    pub next_skip: i64,
    pub total_count: Option<i64>,
}

#[derive(Default)]
pub struct Stats {
    pub pending: u64,
    pub in_progress: u64,
    pub done: u64,
    pub subdivided: u64,
    pub failed: u64,
    pub caches: u64,
    pub premium: u64,
    pub coordless: u64,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "queue: {} pending, {} in_progress, {} done, {} subdivided, {} failed | \
             caches: {} ({} premium, {} without coords)",
            self.pending,
            self.in_progress,
            self.done,
            self.subdivided,
            self.failed,
            self.caches,
            self.premium,
            self.coordless,
        )
    }
}

#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

/// Add a column to an existing table if it isn't there yet.
fn ensure_column(c: &Connection, table: &str, col: &str, ddl: &str) -> Result<()> {
    let mut stmt = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    if !names.iter().any(|n| n == col) {
        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"))?;
    }
    Ok(())
}

/// Content hash for change detection. FNV-1a: deterministic across
/// processes (unlike DefaultHasher), good enough for fingerprints.
fn content_hash(raw: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(60))?;
        conn.execute_batch(SCHEMA)?;
        ensure_column(&conn, "queue", "next_skip", "next_skip INTEGER NOT NULL DEFAULT 0")?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a blocking DB op on the blocking thread pool.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        spawn_blocking(move || {
            let mut conn = inner.lock().unwrap();
            f(&mut conn)
        })
        .await
        .context("db task join")?
    }

    // ── crawl queue ─────────────────────────────────────────────────────

    pub async fn reset(&self) -> Result<()> {
        self.run(|c| {
            c.execute_batch("DELETE FROM queue; DELETE FROM caches;")?;
            Ok(())
        })
        .await
    }

    pub async fn reset_failed(&self) -> Result<u64> {
        self.run(|c| {
            Ok(c.execute(
                "UPDATE queue SET status='pending', updated_at=CURRENT_TIMESTAMP \
                 WHERE status='failed'",
                [],
            )? as u64)
        })
        .await
    }

    pub async fn reap_stale(&self, max_age_secs: i64) -> Result<u64> {
        self.run(move |c| {
            Ok(c.execute(
                "UPDATE queue SET status='pending', updated_at=CURRENT_TIMESTAMP \
                 WHERE status='in_progress' AND updated_at < \
                   datetime('CURRENT_TIMESTAMP', ?1)",
                params![format!("-{} seconds", max_age_secs)],
            )? as u64)
        })
        .await
    }

    pub async fn seed(&self, cells: &[Cell]) -> Result<()> {
        let cells: Vec<(String, f64, f64, f64)> = cells
            .iter()
            .map(|cl| (cl.id.clone(), cl.lat, cl.lon, cl.radius))
            .collect();
        self.run(move |c| {
            let mut stmt = c.prepare(
                "INSERT OR IGNORE INTO queue (id, lat, lon, radius, status) \
                 VALUES (?1, ?2, ?3, ?4, 'pending')",
            )?;
            for (id, lat, lon, radius) in &cells {
                stmt.execute(params![id, lat, lon, radius])?;
            }
            Ok(())
        })
        .await
    }

    pub async fn queue_is_empty(&self) -> Result<bool> {
        self.run(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM queue \
                 WHERE status IN ('pending','in_progress')",
                [],
                |r| r.get(0),
            )?;
            Ok(n == 0)
        })
        .await
    }

    /// Atomically reserve the largest-radius pending cell (breadth-first).
    pub async fn claim(&self) -> Result<Option<ClaimedCell>> {
        self.run(|c| {
            let row = c
                .query_row(
                    "UPDATE queue SET status='in_progress', attempts=attempts+1, \
                     updated_at=CURRENT_TIMESTAMP WHERE id = ( \
                       SELECT id FROM queue WHERE status='pending' \
                       ORDER BY radius DESC LIMIT 1 ) \
                     RETURNING id, lat, lon, radius, next_skip, total_count",
                    [],
                    |r| {
                        Ok(ClaimedCell {
                            cell: Cell {
                                id: r.get(0)?,
                                lat: r.get(1)?,
                                lon: r.get(2)?,
                                radius: r.get(3)?,
                            },
                            next_skip: r.get(4)?,
                            total_count: r.get(5)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Hand a claim back to `pending` (graceful shutdown). next_skip is
    /// preserved, so the next claimant resumes mid-pagination.
    pub async fn release_claim(&self, id: String) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='pending', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1 AND status='in_progress'",
                params![id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn in_progress_count(&self) -> Result<u64> {
        self.run(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM queue WHERE status='in_progress'",
                [],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        })
        .await
    }

    // ── caches ──────────────────────────────────────────────────────────

    /// Persist one fetched page: upsert its items (versioning changes)
    /// and advance the cell's next_skip checkpoint — atomically.
    pub async fn record_page(
        &self,
        id: String,
        next_skip: i64,
        total_count: Option<i64>,
        items: Vec<Value>,
    ) -> Result<u64> {
        self.run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE queue SET next_skip=?1, \
                 total_count=COALESCE(?2, total_count), \
                 updated_at=CURRENT_TIMESTAMP WHERE id=?3",
                params![next_skip, total_count, id],
            )?;
            let mut inserted = 0u64;
            {
                let mut sel = tx.prepare(
                    "SELECT raw_json, content_hash FROM caches WHERE guid=?1",
                )?;
                let mut ins = tx.prepare(
                    "INSERT INTO caches (guid, name, cache_type, latitude, longitude, \
                       difficulty, terrain, size, premium, placed_utc, favorite_points, \
                       owner, region, country, raw_json, status, content_hash, version_seq) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15, \
                       'active',?16,1)",
                )?;
                for item in &items {
                    let Some(guid) = item.get("code").and_then(|g| g.as_str()) else {
                        continue;
                    };
                    let raw = serde_json::to_string(item)?;
                    let hash = content_hash(&raw);
                    let existing: Option<(String, Option<String>)> = sel
                        .query_row(params![guid], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    match existing {
                        None => {
                            insert_cache(&mut ins, guid, item, &raw, &hash)?;
                            inserted += 1;
                        }
                        Some((_, Some(h))) if h == hash => {}
                        Some(_) => {
                            tx.execute(
                                "INSERT INTO cache_versions \
                                   (guid, version_seq, change, status, raw_json) \
                                 SELECT guid, COALESCE(version_seq,1), 'updated', \
                                   COALESCE(status,'active'), raw_json \
                                 FROM caches WHERE guid=?1",
                                params![guid],
                            )?;
                            update_cache(&tx, guid, item, &raw, &hash)?;
                        }
                    }
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    /// Mark a cell fully processed. Items were already saved per-page.
    pub async fn mark_done(&self, id: String, total_count: Option<i64>) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='done', \
                 total_count=COALESCE(?1, total_count), \
                 updated_at=CURRENT_TIMESTAMP WHERE id=?2",
                params![total_count, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn subdivide(&self, id: String, children: Vec<Cell>) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE queue SET status='subdivided', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1",
                params![id],
            )?;
            {
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO queue (id, lat, lon, radius, status) \
                     VALUES (?1, ?2, ?3, ?4, 'pending')",
                )?;
                for cell in &children {
                    stmt.execute(params![cell.id, cell.lat, cell.lon, cell.radius])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Mark a cell failed. Its next_skip checkpoint survives — a later
    /// `--reset-failed` run resumes pagination where it died.
    pub async fn mark_failed(&self, id: String, reason: String) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE queue SET status='failed', updated_at=CURRENT_TIMESTAMP \
                 WHERE id=?1",
                params![id],
            )?;
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![format!("failure:{id}"), reason],
            )?;
            Ok(())
        })
        .await
    }

    // ── refresh mode ────────────────────────────────────────────────────

    /// Re-queue finished cells for a refresh pass. Discovery inserts are
    /// idempotent (guid dedup), so re-walking only adds newly-appeared
    /// caches plus version rows for changed ones.
    pub async fn requeue_done(&self) -> Result<u64> {
        self.run(|c| {
            Ok(c.execute(
                "UPDATE queue SET status='pending', next_skip=0, attempts=0, \
                 updated_at=CURRENT_TIMESTAMP WHERE status IN ('done','failed')",
                [],
            )? as u64)
        })
        .await
    }

    pub async fn stats(&self) -> Result<Stats> {
        self.run(|c| {
            let mut st = Stats::default();
            let mut q = c.prepare("SELECT status, COUNT(*) FROM queue GROUP BY status")?;
            for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (s, n) = row?;
                match s.as_str() {
                    "pending" => st.pending = n as u64,
                    "in_progress" => st.in_progress = n as u64,
                    "done" => st.done = n as u64,
                    "subdivided" => st.subdivided = n as u64,
                    "failed" => st.failed = n as u64,
                    _ => {}
                }
            }
            st.caches = c.query_row("SELECT COUNT(*) FROM caches", [], |r| {
                r.get::<_, i64>(0)
            })? as u64;
            st.premium = c.query_row(
                "SELECT COUNT(*) FROM caches WHERE premium=1",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64;
            st.coordless = c.query_row(
                "SELECT COUNT(*) FROM caches WHERE latitude IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64;
            Ok(st)
        })
        .await
    }

    pub async fn labs_count(&self) -> Result<u64> {
        self.run(|c| {
            Ok(c.query_row("SELECT COUNT(*) FROM caches", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
        })
        .await
    }

    /// Labs total + known-dead count for the coverage verify.
    pub async fn coverage_counts(&self) -> Result<(u64, u64)> {
        self.run(|c| {
            let total: i64 =
                c.query_row("SELECT COUNT(*) FROM caches", [], |r| r.get(0))?;
            let dead: i64 = c.query_row(
                "SELECT COUNT(*) FROM caches \
                 WHERE COALESCE(status,'active') = 'removed'",
                [],
                |r| r.get(0),
            )?;
            Ok((total as u64, dead as u64))
        })
        .await
    }
}

// ── row helpers ─────────────────────────────────────────────────────────

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(String::from)
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

fn b(v: &Value, k: &str) -> Option<i64> {
    if v.get(k).and_then(|x| x.as_bool()) == Some(true) {
        Some(1)
    } else {
        None
    }
}

fn coords(v: &Value) -> (Option<f64>, Option<f64>) {
    let l = v.get("postedCoordinates").cloned().unwrap_or(Value::Null);
    (f(&l, "latitude"), f(&l, "longitude"))
}

type InsStmt<'a> = rusqlite::Statement<'a>;

fn insert_cache(
    ins: &mut InsStmt,
    guid: &str,
    item: &Value,
    raw: &str,
    hash: &str,
) -> Result<()> {
    let (lat, lon) = coords(item);
    ins.execute(params![
        guid,
        s(item, "name"),
        item.get("geocacheType").and_then(|x| x.as_i64()),
        lat,
        lon,
        f(item, "difficulty"),
        f(item, "terrain"),
        item.get("containerType").and_then(|x| x.as_i64()),
        b(item, "premiumOnly"),
        s(item, "placedDate"),
        item.get("favoritePoints").and_then(|x| x.as_i64()),
        item.get("owner").and_then(|o| o.get("username")).and_then(|u| u.as_str()),
        s(item, "region"),
        s(item, "country"),
        raw,
        hash,
    ])?;
    Ok(())
}

fn update_cache(
    tx: &rusqlite::Transaction,
    guid: &str,
    item: &Value,
    raw: &str,
    hash: &str,
) -> Result<()> {
    let (lat, lon) = coords(item);
    tx.execute(
        "UPDATE caches SET name=?2, cache_type=?3, latitude=?4, longitude=?5, \
           difficulty=?6, terrain=?7, size=?8, premium=?9, placed_utc=?10, \
           favorite_points=?11, owner=?12, region=?13, country=?14, raw_json=?15, \
           status='active', removed_at=NULL, content_hash=?16, \
           failed_attempts=0, next_retry_at=NULL, \
           fetched_at=CURRENT_TIMESTAMP, version_seq=COALESCE(version_seq,1)+1 \
         WHERE guid=?1",
        params![
            guid,
            s(item, "name"),
            item.get("geocacheType").and_then(|x| x.as_i64()),
            lat,
            lon,
            f(item, "difficulty"),
            f(item, "terrain"),
            item.get("containerType").and_then(|x| x.as_i64()),
            b(item, "premiumOnly"),
            s(item, "placedDate"),
            item.get("favoritePoints").and_then(|x| x.as_i64()),
            item.get("owner").and_then(|o| o.get("username")).and_then(|u| u.as_str()),
            s(item, "region"),
            s(item, "country"),
            raw,
            hash,
        ],
    )?;
    Ok(())
}
