// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]
#![allow(dead_code)]
#![allow(unused_variables)]
use aptos_crypto::HashValue;
use aptos_jellyfish_merkle::{node_type::Node, Key, TreeUpdateBatch};
use aptos_storage_interface::Result;
use aptos_types::{
    nibble::nibble_path::NibblePath,
    proof::{SparseMerkleProof, SparseMerkleProofExt, SparseMerkleRangeProof},
    transaction::Version,
};
use bytes::Bytes;
use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
};
#[cfg(test)]
pub mod tests;

const MAX_ITEMS: usize = 64 * (10 << 20); // 64M items
const ITEM_SIZE: usize = 48;
const MAX_BYTES: usize = 32 * (10 << 30); // 32 GB
const MAX_BYTES_PER_ITEM: usize = 1024;

struct StateKeyHash(HashValue);

type ItemId = u32;

struct Item {
    id_num: ItemId,
    prev: ItemId,
    next: ItemId,
    value: Value,
}

enum Value {
    InMemory { bytes: Bytes },
    OnDisk { size: u16 },
}

struct ActiveStateTree<K> {
    items: HashSet<StateKeyHash, Item>,
    internal_nodes: [HashValue; MAX_ITEMS],
    latest_item: ItemId,
    oldest_item_with_in_mem_value: ItemId,
    oldest_item: ItemId,
    phantom_value: PhantomData<K>,
}

impl<K> ActiveStateTree<K>
where
    K: Key,
{
    pub fn batch_put_value_set_for_shard(
        &self,
        shard_id: u8,
        value_set: Vec<(HashValue, Option<&(HashValue, K)>)>,
        node_hashes: Option<&HashMap<NibblePath, HashValue>>,
        persisted_version: Option<Version>,
        version: Version,
    ) -> Result<(Node<K>, TreeUpdateBatch<K>)> {
        unimplemented!()
    }

    pub fn put_top_levels_nodes(
        &self,
        shard_root_nodes: Vec<Node<K>>,
        persisted_version: Option<Version>,
        version: Version,
    ) -> Result<(HashValue, TreeUpdateBatch<K>)> {
        unimplemented!()
    }

    pub fn get_with_proof(
        &self,
        key: HashValue,
        version: Version,
    ) -> Result<(Option<(HashValue, (K, Version))>, SparseMerkleProof)> {
        unimplemented!()
    }

    pub fn get_with_proof_ext(
        &self,
        key: HashValue,
        version: Version,
    ) -> Result<(Option<(HashValue, (K, Version))>, SparseMerkleProofExt)> {
        unimplemented!()
    }

    pub fn get_range_proof(
        &self,
        rightmost_key_to_prove: HashValue,
        version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        unimplemented!()
    }
}
