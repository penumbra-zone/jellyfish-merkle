// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Node types of [`JellyfishMerkleTree`](crate::JellyfishMerkleTree)
//!
//! This module defines two types of Jellyfish Merkle tree nodes: [`InternalNode`]
//! and [`LeafNode`] as building blocks of a 256-bit
//! [`JellyfishMerkleTree`](crate::JellyfishMerkleTree). [`InternalNode`] represents a 4-level
//! binary tree to optimize for IOPS: it compresses a tree with 31 nodes into one node with 16
//! chidren at the lowest level. [`LeafNode`] stores the full key and the value associated.

use std::{
    convert::TryFrom,
    io::{prelude::*, Cursor, Read, SeekFrom, Write},
    mem::size_of,
};

use anyhow::{ensure, Context, Result};
use byteorder::{BigEndian, LittleEndian, ReadBytesExt, WriteBytesExt};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::cast::FromPrimitive;
#[cfg(any(test, feature = "fuzzing"))]
use proptest::prelude::*;
#[cfg(any(test, feature = "fuzzing"))]
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    metrics::{DIEM_JELLYFISH_INTERNAL_ENCODED_BYTES, DIEM_JELLYFISH_LEAF_ENCODED_BYTES},
    types::{
        nibble::{nibble_path::NibblePath, Nibble, ROOT_NIBBLE_HEIGHT},
        proof::{SparseMerkleInternalNode, SparseMerkleLeafNode},
        Version,
    },
    KeyHash, ValueHash, SPARSE_MERKLE_PLACEHOLDER_HASH,
};

/// The unique key of each node.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct NodeKey {
    // The version at which the node is created.
    version: Version,
    // The nibble path this node represents in the tree.
    nibble_path: NibblePath,
}

impl NodeKey {
    /// Creates a new `NodeKey`.
    pub(crate) fn new(version: Version, nibble_path: NibblePath) -> Self {
        Self {
            version,
            nibble_path,
        }
    }

    /// A shortcut to generate a node key consisting of a version and an empty nibble path.
    pub(crate) fn new_empty_path(version: Version) -> Self {
        Self::new(version, NibblePath::new(vec![]))
    }

    /// Gets the version.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Gets the nibble path.
    pub(crate) fn nibble_path(&self) -> &NibblePath {
        &self.nibble_path
    }

    /// Generates a child node key based on this node key.
    pub(crate) fn gen_child_node_key(&self, version: Version, n: Nibble) -> Self {
        let mut node_nibble_path = self.nibble_path().clone();
        node_nibble_path.push(n);
        Self::new(version, node_nibble_path)
    }

    /// Generates parent node key at the same version based on this node key.
    pub(crate) fn gen_parent_node_key(&self) -> Self {
        let mut node_nibble_path = self.nibble_path().clone();
        assert!(
            node_nibble_path.pop().is_some(),
            "Current node key is root.",
        );
        Self::new(self.version, node_nibble_path)
    }

    /// Sets the version to the given version.
    pub(crate) fn set_version(&mut self, version: Version) {
        self.version = version;
    }

    /// Serializes to bytes for physical storage enforcing the same order as that in memory.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = vec![];
        out.write_u64::<BigEndian>(self.version())?;
        out.write_u8(self.nibble_path().num_nibbles() as u8)?;
        out.write_all(self.nibble_path().bytes())?;
        Ok(out)
    }

    /// Recovers from serialized bytes in physical storage.
    pub fn decode(val: &[u8]) -> Result<NodeKey> {
        let mut reader = Cursor::new(val);
        let version = reader.read_u64::<BigEndian>()?;
        let num_nibbles = reader.read_u8()? as usize;
        ensure!(
            num_nibbles <= ROOT_NIBBLE_HEIGHT,
            "Invalid number of nibbles: {}",
            num_nibbles,
        );
        let mut nibble_bytes = Vec::with_capacity((num_nibbles + 1) / 2);
        reader.read_to_end(&mut nibble_bytes)?;
        ensure!(
            (num_nibbles + 1) / 2 == nibble_bytes.len(),
            "encoded num_nibbles {} mismatches nibble path bytes {:?}",
            num_nibbles,
            nibble_bytes
        );
        let nibble_path = if num_nibbles % 2 == 0 {
            NibblePath::new(nibble_bytes)
        } else {
            let padding = nibble_bytes.last().unwrap() & 0x0f;
            ensure!(
                padding == 0,
                "Padding nibble expected to be 0, got: {}",
                padding,
            );
            NibblePath::new_odd(nibble_bytes)
        };
        Ok(NodeKey::new(version, nibble_path))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeType {
    Leaf,
    /// A internal node that haven't been finished the leaf count migration, i.e. None or not all
    /// of the children leaf counts are known.
    InternalLegacy,
    Internal {
        leaf_count: usize,
    },
}

#[cfg(any(test, feature = "fuzzing"))]
impl Arbitrary for NodeType {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        prop_oneof![
            Just(NodeType::Leaf),
            Just(NodeType::InternalLegacy),
            (2..100usize).prop_map(|leaf_count| NodeType::Internal { leaf_count })
        ]
        .boxed()
    }
}

