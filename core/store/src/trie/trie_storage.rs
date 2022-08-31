use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use near_metrics::prometheus;
use near_metrics::prometheus::core::{GenericCounter, GenericGauge};
use near_primitives::hash::CryptoHash;
use near_primitives::trie_key::TrieKey;
use tracing::error;

use crate::db::refcount::decode_value_with_rc;
use crate::trie::POISONED_LOCK_ERR;
use crate::{metrics, DBCol, StorageError, Store};
use lru::LruCache;
use near_o11y::log_assert;
use near_primitives::shard_layout::ShardUId;
use near_primitives::types::{TrieCacheMode, TrieNodesCount};
use std::cell::{Cell, RefCell};
use std::io::ErrorKind;

pub(crate) struct BoundedQueue<T> {
    queue: VecDeque<T>,
    /// If queue size exceeds capacity, item from the tail is removed.
    capacity: usize,
}

impl<T> BoundedQueue<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        // Reserve space for one extra element to simplify `put`.
        Self { queue: VecDeque::with_capacity(capacity + 1), capacity }
    }

    pub(crate) fn clear(&mut self) {
        self.queue.clear();
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    pub(crate) fn put(&mut self, key: T) -> Option<T> {
        self.queue.push_back(key);
        if self.queue.len() > self.capacity {
            Some(self.pop().expect("Queue cannot be empty"))
        } else {
            None
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }
}

/// In-memory cache for trie items - nodes and values. All nodes are stored in the LRU cache with three modifications.
/// 1) Size of each value must not exceed `TRIE_LIMIT_CACHED_VALUE_SIZE`.
/// Needed to avoid caching large values like contract codes.
/// 2) If we put new value to LRU cache and total size of existing values exceeds `total_sizes_capacity`, we evict
/// values from it until that is no longer the case. So the actual total size should never exceed
/// `total_size_limit` + `TRIE_LIMIT_CACHED_VALUE_SIZE`.
/// Needed because value sizes generally vary from 1 B to 500 B and we want to count cache size precisely.
/// 3) If value is popped, it is put to the `deletions` queue with `deletions_queue_capacity` first. If popped value
/// doesn't fit in the queue, the last value is removed from the queue and LRU cache, and newly popped value is inserted
/// to the queue.
/// Needed to delay deletions when we have forks. In such case, many blocks can share same parent, and we want to keep
/// old nodes in cache for a while to process all new roots. For example, it helps to read old state root.
pub struct TrieCacheInner {
    /// LRU cache keeping mapping from keys to values.
    cache: LruCache<CryptoHash, Arc<[u8]>>,
    /// Queue of items which were popped, which postpones deletion of old nodes.
    deletions: BoundedQueue<CryptoHash>,
    /// Current total size of all values in the cache.
    total_size: u64,
    /// Upper bound for the total size.
    total_size_limit: u64,
    /// Shard id of the nodes being cached.
    shard_id: u32,
    /// Whether cache is used for view calls execution.
    is_view: bool,
    // Counters tracking operations happening inside the shard cache.
    // Stored here to avoid overhead of looking them up on hot paths.
    metrics: TrieCacheMetrics,
}

struct TrieCacheMetrics {
    shard_cache_too_large: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_pop_hits: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_pop_misses: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_pop_lru: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_gc_pop_misses: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_deletions_size: GenericGauge<prometheus::core::AtomicI64>,
}

impl TrieCacheInner {
    pub(crate) fn new(
        cache_capacity: usize,
        deletions_queue_capacity: usize,
        total_size_limit: u64,
        shard_id: u32,
        is_view: bool,
    ) -> Self {
        assert!(cache_capacity > 0 && total_size_limit > 0);
        let metrics_labels: [&str; 2] = [&format!("{}", shard_id), &format!("{}", is_view as u8)];
        let metrics = TrieCacheMetrics {
            shard_cache_too_large: metrics::SHARD_CACHE_TOO_LARGE
                .with_label_values(&metrics_labels),
            shard_cache_pop_hits: metrics::SHARD_CACHE_POP_HITS.with_label_values(&metrics_labels),
            shard_cache_pop_misses: metrics::SHARD_CACHE_POP_MISSES
                .with_label_values(&metrics_labels),
            shard_cache_pop_lru: metrics::SHARD_CACHE_POP_LRU.with_label_values(&metrics_labels),
            shard_cache_gc_pop_misses: metrics::SHARD_CACHE_GC_POP_MISSES
                .with_label_values(&metrics_labels),
            shard_cache_deletions_size: metrics::SHARD_CACHE_DELETIONS_SIZE
                .with_label_values(&metrics_labels),
        };
        Self {
            cache: LruCache::new(cache_capacity),
            deletions: BoundedQueue::new(deletions_queue_capacity),
            total_size: 0,
            total_size_limit,
            shard_id,
            is_view,
            metrics,
        }
    }

    pub(crate) fn get(&mut self, key: &CryptoHash) -> Option<Arc<[u8]>> {
        self.cache.get(key).cloned()
    }

    pub(crate) fn clear(&mut self) {
        self.total_size = 0;
        self.deletions.clear();
        self.cache.clear();
    }

    pub(crate) fn put(&mut self, key: CryptoHash, value: Arc<[u8]>) {
        while self.total_size > self.total_size_limit || self.cache.len() == self.cache.cap() {
            // First, try to evict value using the key from deletions queue.
            match self.deletions.pop() {
                Some(key) => match self.cache.pop(&key) {
                    Some(value) => {
                        self.metrics.shard_cache_pop_hits.inc();
                        self.total_size -= value.len() as u64;
                        continue;
                    }
                    None => {
                        self.metrics.shard_cache_pop_misses.inc();
                    }
                },
                None => {}
            }

            // Second, pop LRU value.
            self.metrics.shard_cache_pop_lru.inc();
            let (_, value) =
                self.cache.pop_lru().expect("Cannot fail because total size capacity is > 0");
            self.total_size -= value.len() as u64;
        }

        // Add value to the cache.
        self.total_size += value.len() as u64;
        match self.cache.push(key, value) {
            Some((evicted_key, evicted_value)) => {
                log_assert!(key == evicted_key, "LRU cache with shard_id = {}, is_view = {} can't be full before inserting key {}", self.shard_id, self.is_view, key);
                self.total_size -= evicted_value.len() as u64;
            }
            None => {}
        };
    }

    // Adds key to the deletions queue if it is present in cache.
    // Returns key-value pair which are popped if deletions queue is full.
    pub(crate) fn pop(&mut self, key: &CryptoHash) -> Option<(CryptoHash, Arc<[u8]>)> {
        self.metrics.shard_cache_deletions_size.set(self.deletions.len() as i64);
        // Do nothing if key was removed before.
        if self.cache.contains(key) {
            // Put key to the queue of deletions and possibly remove another key we have to delete.
            match self.deletions.put(key.clone()) {
                Some(key_to_delete) => match self.cache.pop(&key_to_delete) {
                    Some(evicted_value) => {
                        self.metrics.shard_cache_pop_hits.inc();
                        self.total_size -= evicted_value.len() as u64;
                        Some((key_to_delete, evicted_value))
                    }
                    None => {
                        self.metrics.shard_cache_pop_misses.inc();
                        None
                    }
                },
                None => None,
            }
        } else {
            self.metrics.shard_cache_gc_pop_misses.inc();
            None
        }
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn current_total_size(&self) -> u64 {
        self.total_size
    }
}

/// Wrapper over LruCache to handle concurrent access.
#[derive(Clone)]
pub struct TrieCache(Arc<Mutex<TrieCacheInner>>);

impl TrieCache {
    pub fn new(shard_id: u32, is_view: bool) -> Self {
        Self::with_capacities(TRIE_DEFAULT_SHARD_CACHE_SIZE, shard_id, is_view)
    }

    pub fn with_capacities(cap: usize, shard_id: u32, is_view: bool) -> Self {
        Self(Arc::new(Mutex::new(TrieCacheInner::new(
            cap,
            DEFAULT_SHARD_CACHE_DELETIONS_QUEUE_CAPACITY,
            DEFAULT_SHARD_CACHE_TOTAL_SIZE_LIMIT,
            shard_id,
            is_view,
        ))))
    }

    pub fn get(&self, key: &CryptoHash) -> Option<Arc<[u8]>> {
        self.0.lock().expect(POISONED_LOCK_ERR).get(key)
    }

    pub fn clear(&self) {
        self.0.lock().expect(POISONED_LOCK_ERR).clear()
    }

    pub fn update_cache(&self, ops: Vec<(CryptoHash, Option<&Vec<u8>>)>) {
        let mut guard = self.0.lock().expect(POISONED_LOCK_ERR);
        for (hash, opt_value_rc) in ops {
            if let Some(value_rc) = opt_value_rc {
                if let (Some(value), _rc) = decode_value_with_rc(&value_rc) {
                    if value.len() < TRIE_LIMIT_CACHED_VALUE_SIZE {
                        guard.put(hash, value.into());
                    } else {
                        guard.metrics.shard_cache_too_large.inc();
                    }
                } else {
                    guard.pop(&hash);
                }
            } else {
                guard.pop(&hash);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        let guard = self.0.lock().expect(POISONED_LOCK_ERR);
        guard.len()
    }
}

pub trait TrieStorage {
    /// Get bytes of a serialized TrieNode.
    /// # Errors
    /// StorageError if the storage fails internally or the hash is not present.
    fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError>;

    fn as_caching_storage(&self) -> Option<&TrieCachingStorage> {
        None
    }

    fn as_recording_storage(&self) -> Option<&TrieRecordingStorage> {
        None
    }

    fn as_partial_storage(&self) -> Option<&TrieMemoryPartialStorage> {
        None
    }

    fn get_trie_nodes_count(&self) -> TrieNodesCount;
}

/// Records every value read by retrieve_raw_bytes.
/// Used for obtaining state parts (and challenges in the future).
/// TODO (#6316): implement proper nodes counting logic as in TrieCachingStorage
pub struct TrieRecordingStorage {
    pub(crate) store: Store,
    pub(crate) shard_uid: ShardUId,
    pub(crate) recorded: RefCell<HashMap<CryptoHash, Arc<[u8]>>>,
}

impl TrieStorage for TrieRecordingStorage {
    fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        if let Some(val) = self.recorded.borrow().get(hash).cloned() {
            return Ok(val);
        }
        let key = TrieCachingStorage::get_key_from_shard_uid_and_hash(self.shard_uid, hash);
        let val = self
            .store
            .get(DBCol::State, key.as_ref())
            .map_err(|_| StorageError::StorageInternalError)?;
        if let Some(val) = val {
            let val = Arc::from(val);
            self.recorded.borrow_mut().insert(*hash, Arc::clone(&val));
            Ok(val)
        } else {
            Err(StorageError::StorageInconsistentState("Trie node missing".to_string()))
        }
    }

    fn as_recording_storage(&self) -> Option<&TrieRecordingStorage> {
        Some(self)
    }

    fn get_trie_nodes_count(&self) -> TrieNodesCount {
        unimplemented!();
    }
}

/// Storage for validating recorded partial storage.
/// visited_nodes are to validate that partial storage doesn't contain unnecessary nodes.
pub struct TrieMemoryPartialStorage {
    pub(crate) recorded_storage: HashMap<CryptoHash, Arc<[u8]>>,
    pub(crate) visited_nodes: RefCell<HashSet<CryptoHash>>,
}

impl TrieStorage for TrieMemoryPartialStorage {
    fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        let result = self.recorded_storage.get(hash).cloned().ok_or(StorageError::TrieNodeMissing);
        if result.is_ok() {
            self.visited_nodes.borrow_mut().insert(*hash);
        }
        result
    }

    fn as_partial_storage(&self) -> Option<&TrieMemoryPartialStorage> {
        Some(self)
    }

    fn get_trie_nodes_count(&self) -> TrieNodesCount {
        unimplemented!();
    }
}

/// Default number of cache entries.
/// It was chosen to fit into RAM well. RAM spend on trie cache should not exceed 50_000 * 4 (number of shards) *
/// TRIE_LIMIT_CACHED_VALUE_SIZE * 2 (number of caches - for regular and view client) = 0.4 GB.
/// In our tests on a single shard, it barely occupied 40 MB, which is dominated by state cache size
/// with 512 MB limit. The total RAM usage for a single shard was 1 GB.
#[cfg(not(feature = "no_cache"))]
const TRIE_DEFAULT_SHARD_CACHE_SIZE: usize = 50000;
#[cfg(feature = "no_cache")]
const TRIE_DEFAULT_SHARD_CACHE_SIZE: usize = 1;

/// Default total size of values which may simultaneously exist the cache.
/// It is chosen by the estimation of the largest contract storage size we are aware as of 23/08/2022.
#[cfg(not(feature = "no_cache"))]
const DEFAULT_SHARD_CACHE_TOTAL_SIZE_LIMIT: u64 = 3_000_000_000;
#[cfg(feature = "no_cache")]
const DEFAULT_SHARD_CACHE_TOTAL_SIZE_LIMIT: u64 = 1;

/// Capacity for the deletions queue.
/// It is chosen to fit all hashes of deleted nodes for 3 completely full blocks.
#[cfg(not(feature = "no_cache"))]
const DEFAULT_SHARD_CACHE_DELETIONS_QUEUE_CAPACITY: usize = 100_000;
#[cfg(feature = "no_cache")]
const DEFAULT_SHARD_CACHE_DELETIONS_QUEUE_CAPACITY: usize = 1;

/// Values above this size (in bytes) are never cached.
/// Note that most of Trie inner nodes are smaller than this - e.g. branches use around 32 * 16 = 512 bytes.
pub(crate) const TRIE_LIMIT_CACHED_VALUE_SIZE: usize = 1000;

pub struct TrieCachingStorage {
    pub(crate) store: Store,
    pub(crate) shard_uid: ShardUId,

    /// Caches ever requested items for the shard `shard_uid`. Used to speed up DB operations, presence of any item is
    /// not guaranteed.
    pub(crate) shard_cache: TrieCache,
    /// Caches all items requested in the mode `TrieCacheMode::CachingChunk`. It is created in
    /// `apply_transactions_with_optional_storage_proof` by calling `get_trie_for_shard`. Before we start to apply
    /// txs and receipts in the chunk, it must be empty, and all items placed here must remain until applying
    /// txs/receipts ends. Then cache is removed automatically in `apply_transactions_with_optional_storage_proof` when
    /// `TrieCachingStorage` is removed.
    /// Note that for both caches key is the hash of value, so for the fixed key the value is unique.
    pub(crate) chunk_cache: RefCell<HashMap<CryptoHash, Arc<[u8]>>>,
    pub(crate) cache_mode: Cell<TrieCacheMode>,

    /// Prefetching IO threads will insert fetched data here. This is also used
    /// to mark what is already being fetched, to avoid fetching the same data
    /// multiple times.
    prefetching: Arc<Mutex<PrefetchStagingArea>>,
    /// Set to true to stop all io threads.
    io_kill_switch: Arc<AtomicBool>,

    /// Counts potentially expensive trie node reads which are served from disk in the worst case. Here we count reads
    /// from DB or shard cache.
    pub(crate) db_read_nodes: Cell<u64>,
    /// Counts trie nodes retrieved from the chunk cache.
    pub(crate) mem_read_nodes: Cell<u64>,
    // Counters tracking operations happening inside the shard cache.
    // Stored here to avoid overhead of looking them up on hot paths.
    metrics: TrieCacheInnerMetrics,
}

struct TrieCacheInnerMetrics {
    chunk_cache_hits: GenericCounter<prometheus::core::AtomicU64>,
    chunk_cache_misses: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_hits: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_misses: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_too_large: GenericCounter<prometheus::core::AtomicU64>,
    shard_cache_size: GenericGauge<prometheus::core::AtomicI64>,
    chunk_cache_size: GenericGauge<prometheus::core::AtomicI64>,
    shard_cache_current_total_size: GenericGauge<prometheus::core::AtomicI64>,
}

#[derive(Clone, Debug)]
pub(crate) enum PrefetchSlot {
    PendingPrefetch,
    PendingFetch,
    Done(Arc<[u8]>),
}

pub enum IoThreadCmd {
    PrefetchTrieNode(TrieKey),
    StopSelf,
}

pub type IoRequestQueue = Arc<Mutex<VecDeque<IoThreadCmd>>>;

impl TrieCachingStorage {
    pub fn new(
        store: Store,
        shard_cache: TrieCache,
        shard_uid: ShardUId,
        is_view: bool,
    ) -> TrieCachingStorage {
        let metrics_labels: [&str; 2] =
            [&format!("{}", shard_uid.shard_id), &format!("{}", is_view as u8)];
        let metrics = TrieCacheInnerMetrics {
            chunk_cache_hits: metrics::CHUNK_CACHE_HITS.with_label_values(&metrics_labels),
            chunk_cache_misses: metrics::CHUNK_CACHE_MISSES.with_label_values(&metrics_labels),
            shard_cache_hits: metrics::SHARD_CACHE_HITS.with_label_values(&metrics_labels),
            shard_cache_misses: metrics::SHARD_CACHE_MISSES.with_label_values(&metrics_labels),
            shard_cache_too_large: metrics::SHARD_CACHE_TOO_LARGE
                .with_label_values(&metrics_labels),
            shard_cache_size: metrics::SHARD_CACHE_SIZE.with_label_values(&metrics_labels),
            chunk_cache_size: metrics::CHUNK_CACHE_SIZE.with_label_values(&metrics_labels),
            shard_cache_current_total_size: metrics::SHARD_CACHE_CURRENT_TOTAL_SIZE
                .with_label_values(&metrics_labels),
        };
        TrieCachingStorage {
            store,
            shard_uid,
            shard_cache,
            cache_mode: Cell::new(TrieCacheMode::CachingShard),
            prefetching: Default::default(),
            chunk_cache: RefCell::new(Default::default()),
            db_read_nodes: Cell::new(0),
            mem_read_nodes: Cell::new(0),
            metrics,
            io_kill_switch: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn get_shard_uid_and_hash_from_key(
        key: &[u8],
    ) -> Result<(ShardUId, CryptoHash), std::io::Error> {
        if key.len() != 40 {
            return Err(std::io::Error::new(ErrorKind::Other, "Key is always shard_uid + hash"));
        }
        let id = ShardUId::try_from(&key[..8]).unwrap();
        let hash = CryptoHash::try_from(&key[8..]).unwrap();
        Ok((id, hash))
    }

    pub(crate) fn get_key_from_shard_uid_and_hash(
        shard_uid: ShardUId,
        hash: &CryptoHash,
    ) -> [u8; 40] {
        let mut key = [0; 40];
        key[0..8].copy_from_slice(&shard_uid.to_bytes());
        key[8..].copy_from_slice(hash.as_ref());
        key
    }

    fn inc_db_read_nodes(&self) {
        self.db_read_nodes.set(self.db_read_nodes.get() + 1);
    }

    fn inc_mem_read_nodes(&self) {
        self.mem_read_nodes.set(self.mem_read_nodes.get() + 1);
    }

    /// Set cache mode.
    pub fn set_mode(&self, state: TrieCacheMode) {
        self.cache_mode.set(state);
    }

    pub fn prefetcher_storage(&self) -> TriePrefetchingStorage {
        TriePrefetchingStorage::new(
            self.store.clone(),
            self.shard_uid.clone(),
            self.shard_cache.clone(),
            self.prefetching.clone(),
        )
    }

    pub fn io_kill_switch(&self) -> &Arc<AtomicBool> {
        &self.io_kill_switch
    }

    pub fn start_io_thread(
        root: CryptoHash,
        prefetcher_storage: Box<dyn TrieStorage + Send>,
        request_queue: IoRequestQueue,
        kill_switch: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            // `Trie` cannot be sent across threads but `TriePrefetchingStorage` can.
            //  Therefore, construct `Trie` in the new thread.
            let prefetcher_trie = crate::Trie::new(prefetcher_storage, root, None);

            // Keep looping until either the kill switch signals that prefetching is no
            // longer needed or a `StopSelf` command indicates that no more prefetch
            // requests are available.
            while !kill_switch.load(Ordering::Acquire) {
                let maybe_request = request_queue.lock().expect(POISONED_LOCK_ERR).pop_front();
                match maybe_request {
                    Some(IoThreadCmd::PrefetchTrieNode(trie_key)) => {
                        let storage_key = trie_key.to_vec();
                        if let Ok(Some(_value)) = prefetcher_trie.get(&storage_key) {
                            near_o11y::io_trace!(count: "prefetch_success");
                        } else {
                            // This may happen in rare occasions and can be ignored safely.
                            near_o11y::io_trace!(count: "prefetch_failure");
                        }
                    }
                    Some(IoThreadCmd::StopSelf) => return,
                    None => std::thread::sleep(Duration::from_micros(10)),
                }
            }
        })
    }

    pub fn stop_prefetcher(&self) {
        self.io_kill_switch.store(true, Ordering::Release);
        // For as long as the assertions are still in place, do not "clear" old
        // value. Some I/O threads might be in the middle of fetching data. They
        // expect certain states to be present.
        // self.prefetching.lock().expect(POISONED_LOCK_ERR).clear();
    }
}

impl TrieStorage for TrieCachingStorage {
    fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        self.metrics.chunk_cache_size.set(self.chunk_cache.borrow().len() as i64);
        // Try to get value from chunk cache containing nodes with cheaper access. We can do it for any `TrieCacheMode`,
        // because we charge for reading nodes only when `CachingChunk` mode is enabled anyway.
        if let Some(val) = self.chunk_cache.borrow_mut().get(hash) {
            self.metrics.chunk_cache_hits.inc();
            self.inc_mem_read_nodes();
            return Ok(val.clone());
        }
        self.metrics.chunk_cache_misses.inc();

        // Try to get value from shard cache containing most recently touched nodes.
        let mut guard = self.shard_cache.0.lock().expect(POISONED_LOCK_ERR);
        self.metrics.shard_cache_size.set(guard.len() as i64);
        self.metrics.shard_cache_current_total_size.set(guard.current_total_size() as i64);
        let val = match guard.get(hash) {
            Some(val) => {
                self.metrics.shard_cache_hits.inc();
                near_o11y::io_trace!(count: "shard_cache_hit");
                val.clone()
            }
            None => {
                self.metrics.shard_cache_misses.inc();
                near_o11y::io_trace!(count: "shard_cache_miss");
                // If data is already being prefetched, wait for that instead of sending a new request.
                let prefetch_state = PrefetchStagingArea::get_and_set_if_empty(
                    &self.prefetching,
                    hash.clone(),
                    PrefetchSlot::PendingFetch,
                );
                // Keep lock until here to avoid race condition between shard cache lookup and reserving prefetch slot.
                std::mem::drop(guard);

                let val = match prefetch_state {
                    PrefetcherResult::SlotReserved | PrefetcherResult::MemoryLimitReached => {
                        self.read_from_db(hash)?
                    }
                    PrefetcherResult::Prefetched(value) => value,
                    PrefetcherResult::Pending => {
                        std::thread::sleep(Duration::from_micros(1));
                        // Unwrap: Only main thread  (this one) removes values from staging area,
                        // therefore blocking read will not return empty.
                        PrefetchStagingArea::blocking_get(&self.prefetching, hash.clone()).unwrap()
                    }
                };

                // Insert value to shard cache, if its size is small enough.
                // It is fine to have a size limit for shard cache and **not** have a limit for chunk cache, because key
                // is always a value hash, so for each key there could be only one value, and it is impossible to have
                // **different** values for the given key in shard and chunk caches.
                if val.len() < TRIE_LIMIT_CACHED_VALUE_SIZE {
                    let mut guard = self.shard_cache.0.lock().expect(POISONED_LOCK_ERR);
                    guard.put(*hash, val.clone());
                } else {
                    self.metrics.shard_cache_too_large.inc();
                    near_o11y::io_trace!(count: "shard_cache_too_large");
                }

                // Only release after insertion in shard cache. See comment on fn release.
                self.prefetching.lock().expect(POISONED_LOCK_ERR).release(hash);

                val
            }
        };

        // Because node is not present in chunk cache, increment the nodes counter and optionally insert it into the
        // chunk cache.
        // Note that we don't have a size limit for values in the chunk cache. There are two reasons:
        // - for nodes, value size is an implementation detail. If we change internal representation of a node (e.g.
        // change `memory_usage` field from `RawTrieNodeWithSize`), this would have to be a protocol upgrade.
        // - total size of all values is limited by the runtime fees. More thoroughly:
        // - - number of nodes is limited by receipt gas limit / touching trie node fee ~= 500 Tgas / 16 Ggas = 31_250;
        // - - size of trie keys and values is limited by receipt gas limit / lowest per byte fee
        // (`storage_read_value_byte`) ~= (500 * 10**12 / 5611005) / 2**20 ~= 85 MB.
        // All values are given as of 16/03/2022. We may consider more precise limit for the chunk cache as well.
        self.inc_db_read_nodes();
        if let TrieCacheMode::CachingChunk = self.cache_mode.borrow().get() {
            self.chunk_cache.borrow_mut().insert(*hash, val.clone());
        };

        Ok(val)
    }

    fn as_caching_storage(&self) -> Option<&TrieCachingStorage> {
        Some(self)
    }

    fn get_trie_nodes_count(&self) -> TrieNodesCount {
        TrieNodesCount { db_reads: self.db_read_nodes.get(), mem_reads: self.mem_read_nodes.get() }
    }
}

impl TrieCachingStorage {
    fn read_from_db(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        let key = Self::get_key_from_shard_uid_and_hash(self.shard_uid, hash);
        let val = self
            .store
            .get(DBCol::State, key.as_ref())
            .map_err(|_| StorageError::StorageInternalError)?
            .ok_or_else(|| {
                StorageError::StorageInconsistentState("Trie node missing".to_string())
            })?;
        Ok(val.into())
    }
}

/// Storage used by I/O threads to prefetch data.
///
/// Is always linked to a parent `TrieCachingStorage`. One  caching storage can
/// produce many I/O threads and each will have its own prefetching storage.
/// However, the underlying shard cache and prefetching map is shared among all
/// instances, including the parent.
#[derive(Clone)]
pub struct TriePrefetchingStorage {
    /// Store is shared with parent `TrieCachingStorage`.
    store: Store,
    shard_uid: ShardUId,
    /// Shard cache is shared with parent `TrieCachingStorage`. But the
    /// pre-fetcher uses this in read-only mode to avoid premature evictions.
    shard_cache: TrieCache,
    /// Shared with parent `TrieCachingStorage`.
    prefetching: Arc<Mutex<PrefetchStagingArea>>,
}

/// Staging area for in-flight prefetch requests and a buffer for prefetched data,
///
/// Before starting a pre-fetch, a slot is reserved for it. Once the data is
/// here, it will be put in that slot. The parent `TrieCachingStorage` needs
/// to take it out and move it to the shard cache.
///
/// This design is to avoid prematurely filling up the shard cache.
#[derive(Default)]
struct PrefetchStagingArea {
    slots: HashMap<CryptoHash, PrefetchSlot>,
    size_bytes: usize,
}

const MAX_PREFETCH_STAGING_MEMORY: usize = 200 * 1024 * 1024;
/// How much memory capacity is reserved for each prefetch request.
/// Set to 4MiB, the same as `max_length_storage_value`.
const PREFETCH_RESERVED_BYTES_PER_SLOT: usize = 4 * 1024 * 1024;

impl TriePrefetchingStorage {
    fn new(
        store: Store,
        shard_uid: ShardUId,
        shard_cache: TrieCache,
        prefetching: Arc<Mutex<PrefetchStagingArea>>,
    ) -> Self {
        Self { store, shard_uid, shard_cache, prefetching }
    }
}

enum PrefetcherResult {
    SlotReserved,
    Pending,
    Prefetched(Arc<[u8]>),
    MemoryLimitReached,
}

impl PrefetchStagingArea {
    fn insert_fetched(&mut self, key: CryptoHash, value: Arc<[u8]>) {
        self.size_bytes -= PREFETCH_RESERVED_BYTES_PER_SLOT;
        self.size_bytes += value.len();
        let pending = self.slots.insert(key, PrefetchSlot::Done(value));
        assert!(prefetch_state_matches(PrefetchSlot::PendingPrefetch, &pending.unwrap()));
    }

    /// Release a slot in the prefetcher staging area.
    ///
    /// This must only be called after inserting the value to the shard cache.
    /// Otherwise, the following scenario becomes possible:
    /// 1: Main thread removes a value from the prefetch staging area
    /// 2: IO thread misses in the shard cache on the same key and starts fetching it again.
    /// 3: Main thread value is inserted in shard cache.
    fn release(&mut self, key: &CryptoHash) {
        let dropped = self.slots.remove(key);
        // `Done` is the result after a successful prefetch.
        // `PendingFetch` means the value has been read without a prefetch.
        // `None` means prefetching was stopped due to memory limits.
        debug_assert!(
            dropped.is_none()
                || prefetch_state_matches(
                    PrefetchSlot::Done(Arc::new([])),
                    dropped.as_ref().unwrap()
                )
                || prefetch_state_matches(PrefetchSlot::PendingFetch, dropped.as_ref().unwrap()),
        );
        match dropped {
            Some(PrefetchSlot::Done(value)) => self.size_bytes -= value.len(),
            Some(PrefetchSlot::PendingFetch) => self.size_bytes -= PREFETCH_RESERVED_BYTES_PER_SLOT,
            None => { /* NOP */ }
            _ => {
                error!(target: "prefetcher", "prefetcher bug detected, trying to release {dropped:?}");
            }
        }
    }

    /// Block until value is prefetched and then return it.
    fn blocking_get(this: &Arc<Mutex<Self>>, key: CryptoHash) -> Option<Arc<[u8]>> {
        loop {
            match this.lock().expect(POISONED_LOCK_ERR).slots.get(&key) {
                Some(PrefetchSlot::Done(value)) => return Some(value.clone()),
                Some(_) => { /* NOP */ }
                None => return None,
            }
            std::thread::sleep(std::time::Duration::from_micros(1));
        }
    }

    /// Get prefetched value if available and otherwise atomically insert the
    /// given `PrefetchSlot` if no request is pending yet.
    fn get_and_set_if_empty(
        this: &Arc<Mutex<Self>>,
        key: CryptoHash,
        set_if_empty: PrefetchSlot,
    ) -> PrefetcherResult {
        let mut guard = this.lock().expect(POISONED_LOCK_ERR);
        let full = match guard.size_bytes.checked_add(PREFETCH_RESERVED_BYTES_PER_SLOT) {
            Some(size_after) => size_after > MAX_PREFETCH_STAGING_MEMORY,
            None => true,
        };
        if full {
            return PrefetcherResult::MemoryLimitReached;
        }
        match guard.slots.entry(key) {
            Entry::Occupied(entry) => match entry.get() {
                PrefetchSlot::Done(value) => {
                    near_o11y::io_trace!(count: "prefetch_hit");
                    PrefetcherResult::Prefetched(value.clone())
                }
                PrefetchSlot::PendingPrefetch | PrefetchSlot::PendingFetch => {
                    std::mem::drop(guard);
                    near_o11y::io_trace!(count: "prefetch_pending");
                    PrefetcherResult::Pending
                }
            },
            Entry::Vacant(entry) => {
                entry.insert(set_if_empty);
                guard.size_bytes += PREFETCH_RESERVED_BYTES_PER_SLOT;
                PrefetcherResult::SlotReserved
            }
        }
    }
}

impl TrieStorage for TriePrefetchingStorage {
    fn retrieve_raw_bytes(&self, hash: &CryptoHash) -> Result<Arc<[u8]>, StorageError> {
        // Try to get value from shard cache containing most recently touched nodes.
        let mut shard_cache_guard = self.shard_cache.0.lock().expect(POISONED_LOCK_ERR);
        let maybe_val = { shard_cache_guard.get(hash) };

        match maybe_val {
            Some(val) => Ok(val),
            None => {
                // If data is already being prefetched, wait for that instead of sending a new request.
                let prefetch_state = PrefetchStagingArea::get_and_set_if_empty(
                    &self.prefetching,
                    hash.clone(),
                    PrefetchSlot::PendingPrefetch,
                );
                // Keep lock until here to avoid race condition between shard cache insertion and reserving prefetch slot.
                std::mem::drop(shard_cache_guard);

                match prefetch_state {
                    PrefetcherResult::SlotReserved => {
                        let key = TrieCachingStorage::get_key_from_shard_uid_and_hash(
                            self.shard_uid,
                            hash,
                        );
                        let value: Arc<[u8]> = self
                            .store
                            .get(DBCol::State, key.as_ref())
                            .map_err(|_| StorageError::StorageInternalError)?
                            .ok_or_else(|| {
                                StorageError::StorageInconsistentState(
                                    "Trie node missing".to_string(),
                                )
                            })?
                            .into();

                        self.prefetching
                            .lock()
                            .expect(POISONED_LOCK_ERR)
                            .insert_fetched(hash.clone(), value.clone());
                        Ok(value)
                    }
                    PrefetcherResult::Prefetched(value) => Ok(value),
                    PrefetcherResult::Pending => {
                        std::thread::sleep(Duration::from_micros(1));
                        // Unwrap: Only main thread  (this one) removes values from staging area,
                        // therefore blocking read will not return empty.
                        PrefetchStagingArea::blocking_get(&self.prefetching, hash.clone())
                            .or_else(|| {
                                // `blocking_get` will return None if the prefetch slot has been removed
                                // by the main thread and the value inserted into the shard cache.
                                let mut guard = self.shard_cache.0.lock().expect(POISONED_LOCK_ERR);
                                guard.get(hash)
                            })
                            .ok_or_else(|| {
                                // This could only happen if this thread started prefetching a value
                                // while also another thread was already prefetching it. Then the other
                                // other thread finished, the main thread takes it out, and moves it to
                                // the shard cache. And then this current thread gets delayed for long
                                // enough that the value gets evicted from the shard cache again before
                                // this thread has a change to read it.
                                // In this rare occasion, we shall abort the current prefetch request and
                                // move on to the next.
                                StorageError::StorageInconsistentState(
                                    "Prefetcher failed".to_owned(),
                                )
                            })
                    }
                    PrefetcherResult::MemoryLimitReached => {
                        Err(StorageError::StorageInconsistentState("Prefetcher failed".to_owned()))
                    }
                }
            }
        }
    }

    fn get_trie_nodes_count(&self) -> TrieNodesCount {
        unimplemented!()
    }
}

fn prefetch_state_matches(expected: PrefetchSlot, actual: &PrefetchSlot) -> bool {
    match (expected, actual) {
        (PrefetchSlot::PendingPrefetch, PrefetchSlot::PendingPrefetch)
        | (PrefetchSlot::PendingFetch, PrefetchSlot::PendingFetch)
        | (PrefetchSlot::Done(_), PrefetchSlot::Done(_)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod bounded_queue_tests {
    use crate::trie::trie_storage::BoundedQueue;

    #[test]
    fn test_put_pop() {
        let mut queue = BoundedQueue::new(2);
        assert_eq!(queue.put(1), None);
        assert_eq!(queue.put(2), None);
        assert_eq!(queue.put(3), Some(1));

        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn test_clear() {
        let mut queue = BoundedQueue::new(2);
        queue.put(1);
        assert_eq!(queue.queue.get(0), Some(&1));
        queue.clear();
        assert!(queue.queue.is_empty());
    }

    #[test]
    fn test_zero_capacity() {
        let mut queue = BoundedQueue::new(0);
        assert_eq!(queue.put(1), Some(1));
        assert!(queue.queue.is_empty());
    }
}

#[cfg(test)]
mod trie_cache_tests {
    use crate::trie::trie_storage::TrieCacheInner;
    use near_primitives::hash::hash;

    fn put_value(cache: &mut TrieCacheInner, value: &[u8]) {
        cache.put(hash(value), value.into());
    }

    #[test]
    fn test_size_limit() {
        let mut cache = TrieCacheInner::new(100, 100, 5, 0, false);
        // Add three values. Before each put, condition on total size should not be triggered.
        put_value(&mut cache, &[1, 1]);
        assert_eq!(cache.total_size, 2);
        put_value(&mut cache, &[1, 1, 1]);
        assert_eq!(cache.total_size, 5);
        put_value(&mut cache, &[1]);
        assert_eq!(cache.total_size, 6);

        // Add one of previous values. LRU value should be evicted.
        put_value(&mut cache, &[1, 1, 1]);
        assert_eq!(cache.total_size, 4);
        assert_eq!(cache.cache.pop_lru(), Some((hash(&[1]), vec![1].into())));
        assert_eq!(cache.cache.pop_lru(), Some((hash(&[1, 1, 1]), vec![1, 1, 1].into())));
    }

    #[test]
    fn test_deletions_queue() {
        let mut cache = TrieCacheInner::new(100, 2, 100, 0, false);
        // Add two values to the cache.
        put_value(&mut cache, &[1]);
        put_value(&mut cache, &[1, 1]);

        // Call pop for inserted values. Because deletions queue is not full, no elements should be deleted.
        assert_eq!(cache.pop(&hash(&[1, 1])), None);
        assert_eq!(cache.pop(&hash(&[1])), None);

        // Call pop two times for a value existing in cache. Because queue is full, both elements should be deleted.
        assert_eq!(cache.pop(&hash(&[1])), Some((hash(&[1, 1]), vec![1, 1].into())));
        assert_eq!(cache.pop(&hash(&[1])), Some((hash(&[1]), vec![1].into())));
    }

    #[test]
    fn test_cache_capacity() {
        let mut cache = TrieCacheInner::new(2, 100, 100, 0, false);
        put_value(&mut cache, &[1]);
        put_value(&mut cache, &[2]);
        put_value(&mut cache, &[3]);

        assert!(!cache.cache.contains(&hash(&[1])));
        assert!(cache.cache.contains(&hash(&[2])));
        assert!(cache.cache.contains(&hash(&[3])));
    }
}
