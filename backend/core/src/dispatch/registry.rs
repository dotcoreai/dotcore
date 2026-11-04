//! Private immutable keyed-registry mechanics shared by runtime dispatch facades.

use std::{collections::BTreeMap, sync::Arc};

use dotcore_domain::valid_id;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum KeyedRegistryError {
    InvalidKey,

    DuplicateKey(String),
}

pub(super) struct KeyedRegistry<T: ?Sized> {
    entries: Arc<BTreeMap<String, Arc<T>>>,
}

impl<T: ?Sized> KeyedRegistry<T> {
    pub(super) fn new() -> Self {
        Self {
            entries: Arc::new(BTreeMap::new()),
        }
    }

    pub(super) fn insert(
        &mut self,
        key: impl Into<String>,
        value: Arc<T>,
    ) -> Result<(), KeyedRegistryError> {
        let key = key.into();
        if !valid_id(&key) {
            return Err(KeyedRegistryError::InvalidKey);
        }
        let entries = Arc::make_mut(&mut self.entries);
        if entries.contains_key(&key) {
            return Err(KeyedRegistryError::DuplicateKey(key));
        }
        entries.insert(key, value);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub(super) fn get(&self, key: &str) -> Option<&Arc<T>> {
        self.entries.get(key)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &Arc<T>)> {
        self.entries.iter()
    }
}

impl<T: ?Sized> Clone for KeyedRegistry<T> {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
        }
    }
}
