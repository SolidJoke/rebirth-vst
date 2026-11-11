//! The plugin played from a keyboard, through its `process`, without a host (RV-02): a note
//! sounds with ReBirth's transport stopped, stops after its note-off, accents from velocity 100,
//! folds into the 303's three octaves, and runs at the host's rate with the latency it reports.

mod common;

use common::{off, on, onset, peak, pitch, plugin, render, rms};
use orb_vst::synth::{ORIGIN, step_for};

/// C2 (MIDI 36): the 303's step `{note 0}` at the default tune.
const C2: u8 = ORIGIN;
/// A velocity below the accent threshold, and one above.
const SOFT: u8 = 80;
const HARD: u8 = 110;

/// AC: a note-on then a note-off at 48 kHz; sound between them, silence after the release.
#[test]
fn a_note_sounds_with_the_transport_stopped_and_dies_after_its_note_off() {
    let rate = 48_000;
    let (mut p, _) = plugin(rate);
    // the note held 0.5 s, then 1.5 s of release
    let (l, r) = render(&mut p, 2 * 48_000, &[on(0, C2, SOFT), off(24_000, C2)]);
    let held = rms(&l[2_400..24_000]);
    let held_r = rms(&r[2_400..24_000]);
    let tail = peak(&l[72_000..]).max(peak(&r[72_000..]));
    println!("held rms L {held:.4} R {held_r:.4}, tail peak {tail:e}");
    assert!(
        held > 0.01 && held_r > 0.01,
        "silent while held: {held} {held_r}"
    );
    assert!(tail < 1e-4, "still sounding 1 s after the note-off: {tail}");
    let s = p.synth().unwrap();
    assert!(!s.transport_playing(), "the transport was started");
    assert_eq!(s.errors(), 0);
}

/// AC: the same note at velocity >= 100 (accent) renders differently.
#[test]
fn an_accented_note_differs_from_the_same_note_unaccented() {
    let events = |v| [on(0, C2, v), off(12_000, C2)];
    let (mut a, _) = plugin(48_000);
    let (soft, _) = render(&mut a, 24_064, &events(SOFT));
    let (mut b, _) = plugin(48_000);
    let (hard, _) = render(&mut b, 24_064, &events(HARD));
    let diff = soft
        .iter()
        .zip(&hard)
        .fold(0.0f32, |m, (x, y)| m.max((x - y).abs()));
    println!(
        "max |soft - hard| {diff}, peaks {} {}",
        peak(&soft),
        peak(&hard)
    );
    assert!(diff > 0.01, "the accent changed nothing: {diff}");
    // and the soft velocities below the threshold do not accent
    let (mut c, _) = plugin(48_000);
    let (softer, _) = render(&mut c, 24_064, &events(99));
    assert_eq!(soft, softer);
}

/// MIDI notes fold by octaves into the 303's C1..C4 and split into its step bytes.
#[test]
fn midi_notes_fold_into_the_303s_three_octaves() {
    // [note, slide, accent, up, down, rest]
    assert_eq!(step_for(36, 64), [0, 0, 0, 0, 0, 0]);
    assert_eq!(step_for(24, 64), [0, 0, 0, 0, 1, 0]);
    assert_eq!(step_for(35, 64), [11, 0, 0, 0, 1, 0]);
    assert_eq!(step_for(48, 64), [12, 0, 0, 0, 0, 0]);
    assert_eq!(step_for(49, 64), [1, 0, 0, 1, 0, 0]);
    assert_eq!(step_for(60, 64), [12, 0, 0, 1, 0, 0]);
    // out of range: by octaves
    assert_eq!(step_for(23, 64), step_for(35, 64));
    assert_eq!(step_for(0, 64), step_for(24, 64));
    assert_eq!(step_for(61, 64), step_for(49, 64));
    assert_eq!(step_for(127, 64), step_for(55, 64));
    // the accent threshold
    assert_eq!(step_for(36, 99)[2], 0);
    assert_eq!(step_for(36, 100)[2], 1);
}

/// The plugin plays at 44.1, 48 and 96 kHz: C2 sounds at 65.41 Hz at every rate (the pitch
/// table's entry 3072 is `55 * 2^(3/12)` Hz), and is released after its note-off.
#[test]
fn every_host_rate_plays_c2_at_its_pitch() {
    let c2 = 55.0 * 2f64.powf(3.0 / 12.0);
    for rate in [44_100, 48_000, 96_000] {
        let frames = (rate as usize / 128) * 128;
        let (mut p, _) = plugin(rate);
        let half = frames / 2;
        let (l, _) = render(&mut p, frames, &[on(0, C2, SOFT), off(half, C2)]);
        let held = rms(&l[frames / 20..half]);
        let tail = peak(&l[frames - frames / 10..]);
        let f = pitch(&l[frames / 10..frames / 10 + rate as usize / 4], rate);
        println!("{rate} Hz: held rms {held:.4}, tail peak {tail:e}, pitch {f:.2} Hz");
        assert!(held > 0.01, "{rate} Hz: silent");
        assert!(tail < 1e-4, "{rate} Hz: no release: {tail}");
        assert!(
            (f / c2 - 1.0).abs() < 0.02,
            "{rate} Hz: C2 at {f:.2} Hz, not {c2:.2}"
        );
        assert_eq!(
            p.synth().unwrap().errors(),
            0,
            "{rate} Hz: resampler errors"
        );
    }
}

/// The latency reported to the host is the resampler's delay (0 at 44.1 kHz), and it is the
/// delay measured between the onsets of the same note at 44.1 and 48 kHz.
#[test]
fn the_reported_latency_is_the_measured_resampler_delay() {
    let (mut a, lat44) = plugin(44_100);
    assert_eq!(lat44, Some(0));
    let (mut b, lat48) = plugin(48_000);
    let lat48 = lat48.expect("no latency reported at 48 kHz");
    let (l44, _) = render(&mut a, 4_096, &[on(0, C2, HARD)]);
    let (l48, _) = render(&mut b, 4_096, &[on(0, C2, HARD)]);
    let o44 = onset(&l44, 1e-3).unwrap();
    let o48 = onset(&l48, 1e-3).unwrap();
    // the 44.1 kHz onset at the 48 kHz rate, then the delay the resampler added
    let measured = o48 as f64 - o44 as f64 * 48_000.0 / 44_100.0;
    println!("onsets {o44} @44.1k, {o48} @48k; delay {measured:.1} frames, reported {lat48}");
    assert!(lat48 > 0);
    assert!(
        (measured - lat48 as f64).abs() <= 4.0,
        "measured {measured:.1}, reported {lat48}"
    );
}
