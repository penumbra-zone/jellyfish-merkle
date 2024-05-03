// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module implements `JellyfishMerkleIterator`. Initialized with a version and a key, the
//! iterator generates all the key-value pairs in this version of the tree, starting from the
//! smallest key that is greater or equal to the given key, by performing a depth first traversal
//! on the tree.

use alloc::{sync::Arc, vec::Vec};

use anyhow::{bail, ensure, format_err};

use crate::{
    node_type::{Child, InternalNode, Node, NodeKey},
    storage::TreeReader,
    types::{
        nibble::{nibble_path::NibblePath, Nibble, ROOT_NIBBLE_HEIGHT},
        Version,
    },
    KeyHash, OwnedValue,
};

/// `NodeVisitInfo` keeps track of the status of an internal node during the iteration process. It
/// indicates which ones of its children have been visited.
#[derive(Debug)]
struct NodeVisitInfo {
    /// The key to this node.
    node_key: NodeKey,

    /// The node itself.
    node: InternalNode,

    /// The bitmap indicating which children exist. It is generated by running
    /// `self.node.generate_bitmaps().0` and cached here.
    children_bitmap: u16,

    /// This integer always has exactly one 1-bit. The position of the 1-bit (from LSB) indicates
    /// the next child to visit in the iteration process. All the ones on the left have already
    /// been visited. All the chilren on the right (including this one) have not been visited yet.
    next_child_to_visit: u16,
}

impl NodeVisitInfo {
    /// Constructs a new `NodeVisitInfo` with given node key and node. `next_child_to_visit` will
    /// be set to the leftmost child.
    fn new(node_key: NodeKey, node: InternalNode) -> Self {
        let (children_bitmap, _) = node.generate_bitmaps();
        assert!(children_bitmap != 0);
        Self {
            node_key,
            node,
            children_bitmap,
            next_child_to_visit: 1 << children_bitmap.trailing_zeros(),
        }
    }

    /// Same as `new` but points `next_child_to_visit` to a specific location. If the child
    /// corresponding to `next_child_to_visit` does not exist, set it to the next one on the
    /// right.
    fn new_next_child_to_visit(
        node_key: NodeKey,
        node: InternalNode,
        next_child_to_visit: Nibble,
    ) -> Self {
        let (children_bitmap, _) = node.generate_bitmaps();
        let mut next_child_to_visit = 1 << u8::from(next_child_to_visit);
        assert!(children_bitmap >= next_child_to_visit);
        while next_child_to_visit & children_bitmap == 0 {
            next_child_to_visit <<= 1;
        }
        Self {
            node_key,
            node,
            children_bitmap,
            next_child_to_visit,
        }
    }

    /// Whether the next child to visit is the rightmost one.
    fn is_rightmost(&self) -> bool {
        assert!(self.next_child_to_visit.leading_zeros() >= self.children_bitmap.leading_zeros());
        self.next_child_to_visit.leading_zeros() == self.children_bitmap.leading_zeros()
    }

    /// Advances `next_child_to_visit` to the next child on the right.
    fn advance(&mut self) {
        assert!(!self.is_rightmost(), "Advancing past rightmost child.");
        self.next_child_to_visit <<= 1;
        while self.next_child_to_visit & self.children_bitmap == 0 {
            self.next_child_to_visit <<= 1;
        }
    }
}

/// An iterator over all key-value pairs in a [`JellyfishMerkleTree`](crate::JellyfishMerkleTree).
///
/// Initialized with a version and a key, the iterator generates all the
/// key-value pairs in this version of the tree, starting from the smallest key
/// that is greater or equal to the given key, by performing a depth first
/// traversal on the tree.
pub struct JellyfishMerkleIterator<R> {
    /// The storage engine from which we can read nodes using node keys.
    reader: Arc<R>,

    /// The version of the tree this iterator is running on.
    version: Version,

    /// The stack used for depth first traversal.
    parent_stack: Vec<NodeVisitInfo>,

    /// Whether the iteration has finished. Usually this can be determined by checking whether
    /// `self.parent_stack` is empty. But in case of a tree with a single leaf, we need this
    /// additional bit.
    done: bool,
}

