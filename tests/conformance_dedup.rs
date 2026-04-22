//! Conformance tests for gift-wrap deduplication via LRU cache.
//!
//! Both the client and server transports use an `LruCache<EventId, ()>` to skip
//! duplicate outer gift-wrap event IDs. The dedup check happens *before* decrypt
//! and the insert happens only *after* successful decrypt + inner `verify()`.
//! These tests exercise the LRU cache logic in isolation — no async, no transport.

use std::num::NonZeroUsize;

use lru::LruCache;
use nostr_sdk::prelude::*;

use contextvm_sdk::core::constants::DEFAULT_LRU_SIZE;

/// Helper: build a cache with the same capacity used by both transports.
fn new_dedup_cache() -> LruCache<EventId, ()> {
    LruCache::new(NonZeroUsize::new(DEFAULT_LRU_SIZE).expect("DEFAULT_LRU_SIZE must be non-zero"))
}

fn event_id_from_byte(b: u8) -> EventId {
    EventId::from_byte_array([b; 32])
}

// ── Gift-wrap kind 1059 dedup ─────────────────────────────────────────────────

#[test]
fn client_dedup_skips_duplicate_outer_gift_wrap_id() {
    let mut cache = new_dedup_cache();
    let outer_id = event_id_from_byte(0x01);

    // First delivery: not yet seen, decrypt succeeds, insert into cache.
    assert!(
        !cache.contains(&outer_id),
        "first delivery must not be in cache yet"
    );
    cache.put(outer_id, ());

    // Second delivery: same outer id is already cached, skip before decrypt.
    assert!(
        cache.contains(&outer_id),
        "second delivery of the same outer id must be rejected"
    );
}

#[test]
fn client_dedup_ephemeral_gift_wrap_skips_duplicate() {
    let mut cache = new_dedup_cache();
    let ephemeral_outer_id = event_id_from_byte(0xE1);

    // First delivery of an ephemeral gift-wrap (kind 21059).
    assert!(
        !cache.contains(&ephemeral_outer_id),
        "first delivery must not be in cache yet"
    );
    cache.put(ephemeral_outer_id, ());

    // Second delivery: same outer id is already cached, skip before decrypt.
    assert!(
        cache.contains(&ephemeral_outer_id),
        "second delivery of the same ephemeral outer id must be rejected"
    );
}

// ── Server dedup ──────────────────────────────────────────────────────────────

#[test]
fn server_dedup_ephemeral_gift_wrap_skips_duplicate() {
    let mut cache = new_dedup_cache();
    let ephemeral_outer_id = event_id_from_byte(0xE2);

    // First delivery of an ephemeral gift-wrap (kind 21059).
    assert!(
        !cache.contains(&ephemeral_outer_id),
        "first delivery must not be in cache yet"
    );
    cache.put(ephemeral_outer_id, ());

    // Second delivery: same outer id is already cached, skip before decrypt.
    assert!(
        cache.contains(&ephemeral_outer_id),
        "second delivery of the same ephemeral outer id must be rejected"
    );
}

#[test]
fn server_dedup_lru_evicts_oldest_when_capacity_reached() {
    let capacity = 3;
    let mut cache: LruCache<EventId, ()> =
        LruCache::new(NonZeroUsize::new(capacity).expect("non-zero"));

    let id_0 = event_id_from_byte(0x00);
    let id_1 = event_id_from_byte(0x01);
    let id_2 = event_id_from_byte(0x02);
    let id_3 = event_id_from_byte(0x03);

    cache.put(id_0, ());
    cache.put(id_1, ());
    cache.put(id_2, ());

    // Cache is at capacity (3). Inserting a fourth must evict the oldest (id_0).
    cache.put(id_3, ());

    assert!(
        !cache.contains(&id_0),
        "oldest entry must be evicted when capacity is exceeded"
    );
    assert!(cache.contains(&id_1), "second entry must still be present");
    assert!(cache.contains(&id_2), "third entry must still be present");
    assert!(
        cache.contains(&id_3),
        "newly inserted entry must be present"
    );
}
