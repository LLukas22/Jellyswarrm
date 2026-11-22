//! Shared storage for the items of cached catalog listings.
//!
//! A listing keeps an ordered list of [`Entry`] handles instead of its own
//! copy of every item. Items are stored once as compact parsed records in a
//! reference-counted slab, so two sorts of one library, two devices of one
//! viewer, or two viewers who see the same episode share its record. Each
//! item's `UserData` is a separate record, so viewers with different watched
//! state still share the rest of the item.
//!
//! Records are freed when the last listing referencing them is dropped, which
//! happens when the cache evicts or replaces it. The store therefore knows its
//! exact retained size, and the cache charges shared records once.
//!
//! Records are matched by exact content, including which fields are present.
//! A response that requested fewer fields gets its own record and never
//! renders fields it did not receive.
use crate::application::error::ApplicationError;
use serde_json::{Map, Number, Value};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    num::NonZeroU32,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
};

use super::postprocessing::Pagination;

/// Items converted or interned per lock acquisition, so a large listing
/// never blocks other readers or writers for long.
const CHUNK: usize = 1024;
/// Object keys shared through the key table. Further keys stay inline.
const MAX_SHARED_KEYS: usize = 4096;
const USER_DATA: &str = "UserData";

/// A parsed JSON value without per-value key strings or spare capacity.
#[derive(Debug, PartialEq)]
enum Compact {
    Null,
    Bool(bool),
    Number(Number),
    String(Box<str>),
    Array(Box<[Compact]>),
    /// Field names from the shared key table, in their original order.
    Object(Box<[(u32, Compact)]>),
    /// An object with at least one key that is not in the key table, such as
    /// a map keyed by image tag.
    OpenObject(Box<[(Box<str>, Compact)]>),
}

impl Compact {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Null | Self::Bool(_) | Self::Number(_) => 0,
            Self::String(value) => value.len(),
            Self::Array(values) => values
                .iter()
                .map(|value| std::mem::size_of::<Compact>() + value.heap_bytes())
                .sum(),
            Self::Object(fields) => fields
                .iter()
                .map(|(_, value)| std::mem::size_of::<(u32, Compact)>() + value.heap_bytes())
                .sum(),
            Self::OpenObject(fields) => fields
                .iter()
                .map(|(key, value)| {
                    std::mem::size_of::<(Box<str>, Compact)>() + key.len() + value.heap_bytes()
                })
                .sum(),
        }
    }

    fn hash_into(&self, state: &mut impl Hasher) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Bool(value) => value.hash(state),
            Self::Number(value) => {
                if let Some(value) = value.as_u64() {
                    (0u8, value).hash(state);
                } else if let Some(value) = value.as_i64() {
                    (1u8, value).hash(state);
                } else {
                    (2u8, value.as_f64().map(f64::to_bits)).hash(state);
                }
            }
            Self::String(value) => value.hash(state),
            Self::Array(values) => {
                values.len().hash(state);
                for value in values.iter() {
                    value.hash_into(state);
                }
            }
            Self::Object(fields) => {
                fields.len().hash(state);
                for (key, value) in fields.iter() {
                    key.hash(state);
                    value.hash_into(state);
                }
            }
            Self::OpenObject(fields) => {
                fields.len().hash(state);
                for (key, value) in fields.iter() {
                    key.hash(state);
                    value.hash_into(state);
                }
            }
        }
    }

    fn content_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash_into(&mut hasher);
        hasher.finish()
    }
}

type PendingEntry = ((Compact, u64), Option<(Compact, u64)>);

/// Field names shared by every record. Only names shaped like Jellyfin field
/// names are shared; IDs and image tags used as map keys stay inline so the
/// table cannot fill with per-item values.
#[derive(Default)]
struct KeyTable {
    names: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
}

fn is_field_name(key: &str) -> bool {
    key.len() <= 64
        && key.starts_with(|c: char| c.is_ascii_uppercase())
        && key.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Index of an occupied slot. Debug builds also check the slot's generation,
/// so a released record can never be read through a stale handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle {
    index: NonZeroU32,
    #[cfg(debug_assertions)]
    generation: u32,
}

impl Handle {
    fn slot(self) -> usize {
        self.index.get() as usize - 1
    }
}