/// Each child of [`InternalNode`] encapsulates a nibble forking at this node.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct Child {
    /// The hash value of this child node.
    pub hash: [u8; 32],
    /// `version`, the `nibble_path` of the ['NodeKey`] of this [`InternalNode`] the child belongs
    /// to and the child's index constitute the [`NodeKey`] to uniquely identify this child node
    /// from the storage. Used by `[`NodeKey::gen_child_node_key`].
    pub version: Version,
    /// Indicates if the child is a leaf, or if it's an internal node, the total number of leaves
    /// under it (though it can be unknown during migration).
    pub node_type: NodeType,
}

impl Child {
    pub fn new(hash: [u8; 32], version: Version, node_type: NodeType) -> Self {
        Self {
            hash,
            version,
            node_type,
        }
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self.node_type, NodeType::Leaf)
    }

    pub fn leaf_count(&self) -> Option<usize> {
        match self.node_type {
            NodeType::Leaf => Some(1),
            NodeType::InternalLegacy => None,
            NodeType::Internal { leaf_count } => Some(leaf_count),
        }
    }
}

/// [`Children`] is just a collection of children belonging to a [`InternalNode`], indexed from 0 to
/// 15, inclusive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Children {
    /// The actual children. We box this array to avoid stack overflows, since the space consumed
    /// is somewhat large
    children: Box<[Option<Child>; 16]>,
    num_children: usize,
}

#[cfg(any(test, feature = "fuzzing"))]
impl Arbitrary for Children {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (any::<Box<[Option<Child>; 16]>>().prop_map(|children| {
            let num_children = children.iter().filter(|child| child.is_some()).count();
            Self {
                children,
                num_children,
            }
        }))
        .boxed()
    }
}

impl Children {
    /// Create an empty set of children.
    pub fn new() -> Self {
        Default::default()
    }

    /// Insert a new child. Insert is guaranteed not to allocate.
    pub fn insert(&mut self, nibble: Nibble, child: Child) {
        let idx = nibble.as_usize();
        if self.children[idx].is_none() {
            self.num_children += 1;
        }
        self.children[idx] = Some(child);
    }

    /// Get the child at the provided nibble.
    pub fn get(&self, nibble: Nibble) -> &Option<Child> {
        &self.children[nibble.as_usize()]
    }

    /// Check if the struct contains any children.
    pub fn is_empty(&self) -> bool {
        self.num_children == 0
    }

    /// Remove the child at the provided nibble.
    pub fn remove(&mut self, nibble: Nibble) {
        let idx = nibble.as_usize();
        if self.children[idx].is_some() {
            self.num_children -= 1;
        }
        self.children[idx] = None;
    }

    /// Returns a (possibly unsorted) iterator over the children.
    pub fn values(&self) -> impl Iterator<Item = &Child> {
        self.children.iter().filter_map(|child| child.as_ref())
    }

    /// Returns a (possibly unsorted) iterator over the children and their respective Nibbles.
    pub fn iter(&self) -> impl Iterator<Item = (Nibble, &Child)> {
        self.iter_sorted()
    }

    /// Returns a (possibly unsorted) mutable iterator over the children, also yielding their respective nibbles.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Nibble, &mut Child)> {
        self.children
            .iter_mut()
            .enumerate()
            .filter_map(|(nibble, child)| {
                if let Some(child) = child {
                    Some((Nibble::from(nibble as u8), child))
                } else {
                    None
                }
            })
    }

    /// Returns the number of children.
    pub fn num_children(&self) -> usize {
        self.num_children
    }

    /// Returns an iterator that yields the children and their respective Nibbles in sorted order.
    pub fn iter_sorted(&self) -> impl Iterator<Item = (Nibble, &Child)> {
        self.children
            .iter()
            .enumerate()
            .filter_map(|(nibble, child)| {
                if let Some(child) = child {
                    Some((Nibble::from(nibble as u8), child))
                } else {
                    None
                }
            })
    }
}

