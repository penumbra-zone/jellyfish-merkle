// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, sync::Arc};

use anyhow::Result;
use rand::{rngs::StdRng, SeedableRng};

use super::{
    helper::{plus_one, ValueBlob},
    mock_tree_store::MockTreeStore,
};
use crate::{
    hash::HashValue, iterator::JellyfishMerkleIterator, types::Version, JellyfishMerkleTree,
};

#[test]
fn test_iterator_same_version() {
    for i in (1..100).step_by(11) {
        test_n_leaves_same_version(i);
    }
}

#[test]
fn test_iterator_multiple_versions() {
    test_n_leaves_multiple_versions(50);
}

#[test]
fn test_long_path() {
    test_n_consecutive_addresses(50);
}

fn test_n_leaves_same_version(n: usize) {
    let db = Arc::new(MockTreeStore::default());
    let tree = JellyfishMerkleTree::new(&*db);

    let mut rng = StdRng::from_seed([1; 32]);

    let mut btree = BTreeMap::new();
    for i in 0..n {
        let key = HashValue::random_with_rng(&mut rng);
        let value = ValueBlob::from(i.to_be_bytes().to_vec());
        assert_eq!(btree.insert(key, value), None);
    }

    let (_root_hash, batch) = tree
        .put_value_set(btree.clone().into_iter().collect(), 0 /* version */)
        .unwrap();
    db.write_tree_update_batch(batch).unwrap();

    run_tests(db, &btree, 0 /* version */);
}

fn test_n_leaves_multiple_versions(n: usize) {
    let db = Arc::new(MockTreeStore::default());
    let tree = JellyfishMerkleTree::new(&*db);

    let mut rng = StdRng::from_seed([1; 32]);

    let mut btree = BTreeMap::new();
    for i in 0..n {
        let key = HashValue::random_with_rng(&mut rng);
        let value = ValueBlob::from(i.to_be_bytes().to_vec());
        assert_eq!(btree.insert(key, value.clone()), None);
        let (_root_hash, batch) = tree
            .put_value_set(vec![(key, value)], i as Version)
            .unwrap();
        db.write_tree_update_batch(batch).unwrap();
        run_tests(Arc::clone(&db), &btree, i as Version);
    }
}

fn test_n_consecutive_addresses(n: usize) {
    let db = Arc::new(MockTreeStore::default());
    let tree = JellyfishMerkleTree::new(&*db);

    let btree: BTreeMap<_, _> = (0..n)
        .map(|i| {
            (
                HashValue::from_u64(i as u64),
                ValueBlob::from(i.to_be_bytes().to_vec()),
            )
        })
        .collect();

    let (_root_hash, batch) = tree
        .put_value_set(btree.clone().into_iter().collect(), 0 /* version */)
        .unwrap();
    db.write_tree_update_batch(batch).unwrap();

    run_tests(db, &btree, 0 /* version */);
}

fn run_tests<V>(db: Arc<MockTreeStore<V>>, btree: &BTreeMap<HashValue, V>, version: Version)
where
    V: crate::tests::TestValue,
{
    {
        let iter =
            JellyfishMerkleIterator::new(Arc::clone(&db), version, HashValue::zero()).unwrap();
        assert_eq!(
            iter.collect::<Result<Vec<_>>>().unwrap(),
            btree.clone().into_iter().collect::<Vec<_>>(),
        );
    }

    for i in 0..btree.len() {
        {
            let iter = JellyfishMerkleIterator::new_by_index(Arc::clone(&db), version, i).unwrap();
            assert_eq!(
                iter.collect::<Result<Vec<_>>>().unwrap(),
                btree.clone().into_iter().skip(i).collect::<Vec<_>>(),
            );
        }

        let ith_key = *btree.keys().nth(i).unwrap();

        {
            let iter = JellyfishMerkleIterator::new(Arc::clone(&db), version, ith_key).unwrap();
            assert_eq!(
                iter.collect::<Result<Vec<_>>>().unwrap(),
                btree.clone().into_iter().skip(i).collect::<Vec<_>>(),
            );
        }

        {
            let ith_key_plus_one = plus_one(ith_key);
            let iter =
                JellyfishMerkleIterator::new(Arc::clone(&db), version, ith_key_plus_one).unwrap();
            assert_eq!(
                iter.collect::<Result<Vec<_>>>().unwrap(),
                btree.clone().into_iter().skip(i + 1).collect::<Vec<_>>(),
            );
        }
    }

    {
        let iter =
            JellyfishMerkleIterator::new_by_index(Arc::clone(&db), version, btree.len()).unwrap();
        assert_eq!(iter.collect::<Result<Vec<_>>>().unwrap(), vec![]);
    }

    {
        let iter = JellyfishMerkleIterator::new(
            Arc::clone(&db),
            version,
            HashValue::new([0xFF; HashValue::LENGTH]),
        )
        .unwrap();
        assert_eq!(iter.collect::<Result<Vec<_>>>().unwrap(), vec![]);
    }
}
