//! Type resolution cache for tsgo queries.
//!
//! This module provides caching for resolved types from tsgo to avoid
//! redundant LSP queries, which have IPC overhead of ~5-10ms each.

use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A position in a file where a type was queried.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct TypePosition {
    /// File URI (e.g., "file:///path/to/file.ts")
    pub file_uri: String,
    /// Line number (0-based)
    pub line: u32,
    /// Character position (0-based)
    pub character: u32,
}

impl TypePosition {
    /// Create a new TypePosition.
    pub fn new(file_uri: impl Into<String>, line: u32, character: u32) -> Self {
        Self {
            file_uri: file_uri.into(),
            line,
            character,
        }
    }
}

/// Cache for resolved types from tsgo.
///
/// This cache stores type strings by their file position to avoid
/// redundant LSP queries during a single monoripple run.
#[derive(Debug, Default)]
pub struct TypeCache {
    /// Map from position to resolved type string.
    entries: FxHashMap<TypePosition, String>,
    /// Number of cache hits.
    hits: AtomicUsize,
    /// Number of cache misses.
    misses: AtomicUsize,
}

#[allow(dead_code)]
impl TypeCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a cached type string by position.
    ///
    /// Returns `Some(&String)` if the position is in the cache,
    /// `None` otherwise. Updates hit/miss statistics.
    pub fn get(&self, pos: &TypePosition) -> Option<&String> {
        let result = self.entries.get(pos);
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Check if a position is in the cache without updating statistics.
    pub fn contains(&self, pos: &TypePosition) -> bool {
        self.entries.contains_key(pos)
    }

    /// Insert a type string for a position.
    pub fn insert(&mut self, pos: TypePosition, type_str: String) {
        self.entries.insert(pos, type_str);
    }

    /// Get cache statistics (hits, misses).
    pub fn stats(&self) -> (usize, usize) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Get the number of entries in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries from the cache.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Clear all entries for a specific file.
    ///
    /// This should be called when a file's content changes, as cached
    /// positions are no longer valid.
    pub fn clear_for_file(&mut self, file_uri: &str) {
        self.entries.retain(|pos, _| pos.file_uri != file_uri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_position_equality() {
        let pos1 = TypePosition::new("file:///test.ts", 10, 5);
        let pos2 = TypePosition::new("file:///test.ts", 10, 5);
        let pos3 = TypePosition::new("file:///test.ts", 10, 6);

        assert_eq!(pos1, pos2);
        assert_ne!(pos1, pos3);
    }

    #[test]
    fn test_cache_insert_and_get() {
        let mut cache = TypeCache::new();
        let pos = TypePosition::new("file:///test.ts", 10, 5);

        assert!(cache.get(&pos).is_none());
        assert_eq!(cache.stats(), (0, 1)); // 1 miss

        cache.insert(pos.clone(), "string".to_string());

        assert_eq!(cache.get(&pos), Some(&"string".to_string()));
        assert_eq!(cache.stats(), (1, 1)); // 1 hit, 1 miss
    }

    #[test]
    fn test_cache_contains() {
        let mut cache = TypeCache::new();
        let pos = TypePosition::new("file:///test.ts", 10, 5);

        assert!(!cache.contains(&pos));

        cache.insert(pos.clone(), "number".to_string());

        assert!(cache.contains(&pos));
        // contains doesn't update stats
        assert_eq!(cache.stats(), (0, 0));
    }

    #[test]
    fn test_cache_clear() {
        let mut cache = TypeCache::new();
        let pos = TypePosition::new("file:///test.ts", 10, 5);

        cache.insert(pos.clone(), "boolean".to_string());
        assert_eq!(cache.len(), 1);

        // Generate some stats
        cache.get(&pos);
        cache.get(&TypePosition::new("other.ts", 0, 0));
        assert_eq!(cache.stats(), (1, 1));

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert_eq!(cache.stats(), (0, 0));
    }

    #[test]
    fn test_cache_multiple_entries() {
        let mut cache = TypeCache::new();

        let positions = vec![
            (TypePosition::new("file:///a.ts", 0, 0), "string"),
            (TypePosition::new("file:///a.ts", 0, 10), "number"),
            (TypePosition::new("file:///b.ts", 5, 3), "boolean"),
        ];

        for (pos, type_str) in &positions {
            cache.insert(pos.clone(), type_str.to_string());
        }

        assert_eq!(cache.len(), 3);

        for (pos, expected) in &positions {
            assert_eq!(cache.get(pos), Some(&expected.to_string()));
        }

        assert_eq!(cache.stats(), (3, 0));
    }
}
