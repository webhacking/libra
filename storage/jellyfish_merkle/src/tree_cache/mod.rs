// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! A transaction can have multiple operations on state. For example, it might update values
//! for a few existing keys. Imagine that we have the following tree.
//!
//! ```text
//!                 root0
//!                 /    \
//!                /      \
//!  key1 => value11        key2 => value21
//! ```
//!
//! The next transaction updates `key1`'s value to `value12` and `key2`'s value to `value22`.
//! Let's assume we update key2 first. Then the tree becomes:
//!
//! ```text
//!                   (on disk)              (in memory)
//!                     root0                  root1'
//!                    /     \                /     \
//!                   /   ___ \ _____________/       \
//!                  /  _/     \                      \
//!                 / _/        \                      \
//!                / /           \                      \
//!   key1 => value11           key2 => value21       key2 => value22
//!      (on disk)                 (on disk)            (in memory)
//! ```
//!
//! Note that
//!   1) we created a new version of the tree with `root1'` and the new `key2` node generated;
//!   2) both `root1'` and the new `key2` node are still held in memory within a batch of nodes
//!      that will be written into db atomically.
//!
//! Next, we need to update `key1`'s value. This time we are dealing with the tree starting from
//! the new root. Part of the tree is in memory and the rest of it is in database. We'll update the
//! left child and the new root. We should
//!   1) create a new version for `key1` child.
//!   2) update `root1'` directly instead of making another version.
//! The resulting tree should look like:
//!
//! ```text
//!                   (on disk)                                     (in memory)
//!                     root0                                         root1''
//!                    /     \                                       /     \
//!                   /       \                                     /       \
//!                  /         \                                   /         \
//!                 /           \                                 /           \
//!                /             \                               /             \
//!   key1 => value11             key2 => value21  key1 => value12              key2 => value22
//!      (on disk)                   (on disk)       (in memory)                  (in memory)
//! ```
//!
//! This means that we need to be able to tell whether to create a new version of a node or to
//! update an existing node by deleting it and creating a new node directly. `TreeCache` provides
//! APIs to cache intermediate nodes and blobs in memory and simplify the actual tree
//! implementation.
//!
//! If we are dealing with a single-version tree, any complex tree operation can be seen as a
//! collection of the following operations:
//!   - Put a new node.
//!   - Delete a node.
//! When we apply these operations on a multi-version tree:
//!   1) Put a new node.
//!   2) When we remove a node, if the node is in the previous on-disk version, we don't need to do
//!      anything. Otherwise we delete it from the tree cache.
//! Updating node could be operated as deletion of the node followed by insertion of the updated
//! node.

#[cfg(test)]
mod tree_cache_test;

use crate::{
    node_type::{Node, NodeKey},
    StaleNodeIndex, TreeReader, TreeUpdateBatch,
};
use crypto::{hash::SPARSE_MERKLE_PLACEHOLDER_HASH, HashValue};
use failure::prelude::*;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    convert::Into,
};
use types::transaction::Version;

/// `FrozenTreeCache` is used as a field of `TreeCache` storing all the nodes and blobs that are
/// are generated by earlier transactions so they have to be immutable. The motivation of
/// `FrozenTreeCache` is to let `TreeCache` freeze intermediate results from each transaction to
/// help commit more than one transaction in a row atomically.
#[derive(Default)]
struct FrozenTreeCache {
    /// Immutable node_cache.
    node_cache: HashMap<NodeKey, Node>,

    /// Immutable stale_node_index_cache.
    stale_node_index_cache: HashSet<StaleNodeIndex>,

    /// Frozen root hashes after each earlier transaction.
    root_hashes: Vec<HashValue>,
}

/// `TreeCache` is a in-memory cache for per-transaction updates of sparse Merkle nodes and value
/// blobs.
pub struct TreeCache<'a, R: 'a + TreeReader> {
    /// `NodeKey` of the current root node in cache.
    root_node_key: Option<NodeKey>,

    /// The version of the transaction to which the upcoming `put`s will be related.
    next_version: Version,

