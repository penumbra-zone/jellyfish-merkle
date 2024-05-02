// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module implements the functionality to restore a
//! [`JellyfishMerkleTree`](crate::JellyfishMerkleTree) from small chunks of
//! key/value pairs.

use core::marker::PhantomData;

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use anyhow::{bail, ensure};
use mirai_annotations::*;

use crate::{
    node_type::{
        get_child_and_sibling_half_start, Child, Children, InternalNode, LeafNode, Node, NodeKey,
        NodeType,
    },
    storage::{NodeBatch, TreeReader, TreeWriter},
    types::{
        nibble::{
            nibble_path::{NibbleIterator, NibblePath},
            Nibble,
        },
        proof::{SparseMerkleInternalNode, SparseMerkleLeafNode, SparseMerkleRangeProof},
        Version,
    },
    Bytes32Ext, KeyHash, OwnedValue, RootHash, SimpleHasher, ValueHash, ROOT_NIBBLE_HEIGHT,
    SPARSE_MERKLE_PLACEHOLDER_HASH,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChildInfo {
    /// This child is an internal node. The hash of the internal node is stored here if it is
    /// known, otherwise it is `None`. In the process of restoring a tree, we will only know the
    /// hash of an internal node after we see all the keys that share the same prefix.
    Internal {
        hash: Option<[u8; 32]>,
        leaf_count: usize,
    },

    /// This child is a leaf node.
    Leaf { node: LeafNode },
}

impl ChildInfo {
    /// Converts `self` to a child, assuming the hash is known if it's an internal node.
    fn into_child<H: SimpleHasher>(self, version: Version) -> Child {
        match self {
            Self::Internal { hash, leaf_count } => Child::new(
                hash.expect("Must have been initialized."),
                version,
                NodeType::Internal { leaf_count },
            ),
            Self::Leaf { node } => Child::new(node.hash::<H>(), version, NodeType::Leaf),
        }
    }
}

#[derive(Clone, Debug)]
struct InternalInfo {
    /// The node key of this internal node.
    node_key: NodeKey,

    /// The existing children. Every time a child appears, the corresponding position will be set
    /// to `Some`.
    children: [Option<ChildInfo>; 16],
}

impl InternalInfo {
    /// Creates an empty internal node with no children.
    fn new_empty(node_key: NodeKey) -> Self {
        Self {
            node_key,
            children: Default::default(),
        }
    }

    fn set_child(&mut self, index: usize, child_info: ChildInfo) {
        precondition!(index < 16);
        self.children[index] = Some(child_info);
    }

    /// Converts `self` to an internal node, assuming all of its children are already known and
    /// fully initialized.
    fn into_internal_node<H: SimpleHasher>(mut self, version: Version) -> (NodeKey, InternalNode) {
        let mut children = Children::new();

        // Calling `into_iter` on an array is equivalent to calling `iter`:
        // https://github.com/rust-lang/rust/issues/25725. So we use `iter_mut` and `take`.
        for (index, child_info_option) in self.children.iter_mut().enumerate() {
            if let Some(child_info) = child_info_option.take() {
                children.insert((index as u8).into(), child_info.into_child::<H>(version));
            }
        }

        (self.node_key, InternalNode::new(children))
    }
}

/// Implements the functionality to restore a
/// [`JellyfishMerkleTree`](crate::JellyfishMerkleTree) from small chunks of
/// key-value pairs.
pub struct JellyfishMerkleRestore<H: SimpleHasher, WriteError> {
    /// The underlying storage.
    store: Arc<dyn TreeWriter<Error = WriteError>>,

    /// The version of the tree we are restoring.
    version: Version,

    /// The nodes we have partially restored. Each `partial_nodes[i-1]` is the parent of
    /// `partial_nodes[i]`. If a node `partial_nodes[i-1]` has multiple children, only the
    /// rightmost known child will appear here as `partial_nodes[i]`, because any other children on
    /// the left would have been frozen.
    ///
    /// At any point in time, the structure looks like the following:
    ///
    /// ```text
    /// +----+----+----+----+----+----+----+----+
    /// |    |    |    |    |    |    |    | C  |  partial_nodes[0]
    /// +----+----+----+----+----+----+----+----+
    ///   |         |              |
    ///   |         |              |
    ///   |         |              |
    ///   v         v              v
    /// Frozen    Frozen     +----+----+----+----+----+----+----+----+
    ///                      |    |    |    | B  |    |    | A  |    |  partial_nodes[1]
    ///                      +----+----+----+----+----+----+----+----+
    ///                             |         |
    ///                             |         |
    ///                             |         |
    ///                             v         v
    ///                            Frozen    Previously inserted account
    /// ```
    ///
    /// We insert the accounts from left to right. So if the next account appears at position `A`,
    /// it will cause the leaf at position `B` to be frozen. If it appears at position `B`, it
    /// might cause a few internal nodes to be created additionally. If it appears at position `C`,
    /// it will also cause `partial_nodes[1]` to be added to `frozen_nodes` as an internal node and
    /// be removed from `partial_nodes`.
    partial_nodes: Vec<InternalInfo>,

    /// The nodes that have been fully restored and are ready to be written to storage.
    frozen_nodes: NodeBatch,

    /// The most recently added leaf. This is used to ensure the keys come in increasing order and
    /// do proof verification.
    previous_leaf: Option<LeafNode>,

    /// The number of keys we have received since the most recent restart.
    num_keys_received: u64,

    /// When the restoration process finishes, we expect the tree to have this root hash.
    expected_root_hash: RootHash,

    _phantom_hasher: PhantomData<H>,
}

impl<H: SimpleHasher, E> JellyfishMerkleRestore<H, E> {
    pub fn new<D>(
        store: Arc<D>,
        version: Version,
        expected_root_hash: RootHash,
    ) -> Result<Self, anyhow::Error>
    where
        D: 'static + TreeReader + TreeWriter<Error = E>,
        <D as TreeReader>::Error: std::error::Error + Send + Sync + 'static,
    {
        let tree_reader = Arc::clone(&store);
        let (partial_nodes, previous_leaf) =
            if let Some((node_key, leaf_node)) = tree_reader.get_rightmost_leaf()? {
                // TODO: confirm rightmost leaf is at the desired version
                // If the system crashed in the middle of the previous restoration attempt, we need
                // to recover the partial nodes to the state right before the crash.
                (
                    Self::recover_partial_nodes(tree_reader.as_ref(), version, node_key)?,
                    Some(leaf_node),
                )
            } else {
                (
                    vec![InternalInfo::new_empty(NodeKey::new_empty_path(version))],
                    None,
                )
            };

        Ok(Self {
            store,
            version,
            partial_nodes,
            frozen_nodes: Default::default(),
            previous_leaf,
            num_keys_received: 0,
            expected_root_hash,
            _phantom_hasher: Default::default(),
        })
    }

    pub fn new_overwrite<D: 'static + TreeWriter<Error = E>>(
        store: Arc<D>,
        version: Version,
        expected_root_hash: RootHash,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            store,
            version,
            partial_nodes: vec![InternalInfo::new_empty(NodeKey::new_empty_path(version))],
            frozen_nodes: Default::default(),
            previous_leaf: None,
            num_keys_received: 0,
            expected_root_hash,
            _phantom_hasher: Default::default(),
        })
    }

    /// Recovers partial nodes from storage. We do this by looking at all the ancestors of the
    /// rightmost leaf. The ones do not exist in storage are the partial nodes.
    fn recover_partial_nodes<RE>(
        store: &dyn TreeReader<Error = RE>,
        version: Version,
        rightmost_leaf_node_key: NodeKey,
    ) -> Result<Vec<InternalInfo>, anyhow::Error>
    where
        RE: std::error::Error + Send + Sync + 'static,
    {
        ensure!(
            !rightmost_leaf_node_key.nibble_path().is_empty(),
            "Root node would not be written until entire restoration process has completed \
             successfully.",
        );

        // Start from the parent of the rightmost leaf. If this internal node exists in storage, it
        // is not a partial node. Go to the parent node and repeat until we see a node that does
        // not exist. This node and all its ancestors will be the partial nodes.
        let mut node_key = rightmost_leaf_node_key.gen_parent_node_key();
        while store.get_node_option(&node_key)?.is_some() {
            node_key = node_key.gen_parent_node_key();
        }

        // Next we reconstruct all the partial nodes up to the root node, starting from the bottom.
        // For all of them, we scan all its possible child positions and see if there is one at
        // each position. If the node is not the bottom one, there is additionally a partial node
        // child at the position `previous_child_index`.
        let mut partial_nodes = vec![];
        // Initialize `previous_child_index` to `None` for the first iteration of the loop so the
        // code below treats it differently.
        let mut previous_child_index = None;

        loop {
            let mut internal_info = InternalInfo::new_empty(node_key.clone());

            for i in 0..previous_child_index.unwrap_or(16) {
                let child_node_key = node_key.gen_child_node_key(version, (i as u8).into());
                if let Some(node) = store.get_node_option(&child_node_key)? {
                    let child_info = match node {
                        Node::Internal(internal_node) => ChildInfo::Internal {
                            hash: Some(internal_node.hash::<H>()),
                            leaf_count: internal_node.leaf_count(),
                        },
                        Node::Leaf(leaf_node) => ChildInfo::Leaf { node: leaf_node },
                        Node::Null => bail!("Null node should not appear in storage."),
                    };
                    internal_info.set_child(i, child_info);
                }
            }

            // If this is not the lowest partial node, it will have a partial node child at
            // `previous_child_index`. Set the hash of this child to `None` because it is a
            // partial node and we do not know its hash yet. For the lowest partial node, we just
            // find all its known children from storage in the loop above.
            if let Some(index) = previous_child_index {
                internal_info.set_child(
                    index,
                    ChildInfo::Internal {
                        hash: None,
                        leaf_count: 0,
                    },
                );
            }

            partial_nodes.push(internal_info);
            if node_key.nibble_path().is_empty() {
                break;
            }
            previous_child_index = node_key.nibble_path().last().map(|x| u8::from(x) as usize);
            node_key = node_key.gen_parent_node_key();
        }

        partial_nodes.reverse();
        Ok(partial_nodes)
    }
}

