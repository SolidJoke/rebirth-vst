//! The plugin's `process` (and `reset`, which hosts may call on the audio thread) allocates and
//! frees nothing, and the plugin's code names no lock (RV-02).
//!
//! As `orb-player`'s `no_alloc` test: this binary's global allocator counts the allocations,
//! reallocations and frees made on a thread while it is armed; the test arms itself around each
//! call of `process` only (the stand-in host's buffer setup allocates, outside). The calls play
//! notes, accents, overlapping notes and releases, at 44.1 kHz (direct) and at 48 and 96 kHz
//! (resampled), with events inside host buffers.
//!
//! The lock check is textual and narrow, as `orb-player`'s: `crates/orb-vst/src` must not name
//! `Mutex`, `RwLock`, `Condvar`, `Barrier`, `Once`, `mpsc` or `parking_lot`. The engine's sources
//! are checked by `orb-player`'s `the_callback_crates_hold_no_lock`; the note queue is `rtrb`'s
//! lock-free one, and rubato's `process_into_buffer` is lock-free by its documentation. The code
//! nih-plug's VST3 wrapper runs around `process` is not covered here.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Host, off, on, plugin};

struct Counting;

static EVENTS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if ARMED.try_with(|a| a.get()).unwrap_or(false) {
        EVENTS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Runs `f` armed and returns the allocator events it caused.
fn armed(f: impl FnOnce()) -> usize {
    let before = EVENTS.load(Ordering::Relaxed);
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    EVENTS.load(Ordering::Relaxed) - before
}

/// The counter works: an allocation made armed is counted.
#[test]
fn the_counter_counts() {
    let n = armed(|| drop(std::hint::black_box(vec![1u8; 10])));
    assert_eq!(n, 2);
}

#[test]
fn process_and_reset_do_not_allocate() {
    for rate in [44_100, 48_000, 96_000] {
        let (mut p, _) = plugin(rate);
        // overlapping notes, accents, a note-off of a note no longer held, events mid-buffer
        let events = [
            on(0, 36, 80),
            on(1_000, 43, 110),
            off(1_500, 36),
            on(3_000, 61, 127),
            off(5_000, 61),
            off(6_100, 43),
            on(9_000, 24, 100),
            off(20_000, 24),
        ];
        let mut host = Host::new(128 * 256, &events);
        let mut n = 0;
        for k in 0..host.buffers() {
            host.buffer(&mut p, k, |f| n += armed(f));
            if k == 100 {
                n += armed(|| nih_plug::prelude::Plugin::reset(&mut p));
            }
        }
        assert!(common::peak(&host.l) > 0.01, "{rate} Hz: silent");
        assert_eq!(n, 0, "{rate} Hz: process allocated or freed {n} times");
    }
}

/// No lock type in the plugin's sources.
#[test]
fn the_plugin_names_no_lock() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let text = std::fs::read_to_string(&p).unwrap();
        for w in [
            "Mutex",
            "RwLock",
            "Condvar",
            "Barrier",
            "Once",
            "mpsc",
            "parking_lot",
        ] {
            if text.contains(w) {
                bad.push(format!("{}: {w}", p.display()));
            }
        }
    }
    assert!(bad.is_empty(), "{bad:?}");
}