/// Represents a 4-level subtree with 16 children at the bottom level. Theoretically, this reduces
/// IOPS to query a tree by 4x since we compress 4 levels in a standard Merkle tree into 1 node.
/// Though we choose the same internal node structure as that of Patricia Merkle tree, the root hash
/// computation logic is similar to a 4-level sparse Merkle tree except for some customizations. See
/// the `CryptoHash` trait implementation below for details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InternalNode {
    /// Up to 16 children.
    children: Children,
    /// Total number of leaves under this internal node
    leaf_count: Option<usize>,
    /// serialize leaf counts
    leaf_count_migration: bool,
}

/// Computes the hash of internal node according to [`JellyfishTree`](crate::JellyfishTree)
/// data structure in the logical view. `start` and `nibble_height` determine a subtree whose
/// root hash we want to get. For an internal node with 16 children at the bottom level, we compute
/// the root hash of it as if a full binary Merkle tree with 16 leaves as below:
///
/// ```text
///   4 ->              +------ root hash ------+
///                     |                       |
///   3 ->        +---- # ----+           +---- # ----+
///               |           |           |           |
///   2 ->        #           #           #           #
///             /   \       /   \       /   \       /   \
///   1 ->     #     #     #     #     #     #     #     #
///           / \   / \   / \   / \   / \   / \   / \   / \
///   0 ->   0   1 2   3 4   5 6   7 8   9 A   B C   D E   F
///   ^
/// height
/// ```
///
/// As illustrated above, at nibble height 0, `0..F` in hex denote 16 chidren hashes.  Each `#`
/// means the hash of its two direct children, which will be used to generate the hash of its
/// parent with the hash of its sibling. Finally, we can get the hash of this internal node.
///
/// However, if an internal node doesn't have all 16 chidren exist at height 0 but just a few of
/// them, we have a modified hashing rule on top of what is stated above:
/// 1. From top to bottom, a node will be replaced by a leaf child if the subtree rooted at this
/// node has only one child at height 0 and it is a leaf child.
/// 2. From top to bottom, a node will be replaced by the placeholder node if the subtree rooted at
/// this node doesn't have any child at height 0. For example, if an internal node has 3 leaf
/// children at index 0, 3, 8, respectively, and 1 internal node at index C, then the computation
/// graph will be like:
///
/// ```text
///   4 ->              +------ root hash ------+
///                     |                       |
///   3 ->        +---- # ----+           +---- # ----+
///               |           |           |           |
///   2 ->        #           @           8           #
///             /   \                               /   \
///   1 ->     0     3                             #     @
///                                               / \
///   0 ->                                       C   @
///   ^
/// height
/// Note: @ denotes placeholder hash.
/// ```
#[cfg(any(test, feature = "fuzzing"))]
impl Arbitrary for InternalNode {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: ()) -> Self::Strategy {
        (any::<Children>().prop_filter(
            "InternalNode constructor panics when its only child is a leaf.",
            |children| {
                !(children.num_children() == 1
                    && children.values().next().expect("Must exist.").is_leaf())
            },
        ))
        .prop_map(InternalNode::new)
        .boxed()
    }
}

impl InternalNode {
    /// Creates a new Internal node.
    pub fn new(children: Children) -> Self {
        Self::new_migration(children, true /* leaf_count_migration */)
    }

    pub fn new_migration(children: Children, leaf_count_migration: bool) -> Self {
        Self::new_impl(children, leaf_count_migration).expect("Input children are logical.")
    }

    pub fn new_impl(children: Children, leaf_count_migration: bool) -> Result<Self> {
        // Assert the internal node must have >= 1 children. If it only has one child, it cannot be
        // a leaf node. Otherwise, the leaf node should be a child of this internal node's parent.
        ensure!(!children.is_empty(), "Children must not be empty");
        if children.num_children() == 1 {
            ensure!(
                !children
                    .values()
                    .next()
                    .expect("Must have 1 element")
                    .is_leaf(),
                "If there's only one child, it must not be a leaf."
            );
        }

        let leaf_count = Self::sum_leaf_count(&children);
        Ok(Self {
            children,
            leaf_count,
            leaf_count_migration,
        })
    }

    fn sum_leaf_count(children: &Children) -> Option<usize> {
        let mut leaf_count = 0;
        for child in children.values() {
            if let Some(n) = child.leaf_count() {
                leaf_count += n;
            } else {
                return None;
            }
        }
        Some(leaf_count)
    }

