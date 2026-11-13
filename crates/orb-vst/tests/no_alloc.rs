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

use common::{Host, off, off_ch, on, on_ch, plugin};
use orb_vst::KeyboardTarget;
use orb_vst::synth::Voice;

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

/// RV-03's paths: legato slides, the return to a held key, notes on both 303s by channel, a key
/// pressed beyond the stack's depth, and a change of the keyboard target (which releases every
/// key) between two buffers.
#[test]
fn legato_two_voices_and_a_target_change_do_not_allocate() {
    for rate in [44_100, 48_000, 96_000] {
        let (mut p, _) = plugin(rate);
        let mut events = vec![
            // 303 #1: a held key, a legato slide over it, the return to the held key
            on(0, 36, 80),
            on(1_000, 43, 110),
            off(3_000, 43),
            // a key that is not sounding leaves, then the sounding one
            on(4_000, 40, 80),
            on(4_500, 47, 100),
            off(5_000, 40),
            off(6_000, 47),
            // 303 #2 (MIDI channel 2) at the same time as 303 #1
            on_ch(8_000, 2, 48, 90),
            on_ch(8_500, 2, 55, 127),
            on_ch(9_000, 1, 36, 80),
            off_ch(11_000, 2, 55),
            off_ch(12_000, 2, 48),
            off_ch(13_000, 1, 36),
            // keys beyond the stack's 16, then all of them up
        ];
        for k in 0..20u8 {
            events.push(on(14_000 + k as usize * 10, 30 + k, 80));
        }
        for k in 0..20u8 {
            events.push(off(16_000 + k as usize * 10, 30 + k));
        }
        // keys held across the target change, which releases them
        events.push(on_ch(20_000, 1, 36, 80));
        events.push(on_ch(20_500, 2, 43, 80));
        let mut host = Host::new(128 * 256, &events);
        let mut n = 0;
        for k in 0..host.buffers() {
            // the host changes the parameter between two process calls (the change itself is
            // atomic stores in nih-plug's wrapper, outside the plugin's code)
            if k == 200 {
                common::set_target(&p, KeyboardTarget::B);
            }
            host.buffer(&mut p, k, |f| n += armed(f));
        }
        assert!(common::peak(&host.l) > 0.01, "{rate} Hz: silent");
        let s = p.synth().unwrap();
        assert_eq!(
            s.held(Voice::A) + s.held(Voice::B),
            0,
            "{rate} Hz: keys held"
        );
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