/// Errors that can occur when a [`JellyfishMerkleRestore<H, E>`] adds a chunk.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError<E> {
    #[error("Should not add empty chunks.")]
    EmptyChunk,
    #[error("Account keys must come in increasing order.")]
    OutOfOrder,
    // TODO(kate): this is left as a boxed error for now.
    #[error(transparent)]
    VerificationFailed(anyhow::Error),
    #[error(transparent)]
    WriteFailed(E),
}

impl<H: SimpleHasher, WriteError> JellyfishMerkleRestore<H, WriteError>
where
    WriteError: std::error::Error + Send + Sync + 'static,
{
    /// Restores a chunk of accounts. This function will verify that the given chunk is correct
    /// using the proof and root hash, then write things to storage. If the chunk is invalid, an
    /// error will be returned and nothing will be written to storage.
    fn add_chunk_impl(
        &mut self,
        chunk: Vec<(KeyHash, OwnedValue)>,
        proof: SparseMerkleRangeProof<H>,
    ) -> Result<(), RestoreError<WriteError>> {
        if chunk.is_empty() {
            return Err(RestoreError::EmptyChunk);
        }

        for (key, value) in chunk {
            if let Some(ref prev_leaf) = self.previous_leaf {
                if key <= prev_leaf.key_hash() {
                    return Err(RestoreError::OutOfOrder);
                }
            }
            let value_hash = ValueHash::with::<H>(value.as_slice());
            self.frozen_nodes.insert_value(self.version, key, value);

            self.add_one(key, value_hash);
            self.previous_leaf.replace(LeafNode::new(key, value_hash));
            self.num_keys_received += 1;
        }

        // Verify what we have added so far is all correct.
        self.verify(proof)
            .map_err(RestoreError::VerificationFailed)?;

        // Write the frozen nodes to storage.
        self.store
            .write_node_batch(&self.frozen_nodes)
            .map_err(RestoreError::WriteFailed)?;
        self.frozen_nodes.clear();

        Ok(())
    }

    /// Restores one account.
    fn add_one(&mut self, new_key: KeyHash, value_hash: ValueHash) {
        let nibble_path = NibblePath::new(new_key.0.to_vec());
        let mut nibbles = nibble_path.nibbles();

        for i in 0..ROOT_NIBBLE_HEIGHT {
            let child_index = u8::from(nibbles.next().expect("This nibble must exist.")) as usize;

            assert!(i < self.partial_nodes.len());
            match self.partial_nodes[i].children[child_index] {
                Some(ref child_info) => {
                    // If there exists an internal node at this position, we just continue the loop
                    // with the next nibble. Here we deal with the leaf case.
                    if let ChildInfo::Leaf { node } = child_info {
                        assert_eq!(
                            i,
                            self.partial_nodes.len() - 1,
                            "If we see a leaf, there will be no more partial internal nodes on \
                             lower level, since they would have been frozen.",
                        );

                        let existing_leaf = node.clone();
                        self.insert_at_leaf(
                            child_index,
                            existing_leaf,
                            new_key,
                            value_hash,
                            nibbles,
                        );
                        break;
                    }
                }
                None => {
                    // This means that we are going to put a leaf in this position. For all the
                    // descendants on the left, they are now frozen.
                    self.freeze(i + 1);

                    // Mark this position as a leaf child.
                    self.partial_nodes[i].set_child(
                        child_index,
                        ChildInfo::Leaf {
                            node: LeafNode::new(new_key, value_hash),
                        },
                    );

                    // We do not add this leaf node to self.frozen_nodes because we don't know its
                    // node key yet. We will know its node key when the next account comes.
                    break;
                }
            }
        }
    }

    /// Inserts a new account at the position of the existing leaf node. We may need to create
    /// multiple internal nodes depending on the length of the common prefix of the existing key
    /// and the new key.
    fn insert_at_leaf(
        &mut self,
        child_index: usize,
        existing_leaf: LeafNode,
        new_key: KeyHash,
        value_hash: ValueHash,
        mut remaining_nibbles: NibbleIterator,
    ) {
        let num_existing_partial_nodes = self.partial_nodes.len();

        // The node at this position becomes an internal node. Since we may insert more nodes at
        // this position in the future, we do not know its hash yet.
        self.partial_nodes[num_existing_partial_nodes - 1].set_child(
            child_index,
            ChildInfo::Internal {
                hash: None,
                leaf_count: 0,
            },
        );

        // Next we build the new internal nodes from top to bottom. All these internal node except
        // the bottom one will now have a single internal node child.
        let common_prefix_len = existing_leaf
            .key_hash()
            .0
            .common_prefix_nibbles_len(&new_key.0);
        for _ in num_existing_partial_nodes..common_prefix_len {
            let visited_nibbles = remaining_nibbles.visited_nibbles().collect();
            let next_nibble = remaining_nibbles.next().expect("This nibble must exist.");
            let new_node_key = NodeKey::new(self.version, visited_nibbles);

            let mut internal_info = InternalInfo::new_empty(new_node_key);
            internal_info.set_child(
                u8::from(next_nibble) as usize,
                ChildInfo::Internal {
                    hash: None,
                    leaf_count: 0,
                },
            );
            self.partial_nodes.push(internal_info);
        }

        // The last internal node will have two leaf node children.
        let visited_nibbles = remaining_nibbles.visited_nibbles().collect();
        let new_node_key = NodeKey::new(self.version, visited_nibbles);
        let mut internal_info = InternalInfo::new_empty(new_node_key);

        // Next we put the existing leaf as a child of this internal node.
        let existing_child_index = existing_leaf.key_hash().0.get_nibble(common_prefix_len);
        internal_info.set_child(
            u8::from(existing_child_index) as usize,
            ChildInfo::Leaf {
                node: existing_leaf,
            },
        );

        // Do not set the new child for now. We always call `freeze` first, then set the new child
        // later, because this way it's easier in `freeze` to find the correct leaf to freeze --
        // it's always the rightmost leaf on the lowest level.
        self.partial_nodes.push(internal_info);
        self.freeze(self.partial_nodes.len());

        // Now we set the new child.
        let new_child_index = new_key.0.get_nibble(common_prefix_len);
        assert!(
            new_child_index > existing_child_index,
            "New leaf must be on the right.",
        );
        self.partial_nodes
            .last_mut()
            .expect("This node must exist.")
            .set_child(
                u8::from(new_child_index) as usize,
                ChildInfo::Leaf {
                    node: LeafNode::new(new_key, value_hash),
                },
            );
    }

    /// Puts the nodes that will not be changed later in `self.frozen_nodes`.
    fn freeze(&mut self, num_remaining_partial_nodes: usize) {
        self.freeze_previous_leaf();
        self.freeze_internal_nodes(num_remaining_partial_nodes);
    }

    /// Freezes the previously added leaf node. It should always be the rightmost leaf node on the
    /// lowest level, inserted in the previous `add_one` call.
    fn freeze_previous_leaf(&mut self) {
        // If this is the very first key, there is no previous leaf to freeze.
        if self.num_keys_received == 0 {
            return;
        }

        let last_node = self
            .partial_nodes
            .last()
            .expect("Must have at least one partial node.");
        let rightmost_child_index = last_node
            .children
            .iter()
            .rposition(|x| x.is_some())
            .expect("Must have at least one child.");

        match last_node.children[rightmost_child_index] {
            Some(ChildInfo::Leaf { ref node }) => {
                let child_node_key = last_node
                    .node_key
                    .gen_child_node_key(self.version, (rightmost_child_index as u8).into());
                self.frozen_nodes
                    .insert_node(child_node_key, node.clone().into());
            }
            _ => panic!("Must have at least one child and must not have further internal nodes."),
        }
    }

    /// Freeze extra internal nodes. Only `num_remaining_nodes` partial internal nodes will be kept
    /// and the ones on the lower level will be frozen.
    fn freeze_internal_nodes(&mut self, num_remaining_nodes: usize) {
        while self.partial_nodes.len() > num_remaining_nodes {
            let last_node = self.partial_nodes.pop().expect("This node must exist.");
            let (node_key, internal_node) = last_node.into_internal_node::<H>(self.version);
            // Keep the hash of this node before moving it into `frozen_nodes`, so we can update
            // its parent later.
            let node_hash = internal_node.hash::<H>();
            let node_leaf_count = internal_node.leaf_count();
            self.frozen_nodes
                .insert_node(node_key, internal_node.into());

            // Now that we have computed the hash of the internal node above, we will also update
            // its parent unless it is root node.
            if let Some(parent_node) = self.partial_nodes.last_mut() {
                // This internal node must be the rightmost child of its parent at the moment.
                let rightmost_child_index = parent_node
                    .children
                    .iter()
                    .rposition(|x| x.is_some())
                    .expect("Must have at least one child.");

                match parent_node.children[rightmost_child_index] {
                    Some(ChildInfo::Internal {
                        ref mut hash,
                        ref mut leaf_count,
                    }) => {
                        assert_eq!(hash.replace(node_hash), None);
                        assert_eq!(*leaf_count, 0);
                        *leaf_count = node_leaf_count;
                    }
                    _ => panic!(
                        "Must have at least one child and the rightmost child must not be a leaf."
                    ),
                }
            }
        }
    }

    /// Verifies that all accounts that have been added so far (from the leftmost one to
    /// `self.previous_leaf`) are correct, i.e., we are able to construct `self.expected_root_hash`
    /// by combining all existing accounts and `proof`.
    #[allow(clippy::collapsible_if)]
    fn verify(&self, proof: SparseMerkleRangeProof<H>) -> Result<(), anyhow::Error> {
        let previous_leaf = self
            .previous_leaf
            .as_ref()
            .expect("The previous leaf must exist.");
        let previous_key = previous_leaf.key_hash();

        // If we have all siblings on the path from root to `previous_key`, we should be able to
        // compute the root hash. The siblings on the right are already in the proof. Now we
        // compute the siblings on the left side, which represent all the accounts that have ever
        // been added.
        let mut left_siblings = vec![];

        // The following process might add some extra placeholder siblings on the left, but it is
        // nontrivial to determine when the loop should stop. So instead we just add these
        // siblings for now and get rid of them in the next step.
        let mut num_visited_right_siblings = 0;
        for (i, bit) in previous_key.0.iter_bits().enumerate() {
            if bit {
                // This node is a right child and there should be a sibling on the left.
                let sibling = if i >= self.partial_nodes.len() * 4 {
                    SPARSE_MERKLE_PLACEHOLDER_HASH
                } else {
                    Self::compute_left_sibling(
                        &self.partial_nodes[i / 4],
                        previous_key.0.get_nibble(i / 4),
                        (3 - i % 4) as u8,
                    )
                };
                left_siblings.push(sibling);
            } else {
                // This node is a left child and there should be a sibling on the right.
                num_visited_right_siblings += 1;
            }
        }
        ensure!(
            num_visited_right_siblings >= proof.right_siblings().len(),
            "Too many right siblings in the proof.",
        );

        // Now we remove any extra placeholder siblings at the bottom. We keep removing the last
        // sibling if 1) it's a placeholder 2) it's a sibling on the left.
        for bit in previous_key.0.iter_bits().rev() {
            if bit {
                if *left_siblings.last().expect("This sibling must exist.")
                    == SPARSE_MERKLE_PLACEHOLDER_HASH
                {
                    left_siblings.pop();
                } else {
                    break;
                }
            } else if num_visited_right_siblings > proof.right_siblings().len() {
                num_visited_right_siblings -= 1;
            } else {
                break;
            }
        }

        // Left siblings must use the same ordering as the right siblings in the proof
        left_siblings.reverse();

        // Verify the proof now that we have all the siblings
        proof
            .verify(
                self.expected_root_hash,
                SparseMerkleLeafNode::new(previous_key, previous_leaf.value_hash()),
                left_siblings,
            )
            .map_err(Into::into)
    }

    /// Computes the sibling on the left for the `n`-th child.
    fn compute_left_sibling(partial_node: &InternalInfo, n: Nibble, height: u8) -> [u8; 32] {
        assert!(height < 4);
        let width = 1usize << height;
        let start = get_child_and_sibling_half_start(n, height).1 as usize;
        Self::compute_left_sibling_impl(&partial_node.children[start..start + width]).0
    }

    /// Returns the hash for given portion of the subtree and whether this part is a leaf node.
    fn compute_left_sibling_impl(children: &[Option<ChildInfo>]) -> ([u8; 32], bool) {
        assert!(!children.is_empty());

        let num_children = children.len();
        assert!(num_children.is_power_of_two());

        if num_children == 1 {
            match &children[0] {
                Some(ChildInfo::Internal { hash, .. }) => {
                    (*hash.as_ref().expect("The hash must be known."), false)
                }
                Some(ChildInfo::Leaf { node }) => (node.hash::<H>(), true),
                None => (SPARSE_MERKLE_PLACEHOLDER_HASH, true),
            }
        } else {
            let (left_hash, left_is_leaf) =
                Self::compute_left_sibling_impl(&children[..num_children / 2]);
            let (right_hash, right_is_leaf) =
                Self::compute_left_sibling_impl(&children[num_children / 2..]);

            if left_hash == SPARSE_MERKLE_PLACEHOLDER_HASH && right_is_leaf {
                (right_hash, true)
            } else if left_is_leaf && right_hash == SPARSE_MERKLE_PLACEHOLDER_HASH {
                (left_hash, true)
            } else {
                (
                    SparseMerkleInternalNode::new(left_hash, right_hash).hash::<H>(),
                    false,
                )
            }
        }
    }

    /// Finishes the restoration process. This tells the code that there is no more account,
    /// otherwise we can not freeze the rightmost leaf and its ancestors.
    fn finish_impl(mut self) -> Result<(), WriteError> {
        // Deal with the special case when the entire tree has a single leaf.
        if self.partial_nodes.len() == 1 {
            let mut num_children = 0;
            let mut leaf = None;
            for i in 0..16 {
                if let Some(ref child_info) = self.partial_nodes[0].children[i] {
                    num_children += 1;
                    if let ChildInfo::Leaf { node } = child_info {
                        leaf = Some(node.clone());
                    }
                }
            }

            if num_children == 1 {
                if let Some(node) = leaf {
                    let node_key = NodeKey::new_empty_path(self.version);
                    assert!(self.frozen_nodes.is_empty());
                    self.frozen_nodes.insert_node(node_key, node.into());
                    self.store.write_node_batch(&self.frozen_nodes)?;
                    return Ok(());
                }
            }
        }

        self.freeze(0);
        self.store.write_node_batch(&self.frozen_nodes)
    }
}

