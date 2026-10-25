//! Reverse membership indexes. Empty buckets are removed with their last member.
use crate::{Id, Version};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Index<K>(BTreeMap<K, BTreeSet<Id>>);

impl<K> Default for Index<K> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

/// Counts live memberships by implementation version without retaining copies per program.
pub(crate) struct VersionCounts<K>(BTreeMap<K, BTreeMap<Version, usize>>);
impl<K> Default for VersionCounts<K> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}
impl<K: Ord> VersionCounts<K> {
    pub fn insert(&mut self, key: K, version: Version) {
        *self.0.entry(key).or_default().entry(version).or_default() += 1;
    }
    pub fn remove(&mut self, key: &K, version: Version) {
        if let Some(versions) = self.0.get_mut(key) {
            let count = versions.get_mut(&version).expect("indexed version");
            *count -= 1;
            if *count == 0 {
                versions.remove(&version);
            }
            if versions.is_empty() {
                self.0.remove(key);
            }
        }
    }
    pub fn count(&self, key: &K, version: Version) -> usize {
        self.0
            .get(key)
            .and_then(|versions| versions.get(&version))
            .copied()
            .unwrap_or(0)
    }
    pub fn versions<'a>(&'a self, key: &K) -> impl Iterator<Item = (Version, usize)> + use<'a, K> {
        self.0
            .get(key)
            .into_iter()
            .flat_map(|versions| versions.iter().map(|(version, count)| (*version, *count)))
    }
}
impl<K: Ord> Index<K> {
    pub fn insert(&mut self, key: K, id: Id) {
        self.0.entry(key).or_default().insert(id);
    }
    pub fn remove(&mut self, key: &K, id: Id) {
        if let Some(ids) = self.0.get_mut(key) {
            ids.remove(&id);
            if ids.is_empty() {
                self.0.remove(key);
            }
        }
    }
    pub fn members<'a>(&'a self, key: &K) -> impl Iterator<Item = Id> + use<'a, K> {
        self.0.get(key).into_iter().flatten().copied()
    }
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }
    pub fn take(&mut self, key: &K) -> BTreeSet<Id> {
        self.0.remove(key).unwrap_or_default()
    }
}