/// One listed item: its shared record and the viewer's `UserData`, if any.
#[derive(Clone, Copy, Debug)]
struct Entry {
    item: Handle,
    user_data: Option<Handle>,
}

struct Slot {
    generation: u32,
    references: u32,
    /// References held by listings that are in the cache.
    cached: u32,
    hash: u64,
    /// Next slot with the same hash.
    next: Option<NonZeroU32>,
    bytes: usize,
    value: Option<Compact>,
}

/// Accounted per record in addition to its value: the slot and its share of
/// the hash index.
const SLOT_OVERHEAD: usize = std::mem::size_of::<Slot>() + 24;

#[derive(Default)]
struct Records {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// First slot of each hash chain.
    index: HashMap<u64, NonZeroU32>,
    bytes: usize,
    records: usize,
    /// Bytes of the records referenced by at least one cached listing.
    cached_bytes: usize,
}

impl Records {
    /// Reference an existing record with this exact content or add it.
    /// Returns the handle and the bytes newly allocated for it.
    fn acquire(&mut self, value: Compact, hash: u64) -> (Handle, usize) {
        let mut cursor = self.index.get(&hash).copied();
        while let Some(index) = cursor {
            let slot = &mut self.slots[index.get() as usize - 1];
            if slot.value.as_ref() == Some(&value) {
                slot.references += 1;
                return (
                    Handle {
                        index,
                        #[cfg(debug_assertions)]
                        generation: slot.generation,
                    },
                    0,
                );
            }
            cursor = slot.next;
        }
        let bytes = SLOT_OVERHEAD + value.heap_bytes();
        let next = self.index.get(&hash).copied();
        let position = match self.free.pop() {
            Some(position) => position as usize,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    references: 0,
                    cached: 0,
                    hash: 0,
                    next: None,
                    bytes: 0,
                    value: None,
                });
                self.slots.len() - 1
            }
        };
        let index = NonZeroU32::new(
            u32::try_from(position + 1).expect("item store exceeds u32::MAX records"),
        )
        .expect("slot indexes start at one");
        let slot = &mut self.slots[position];
        slot.references = 1;
        slot.cached = 0;
        slot.hash = hash;
        slot.next = next;
        slot.bytes = bytes;
        slot.value = Some(value);
        self.index.insert(hash, index);
        self.bytes += bytes;
        self.records += 1;
        (
            Handle {
                index,
                #[cfg(debug_assertions)]
                generation: slot.generation,
            },
            bytes,
        )
    }

    fn get(&self, handle: Handle) -> &Compact {
        let slot = &self.slots[handle.slot()];
        #[cfg(debug_assertions)]
        assert_eq!(slot.generation, handle.generation, "stale item handle");
        debug_assert!(slot.references > 0, "released item handle");
        slot.value
            .as_ref()
            .expect("referenced records are occupied")
    }

    fn release(&mut self, handle: Handle) {
        let position = handle.slot();
        let slot = &mut self.slots[position];
        #[cfg(debug_assertions)]
        assert_eq!(slot.generation, handle.generation, "stale item handle");
        slot.references = slot
            .references
            .checked_sub(1)
            .expect("item record released more often than acquired");
        if slot.references > 0 {
            return;
        }
        debug_assert_eq!(slot.cached, 0, "cached record released");
        let (hash, next) = (slot.hash, slot.next.take());
        self.bytes -= slot.bytes;
        self.records -= 1;
        slot.value = None;
        slot.bytes = 0;
        slot.generation = slot.generation.wrapping_add(1);
        // Unlink the slot from its hash chain.
        let head = self.index.get(&hash).copied();
        if head == Some(handle.index) {
            match next {
                Some(next) => {
                    self.index.insert(hash, next);
                }
                None => {
                    self.index.remove(&hash);
                }
            }
        } else {
            let mut cursor = head;
            while let Some(index) = cursor {
                let previous = &mut self.slots[index.get() as usize - 1];
                if previous.next == Some(handle.index) {
                    previous.next = next;
                    break;
                }
                cursor = previous.next;
            }
        }
        self.free.push(position as u32);
        // Let a store that shrank a lot return its slab to the allocator.
        if self.records == 0 {
            self.slots = Vec::new();
            self.free = Vec::new();
            self.index = HashMap::new();
        }
    }

    /// Count a cached listing's reference. A record shared by several cached
    /// listings is charged once.
    fn retain_cached(&mut self, handle: Handle) {
        let slot = &mut self.slots[handle.slot()];
        slot.cached += 1;
        if slot.cached == 1 {
            self.cached_bytes += slot.bytes;
        }
    }

    fn release_cached(&mut self, handle: Handle) {
        let slot = &mut self.slots[handle.slot()];
        slot.cached = slot
            .cached
            .checked_sub(1)
            .expect("cached record released more often than retained");
        if slot.cached == 0 {
            self.cached_bytes -= slot.bytes;
        }
    }
}