/// The interface used with [`JellyfishMerkleRestore`], taken from the Diem `storage-interface` crate.
pub trait StateSnapshotReceiver<H: SimpleHasher> {
    /// The kind of error that may be returned by [`StateSnapshotReceiver::add_chunk()`].
    type AddChunkError;

    fn add_chunk(
        &mut self,
        chunk: Vec<(KeyHash, OwnedValue)>,
        proof: SparseMerkleRangeProof<H>,
    ) -> Result<(), Self::AddChunkError>;

    /// The kind of error that may be returned by [`StateSnapshotReceiver::finish()`].
    type FinishError;

    fn finish(self) -> Result<(), Self::FinishError>;

    fn finish_box(self: Box<Self>) -> Result<(), Self::FinishError>;
}

impl<H: SimpleHasher, E> StateSnapshotReceiver<H> for JellyfishMerkleRestore<H, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    /// Errors that may be returned by [`StateSnapshotReceiver::add_chunk()`].
    type AddChunkError = RestoreError<E>;

    fn add_chunk(
        &mut self,
        chunk: Vec<(KeyHash, OwnedValue)>,
        proof: SparseMerkleRangeProof<H>,
    ) -> Result<(), Self::AddChunkError> {
        self.add_chunk_impl(chunk, proof)
    }

    type FinishError = E;

    fn finish(self) -> Result<(), E> {
        self.finish_impl()
    }

    fn finish_box(self: Box<Self>) -> Result<(), E> {
        self.finish_impl()
    }
}
