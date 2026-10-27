// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::num::NonZeroU64;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::file_data_cache::{
    CacheAccess, CacheKey, ContentRevision, FileDataCache, FileIdentity, FilePage, FilePageIndex,
    FilesystemGeneration, NodeIdentity,
};
use hyper_filesystem::protocol::{
    DIRECTORY_BATCH_BYTES, DirectoryBatchBuilder, DirectoryCursor, Entry, FILE, entry_encode_into,
};

#[path = "../../../../src/fs/vfs/remote/cache.rs"]
mod cache;
#[path = "../../../../src/fs/vfs/remote/records/tree.rs"]
mod tree;

fn identity(revision: u64) -> FileIdentity {
    FileIdentity::new(
        FilesystemGeneration::new(NonZeroU64::MIN),
        NodeIdentity::new(NonZeroU64::MIN),
        ContentRevision::new(NonZeroU64::new(revision).unwrap_or(NonZeroU64::MIN)),
    )
}

fn entry(name: &str) -> Entry {
    let mut entry = Entry::empty();
    entry.kind = FILE;
    entry.mode = 0o644;
    entry.size = 8192;
    entry.name_len = name.len();
    entry.name[..name.len()].copy_from_slice(name.as_bytes());
    entry
}

fn publish<Owner>(cache: &FileDataCache<FilePage<Owner>>, key: CacheKey, payload: FilePage<Owner>) {
    let CacheAccess::Load(load) = crate::require_ok(cache.access(key)) else {
        panic!("test cache must admit a fresh payload");
    };
    drop(crate::require_ok(load.fill_and_publish(
        || Ok(payload),
        |_| panic!("test cache must not reuse a payload"),
    )));
}

#[test]
fn same_numeric_key_keeps_different_cache_payloads_separate() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let keys = [
        CacheKey::new(identity(1), FilePageIndex::new(0)),
        CacheKey::directory(identity(1), 0),
        CacheKey::lookup(identity(1), 0),
    ];
    for (value, key) in keys.into_iter().enumerate() {
        publish(
            &cache,
            key,
            crate::require_ok(FilePage::try_copy(&[value as u8], ())),
        );
    }
    for (value, key) in keys.into_iter().enumerate() {
        let Some(page) = crate::require_ok(cache.lookup(key)) else {
            panic!("cached kind missing");
        };
        assert_eq!(page.value().bytes(), &[value as u8]);
    }
    assert_eq!(cache.reclaim_batch(4), 3);
    assert_eq!(cache.snapshot().live_payloads, 0);
}

#[test]
fn name_cache_validates_full_key_and_revision_and_bypasses_oversize_keys() {
    let cache = crate::require_ok(FileDataCache::try_new(8));
    let expected = entry("readme");
    cache::retain_entry(&cache, identity(1), "etc/readme", &expected, &());
    let mut output = Entry::empty();
    assert!(cache::lookup_entry(
        &cache,
        identity(1),
        "etc/readme",
        &mut output
    ));
    assert_eq!(output.name(), "readme");
    assert_eq!(output.size, expected.size);
    assert!(!cache::lookup_entry(
        &cache,
        identity(2),
        "etc/readme",
        &mut output
    ));
    assert!(!cache::lookup_entry(
        &cache,
        identity(1),
        "etc/README",
        &mut output
    ));

    let before = cache.snapshot();
    cache::retain_entry(&cache, identity(1), &"x".repeat(4096), &expected, &());
    assert_eq!(cache.snapshot().live_payloads, before.live_payloads);
    assert_eq!(cache.snapshot().evictions, before.evictions);
    cache.set_admission(false);
    cache::retain_entry(&cache, identity(1), "another", &expected, &());
    assert!(!cache::lookup_entry(
        &cache,
        identity(1),
        "another",
        &mut output
    ));
}