/// Retained size of the shared records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoreStats {
    pub(crate) records: usize,
    pub(crate) bytes: usize,
    /// Bytes of the records that cached listings reference. Records held only
    /// by running scans or by readers of evicted listings are not included.
    pub(crate) cached_bytes: usize,
}

#[derive(Default)]
pub(crate) struct ItemStore {
    records: RwLock<Records>,
    keys: RwLock<KeyTable>,
}

impl ItemStore {
    pub(crate) fn stats(&self) -> StoreStats {
        let records = self.read();
        StoreStats {
            records: records.records,
            bytes: records.bytes,
            cached_bytes: records.cached_bytes,
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Records> {
        // Records are only mutated in short sections that cannot panic halfway.
        self.records.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Records> {
        self.records.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn key_id(&self, key: &str) -> Option<u32> {
        if !is_field_name(key) {
            return None;
        }
        if let Some(id) = self
            .keys
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .ids
            .get(key)
        {
            return Some(*id);
        }
        let mut keys = self.keys.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = keys.ids.get(key) {
            return Some(*id);
        }
        if keys.names.len() >= MAX_SHARED_KEYS {
            return None;
        }
        let id = keys.names.len() as u32;
        keys.names.push(key.into());
        keys.ids.insert(key.into(), id);
        Some(id)
    }

    fn compact(&self, value: Value) -> Compact {
        match value {
            Value::Null => Compact::Null,
            Value::Bool(value) => Compact::Bool(value),
            Value::Number(value) => Compact::Number(value),
            Value::String(value) => Compact::String(value.into_boxed_str()),
            Value::Array(values) => Compact::Array(
                values
                    .into_iter()
                    .map(|value| self.compact(value))
                    .collect(),
            ),
            Value::Object(fields) => {
                let ids = fields
                    .keys()
                    .map(|key| self.key_id(key))
                    .collect::<Option<Vec<_>>>();
                match ids {
                    Some(ids) => Compact::Object(
                        ids.into_iter()
                            .zip(fields.into_iter().map(|(_, value)| self.compact(value)))
                            .collect(),
                    ),
                    None => Compact::OpenObject(
                        fields
                            .into_iter()
                            .map(|(key, value)| (key.into_boxed_str(), self.compact(value)))
                            .collect(),
                    ),
                }
            }
        }
    }

    fn expand(&self, value: &Compact, keys: &KeyTable) -> Value {
        match value {
            Compact::Null => Value::Null,
            Compact::Bool(value) => Value::Bool(*value),
            Compact::Number(value) => Value::Number(value.clone()),
            Compact::String(value) => Value::String(value.to_string()),
            Compact::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|value| self.expand(value, keys))
                    .collect(),
            ),
            Compact::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        (
                            keys.names[*key as usize].to_string(),
                            self.expand(value, keys),
                        )
                    })
                    .collect(),
            ),
            Compact::OpenObject(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.to_string(), self.expand(value, keys)))
                    .collect(),
            ),
        }
    }

    /// Move a materialized catalog response into shared records. Typed items
    /// are serialized and compacted one at a time, so the whole listing never
    /// exists as one JSON tree.
    pub(super) fn store_listing(
        self: &Arc<Self>,
        response: impl Into<super::snapshots::SnapshotBody>,
    ) -> Result<StoredListing, ApplicationError> {
        use crate::protocol::models::ItemsResponseVariants;
        match response.into() {
            super::snapshots::SnapshotBody::Stored(listing) => Ok(listing),
            super::snapshots::SnapshotBody::Items(ItemsResponseVariants::WithCount(counted)) => {
                let mut envelope = Map::new();
                envelope.insert("TotalRecordCount".into(), counted.total_record_count.into());
                envelope.insert("StartIndex".into(), counted.start_index.into());
                self.store_items(
                    Some(envelope),
                    counted.items.into_iter().map(serde_json::to_value),
                )
            }
            super::snapshots::SnapshotBody::Items(ItemsResponseVariants::Bare(items)) => {
                self.store_items(None, items.into_iter().map(serde_json::to_value))
            }
            super::snapshots::SnapshotBody::Json(Value::Array(items)) => {
                self.store_items(None, items.into_iter().map(Ok))
            }
            super::snapshots::SnapshotBody::Json(Value::Object(mut envelope)) => {
                match envelope.remove("Items") {
                    Some(Value::Array(items)) => {
                        self.store_items(Some(envelope), items.into_iter().map(Ok))
                    }
                    _ => Err(ApplicationError::Internal(anyhow::anyhow!(
                        "materialized catalog Items must be an array"
                    ))),
                }
            }
            super::snapshots::SnapshotBody::Json(_) => Err(ApplicationError::Internal(
                anyhow::anyhow!("invalid materialized catalog envelope"),
            )),
        }
    }

    fn store_items(
        self: &Arc<Self>,
        envelope: Option<Map<String, Value>>,
        items: impl ExactSizeIterator<Item = Result<Value, serde_json::Error>>,
    ) -> Result<StoredListing, ApplicationError> {
        let mut listing = StoredListing::new(self.clone());
        listing.envelope = envelope;
        listing.entries.reserve_exact(items.len());
        listing.extend_values(items)?;
        Ok(listing)
    }

    /// Separate an item's `UserData` object, which is personal, from the rest.
    fn split(&self, item: Value) -> PendingEntry {
        let (item, user_data) = match item {
            Value::Object(mut fields) => match fields.remove(USER_DATA) {
                Some(user_data @ Value::Object(_)) => (Value::Object(fields), Some(user_data)),
                Some(other) => {
                    fields.insert(USER_DATA.into(), other);
                    (Value::Object(fields), None)
                }
                None => (Value::Object(fields), None),
            },
            other => (other, None),
        };
        let with_hash = |value: Value| {
            let value = self.compact(value);
            let hash = value.content_hash();
            (value, hash)
        };
        (with_hash(item), user_data.map(with_hash))
    }
}

