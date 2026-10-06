#[path = "../accounting.rs"]
mod accounting;
#[path = "../capacity.rs"]
mod capacity;
#[path = "../extent.rs"]
mod extent;
#[path = "../uapi.rs"]
mod uapi;
use accounting::*;
#[test]
fn golden_extent() {
    let b = extent::encode(0x1000, 0x2000, 0x1234000, 9, 1).unwrap();
    assert_eq!(&b[0..8], &[0, 16, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&b[16..24], &[0, 64, 35, 1, 0, 0, 0, 0]);
    assert_eq!(&b[36..42], &[7, 0, 0, 0, 1, 0]);
    assert!(b[42..].iter().all(|b| *b == 0));
    assert!(extent::encode(0, 0, 0, 1, 0).is_none());
    assert!(extent::encode(0, 4096, u64::MAX - 4095, 1, 0).is_none());
}
#[test]
fn spec_accounting() {
    let g = 1 << 30;
    let c = Counters {
        capacity: 80 * g,
        allocated: 20 * g,
        releasing: 4 * g,
        quarantined: 2 * g,
        requested: 19 * g,
    };
    assert_eq!(c.available(), Some(54 * g));
    assert!(Counters {
        capacity: 1,
        allocated: 2,
        ..Default::default()
    }
    .available()
    .is_none());
}
#[test]
fn isolation_and_reuse() {
    let mut l = Ledger::new();
    l.capacity = 16384;
    let a = l.alloc(10, 1, 4096).unwrap();
    let b = l.alloc(11, 4097, 4096).unwrap();
    assert_eq!(a.bytes, 4096);
    assert_eq!(b.start, 4096);
    assert_eq!(l.counters().unwrap().requested, 4098);
    assert!(l.release(11, a.handle).is_err());
    l.release(10, a.handle).unwrap();
    assert_eq!(l.alloc(12, 4096, 4096).unwrap().start, 12288);
    assert!(l.alloc(12, 1, 4096).is_err()); // RELEASING cannot be reused
    l.get_mut(10, a.handle).unwrap().state = 3;
    assert_eq!(l.counters().unwrap().quarantined, 4096);
}
#[test]
fn overflow_and_layout() {
    let mut l = Ledger::new();
    l.capacity = 65536;
    assert!(l.alloc(1, u64::MAX, 4096).is_err());
    assert!(l.alloc(1, 1, 6000).is_err());
    assert_eq!(l.counters().unwrap().allocated, 0);
    assert_eq!(std::mem::size_of::<uapi::Request>(), 64);
    assert_eq!(std::mem::size_of::<uapi::Stats>(), 256);
    assert_eq!(std::mem::offset_of!(uapi::Stats, counters), 0x50);
}
#[test]
fn fragmented_alignment() {
    let mut l = Ledger::new();
    l.capacity = 8 * 4096;
    for i in 0..4 {
        l.alloc(i, 4096, 8192).unwrap();
    }
    assert_eq!(l.counters().unwrap().available(), Some(4 * 4096));
    assert!(l.alloc(99, 8192, 4096).is_err());
}

#[test]
fn large_pool_and_mmap_handle_bounds() {
    assert!(capacity::valid_pool(8 << 30));
    assert!(!capacity::valid_pool(0));
    assert!(!capacity::valid_pool((8 << 30) + 4096));
    assert!(!capacity::valid_pool(
        capacity::MAX_POOL_BYTES + (256 << 20)
    ));
    let mut l = Ledger::new();
    l.capacity = 8 << 30;
    assert!(l.alloc(1, (1 << 32) + 1, 4096).is_err());
    assert_eq!(l.counters().unwrap().allocated, 0);
    let low = l.alloc(1, 1 << 32, 4096).unwrap();
    let high = l.alloc(1, 1 << 32, 4096).unwrap();
    assert_eq!(low.start, 0);
    assert_eq!(high.start, 1 << 32);
    assert_eq!(high.start + high.bytes, 8 << 30);
    assert_eq!(l.counters().unwrap().available(), Some(0));
}

#[test]
fn release_arriving_during_vma_zap_waits_for_next_batch() {
    let mut ledger = Ledger::new();
    ledger.capacity = 8192;
    let a = ledger.alloc(1, 4096, 4096).unwrap();
    let b = ledger.alloc(2, 4096, 4096).unwrap();
    ledger.release(1, a.handle).unwrap();
    ledger.begin_revoke();
    // Worker drops State lock to zap VMAs. B may fault again and then RELEASE.
    ledger.get_mut(2, b.handle).unwrap().mapped = true;
    ledger.release(2, b.handle).unwrap();
    let completed: Vec<_> = ledger.finish_revoke().map(|l| l.handle).collect();
    assert_eq!(completed, [a.handle]);
    assert_eq!(ledger.counters().unwrap().releasing, 4096);
    let c = ledger.alloc(3, 4096, 4096).unwrap();
    assert_eq!(c.start, a.start);
    assert!(ledger.alloc(4, 4096, 4096).is_err());
    ledger.begin_revoke();
    assert_eq!(ledger.finish_revoke().next().unwrap().handle, b.handle);
    assert_eq!(ledger.alloc(4, 4096, 4096).unwrap().start, b.start);
}

#[test]
fn fault_during_revoke_never_reclaims_quarantine() {
    let mut ledger = Ledger::new();
    ledger.capacity = 4096;
    let a = ledger.alloc(1, 4096, 4096).unwrap();
    ledger.release(1, a.handle).unwrap();
    ledger.begin_revoke();
    ledger.get_mut(1, a.handle).unwrap().state = 3;
    assert_eq!(ledger.finish_revoke().count(), 0);
    assert_eq!(ledger.counters().unwrap().quarantined, 4096);
    assert!(ledger.alloc(2, 4096, 4096).is_err());
}

#[test]
fn full_ledger_and_handle_exhaustion_are_atomic() {
    let mut ledger = Ledger::new();
    ledger.capacity = (MAX_LEASES as u64 + 1) * 4096;
    for owner in 0..MAX_LEASES {
        ledger.alloc(owner as u64, 1, 4096).unwrap();
    }
    let before = ledger.counters().unwrap();
    assert!(ledger.alloc(999, 1, 4096).is_err());
    assert_eq!(ledger.counters().unwrap(), before);
    for l in &mut ledger.leases {
        l.state = 2;
    }
    ledger.begin_revoke();
    assert_eq!(ledger.finish_revoke().count(), MAX_LEASES);
    ledger.next_handle = (1 << 30) - 1;
    ledger.alloc(1, 1, 4096).unwrap();
    let before = ledger.counters().unwrap();
    assert_eq!(ledger.alloc(2, 1, 4096).unwrap_err(), -75);
    assert_eq!(ledger.counters().unwrap(), before);
}