    pub fn leaf_count(&self) -> Option<usize> {
        self.leaf_count
    }

    pub fn node_type(&self) -> NodeType {
        match self.leaf_count {
            Some(leaf_count) => NodeType::Internal { leaf_count },
            None => NodeType::InternalLegacy,
        }
    }

    pub fn hash(&self) -> [u8; 32] {
        self.merkle_hash(
            0,  /* start index */
            16, /* the number of leaves in the subtree of which we want the hash of root */
            self.generate_bitmaps(),
        )
    }

    pub fn children_sorted(&self) -> impl Iterator<Item = (Nibble, &Child)> {
        // Previously this used `.sorted_by_key()` directly on the iterator but this does not appear
        // to be available in itertools (it does not seem to ever have existed???) for unknown
        // reasons. This satisfies the same behavior. ¯\_(ツ)_/¯
        self.children.iter_sorted()
    }

    pub fn children_unsorted(&self) -> impl Iterator<Item = (Nibble, &Child)> {
        self.children.iter()
    }

    pub fn serialize(&self, binary: &mut Vec<u8>, persist_leaf_counts: bool) -> Result<()> {
        let (mut existence_bitmap, leaf_bitmap) = self.generate_bitmaps();
        binary.write_u16::<LittleEndian>(existence_bitmap)?;
        binary.write_u16::<LittleEndian>(leaf_bitmap)?;
        for _ in 0..existence_bitmap.count_ones() {
            let next_child = existence_bitmap.trailing_zeros() as u8;
            let child = self
                .children
                .get(Nibble::from(next_child))
                .as_ref()
                .expect("child must exist");
            serialize_u64_varint(child.version, binary);
            binary.extend(child.hash.to_vec());
            match child.node_type {
                NodeType::Leaf => (),
                NodeType::InternalLegacy => {
                    if persist_leaf_counts {
                        // It's impossible that a internal node has 0 leaves, use 0 to indicate
                        // "known".
                        // Also n.b., a not-fully-migrated internal is of `NodeType::InternalLegacy`
                        // in memory, but serialized with `NodeTag::Internal` anyway once the
                        // migration starts.
                        serialize_u64_varint(0, binary);
                    }
                }
                NodeType::Internal { leaf_count } => {
                    if persist_leaf_counts {
                        serialize_u64_varint(leaf_count as u64, binary);
                    }
                }
            };
            existence_bitmap &= !(1 << next_child);
        }
        Ok(())
    }

    pub fn deserialize(data: &[u8], read_leaf_counts: bool) -> Result<Self> {
        let mut reader = Cursor::new(data);
        let len = data.len();

        // Read and validate existence and leaf bitmaps
        let mut existence_bitmap = reader.read_u16::<LittleEndian>()?;
        let leaf_bitmap = reader.read_u16::<LittleEndian>()?;
        match existence_bitmap {
            0 => return Err(NodeDecodeError::NoChildren.into()),
            _ if (existence_bitmap & leaf_bitmap) != leaf_bitmap => {
                return Err(NodeDecodeError::ExtraLeaves {
                    existing: existence_bitmap,
                    leaves: leaf_bitmap,
                }
                .into())
            }
            _ => (),
        }

        // Reconstruct children
        let mut children = Children::new();
        for _ in 0..existence_bitmap.count_ones() {
            let next_child = existence_bitmap.trailing_zeros() as u8;
            let version = deserialize_u64_varint(&mut reader)?;
            let pos = reader.position() as usize;
            let remaining = len - pos;

            ensure!(
                remaining >= size_of::<[u8; 32]>(),
                "not enough bytes left, children: {}, bytes: {}",
                existence_bitmap.count_ones(),
                remaining
            );
            let hash = <[u8; 32]>::try_from(&reader.get_ref()[pos..pos + size_of::<[u8; 32]>()])?;
            reader.seek(SeekFrom::Current(size_of::<[u8; 32]>() as i64))?;

            let child_bit = 1 << next_child;
            let node_type = if (leaf_bitmap & child_bit) != 0 {
                NodeType::Leaf
            } else if read_leaf_counts {
                let leaf_count = deserialize_u64_varint(&mut reader)? as usize;
                if leaf_count == 0 {
                    NodeType::InternalLegacy
                } else {
                    NodeType::Internal { leaf_count }
                }
            } else {
                NodeType::InternalLegacy
            };

            children.insert(
                Nibble::from(next_child),
                Child::new(hash, version, node_type),
            );
            existence_bitmap &= !child_bit;
        }
        assert_eq!(existence_bitmap, 0);

        // The "leaf_count_migration" flag doesn't matter here, since a deserialized node should
        // not be persisted again to the DB.
        Self::new_impl(children, read_leaf_counts /* leaf_count_migration */)
    }