/// The ordered items of one cached listing, plus its response envelope.
pub(crate) struct StoredListing {
    store: Arc<ItemStore>,
    /// `None` for a bare array response.
    envelope: Option<Map<String, Value>>,
    entries: Vec<Entry>,
    /// Bytes of the records this listing added to the store.
    new_bytes: usize,
    /// Bytes of every record this listing references, as if unshared.
    referenced_bytes: usize,
    /// Whether the cache currently charges this listing's records.
    cached: AtomicBool,
}

impl StoredListing {
    pub(super) fn new(store: Arc<ItemStore>) -> Self {
        Self {
            store,
            envelope: None,
            entries: Vec::new(),
            new_bytes: 0,
            referenced_bytes: 0,
            cached: AtomicBool::new(false),
        }
    }

    pub(super) fn extend_items(
        &mut self,
        items: Vec<crate::protocol::models::MediaItem>,
    ) -> Result<(), ApplicationError> {
        self.extend_values(items.into_iter().map(serde_json::to_value))
    }

    fn extend_values(
        &mut self,
        mut items: impl Iterator<Item = Result<Value, serde_json::Error>>,
    ) -> Result<(), ApplicationError> {
        loop {
            // Convert outside the lock, then intern the batch. The listing owns
            // the references it acquired even if a later conversion fails.
            let batch = items
                .by_ref()
                .take(CHUNK)
                .map(|item| item.map(|item| self.store.split(item)))
                .collect::<Result<Vec<_>, _>>()
                .map_err(ApplicationError::Serialization)?;
            if batch.is_empty() {
                return Ok(());
            }
            self.intern_batch(batch);
        }
    }

