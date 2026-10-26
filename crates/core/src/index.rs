//! Reverse membership indexes. Empty buckets are removed with their last member.
use crate::{Id, Version};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Index<K>(BTreeMap<K, BTreeSet<Id>>);

impl<K> Default for Index<K> {
    /// Create an empty reverse membership index.
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

/// Counts live memberships by implementation version without retaining copies per program.
pub(crate) struct VersionCounts<K>(BTreeMap<K, BTreeMap<Version, usize>>);
impl<K> Default for VersionCounts<K> {
    /// Create an empty reverse membership index.
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}
impl<K: Ord> VersionCounts<K> {
    /// Increment this key's membership count for one version.
    pub fn insert(&mut self, key: K, version: Version) {
        *self.0.entry(key).or_default().entry(version).or_default() += 1;
    }

    /// Decrement one version count and discard empty buckets.
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

    /// Read the membership count for one key and version.
    pub fn count(&self, key: &K, version: Version) -> usize {
        self.0
            .get(key)
            .and_then(|versions| versions.get(&version))
            .copied()
            .unwrap_or(0)
    }

    /// Iterate only the versions indexed under this key.
    pub fn versions<'a>(&'a self, key: &K) -> impl Iterator<Item = (Version, usize)> + use<'a, K> {
        self.0
            .get(key)
            .into_iter()
            .flat_map(|versions| versions.iter().map(|(version, count)| (*version, *count)))
    }
}
impl<K: Ord> Index<K> {
    /// Add one membership to the selected key bucket.
    pub fn insert(&mut self, key: K, id: Id) {
        self.0.entry(key).or_default().insert(id);
    }

    /// Remove one membership and discard empty buckets.
    pub fn remove(&mut self, key: &K, id: Id) {
        if let Some(ids) = self.0.get_mut(key) {
            ids.remove(&id);
            if ids.is_empty() {
                self.0.remove(key);
            }
        }
    }

    /// Iterate identities indexed under this key.
    pub fn members<'a>(&'a self, key: &K) -> impl Iterator<Item = Id> + use<'a, K> {
        self.0.get(key).into_iter().flatten().copied()
    }

    /// Check whether this key has any live members.
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    /// Remove a key and return its owned membership set.
    pub fn take(&mut self, key: &K) -> BTreeSet<Id> {
        self.0.remove(key).unwrap_or_default()
    }
}
