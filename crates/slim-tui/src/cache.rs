use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Debug)]
pub struct BoundedCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Clone + Eq + Hash, V> BoundedCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }
        if self.map.contains_key(&key) {
            self.order.retain(|candidate| candidate != &key);
        }
        self.map.insert(key.clone(), value);
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
