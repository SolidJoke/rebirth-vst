//! Offline harness (RV-02): one 303 note through the plugin's synth, written to a 16-bit stereo
//! WAV, with no host and no audio device.
//!
//! `cargo run --release -p orb-vst --example note_wav -- OUT.wav [RATE [NOTE [VELOCITY]]]`
//!
//! Defaults: 48000 Hz, MIDI note 36 (C2, 65.41 Hz), velocity 80 (no accent). The note is held
//! 0.5 s, then released; the file is 2 s long. The samples are the engine's float output
//! converted to int16 as the engine's own conversion does (`x * 32768`, clamped, truncated).

use std::io::Write;

use orb_vst::synth::{Synth, Voice};

fn arg(args: &[String], i: usize, default: u32) -> u32 {
    args.get(i)
        .map(|s| s.parse().expect("a number"))
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: note_wav OUT.wav [RATE [NOTE [VELOCITY]]]");
        std::process::exit(2);
    };
    let rate = arg(&args, 1, 48_000);
    let note = arg(&args, 2, 36) as u8;
    let velocity = arg(&args, 3, 80) as u8;
    let mut s = Synth::new(rate).expect("the synth");
    let frames = 2 * rate as usize;
    let held = rate as usize / 2;
    let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    s.note_on(Voice::A, note, velocity);
    s.render(&mut l[..held], &mut r[..held]);
    s.note_off(Voice::A, note);
    s.render(&mut l[held..], &mut r[held..]);
    let peak = l.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
    let tail = l[frames - rate as usize / 2..]
        .iter()
        .fold(0.0f32, |m, v| m.max(v.abs()));
    let mut data = Vec::with_capacity(4 * frames);
    for (a, b) in l.iter().zip(&r) {
        for x in [a, b] {
            let v = (*x as f64 * 32768.0).clamp(-32768.0, 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut f = std::fs::File::create(path).expect("the output file");
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
    f.write_all(&h)
        .and_then(|_| f.write_all(&data))
        .expect("the write");
    println!(
        "{path}: {frames} frames at {rate} Hz, note {note} velocity {velocity} held {held} \
         frames; peak {peak:.4}, last 0.5 s peak {tail:e}; latency {} frames; transport \
         playing: {}",
        s.latency(),
        s.transport_playing()
    );
}