    /// Gets the `n`-th child.
    pub fn child(&self, n: Nibble) -> Option<&Child> {
        self.children.get(n).as_ref()
    }

    /// Generates `existence_bitmap` and `leaf_bitmap` as a pair of `u16`s: child at index `i`
    /// exists if `existence_bitmap[i]` is set; child at index `i` is leaf node if
    /// `leaf_bitmap[i]` is set.
    pub fn generate_bitmaps(&self) -> (u16, u16) {
        let mut existence_bitmap = 0;
        let mut leaf_bitmap = 0;
        for (nibble, child) in self.children.iter() {
            let i = u8::from(nibble);
            existence_bitmap |= 1u16 << i;
            if child.is_leaf() {
                leaf_bitmap |= 1u16 << i;
            }
        }
        // `leaf_bitmap` must be a subset of `existence_bitmap`.
        assert_eq!(existence_bitmap | leaf_bitmap, existence_bitmap);
        (existence_bitmap, leaf_bitmap)
    }

    /// Given a range [start, start + width), returns the sub-bitmap of that range.
    fn range_bitmaps(start: u8, width: u8, bitmaps: (u16, u16)) -> (u16, u16) {
        assert!(start < 16 && width.count_ones() == 1 && start % width == 0);
        assert!(width <= 16 && (start + width) <= 16);
        // A range with `start == 8` and `width == 4` will generate a mask 0b0000111100000000.
        // use as converting to smaller integer types when 'width == 16'
        let mask = (((1u32 << width) - 1) << start) as u16;
        (bitmaps.0 & mask, bitmaps.1 & mask)
    }

    fn merkle_hash(
        &self,
        start: u8,
        width: u8,
        (existence_bitmap, leaf_bitmap): (u16, u16),
    ) -> [u8; 32] {
        // Given a bit [start, 1 << nibble_height], return the value of that range.
        let (range_existence_bitmap, range_leaf_bitmap) =
            Self::range_bitmaps(start, width, (existence_bitmap, leaf_bitmap));
        if range_existence_bitmap == 0 {
            // No child under this subtree
            SPARSE_MERKLE_PLACEHOLDER_HASH
        } else if width == 1 || (range_existence_bitmap.count_ones() == 1 && range_leaf_bitmap != 0)
        {
            // Only 1 leaf child under this subtree or reach the lowest level
            let only_child_index = Nibble::from(range_existence_bitmap.trailing_zeros() as u8);
            self.child(only_child_index)
                .with_context(|| {
                    format!(
                        "Corrupted internal node: existence_bitmap indicates \
                         the existence of a non-exist child at index {:x}",
                        only_child_index
                    )
                })
                .unwrap()
                .hash
        } else {
            let left_child = self.merkle_hash(
                start,
                width / 2,
                (range_existence_bitmap, range_leaf_bitmap),
            );
            let right_child = self.merkle_hash(
                start + width / 2,
                width / 2,
                (range_existence_bitmap, range_leaf_bitmap),
            );
            SparseMerkleInternalNode::new(left_child, right_child).hash()
        }
    }

    /// Gets the child without its corresponding siblings (like using
    /// [`get_child_with_siblings`](InternalNode::get_child_with_siblings) and dropping the
    /// siblings, but more efficient).
    pub fn get_child_without_siblings(&self, node_key: &NodeKey, n: Nibble) -> Option<NodeKey> {
        let (existence_bitmap, leaf_bitmap) = self.generate_bitmaps();

        // Nibble height from 3 to 0.
        for h in (0..4).rev() {
            // Get the number of children of the internal node that each subtree at this height
            // covers.
            let width = 1 << h;
            let child_half_start = get_child_half_start(n, h);

            let (range_existence_bitmap, range_leaf_bitmap) =
                Self::range_bitmaps(child_half_start, width, (existence_bitmap, leaf_bitmap));

            if range_existence_bitmap == 0 {
                // No child in this range.
                return None;
            } else if width == 1
                || (range_existence_bitmap.count_ones() == 1 && range_leaf_bitmap != 0)
            {
                // Return the only 1 leaf child under this subtree or reach the lowest level
                // Even this leaf child is not the n-th child, it should be returned instead of
                // `None` because it's existence indirectly proves the n-th child doesn't exist.
                // Please read proof format for details.
                let only_child_index = Nibble::from(range_existence_bitmap.trailing_zeros() as u8);

                let only_child_version = self
                    .child(only_child_index)
                    // Should be guaranteed by the self invariants, but these are not easy to express at the moment
                    .with_context(|| {
                        format!(
                            "Corrupted internal node: child_bitmap indicates \
                                     the existence of a non-exist child at index {:x}",
                            only_child_index
                        )
                    })
                    .unwrap()
                    .version;

                return Some(node_key.gen_child_node_key(only_child_version, only_child_index));
            }
        }
        unreachable!("Impossible to get here without returning even at the lowest level.")
    }