impl<R> JellyfishMerkleIterator<R>
where
    R: TreeReader,
{
    /// Constructs a new iterator. This puts the internal state in the correct position, so the
    /// following `next` call will yield the smallest key that is greater or equal to
    /// `starting_key`.
    pub fn new(
        reader: Arc<R>,
        version: Version,
        starting_key: KeyHash,
    ) -> Result<Self, anyhow::Error> {
        let mut parent_stack = Vec::new();
        let mut done = false;

        let mut current_node_key = NodeKey::new_empty_path(version);
        let nibble_path = NibblePath::new(starting_key.0.to_vec());
        let mut nibble_iter = nibble_path.nibbles();

        while let Node::Internal(internal_node) = reader.get_node(&current_node_key)? {
            let child_index = nibble_iter.next().expect("Should have enough nibbles.");
            match internal_node.child(child_index) {
                Some(child) => {
                    // If this child exists, we just push the node onto stack and repeat.
                    parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                        current_node_key.clone(),
                        internal_node.clone(),
                        child_index,
                    ));
                    current_node_key =
                        current_node_key.gen_child_node_key(child.version, child_index);
                }
                None => {
                    let (bitmap, _) = internal_node.generate_bitmaps();
                    if u32::from(u8::from(child_index)) < 15 - bitmap.leading_zeros() {
                        // If this child does not exist and there's another child on the right, we
                        // set the child on the right to be the next one to visit.
                        parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                            current_node_key,
                            internal_node,
                            child_index,
                        ));
                    } else {
                        // Otherwise we have done visiting this node. Go backward and clean up the
                        // stack.
                        Self::cleanup_stack(&mut parent_stack);
                    }
                    return Ok(Self {
                        reader,
                        version,
                        parent_stack,
                        done,
                    });
                }
            }
        }

        match reader.get_node(&current_node_key)? {
            Node::Internal(_) => unreachable!("Should have reached the bottom of the tree."),
            Node::Leaf(leaf_node) => {
                if leaf_node.key_hash() < starting_key {
                    Self::cleanup_stack(&mut parent_stack);
                    if parent_stack.is_empty() {
                        done = true;
                    }
                }
            }
            Node::Null => done = true,
        }

        Ok(Self {
            reader,
            version,
            parent_stack,
            done,
        })
    }

    fn cleanup_stack(parent_stack: &mut Vec<NodeVisitInfo>) {
        while let Some(info) = parent_stack.last_mut() {
            if info.is_rightmost() {
                parent_stack.pop();
            } else {
                info.advance();
                break;
            }
        }
    }

    /// Constructs a new iterator. This puts the internal state in the correct position, so the
    /// following `next` call will yield the leaf at `start_idx`.
    pub fn new_by_index(
        reader: Arc<R>,
        version: Version,
        start_idx: usize,
    ) -> Result<Self, anyhow::Error> {
        let mut parent_stack = Vec::new();

        let mut current_node_key = NodeKey::new_empty_path(version);
        let mut current_node = reader.get_node(&current_node_key)?;
        let total_leaves = current_node.leaf_count();
        if start_idx >= total_leaves {
            return Ok(Self {
                reader,
                version,
                parent_stack,
                done: true,
            });
        }

        let mut leaves_skipped = 0;
        for _ in 0..=ROOT_NIBBLE_HEIGHT {
            match current_node {
                Node::Null => {
                    unreachable!("The Node::Null case has already been covered before loop.")
                }
                Node::Leaf(_) => {
                    ensure!(
                        leaves_skipped == start_idx,
                        "Bug: The leaf should be the exact one we are looking for.",
                    );
                    return Ok(Self {
                        reader,
                        version,
                        parent_stack,
                        done: false,
                    });
                }
                Node::Internal(internal_node) => {
                    let (nibble, child) =
                        Self::skip_leaves(&internal_node, &mut leaves_skipped, start_idx)?;
                    let next_node_key = current_node_key.gen_child_node_key(child.version, nibble);
                    parent_stack.push(NodeVisitInfo::new_next_child_to_visit(
                        current_node_key,
                        internal_node,
                        nibble,
                    ));
                    current_node_key = next_node_key;
                }
            };
            current_node = reader.get_node(&current_node_key)?;
        }

        bail!("Bug: potential infinite loop.");
    }

    fn skip_leaves<'a>(
        internal_node: &'a InternalNode,
        leaves_skipped: &mut usize,
        target_leaf_idx: usize,
    ) -> Result<(Nibble, &'a Child), anyhow::Error> {
        for (nibble, child) in internal_node.children_sorted() {
            let child_leaf_count = child.leaf_count();
            // n.b. The index is 0-based, so to reach leaf at N, N previous ones need to be skipped.
            if *leaves_skipped + child_leaf_count <= target_leaf_idx {
                *leaves_skipped += child_leaf_count;
            } else {
                return Ok((nibble, child));
            }
        }

        bail!("Bug: Internal node has less leaves than expected.");
    }
}

impl<R> Iterator for JellyfishMerkleIterator<R>
where
    R: TreeReader,
{
    type Item = Result<(KeyHash, OwnedValue), anyhow::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        if self.parent_stack.is_empty() {
            let root_node_key = NodeKey::new_empty_path(self.version);
            match self.reader.get_node(&root_node_key) {
                Ok(Node::Leaf(leaf_node)) => {
                    // This means the entire tree has a single leaf node. The key of this leaf node
                    // is greater or equal to `starting_key` (otherwise we would have set `done` to
                    // true in `new`). Return the node and mark `self.done` so next time we return
                    // None.
                    self.done = true;
                    return match self
                        .reader
                        .get_value(root_node_key.version(), leaf_node.key_hash())
                    {
                        Ok(value) => Some(Ok((leaf_node.key_hash(), value))),
                        Err(e) => Some(Err(e)),
                    };
                }
                Ok(Node::Internal(_)) => {
                    // This means `starting_key` is bigger than every key in this tree, or we have
                    // iterated past the last key.
                    return None;
                }
                Ok(Node::Null) => unreachable!("We would have set done to true in new."),
                Err(err) => return Some(Err(err)),
            }
        }

        loop {
            let last_visited_node_info = self
                .parent_stack
                .last()
                .expect("We have checked that self.parent_stack is not empty.");
            let child_index =
                Nibble::from(last_visited_node_info.next_child_to_visit.trailing_zeros() as u8);
            let node_key = last_visited_node_info.node_key.gen_child_node_key(
                last_visited_node_info
                    .node
                    .child(child_index)
                    .expect("Child should exist.")
                    .version,
                child_index,
            );
            match self.reader.get_node(&node_key) {
                Ok(Node::Internal(internal_node)) => {
                    let visit_info = NodeVisitInfo::new(node_key, internal_node);
                    self.parent_stack.push(visit_info);
                }
                Ok(Node::Leaf(leaf_node)) => {
                    return match self
                        .reader
                        .get_value(node_key.version(), leaf_node.key_hash())
                    {
                        Ok(value) => {
                            let ret = (leaf_node.key_hash(), value);
                            Self::cleanup_stack(&mut self.parent_stack);
                            Some(Ok(ret))
                        }
                        Err(e) => Some(Err(e)),
                    }
                }
                Ok(Node::Null) => return Some(Err(format_err!("Should not reach a null node."))),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}