#[test]
fn colliding_name_hash_is_a_miss_not_an_alias() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let mut encoded = [0; 1152];
    let length = entry_encode_into(&entry("B"), &mut encoded).unwrap_or(0);
    assert_ne!(length, 0);
    let key_length = 1_u64.to_le_bytes();
    // FNV-1a("A"). Deliberately place B's complete payload under A's key.
    let key = CacheKey::lookup(identity(1), 0xaf63_fc4c_8602_22ec);
    publish(
        &cache,
        key,
        crate::require_ok(FilePage::try_copy_parts(
            &[&key_length, b"B", &encoded[..length]],
            (),
        )),
    );
    let mut output = Entry::empty();
    assert!(!cache::lookup_entry(&cache, identity(1), "A", &mut output));
    cache::retain_entry(&cache, identity(1), "A", &entry("A"), &());
    assert!(!cache::lookup_entry(&cache, identity(1), "A", &mut output));
}

fn batch(bytes: &mut [u8], start: DirectoryCursor) -> usize {
    let Some(mut builder) = DirectoryBatchBuilder::new(bytes, start) else {
        panic!("batch buffer");
    };
    assert_eq!(
        builder.push(
            &entry("item"),
            DirectoryCursor {
                words: [19, 1, 1, 0]
            }
        ),
        Some(true)
    );
    builder.finish(true).unwrap_or(0)
}

#[test]
fn directory_pages_and_their_identity_owners_retire_after_pressure_and_last_pin() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let owner = Arc::new(17_u64);
    let weak = Arc::downgrade(&owner);
    let mut bytes = [0; DIRECTORY_BATCH_BYTES];
    let length = batch(&mut bytes, DirectoryCursor::START);
    cache::retain_batch(
        &cache,
        identity(1),
        DirectoryCursor::START,
        &bytes[..length],
        &owner,
    );
    let Some(pin) = cache::lookup_batch(&cache, identity(1), DirectoryCursor::START) else {
        panic!("directory cache hit");
    };
    drop(owner);
    cache.set_admission(false);
    assert_eq!(cache.reclaim_batch(4), 1);
    assert!(cache::lookup_batch(&cache, identity(1), DirectoryCursor::START).is_none());
    assert!(weak.upgrade().is_some());
    assert_eq!(cache.snapshot().live_payloads, 1);
    drop(pin);
    assert!(weak.upgrade().is_none());
    assert_eq!(cache.snapshot().live_payloads, 0);
}

#[test]
fn cached_directory_cursor_collision_cannot_select_another_continuation() {
    let cache = crate::require_ok(FileDataCache::try_new(4));
    let different = DirectoryCursor {
        words: [42, 3, 8, 0],
    };
    let mut bytes = [0; DIRECTORY_BATCH_BYTES];
    let length = batch(&mut bytes, different);
    // FNV-1a of the 32 zero bytes representing START.
    let start_hash = [0_u8; 32]
        .into_iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    publish(
        &cache,
        CacheKey::directory(identity(1), start_hash),
        crate::require_ok(FilePage::try_copy(&bytes[..length], ())),
    );
    assert!(cache::lookup_batch(&cache, identity(1), DirectoryCursor::START).is_none());
}

#[test]
fn record_index_preserves_order_and_ownership_through_rotations_and_removal() {
    let mut tree = tree::Tree::new();
    let mut expected = BTreeMap::new();
    // Monotonic identities stress the old list's growing lookup cost and force
    // every AVL rotation direction once mixed insertion/removal begins.
    for id in 0..8192_u64 {
        tree.insert(Box::new(tree::Node::new(id, id ^ 0xfeed)));
        expected.insert(id, id ^ 0xfeed);
    }
    assert!(tree.height_for_test() <= 27);
    let mut random = 0x4d59_5df4_d0f3_3173_u64;
    for _ in 0..24000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let id = random % 12288;
        let removed = tree.remove(id);
        assert_eq!(
            removed.as_ref().map(|node| node.value),
            expected.remove(&id)
        );
        if random & 4 != 0 {
            let node = removed.unwrap_or_else(|| Box::new(tree::Node::new(id, id ^ 0xfeed)));
            tree.insert(node);
            expected.insert(id, id ^ 0xfeed);
        }
    }
    assert!(tree.height_for_test() <= 28);
    if let Some(value) = tree.get_mut(7) {
        *value = 42;
        expected.insert(7, 42);
    }
    let mut previous = None;
    for (key, value) in expected {
        assert_eq!(tree.next(previous), Some((key, &value)));
        assert_eq!(tree.get(key), Some(&value));
        previous = Some(key);
    }
    assert_eq!(tree.next(previous), None);
}