    /// Gets the child and its corresponding siblings that are necessary to generate the proof for
    /// the `n`-th child. If it is an existence proof, the returned child must be the `n`-th
    /// child; otherwise, the returned child may be another child. See inline explanation for
    /// details. When calling this function with n = 11 (node `b` in the following graph), the
    /// range at each level is illustrated as a pair of square brackets:
    ///
    /// ```text
    ///     4      [f   e   d   c   b   a   9   8   7   6   5   4   3   2   1   0] -> root level
    ///            ---------------------------------------------------------------
    ///     3      [f   e   d   c   b   a   9   8] [7   6   5   4   3   2   1   0] width = 8
    ///                                  chs <--┘                        shs <--┘
    ///     2      [f   e   d   c] [b   a   9   8] [7   6   5   4] [3   2   1   0] width = 4
    ///                  shs <--┘               └--> chs
    ///     1      [f   e] [d   c] [b   a] [9   8] [7   6] [5   4] [3   2] [1   0] width = 2
    ///                          chs <--┘       └--> shs
    ///     0      [f] [e] [d] [c] [b] [a] [9] [8] [7] [6] [5] [4] [3] [2] [1] [0] width = 1
    ///     ^                chs <--┘   └--> shs
    ///     |   MSB|<---------------------- uint 16 ---------------------------->|LSB
    ///  height    chs: `child_half_start`         shs: `sibling_half_start`
    /// ```
    pub fn get_child_with_siblings(
        &self,
        node_key: &NodeKey,
        n: Nibble,
    ) -> (Option<NodeKey>, Vec<[u8; 32]>) {
        let mut siblings = vec![];
        let (existence_bitmap, leaf_bitmap) = self.generate_bitmaps();

        // Nibble height from 3 to 0.
        for h in (0..4).rev() {
            // Get the number of children of the internal node that each subtree at this height
            // covers.
            let width = 1 << h;
            let (child_half_start, sibling_half_start) = get_child_and_sibling_half_start(n, h);
            // Compute the root hash of the subtree rooted at the sibling of `r`.
            siblings.push(self.merkle_hash(
                sibling_half_start,
                width,
                (existence_bitmap, leaf_bitmap),
            ));

            let (range_existence_bitmap, range_leaf_bitmap) =
                Self::range_bitmaps(child_half_start, width, (existence_bitmap, leaf_bitmap));

            if range_existence_bitmap == 0 {
                // No child in this range.
                return (None, siblings);
            } else if width == 1
                || (range_existence_bitmap.count_ones() == 1 && range_leaf_bitmap != 0)
            {
                // Return the only 1 leaf child under this subtree or reach the lowest level
                // Even this leaf child is not the n-th child, it should be returned instead of
                // `None` because it's existence indirectly proves the n-th child doesn't exist.
                // Please read proof format for details.
                let only_child_index = Nibble::from(range_existence_bitmap.trailing_zeros() as u8);
                return (
                    {
                        let only_child_version = self
                            .child(only_child_index)
                            // Should be guaranteed by the self invariants, but these are not easy to express at the moment
                            .with_context(|| {
                                format!(
                                    "Corrupted internal node: child_bitmap indicates \
                                     the existence of a non-exist child at index {:x}",
                                    only_child_index
                                )
                            })
                            .unwrap()
                            .version;
                        Some(node_key.gen_child_node_key(only_child_version, only_child_index))
                    },
                    siblings,
                );
            }
        }
        unreachable!("Impossible to get here without returning even at the lowest level.")
    }

    #[cfg(test)]
    pub(crate) fn into_legacy_internal(self) -> InternalNode {
        let mut children = self.children;
        children.iter_mut().for_each(|(_, mut child)| {
            if matches!(child.node_type, NodeType::Internal { .. }) {
                child.node_type = NodeType::InternalLegacy
            }
        });

        InternalNode::new_migration(children, false /* leaf_count_migration */)
    }

