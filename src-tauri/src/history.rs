//! History records persistence with async batch writing.
//! Records are buffered in a channel and flushed to SQLite every 300ms.
//! This ensures the rendering path is never blocked by disk I/O.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// A single translation history record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub request_id: String,
    pub source_text: String,
    pub translated_text: String,
    pub source_lang: Option<String>,
    pub target_lang: String,
    /// "selection", "ocr_region", or "realtime"
    pub mode: String,
    pub tokens_used: u32,
    pub cached: bool,
    pub created_at: i64,
}

/// Async history store: accepts records via channel, flushes to SQLite in batches.
pub struct HistoryStore {
    tx: mpsc::UnboundedSender<HistoryRecord>,
    /// Direct DB connection for reads (queries).
    read_conn: Mutex<Connection>,
}

impl HistoryStore {
    /// Open (or create) the history database and start the background flush loop.
    /// Returns the HistoryStore (for writes) and spawns a Tokio task for flushing.
    pub fn open(db_path: &Path) -> Result<Arc<Self>, String> {
        // Connection for reads
        let read_conn = Connection::open(db_path)
            .map_err(|e| format!("failed to open history DB for reads: {e}"))?;

        read_conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("PRAGMA failed: {e}"))?;

        read_conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    request_id TEXT NOT NULL,
                    source_text TEXT NOT NULL,
                    translated_text TEXT NOT NULL,
                    source_lang TEXT,
                    target_lang TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    tokens_used INTEGER DEFAULT 0,
                    cached INTEGER DEFAULT 0,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_history_created
                    ON history(created_at);",
            )
            .map_err(|e| format!("create history table failed: {e}"))?;

        // Connection for writes (separate to avoid blocking reads)
        let write_conn = Connection::open(db_path)
            .map_err(|e| format!("failed to open history DB for writes: {e}"))?;

        write_conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("PRAGMA write conn: {e}"))?;

        let (tx, rx) = mpsc::unbounded_channel();

        let store = Arc::new(Self {
            tx,
            read_conn: Mutex::new(read_conn),
        });

        // Start the background flush loop
        tokio::spawn(flush_loop(rx, write_conn));

        info!(path = %db_path.display(), "history store opened with batch writer");

        Ok(store)
    }

    /// Queue a history record for async batch write. Never blocks.
    pub fn record(&self, entry: HistoryRecord) {
        if let Err(e) = self.tx.send(entry) {
            warn!(error = %e, "history channel send failed (receiver dropped?)");
        }
    }

    /// Query recent history records (newest first).
    pub fn query_recent(&self, limit: usize) -> Vec<HistoryRecord> {
        let conn = self.read_conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT request_id, source_text, translated_text, source_lang,
                    target_lang, mode, tokens_used, cached, created_at
             FROM history ORDER BY created_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "history query prepare failed");
                return Vec::new();
            }
        };

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(HistoryRecord {
                    request_id: row.get(0)?,
                    source_text: row.get(1)?,
                    translated_text: row.get(2)?,
                    source_lang: row.get(3)?,
                    target_lang: row.get(4)?,
                    mode: row.get(5)?,
                    tokens_used: row.get(6)?,
                    cached: row.get::<_, i32>(7)? != 0,
                    created_at: row.get(8)?,
                })
            })
            .ok();

        match rows {
            Some(iter) => iter.filter_map(|r| r.ok()).collect(),
            None => Vec::new(),
        }
    }

    /// Delete history older than the given number of days.
    pub fn cleanup_older_than_days(&self, days: u32) -> usize {
        let conn = self.read_conn.lock();
        let cutoff = now_unix() - (days as i64 * 86400);
        match conn.execute(
            "DELETE FROM history WHERE created_at <= ?1",
            params![cutoff],
        ) {
            Ok(count) => {
                if count > 0 {
                    info!(removed = count, days, "history cleanup");
                }
                count
            }
            Err(e) => {
                warn!(error = %e, "history cleanup failed");
                0
            }
        }
    }
}

/// Background flush loop: collects records from the channel and batch-inserts
/// into SQLite every 300ms. Never blocks the rendering path.
async fn flush_loop(mut rx: mpsc::UnboundedReceiver<HistoryRecord>, conn: Connection) {
    let flush_interval = Duration::from_millis(300);
    let mut buffer: Vec<HistoryRecord> = Vec::with_capacity(32);

    loop {
        // Wait for either the interval or channel close
        tokio::select! {
            _ = tokio::time::sleep(flush_interval) => {}
            msg = rx.recv() => {
                match msg {
                    Some(record) => buffer.push(record),
                    None => {
                        // Channel closed — flush remaining and exit
                        if !buffer.is_empty() {
                            flush_batch(&conn, &buffer);
                        }
                        info!("history flush loop exiting (channel closed)");
                        return;
                    }
                }
            }
        }

        // Drain all pending records from the channel
        while let Ok(record) = rx.try_recv() {
            buffer.push(record);
        }

        if !buffer.is_empty() {
            flush_batch(&conn, &buffer);
            buffer.clear();
        }
    }
}

/// Batch-insert records into SQLite within a transaction.
fn flush_batch(conn: &Connection, records: &[HistoryRecord]) {
    let start = std::time::Instant::now();

    if let Err(e) = conn.execute_batch("BEGIN TRANSACTION") {
        warn!(error = %e, "history batch begin failed");
        return;
    }

    let mut stmt = match conn.prepare_cached(
        "INSERT INTO history
         (request_id, source_text, translated_text, source_lang,
          target_lang, mode, tokens_used, cached, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "history batch prepare failed");
            let _ = conn.execute_batch("ROLLBACK");
            return;
        }
    };

    for record in records {
        if let Err(e) = stmt.execute(params![
            record.request_id,
            record.source_text,
            record.translated_text,
            record.source_lang,
            record.target_lang,
            record.mode,
            record.tokens_used,
            record.cached as i32,
            record.created_at,
        ]) {
            warn!(error = %e, "history insert failed for request_id={}", record.request_id);
        }
    }

    drop(stmt);

    if let Err(e) = conn.execute_batch("COMMIT") {
        warn!(error = %e, "history batch commit failed");
    } else {
        debug!(
            count = records.len(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "history batch flushed"
        );
    }
}

/// Current time as Unix timestamp (seconds).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
