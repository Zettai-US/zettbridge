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
