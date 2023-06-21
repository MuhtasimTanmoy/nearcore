use crate::flat::store_helper;
use crate::Store;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::{shard_layout::ShardUId, state::FlatStateValue};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

#[derive(Debug)]
struct TrieValueData {
    entry_type: u8,
    count: u32,
    len: usize,
}

pub fn construct_trie_from_flat(store: Store, shard_uid: ShardUId) {
    let timer = Instant::now();
    let mut hash_map: HashMap<CryptoHash, TrieValueData> = HashMap::new();

    for (i, (trie_key, value)) in
        store_helper::iter_flat_state_entries(shard_uid, &store, None, None)
            .map(|t| t.unwrap())
            .enumerate()
    {
        let (hash_key, value_len) = match value {
            FlatStateValue::Ref(ref_value) => (ref_value.hash, ref_value.length as usize),
            FlatStateValue::Inlined(inline_value) => (hash(&inline_value), inline_value.len()),
        };
        hash_map
            .entry(hash_key)
            .or_insert(TrieValueData { entry_type: trie_key[0], count: 0, len: value_len })
            .count += 1;

        if i % 1_000_000 == 0 {
            println!("{:.2?} : i {}, hash_map len {}", timer.elapsed(), i, hash_map.len());
        }
    }
    println!("{:.2?} : Completed", timer.elapsed());

    interpret_trie_value_data(hash_map);
}

#[derive(Default, Debug)]
struct ConsolidatedData {
    total_storage_bytes: usize,
    total_dup_count: u32,
    total_dup_bytes: usize,
    type_dup_count: BTreeMap<u8, u32>,
    type_dup_bytes: BTreeMap<u8, usize>,
    count_distribution: BTreeMap<u32, u32>, // map num_replications -> count
}

fn interpret_trie_value_data(hash_map: HashMap<CryptoHash, TrieValueData>) {
    let mut data = ConsolidatedData::default();
    for entry in hash_map.values() {
        let dup_count = entry.count - 1;
        let dup_bytes = dup_count as usize * entry.len;
        data.total_storage_bytes += entry.len;
        data.total_dup_count += dup_count;
        data.total_dup_bytes += dup_bytes;
        *data.type_dup_count.entry(entry.entry_type).or_default() += dup_count;
        *data.type_dup_bytes.entry(entry.entry_type).or_default() += dup_bytes;
        *data.count_distribution.entry(entry.count).or_default() += 1;
    }

    println!("{:#?}", data);
    println!(
        "current leaf storage: {:.2} GB",
        data.total_storage_bytes as f32 / (1024.0 * 1024.0 * 1024.0)
    );
    println!(
        "non deduped leaf storage: {:.2} GB",
        (data.total_storage_bytes + data.total_dup_bytes) as f32 / (1024.0 * 1024.0 * 1024.0)
    );
    println!(
        "% increase: {:.2}%",
        100.0 * data.total_dup_bytes as f32 / data.total_storage_bytes as f32
    );
}
