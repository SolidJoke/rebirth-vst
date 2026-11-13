//! Offline harness (RV-03): the two keyboard phrases of the ticket through the plugin's synth,
//! written to 16-bit stereo WAVs, with no host and no audio device.
//!
//! `cargo run --release -p orb-vst --example keyboard_wavs -- OUT_DIR [RATE]`
//!
//! Writes `legato_48k.wav` and `two_voices_48k.wav` in `OUT_DIR` (named after the default rate;
//! the files take `RATE`'s value, 48000 Hz by default, in their header).
//!
//! * legato: C2 down, G2 down over it (a slide), C2 up under G2 (nothing changes), G2 up (the
//!   gate closes); then a detached E2 (a new note); then C2 down, C3 down over it (a slide up),
//!   C3 up (the slide back to the held C2), C2 up. 5 s.
//! * two voices: 303 #1 holds C2 for 3 s while 303 #2 plays a legato line over it (C3, G3, C4,
//!   the keys overlapping). 4 s.
//!
//! The samples are the engine's float output converted to int16 as the engine's own conversion
//! does (`x * 32768`, clamped, truncated).

use std::io::Write;
use std::path::Path;

use orb_vst::synth::{Synth, Voice};

/// A phrase: (time in seconds, voice, note, velocity); velocity 0 is a note-off.
type Phrase = [(f64, Voice, u8, u8)];

fn play(rate: u32, seconds: f64, phrase: &Phrase) -> (Vec<f32>, Vec<f32>, Synth) {
    let mut s = Synth::new(rate).expect("the synth");
    let frames = (seconds * rate as f64) as usize;
    let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let mut pos = 0;
    for &(t, voice, note, velocity) in phrase {
        let at = ((t * rate as f64) as usize).min(frames);
        if at > pos {
            s.render(&mut l[pos..at], &mut r[pos..at]);
            pos = at;
        }
        if velocity == 0 {
            s.note_off(voice, note);
        } else {
            s.note_on(voice, note, velocity);
        }
    }
    s.render(&mut l[pos..], &mut r[pos..]);
    (l, r, s)
}

fn write_wav(path: &Path, rate: u32, l: &[f32], r: &[f32]) {
    let mut data = Vec::with_capacity(4 * l.len());
    for (a, b) in l.iter().zip(r) {
        for x in [a, b] {
            let v = (*x as f64 * 32768.0).clamp(-32768.0, 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * 4).to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(data.len() as u32).to_le_bytes());
    let mut f = std::fs::File::create(path).expect("the output file");
    f.write_all(&h)
        .and_then(|_| f.write_all(&data))
        .expect("the write");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: keyboard_wavs OUT_DIR [RATE]");
        std::process::exit(2);
    };
    let rate: u32 = args
        .get(1)
        .map(|s| s.parse().expect("a number"))
        .unwrap_or(48_000);
    let dir = Path::new(dir);
    use Voice::{A, B};
    let legato: &Phrase = &[
        (0.0, A, 36, 80),
        (0.5, A, 43, 80),
        (1.0, A, 36, 0),
        (1.5, A, 43, 0),
        (2.0, A, 40, 80),
        (2.5, A, 40, 0),
        (3.0, A, 36, 80),
        (3.5, A, 48, 80),
        (4.0, A, 48, 0),
        (4.5, A, 36, 0),
    ];
    let two: &Phrase = &[
        (0.0, A, 36, 80),
        (0.5, B, 48, 100),
        (1.0, B, 55, 80),
        (1.25, B, 48, 0),
        (1.5, B, 60, 80),
        (1.75, B, 55, 0),
        (2.5, B, 60, 0),
        (3.0, A, 36, 0),
    ];
    for (name, seconds, phrase) in [("legato", 5.0, legato), ("two_voices", 4.0, two)] {
        let (l, r, s) = play(rate, seconds, phrase);
        let path = dir.join(format!("{name}_48k.wav"));
        write_wav(&path, rate, &l, &r);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
        println!(
            "{}: {} frames at {rate} Hz, peak {peak:.4}, errors {}, keys still held {}+{}",
            path.display(),
            l.len(),
            s.errors(),
            s.held(A),
            s.held(B)
        );
    }
}