    /// Intermediate nodes keyed by node hash
    node_cache: HashMap<NodeKey, Node>,

    /// Partial retire log. `NodeKey` to identify the retired record.
    stale_node_index_cache: HashSet<NodeKey>,

    /// The immutable part of this cache, which will be committed to the underlying storage.
    frozen_cache: FrozenTreeCache,

    /// The underlying persistent storage.
    reader: &'a R,
}

impl<'a, R> TreeReader for TreeCache<'a, R>
where
    R: 'a + TreeReader,
{
    /// Gets a node with given hash. If it doesn't exist in node cache, read from `reader`.
    fn get_node(&self, node_key: &NodeKey) -> Result<Node> {
        Ok(if let Some(node) = self.node_cache.get(node_key) {
            node.clone()
        } else if let Some(node) = self.frozen_cache.node_cache.get(node_key) {
            node.clone()
        } else {
            self.reader.get_node(node_key)?
        })
    }
}

impl<'a, R> TreeCache<'a, R>
where
    R: 'a + TreeReader,
{
    /// Constructs a new `TreeCache` instance.
    pub fn new(reader: &'a R, next_version: Version) -> Self {
        Self {
            node_cache: HashMap::default(),
            stale_node_index_cache: HashSet::default(),
            frozen_cache: FrozenTreeCache::default(),
            root_node_key: if next_version == 0 {
                None
            } else {
                Some(NodeKey::new_empty_path(next_version - 1))
            },
            next_version,
            reader,
        }
    }

    /// Gets the current root node.
    pub fn get_root_node(&self) -> Result<Option<Node>> {
        self.root_node_key
            .as_ref()
            .map(|node_key| self.get_node(node_key))
            .transpose()
    }

    /// Set roots `node_key`.
    pub fn set_root_node_key(&mut self, root_node_key: Option<NodeKey>) {
        self.root_node_key = root_node_key;
    }

    /// Puts the node with given hash as key into node_cache.
    pub fn put_node(&mut self, node_key: NodeKey, new_node: Node) -> Result<()> {
        match self.node_cache.entry(node_key) {
            Entry::Vacant(o) => o.insert(new_node),
            Entry::Occupied(o) => bail!("Node with key {:?} already exists in NodeBatch", o.key()),
        };
        Ok(())
    }

    /// Deletes a node with given hash.
    pub fn delete_node(&mut self, old_node_key: &NodeKey) {
        // If node cache doesn't have this node, it means the node is in the previous version of
        // the tree on the disk.
        if self.node_cache.remove(&old_node_key).is_none() {
            let is_new_entry = self.stale_node_index_cache.insert(old_node_key.clone());
            assert!(is_new_entry, "Node retired twice unexpectedly.");
        }
    }

    /// Freezes all the contents in cache to be immutable and clear `node_cache`.
    pub fn freeze(&mut self) -> Result<()> {
        let root_hash = match self.get_root_node()? {
            Some(node) => node.hash(),
            None => *SPARSE_MERKLE_PLACEHOLDER_HASH,
        };
        self.frozen_cache.root_hashes.push(root_hash);
        self.frozen_cache.node_cache.extend(self.node_cache.drain());

        let stale_since_version = self.next_version;
        self.frozen_cache
            .stale_node_index_cache
            .extend(
                self.stale_node_index_cache
                    .drain()
                    .map(|node_key| StaleNodeIndex {
                        stale_since_version,
                        node_key,
                    }),
            );
        self.next_version += 1;
        Ok(())
    }
}

impl<'a, R> Into<(Vec<HashValue>, TreeUpdateBatch)> for TreeCache<'a, R>
where
    R: 'a + TreeReader,
{
    fn into(self) -> (Vec<HashValue>, TreeUpdateBatch) {
        (
            self.frozen_cache.root_hashes,
            TreeUpdateBatch {
                node_batch: self.frozen_cache.node_cache,
                stale_node_index_batch: self.frozen_cache.stale_node_index_cache,
            },
        )
    }
}