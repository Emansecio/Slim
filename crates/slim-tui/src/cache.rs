use std::hash::Hash;
use std::num::NonZeroUsize;

use lru::LruCache;

/// Capacity-bounded LRU cache with O(1) reads, updates, and evictions.
#[derive(Debug)]
pub struct BoundedCache<K: Eq + Hash, V> {
    map: Option<LruCache<K, V>>,
}

impl<K: Eq + Hash, V> BoundedCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: NonZeroUsize::new(capacity).map(LruCache::new),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if let Some(map) = &mut self.map {
            map.put(key, value);
        }
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.map.as_mut()?.get(key)
    }

    pub fn len(&self) -> usize {
        self.map.as_ref().map_or(0, LruCache::len)
    }

    pub fn capacity(&self) -> usize {
        self.map.as_ref().map_or(0, |map| map.cap().get())
    }

    pub fn is_empty(&self) -> bool {
        self.map.as_ref().is_none_or(LruCache::is_empty)
    }
}

/// Capacity- and byte-weighted LRU cache. Beyond bounding the entry count like
/// [`BoundedCache`], it bounds the total retained byte weight of its values and
/// evicts the coldest entry until both budgets fit. Used for the render
/// pipeline's wrapped-body cache (§12.4), whose entries can weigh from a few
/// bytes up to ~128 KiB each.
#[derive(Debug)]
pub struct WeightedCache<K: Eq + Hash, V> {
    map: Option<LruCache<K, (V, usize)>>,
    retained_bytes: usize,
    max_bytes: usize,
    evictions: u64,
}

impl<K: Eq + Hash, V> WeightedCache<K, V> {
    pub fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            map: NonZeroUsize::new(capacity).map(LruCache::new),
            retained_bytes: 0,
            max_bytes,
            evictions: 0,
        }
    }

    /// Inserts `value` with its retained byte weight. Returns `false` when a
    /// single value exceeds the global byte budget and is therefore rejected.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) -> bool {
        if bytes > self.max_bytes {
            return false;
        }
        let Some(map) = &mut self.map else {
            return false;
        };
        // A replacement frees the previous entry's weight before admission:
        // without this the tracker keeps counting dropped bytes, evicting
        // unrelated entries and eventually rejecting every insert.
        if let Some((_, old_bytes)) = map.pop(&key) {
            self.retained_bytes = self.retained_bytes.saturating_sub(old_bytes);
        }
        let max_entries = map.cap().get();
        while map.len() >= max_entries || self.retained_bytes.saturating_add(bytes) > self.max_bytes
        {
            match map.pop_lru() {
                Some((_, (_, entry_bytes))) => {
                    self.retained_bytes = self.retained_bytes.saturating_sub(entry_bytes);
                    self.evictions = self.evictions.saturating_add(1);
                }
                None => return false,
            }
        }
        map.put(key, (value, bytes));
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        true
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.map.as_mut()?.get(key).map(|(value, _)| value)
    }

    pub fn len(&self) -> usize {
        self.map.as_ref().map_or(0, LruCache::len)
    }

    pub fn capacity(&self) -> usize {
        self.map.as_ref().map_or(0, |map| map.cap().get())
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    pub fn is_empty(&self) -> bool {
        self.map.as_ref().is_none_or(LruCache::is_empty)
    }

    pub fn clear(&mut self) {
        if let Some(map) = &mut self.map {
            map.clear();
        }
        self.retained_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_is_always_empty() {
        let mut cache = BoundedCache::new(0);
        cache.insert("key", 1);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.capacity(), 0);
        assert_eq!(cache.get(&"key"), None);
    }

    #[test]
    fn weighted_cache_evicts_coldest_by_bytes() {
        let mut cache = WeightedCache::new(16, 100);
        assert!(cache.insert("a", 1, 60));
        assert!(cache.insert("b", 2, 30));
        assert_eq!(cache.retained_bytes(), 90);
        // Next insert (40 bytes) exceeds the 100-byte budget: evict "a" (LRU).
        assert!(cache.insert("c", 3, 40));
        assert_eq!(cache.retained_bytes(), 70);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.evictions(), 1);
    }

    #[test]
    fn weighted_cache_evicts_by_count_and_reading_refreshes_recency() {
        let mut cache = WeightedCache::new(2, 100);
        assert!(cache.insert("a", 1, 10));
        assert!(cache.insert("b", 2, 10));
        assert_eq!(cache.get(&"a"), Some(&1), "read promotes a");
        assert!(cache.insert("c", 3, 10));
        assert_eq!(cache.get(&"b"), None, "b is now the coldest");
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.evictions(), 1);
    }

    #[test]
    fn weighted_cache_rejects_oversized_value_and_zero_capacity() {
        let mut cache = WeightedCache::<&str, u32>::new(8, 100);
        assert!(
            !cache.insert("big", 1, 101),
            "single value over budget rejected"
        );
        assert!(cache.is_empty());

        let mut empty = WeightedCache::<&str, u32>::new(0, 100);
        assert!(!empty.insert("any", 1, 1), "zero capacity stores nothing");
        assert!(empty.is_empty());
        assert_eq!(empty.capacity(), 0);
    }

    #[test]
    fn weighted_cache_replacement_frees_the_old_weight() {
        let mut cache = WeightedCache::new(4, 100);
        assert!(cache.insert("a", 1, 60));
        assert!(cache.insert("b", 2, 40));
        // Reinserting "a" at a new weight must not count the old 60 bytes.
        assert!(cache.insert("a", 3, 40));
        assert_eq!(cache.retained_bytes(), 80);
        assert_eq!(cache.get(&"a"), Some(&3));
        assert_eq!(cache.get(&"b"), Some(&2), "no innocent eviction");
        assert_eq!(cache.evictions(), 0);

        // Repeated replacements must not leak the tracker into a state that
        // rejects inserts while the map still has room. The first reinsert
        // evicts b to fit the 100-byte budget; the rest replace a alone.
        for _ in 0..8 {
            assert!(cache.insert("a", 9, 80), "replacement stays admissible");
        }
        assert_eq!(cache.retained_bytes(), 80);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.evictions(), 1);
        assert_eq!(cache.get(&"a"), Some(&9));
    }

    #[test]
    fn weighted_cache_clear_resets_bytes_but_not_evictions() {
        let mut cache = WeightedCache::new(4, 100);
        assert!(cache.insert("a", 1, 30));
        assert!(cache.insert("b", 2, 40));
        assert!(cache.insert("c", 3, 40));
        assert!(cache.insert("d", 4, 1), "evicts a to fit");
        assert_eq!(cache.evictions(), 1);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.retained_bytes(), 0);
        assert_eq!(cache.evictions(), 1, "telemetry is cumulative");
    }
}
