//! L2 persistent translation cache backed by SQLite.
//! TTL: 7 days. Key: blake3 hash (same as L1).
//! Complements the in-memory LRU L1 cache for cross-session persistence.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{debug, info, warn};

/// Default TTL for L2 cache entries: 7 days.
const DEFAULT_TTL_SECS: u64 = 7 * 24 * 3600;

/// SQLite-backed translation cache (L2).
pub struct SqliteCache {
    conn: Mutex<Connection>,
    ttl_secs: u64,
}

impl SqliteCache {
    /// Open (or create) the SQLite cache database at the given path.
    pub fn open(db_path: &Path) -> Result<Self, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("failed to open SQLite cache: {e}"))?;

        // WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("PRAGMA failed: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS translation_cache (
                cache_key BLOB PRIMARY KEY,
                translated_text TEXT NOT NULL,
                src_lang TEXT NOT NULL,
                tgt_lang TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_cache_created
                ON translation_cache(created_at);",
        )
        .map_err(|e| format!("create table failed: {e}"))?;

        info!(path = %db_path.display(), "SQLite L2 cache opened");

        Ok(Self {
            conn: Mutex::new(conn),
            ttl_secs: DEFAULT_TTL_SECS,
        })
    }

    /// Look up a cached translation by blake3 key. Returns None if absent or expired.
    pub fn get(&self, key: &[u8; 32]) -> Option<String> {
        let conn = self.conn.lock();
        let cutoff = now_unix() - self.ttl_secs as i64;

        let result: Option<String> = conn
            .query_row(
                "SELECT translated_text FROM translation_cache
                 WHERE cache_key = ?1 AND created_at > ?2",
                params![key.as_slice(), cutoff],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();

        if result.is_some() {
            debug!("L2 cache hit");
        }
        result
    }

    /// Insert a translation result into the L2 cache.
    pub fn insert(
        &self,
        key: &[u8; 32],
        translated_text: &str,
        src_lang: &str,
        tgt_lang: &str,
    ) {
        let conn = self.conn.lock();
        let now = now_unix();
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO translation_cache
             (cache_key, translated_text, src_lang, tgt_lang, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key.as_slice(), translated_text, src_lang, tgt_lang, now],
        ) {
            warn!(error = %e, "L2 cache insert failed");
        }
    }

    /// Remove expired entries. Called periodically from a background task.
    pub fn cleanup_expired(&self) -> usize {
        let conn = self.conn.lock();
        let cutoff = now_unix() - self.ttl_secs as i64;
        match conn.execute(
            "DELETE FROM translation_cache WHERE created_at <= ?1",
            params![cutoff],
        ) {
            Ok(count) => {
                if count > 0 {
                    info!(removed = count, "L2 cache cleanup");
                }
                count
            }
            Err(e) => {
                warn!(error = %e, "L2 cache cleanup failed");
                0
            }
        }
    }

    /// Start a background cleanup loop (runs every hour).
    pub fn start_cleanup_loop(cache: Arc<Self>) {
        std::thread::Builder::new()
            .name("l2-cache-cleanup".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                    cache.cleanup_expired();
                }
            })
            .expect("failed to spawn L2 cache cleanup thread");
    }
}

/// Current time as Unix timestamp (seconds).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