    #[cfg(test)]
    pub(crate) fn children(&self) -> &Children {
        &self.children
    }
}

/// Given a nibble, computes the start position of its `child_half_start` and `sibling_half_start`
/// at `height` level.
pub(crate) fn get_child_and_sibling_half_start(n: Nibble, height: u8) -> (u8, u8) {
    // Get the index of the first child belonging to the same subtree whose root, let's say `r` is
    // at `height` that the n-th child belongs to.
    // Note: `child_half_start` will be always equal to `n` at height 0.
    let child_half_start = (0xff << height) & u8::from(n);

    // Get the index of the first child belonging to the subtree whose root is the sibling of `r`
    // at `height`.
    let sibling_half_start = child_half_start ^ (1 << height);

    (child_half_start, sibling_half_start)
}

/// Given a nibble, computes the start position of its `child_half_start` at `height` level.
pub(crate) fn get_child_half_start(n: Nibble, height: u8) -> u8 {
    // Get the index of the first child belonging to the same subtree whose root, let's say `r` is
    // at `height` that the n-th child belongs to.
    // Note: `child_half_start` will be always equal to `n` at height 0.
    (0xff << height) & u8::from(n)
}

/// Represents a key-value pair in the map.
///
/// Note: this does not store the key itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LeafNode {
    /// The hash of the key for this entry.
    key_hash: KeyHash,
    /// The hash of the value for this entry.
    value_hash: ValueHash,
    /// The value associated with the key.
    value: Vec<u8>,
}

impl LeafNode {
    /// Creates a new leaf node.
    pub fn new(key_hash: KeyHash, value: Vec<u8>) -> Self {
        let value_hash = value.as_slice().into();
        Self {
            key_hash,
            value_hash,
            value,
        }
    }

    /// Gets the key hash.
    pub fn key_hash(&self) -> KeyHash {
        self.key_hash
    }

    /// Gets the associated value itself.
    pub fn value(&self) -> &[u8] {
        self.value.as_ref()
    }

    /// Gets the associated value hash.
    pub(crate) fn value_hash(&self) -> ValueHash {
        self.value_hash
    }

    pub fn hash(&self) -> [u8; 32] {
        SparseMerkleLeafNode::new(self.key_hash, self.value_hash).hash()
    }
}

impl From<LeafNode> for SparseMerkleLeafNode {
    fn from(leaf_node: LeafNode) -> Self {
        Self::new(leaf_node.key_hash, leaf_node.value_hash)
    }
}

#[repr(u8)]
#[derive(FromPrimitive, ToPrimitive)]
enum NodeTag {
    Null = 0,
    InternalLegacy = 1,
    Leaf = 2,
    Internal = 3,
}

/// The concrete node type of [`JellyfishMerkleTree`](crate::JellyfishMerkleTree).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Node {
    /// Represents `null`.
    Null,
    /// A wrapper of [`InternalNode`].
    Internal(InternalNode),
    /// A wrapper of [`LeafNode`].
    Leaf(LeafNode),
}

impl From<InternalNode> for Node {
    fn from(node: InternalNode) -> Self {
        Node::Internal(node)
    }
}

impl From<InternalNode> for Children {
    fn from(node: InternalNode) -> Self {
        node.children
    }
}

impl From<LeafNode> for Node {
    fn from(node: LeafNode) -> Self {
        Node::Leaf(node)
    }
}

impl Node {
    /// Creates the [`Null`](Node::Null) variant.
    pub(crate) fn new_null() -> Self {
        Node::Null
    }

