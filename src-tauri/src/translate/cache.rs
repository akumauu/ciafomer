//! In-memory LRU translation cache with TTL.
//! Key: blake3 hash of (src_lang | tgt_lang | glossary_ver | normalized_text).
//! Capacity: 512, TTL: 10 minutes.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use lru::LruCache;
use parking_lot::Mutex;

struct CacheEntry {
    translated_text: String,
    inserted_at: Instant,
}

pub struct TranslationCache {
    inner: Mutex<LruCache<[u8; 32], CacheEntry>>,
    ttl: Duration,
}

impl TranslationCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity).expect("cache capacity must be > 0"),
            )),
            ttl,
        }
    }

    /// Compute the cache key from translation parameters.
    pub fn compute_key(
        src_lang: &str,
        tgt_lang: &str,
        glossary_ver: u32,
        normalized_text: &str,
    ) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(src_lang.as_bytes());
        hasher.update(b"|");
        hasher.update(tgt_lang.as_bytes());
        hasher.update(b"|");
        hasher.update(&glossary_ver.to_le_bytes());
        hasher.update(b"|");
        hasher.update(normalized_text.as_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Look up a cached translation. Returns None if absent or expired.
    pub fn get(&self, key: &[u8; 32]) -> Option<String> {
        let mut cache = self.inner.lock();
        if let Some(entry) = cache.get(key) {
            if entry.inserted_at.elapsed() < self.ttl {
                return Some(entry.translated_text.clone());
            }
            // Expired — remove it
            cache.pop(key);
        }
        None
    }

    /// Insert a translation result into the cache.
    pub fn insert(&self, key: [u8; 32], translated_text: String) {
        let mut cache = self.inner.lock();
        cache.put(
            key,
            CacheEntry {
                translated_text,
                inserted_at: Instant::now(),
            },
        );
    }
}