    fn intern_batch(&mut self, batch: Vec<PendingEntry>) {
        let mut records = self.store.write();
        let mut acquire = |(value, hash)| {
            let (handle, bytes) = records.acquire(value, hash);
            self.new_bytes += bytes;
            self.referenced_bytes += records.slots[handle.slot()].bytes;
            handle
        };
        self.entries
            .extend(batch.into_iter().map(|(item, user_data)| Entry {
                item: acquire(item),
                user_data: user_data.map(&mut acquire),
            }));
    }

    pub(super) fn append(&mut self, mut source: Self) {
        assert!(Arc::ptr_eq(&self.store, &source.store));
        self.entries.append(&mut source.entries);
        self.new_bytes += source.new_bytes;
        self.referenced_bytes += source.referenced_bytes;
    }

    /// Apply a permutation or subset of the source entries and transfer ownership
    /// of the selected records to the finished listing.
    pub(super) fn finish(
        &mut self,
        order: Vec<usize>,
        shape: super::postprocessing::ResponseShape,
    ) {
        let mut source = self.entries.iter().copied().map(Some).collect::<Vec<_>>();
        let entries = order
            .into_iter()
            .map(|index| {
                source[index]
                    .take()
                    .expect("listing selected an entry twice")
            })
            .collect();
        self.entries = entries;
        for chunk in source.chunks(CHUNK) {
            let mut records = self.store.write();
            for entry in chunk.iter().flatten() {
                for handle in std::iter::once(entry.item).chain(entry.user_data) {
                    self.referenced_bytes -= records.slots[handle.slot()].bytes;
                    records.release(handle);
                }
            }
        }
        if shape == super::postprocessing::ResponseShape::Counted {
            let count = self.len().min(i32::MAX as usize);
            self.envelope = Some(Map::from_iter([
                ("TotalRecordCount".to_string(), Value::from(count)),
                ("StartIndex".to_string(), Value::from(0)),
            ]));
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Bytes owned by the listing itself, excluding shared records.
    pub(crate) fn own_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.envelope.as_ref().map_or(0, |envelope| {
                Value::Object(envelope.clone()).to_string().len()
            })
    }

    /// Bytes the listing would retain if it shared no record.
    pub(crate) fn standalone_bytes(&self) -> usize {
        self.own_bytes() + self.referenced_bytes
    }

    /// Bytes of the records this listing added to the store when it was built.
    #[cfg(test)]
    pub(crate) fn new_record_bytes(&self) -> usize {
        self.new_bytes
    }

    /// Render one page with the same envelope as the stored response.
    pub(super) fn page(&self, pagination: Pagination) -> Value {
        let start = pagination.start_index.min(self.entries.len());
        let end = pagination.limit.map_or(self.entries.len(), |limit| {
            start.saturating_add(limit).min(self.entries.len())
        });
        let mut items = Vec::with_capacity(end - start);
        for chunk in self.entries[start..end].chunks(CHUNK) {
            let records = self.store.read();
            let keys = self
                .store
                .keys
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            items.extend(chunk.iter().map(|entry| {
                let mut item = self.store.expand(records.get(entry.item), &keys);
                if let (Some(user_data), Value::Object(fields)) = (entry.user_data, &mut item) {
                    fields.insert(
                        USER_DATA.into(),
                        self.store.expand(records.get(user_data), &keys),
                    );
                }
                item
            }));
        }
        let Some(envelope) = &self.envelope else {
            return Value::Array(items);
        };
        let mut response = envelope.clone();
        response.insert("Items".into(), Value::Array(items));
        response.insert(
            "StartIndex".into(),
            serde_json::json!(pagination.start_index.min(i32::MAX as usize)),
        );
        Value::Object(response)
    }

    /// Charge this listing's records to the cache. Idempotent.
    pub(super) fn mark_cached(&self) {
        if !self.cached.swap(true, Ordering::AcqRel) {
            self.update_cached(Records::retain_cached);
        }
    }

    /// Stop charging this listing's records to the cache. Idempotent. Readers
    /// may keep the listing alive; its records then count only as store bytes.
    pub(super) fn unmark_cached(&self) {
        if self.cached.swap(false, Ordering::AcqRel) {
            self.update_cached(Records::release_cached);
        }
    }

    fn update_cached(&self, update: fn(&mut Records, Handle)) {
        for chunk in self.entries.chunks(CHUNK) {
            let mut records = self.store.write();
            for entry in chunk {
                for handle in std::iter::once(entry.item).chain(entry.user_data) {
                    update(&mut records, handle);
                }
            }
        }
    }

    /// The stored response envelope without its items.
    #[cfg(test)]
    pub(crate) fn envelope(&self) -> Option<&Map<String, Value>> {
        self.envelope.as_ref()
    }
}

impl Drop for StoredListing {
    fn drop(&mut self) {
        self.unmark_cached();
        for chunk in self.entries.chunks(CHUNK) {
            let mut records = self.store.write();
            for entry in chunk {
                records.release(entry.item);
                if let Some(user_data) = entry.user_data {
                    records.release(user_data);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all() -> Pagination {
        Pagination::unbounded()
    }

    fn episode(id: &str, played: bool) -> Value {
        json!({
            "Id": id,
            "Name": format!("Episode {id}"),
            "Type": "Episode",
            "IndexNumber": 3,
            "CommunityRating": 7.25,
            "RunTimeTicks": -1,
            "ProviderIds": {"Tvdb": "12"},
            "ImageBlurHashes": {"Primary": {"4f2a": "hash"}},
            "UserData": {"Played": played, "Key": format!("key-{id}"), "PlayCount": u8::from(played)},
        })
    }

    #[test]
    fn listings_render_exactly_what_was_stored() {
        let store = Arc::new(ItemStore::default());
        let response = json!({
            "Items": [episode("a", false), episode("b", true), {"Id": "c", "UserData": null}, 5, "x"],
            "TotalRecordCount": 5,
            "StartIndex": 0,
            "FutureEnvelope": {"kept": true},
        });
        let listing = store.store_listing(response.clone()).unwrap();
        assert_eq!(listing.page(all()), response);
        let page = listing.page(Pagination {
            start_index: 1,
            limit: Some(2),
        });
        assert_eq!(
            page["Items"],
            json!([episode("b", true), {"Id": "c", "UserData": null}])
        );
        assert_eq!(page["StartIndex"], 1);
        assert_eq!(page["TotalRecordCount"], 5);
        assert_eq!(page["FutureEnvelope"], json!({"kept": true}));
        let past_end = listing.page(Pagination {
            start_index: 10,
            limit: Some(2),
        });
        assert_eq!(past_end["Items"], json!([]));

        let bare = json!([episode("a", false)]);
        assert_eq!(store.store_listing(bare.clone()).unwrap().page(all()), bare);
    }

    #[test]
    fn identical_items_are_stored_once_and_freed_with_their_last_listing() {
        let store = Arc::new(ItemStore::default());
        let by_name = store
            .store_listing(json!([episode("a", false), episode("b", false)]))
            .unwrap();
        let after_one = store.stats();
        // Same items in another order: no new records at all.
        let reversed = store
            .store_listing(json!([episode("b", false), episode("a", false)]))
            .unwrap();
        assert_eq!(store.stats(), after_one);
        assert_eq!(reversed.new_record_bytes(), 0);
        // Another viewer watched "a": only that UserData record is new.
        let other_viewer = store
            .store_listing(json!([episode("a", true), episode("b", false)]))
            .unwrap();
        assert_eq!(store.stats().records, after_one.records + 1);
        assert_eq!(
            other_viewer.page(all()),
            json!([episode("a", true), episode("b", false)])
        );
        drop(by_name);
        drop(reversed);
        // The unwatched UserData of "a" had no other owner and is gone.
        assert_eq!(store.stats().records, after_one.records);
        assert_eq!(
            other_viewer.page(all()),
            json!([episode("a", true), episode("b", false)])
        );
        drop(other_viewer);
        assert_eq!(store.stats(), StoreStats::default());
    }

    #[test]
    fn a_narrow_response_never_renders_fields_from_a_richer_one() {
        let store = Arc::new(ItemStore::default());
        let rich = store
            .store_listing(json!([{"Id": "a", "Name": "A", "Overview": "plot"}]))
            .unwrap();
        let narrow = store
            .store_listing(json!([{"Id": "a", "Name": "A"}]))
            .unwrap();
        assert_eq!(narrow.page(all()), json!([{"Id": "a", "Name": "A"}]));
        assert_eq!(
            rich.page(all()),
            json!([{"Id": "a", "Name": "A", "Overview": "plot"}])
        );
        // Numbers of different types are different values.
        let int = store
            .store_listing(json!([{"Id": "n", "Value": 1}]))
            .unwrap();
        let float = store
            .store_listing(json!([{"Id": "n", "Value": 1.0}]))
            .unwrap();
        assert_eq!(int.page(all()), json!([{"Id": "n", "Value": 1}]));
        assert_eq!(float.page(all()), json!([{"Id": "n", "Value": 1.0}]));
    }

    #[test]
    fn freed_slots_are_reused_and_hash_chains_survive_removal() {
        let store = Arc::new(ItemStore::default());
        let mut records = store.write();
        // Force three different records into one hash chain.
        let values = ["a", "b", "c"].map(|name| Compact::String(name.into()));
        let handles = values
            .into_iter()
            .map(|value| records.acquire(value, 7).0)
            .collect::<Vec<_>>();
        records.release(handles[1]);
        assert_eq!(records.records, 2);
        assert_eq!(records.get(handles[0]), &Compact::String("a".into()));
        assert_eq!(records.get(handles[2]), &Compact::String("c".into()));
        let (again, _) = records.acquire(Compact::String("a".into()), 7);
        assert_eq!(again.index, handles[0].index);
        let (reused, _) = records.acquire(Compact::String("d".into()), 7);
        assert_eq!(reused.index, handles[1].index, "the freed slot is reused");
        assert_ne!(reused, handles[1], "with a new generation");
        for handle in [handles[0], again, handles[2], reused] {
            records.release(handle);
        }
        assert_eq!(records.records, 0);
        assert_eq!(records.bytes, 0);
        assert!(records.index.is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "stale item handle")]
    fn a_stale_handle_cannot_read_a_reused_slot() {
        let store = Arc::new(ItemStore::default());
        let mut records = store.write();
        let (old, _) = records.acquire(Compact::String("old".into()), 1);
        let (_keep, _) = records.acquire(Compact::String("keep".into()), 2);
        records.release(old);
        let _ = records.acquire(Compact::String("new".into()), 3);
        let _ = records.get(old);
    }

    #[test]
    fn typed_listings_render_like_their_serialized_response() {
        use crate::protocol::models::{ItemsResponseVariants, ItemsResponseWithCount};
        let typed_item = |id: &str, played: bool| {
            let mut item = episode(id, played);
            item["UserData"] = json!({
                "PlaybackPositionTicks": 0,
                "PlayCount": u8::from(played),
                "IsFavorite": false,
                "Played": played,
                "Key": format!("key-{id}"),
                "ItemId": id,
            });
            item
        };
        let items = [
            typed_item("a", true),
            typed_item("b", false),
            json!({"Id": "c", "Type": "Movie"}),
        ]
        .into_iter()
        .map(|item| serde_json::from_value(item).unwrap())
        .collect::<Vec<crate::protocol::models::MediaItem>>();
        let store = Arc::new(ItemStore::default());
        for typed in [
            ItemsResponseVariants::WithCount(ItemsResponseWithCount {
                items: items.clone(),
                total_record_count: 3,
                start_index: 0,
            }),
            ItemsResponseVariants::Bare(items),
        ] {
            let expected = serde_json::to_value(&typed).unwrap();
            let listing = store
                .store_listing(crate::application::catalog::snapshots::SnapshotBody::Items(
                    typed,
                ))
                .unwrap();
            assert_eq!(listing.page(all()), expected);
        }
    }

    #[test]
    fn map_keys_that_are_ids_do_not_enter_the_key_table() {
        let store = Arc::new(ItemStore::default());
        let _listing = store
            .store_listing(json!([
                {"ImageBlurHashes": {"Primary": {"0e6c2e92e5a833bff5e523078ba3aa56": "x"}}},
                {"Trickplay": {"abc123": {"320": {}}}}
            ]))
            .unwrap();
        let keys = store.keys.read().unwrap();
        assert!(keys.ids.contains_key("Primary"));
        assert!(!keys.ids.contains_key("0e6c2e92e5a833bff5e523078ba3aa56"));
        assert!(!keys.ids.contains_key("abc123"));
        assert!(!keys.ids.contains_key("320"));
    }
}