    /// Creates the [`Internal`](Node::Internal) variant.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn new_internal(children: Children) -> Self {
        Node::Internal(InternalNode::new(children))
    }

    /// Creates the [`Leaf`](Node::Leaf) variant.
    pub(crate) fn new_leaf(key_hash: KeyHash, value: Vec<u8>) -> Self {
        Node::Leaf(LeafNode::new(key_hash, value))
    }

    /// Returns `true` if the node is a leaf node.
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf(_))
    }

    /// Returns `NodeType`
    pub(crate) fn node_type(&self) -> NodeType {
        match self {
            // The returning value will be used to construct a `Child` of a internal node, while an
            // internal node will never have a child of Node::Null.
            Self::Null => unreachable!(),
            Self::Leaf(_) => NodeType::Leaf,
            Self::Internal(n) => n.node_type(),
        }
    }

    /// Returns leaf count if known
    pub(crate) fn leaf_count(&self) -> Option<usize> {
        match self {
            Node::Null => Some(0),
            Node::Leaf(_) => Some(1),
            Node::Internal(internal_node) => internal_node.leaf_count,
        }
    }

    /// Serializes to bytes for physical storage.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = vec![];

        match self {
            Node::Null => {
                out.push(NodeTag::Null as u8);
            }
            Node::Internal(internal_node) => {
                let persist_leaf_count = internal_node.leaf_count_migration;
                let tag = if persist_leaf_count {
                    NodeTag::Internal
                } else {
                    NodeTag::InternalLegacy
                };
                out.push(tag as u8);
                internal_node.serialize(&mut out, persist_leaf_count)?;
                DIEM_JELLYFISH_INTERNAL_ENCODED_BYTES.inc_by(out.len() as u64);
            }
            Node::Leaf(leaf_node) => {
                out.push(NodeTag::Leaf as u8);
                out.extend(bcs::to_bytes(&leaf_node)?);
                DIEM_JELLYFISH_LEAF_ENCODED_BYTES.inc_by(out.len() as u64);
            }
        }
        Ok(out)
    }

    /// Computes the hash of nodes.
    pub(crate) fn hash(&self) -> [u8; 32] {
        match self {
            Node::Null => SPARSE_MERKLE_PLACEHOLDER_HASH,
            Node::Internal(internal_node) => internal_node.hash(),
            Node::Leaf(leaf_node) => leaf_node.hash(),
        }
    }

    /// Recovers from serialized bytes in physical storage.
    pub fn decode(val: &[u8]) -> Result<Node> {
        if val.is_empty() {
            return Err(NodeDecodeError::EmptyInput.into());
        }
        let tag = val[0];
        let node_tag = NodeTag::from_u8(tag);
        match node_tag {
            Some(NodeTag::Null) => Ok(Node::Null),
            Some(NodeTag::InternalLegacy) => {
                Ok(Node::Internal(InternalNode::deserialize(&val[1..], false)?))
            }
            Some(NodeTag::Internal) => {
                Ok(Node::Internal(InternalNode::deserialize(&val[1..], true)?))
            }
            Some(NodeTag::Leaf) => Ok(Node::Leaf(bcs::from_bytes(&val[1..])?)),
            None => Err(NodeDecodeError::UnknownTag { unknown_tag: tag }.into()),
        }
    }
}

/// Error thrown when a [`Node`] fails to be deserialized out of a byte sequence stored in physical
/// storage, via [`Node::decode`].
#[derive(Debug, Error, Eq, PartialEq)]
pub enum NodeDecodeError {
    /// Input is empty.
    #[error("Missing tag due to empty input")]
    EmptyInput,

    /// The first byte of the input is not a known tag representing one of the variants.
    #[error("lead tag byte is unknown: {}", unknown_tag)]
    UnknownTag { unknown_tag: u8 },

    /// No children found in internal node
    #[error("No children found in internal node")]
    NoChildren,

    /// Extra leaf bits set
    #[error(
        "Non-existent leaf bits set, existing: {}, leaves: {}",
        existing,
        leaves
    )]
    ExtraLeaves { existing: u16, leaves: u16 },
}

/// Helper function to serialize version in a more efficient encoding.
/// We use a super simple encoding - the high bit is set if more bytes follow.
pub(crate) fn serialize_u64_varint(mut num: u64, binary: &mut Vec<u8>) {
    for _ in 0..8 {
        let low_bits = num as u8 & 0x7f;
        num >>= 7;
        let more = match num {
            0 => 0u8,
            _ => 0x80,
        };
        binary.push(low_bits | more);
        if more == 0 {
            return;
        }
    }
    // Last byte is encoded raw; this means there are no bad encodings.
    assert_ne!(num, 0);
    assert!(num <= 0xff);
    binary.push(num as u8);
}

/// Helper function to deserialize versions from above encoding.
pub(crate) fn deserialize_u64_varint<T>(reader: &mut T) -> Result<u64>
where
    T: Read,
{
    let mut num = 0u64;
    for i in 0..8 {
        let byte = reader.read_u8()?;
        num |= u64::from(byte & 0x7f) << (i * 7);
        if (byte & 0x80) == 0 {
            return Ok(num);
        }
    }
    // Last byte is encoded as is.
    let byte = reader.read_u8()?;
    num |= u64::from(byte) << 56;
    Ok(num)
}
