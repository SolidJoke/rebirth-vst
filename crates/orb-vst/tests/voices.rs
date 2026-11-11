//! Both TB-303s played from the keyboard (RV-03), through the plugin's `process` without a host,
//! at 48 kHz: mono voices with keyboard legato (a key pressed over a held one slides, a key
//! released back to a held one slides back), the accent threshold and the keyboard target as
//! parameters, and the two voices apart.
//!
//! The two 303s at the constructor's values sound exactly alike (same sound, centred), so the
//! audio cannot say which 303 played a note: the routing tests read which voice holds the key
//! ([`orb_vst::synth::Synth::held`]), and the audio tests tell one mono voice from two.

mod common;

use common::{
    Host, fundamental, off, off_ch, on, on_ch, peak, plugin_with, render, rms, set_target,
};
use nih_plug::prelude::*;
use orb_vst::synth::Voice;
use orb_vst::{KeyboardParams, KeyboardTarget, ReBirthVst};

const RATE: u32 = 48_000;
/// C2 and G2 (MIDI 36 and 43), and their pitches: the pitch table's `55 * 2^(n/12)` Hz.
const C2: u8 = 36;
const G2: u8 = 43;
const C2_HZ: f64 = 65.406_391;
const G2_HZ: f64 = 97.998_859;
/// Below the default accent threshold (100).
const SOFT: u8 = 80;

/// Like [`render`], with `hook(k, plugin)` called before host buffer `k` (a parameter change).
fn render_hook(
    p: &mut ReBirthVst,
    frames: usize,
    events: &[(usize, NoteEvent<()>)],
    mut hook: impl FnMut(usize, &mut ReBirthVst),
) -> (Vec<f32>, Vec<f32>) {
    let mut host = Host::new(frames, events);
    for k in 0..host.buffers() {
        hook(k, p);
        host.buffer(p, k, |f| f());
    }
    (host.l, host.r)
}

/// A plugin at 48 kHz whose parameters hold `target` and the accent threshold `accent`.
fn plugin_params(target: KeyboardTarget, accent: i32) -> ReBirthVst {
    let params = KeyboardParams {
        target: EnumParam::new("Keyboard target", target),
        accent_velocity: IntParam::new(
            "Accent velocity",
            accent,
            IntRange::Linear { min: 1, max: 127 },
        ),
    };
    plugin_with(ReBirthVst::with_params(params), RATE).0
}

/// A plugin with the keyboard target set.
fn plugin_for(target: KeyboardTarget) -> ReBirthVst {
    plugin_params(target, 100)
}

/// The left channel of `events` played for `frames` frames by a plugin with `target`.
fn play(target: KeyboardTarget, frames: usize, events: &[(usize, NoteEvent<()>)]) -> Vec<f32> {
    let mut p = plugin_for(target);
    render(&mut p, frames, events).0
}

fn near(f: f64, hz: f64) -> bool {
    (f / hz - 1.0).abs() < 0.02
}

/// The largest difference between two renders from frame `from` on.
fn max_diff(a: &[f32], b: &[f32], from: usize) -> f32 {
    a[from..]
        .iter()
        .zip(&b[from..])
        .fold(0.0f32, |m, (x, y)| m.max((x - y).abs()))
}

/// X is held, Y is pressed over it: the 303 slides into Y. Not a new note: the envelope is not
/// triggered again, and the pitch ends at Y's.
#[test]
fn a_key_pressed_over_a_held_one_slides_into_it() {
    let frames = 48_000;
    let t = 12_000;
    let legato = play(
        KeyboardTarget::A,
        frames,
        &[on(0, C2, SOFT), on(t, G2, SOFT)],
    );
    // the same two notes, the first released just before the second: a new note
    let detached = play(
        KeyboardTarget::A,
        frames,
        &[on(0, C2, SOFT), off(t - 128, C2), on(t, G2, SOFT)],
    );
    let diff = max_diff(&legato, &detached, t);
    // the level of the 2400 frames before Y and of the 2400 after it (the glide is over)
    let before = rms(&legato[t - 2_400..t]);
    let after = rms(&legato[t + 600..t + 3_000]);
    let fresh = rms(&detached[t + 600..t + 3_000]);
    let f = fundamental(&legato[30_000..36_000], RATE);
    println!(
        "max |legato - detached| {diff}; rms before {before:.4}, after {after:.4} (legato), \
         {fresh:.4} (detached); late pitch {f:.2} Hz"
    );
    assert!(diff > 0.01, "a legato key sounds like a new note: {diff}");
    // no trigger: the envelope goes on decaying, the level does not jump back up, where the
    // detached note, triggered again, is louder
    assert!(
        after < before * 1.1,
        "the envelope was triggered again: {before} -> {after}"
    );
    assert!(
        fresh > after * 1.15,
        "the detached note is not louder than the slide: {fresh} vs {after}"
    );
    assert!(near(f, G2_HZ), "ends at {f:.2} Hz, not G2 {G2_HZ:.2}");
}

/// The sharpest proof that a legato key does not trigger the envelope: pressing the note that
/// already sounds again, over itself, is a slide to the same pitch, so the output is exactly
/// that of the note held alone. A new note would start the envelope over.
#[test]
fn a_legato_key_to_the_same_pitch_is_the_held_note_unchanged() {
    let frames = 24_064;
    let alone = play(KeyboardTarget::A, frames, &[on(0, C2, SOFT)]);
    // the same key pressed twice (a host that lost the note-off), 0.25 s in
    let again = play(
        KeyboardTarget::A,
        frames,
        &[on(0, C2, SOFT), on(12_032, C2, SOFT)],
    );
    // the second key goes down at an arbitrary frame: the 303 gets it at a block start, so
    // compare from there on
    assert!(
        alone == again,
        "the envelope was triggered again: max diff {}",
        max_diff(&alone, &again, 0)
    );
    // while a detached second note does change the sound
    let detached = play(
        KeyboardTarget::A,
        frames,
        &[on(0, C2, SOFT), off(12_000, C2), on(12_032, C2, SOFT)],
    );
    assert!(max_diff(&alone, &detached, 0) > 0.01);
}

/// The glide takes time: right after Y goes down the pitch is between X's and Y's, and Y's a few
/// tens of milliseconds later (the 303 moves a twelfth of the way every 64-frame block).
#[test]
fn the_slide_glides() {
    let t = 12_000;
    let legato = play(
        KeyboardTarget::A,
        36_096,
        &[on(0, C2, SOFT), on(t, G2, SOFT)],
    );
    let before = fundamental(&legato[t - 2_900..t - 100], RATE);
    // windows of 2500 frames: the first one spans the glide
    let early = fundamental(&legato[t..t + 2_500], RATE);
    let mid = fundamental(&legato[t + 1_200..t + 3_700], RATE);
    let late = fundamental(&legato[t + 18_000..t + 24_000], RATE);
    println!(
        "pitch before {before:.2}, t..+2500 {early:.2}, +1200..+3700 {mid:.2}, late {late:.2} Hz"
    );
    assert!(near(before, C2_HZ), "before: {before:.2}");
    assert!(near(late, G2_HZ), "late pitch {late:.2}");
    assert!(
        early < 92.0 && early > C2_HZ * 1.1,
        "not gliding: {early:.2} Hz over the first 2500 frames"
    );
    assert!(
        mid > early,
        "the pitch does not rise: {early:.2} then {mid:.2}"
    );
}

/// X held, Y pressed then released: still sounding, and back at X's pitch, without a new note.
#[test]
fn releasing_the_sounding_key_returns_to_the_held_one() {
    let frames = 48_000;
    let (t, u) = (12_000, 24_000);
    let events = [on(0, C2, SOFT), on(t, G2, SOFT), off(u, G2)];
    let back = play(KeyboardTarget::A, frames, &events);
    let held = rms(&back[u + 6_000..u + 18_000]);
    let f = fundamental(&back[36_000..48_000], RATE);
    println!("after Y released: rms {held:.4}, pitch {f:.2} Hz");
    assert!(held > 0.01, "silent after the sounding key went up: {held}");
    assert!(near(f, C2_HZ), "back at {f:.2} Hz, not C2 {C2_HZ:.2}");
    // the return is a slide too: the level does not jump up
    let before = rms(&back[u - 2_400..u]);
    let after = rms(&back[u + 600..u + 3_000]);
    assert!(after < before * 1.1, "retriggered: {before} -> {after}");
}

/// A released key that is not sounding changes nothing audible; the gate closes when the last
/// key goes.
#[test]
fn a_released_key_that_is_not_sounding_changes_nothing() {
    let frames = 4 * 48_000;
    let base = [on(0, C2, SOFT), on(12_000, G2, SOFT)];
    let plain = play(KeyboardTarget::A, frames, &base);
    // X goes up under Y
    let mut events = base.to_vec();
    events.push(off(24_000, C2));
    let under = play(KeyboardTarget::A, frames, &events);
    assert!(plain == under, "releasing the held key changed the sound");
    // Y goes up last: the gate closes only now
    events.push(off(36_000, G2));
    let all = play(KeyboardTarget::A, frames, &events);
    let held = rms(&all[26_000..36_000]);
    let tail = peak(&all[frames - 48_000..]);
    println!("held rms {held:.4}, tail peak {tail:e}");
    assert!(held > 0.01, "silent while Y is still held: {held}");
    assert!(tail < 1e-4, "still sounding after the last key: {tail}");
}

/// Both voices at once in `By channel`: a note on channel 1 and one on channel 2 are heard
/// together, and releasing the channel-1 key leaves the channel-2 voice sounding.
#[test]
fn two_voices_play_together_and_apart() {
    let frames = 4 * 48_000;
    let t = KeyboardTarget::ByChannel;
    let one = play(t, frames, &[on_ch(0, 1, C2, SOFT)]);
    let two = play(t, frames, &[on_ch(0, 2, G2, SOFT)]);
    let both = play(t, frames, &[on_ch(0, 1, C2, SOFT), on_ch(0, 2, G2, SOFT)]);
    // two voices add: the pair is neither voice alone, and louder than either
    assert!(
        max_diff(&both, &one, 0) > 0.01,
        "the second voice is not heard"
    );
    assert!(
        max_diff(&both, &two, 0) > 0.01,
        "the first voice is not heard"
    );
    let (r1, r2, rb) = (
        rms(&one[2_400..24_000]),
        rms(&two[2_400..24_000]),
        rms(&both[2_400..24_000]),
    );
    println!("rms: ch1 {r1:.4}, ch2 {r2:.4}, both {rb:.4}");
    assert!(rb > r1 && rb > r2, "the two voices are not summed");
    // ... where one mono voice plays the last key only, over the first (a slide)
    let mono = play(
        KeyboardTarget::A,
        frames,
        &[on_ch(0, 1, C2, SOFT), on_ch(0, 2, G2, SOFT)],
    );
    assert!(
        max_diff(&both, &mono, 0) > 0.01,
        "two channels on two voices sound like one mono voice"
    );
    // the channel-1 key goes up at 1 s: channel 2 goes on, at its own pitch
    let held = play(
        t,
        frames,
        &[
            on_ch(0, 1, C2, SOFT),
            on_ch(0, 2, G2, SOFT),
            off_ch(48_000, 1, C2),
        ],
    );
    let late = &held[3 * 48_000..];
    let f = fundamental(&late[..12_000], RATE);
    println!(
        "after the channel-1 key: rms {:.4}, pitch {f:.2} Hz",
        rms(late)
    );
    assert!(
        rms(late) > 0.01,
        "the channel-2 voice stopped with channel 1"
    );
    assert!(near(f, G2_HZ), "late pitch {f:.2} Hz");
    // while with the channel-2 key up too, everything dies
    let none = play(
        t,
        frames,
        &[
            on_ch(0, 1, C2, SOFT),
            on_ch(0, 2, G2, SOFT),
            off_ch(48_000, 1, C2),
            off_ch(60_000, 2, G2),
        ],
    );
    assert!(peak(&none[3 * 48_000..]) < 1e-4);
}

/// Plays `note` on MIDI `channel` under `target`, held, and returns how many keys the 303s
/// hold afterwards: (#1, #2), with the audio's level while held.
fn route(target: KeyboardTarget, channel: u8) -> (usize, usize, f64) {
    let mut p = plugin_for(target);
    let (l, _) = render(&mut p, 12_032, &[on_ch(0, channel, C2, SOFT)]);
    let s = p.synth().unwrap();
    (s.held(Voice::A), s.held(Voice::B), rms(&l[2_400..12_000]))
}

#[test]
fn by_channel_sends_channel_2_to_303_b_and_every_other_to_303_a() {
    let t = KeyboardTarget::ByChannel;
    // MIDI channel 10 will be the drums (RV-04); until then it is a 303 #1 channel like the rest
    for ch in [1, 3, 9, 10, 16] {
        let (a, b, level) = route(t, ch);
        assert_eq!((a, b), (1, 0), "channel {ch}: not 303 #1");
        assert!(level > 0.01, "channel {ch}: silent");
    }
    let (a, b, level) = route(t, 2);
    assert_eq!((a, b), (0, 1), "channel 2: not 303 #2");
    // 303 #2 is audible at the constructor's mixer values
    assert!(level > 0.01, "303 #2 is silent: {level}");
}

#[test]
fn target_a_sends_every_channel_to_303_a() {
    for ch in [1, 2, 3, 16] {
        let (a, b, level) = route(KeyboardTarget::A, ch);
        assert_eq!((a, b), (1, 0), "channel {ch}: not 303 #1");
        assert!(level > 0.01, "channel {ch}: silent");
    }
}

#[test]
fn target_b_sends_every_channel_to_303_b() {
    for ch in [1, 2, 3, 16] {
        let (a, b, level) = route(KeyboardTarget::B, ch);
        assert_eq!((a, b), (0, 1), "channel {ch}: not 303 #2");
        assert!(level > 0.01, "channel {ch}: 303 #2 is silent: {level}");
    }
}

/// Changing the target while keys are held releases every voice: nothing keeps sounding.
#[test]
fn changing_the_target_with_keys_held_leaves_no_stuck_note() {
    let frames = 3 * 48_000;
    // both keys are held through the whole render: the host never sends their note-offs
    let events = [on_ch(0, 1, C2, SOFT), on_ch(0, 2, G2, SOFT)];
    let mut p = plugin_for(KeyboardTarget::ByChannel);
    let switch = 12_032 / 128;
    let (l, r) = render_hook(&mut p, frames, &events, |k, p| {
        if k == switch {
            set_target(p, KeyboardTarget::B);
        }
    });
    let before = rms(&l[2_400..12_000]);
    let tail = peak(&l[frames - 48_000..]).max(peak(&r[frames - 48_000..]));
    println!("before the change rms {before:.4}, tail peak {tail:e}");
    assert!(before > 0.01);
    assert!(
        tail < 1e-4,
        "a note is stuck after the target change: {tail}"
    );
    let s = p.synth().unwrap();
    assert_eq!((s.held(Voice::A), s.held(Voice::B)), (0, 0));
    // and a key pressed afterwards plays on the new target
    let mut q = plugin_for(KeyboardTarget::ByChannel);
    let (l2, _) = render_hook(
        &mut q,
        frames,
        &[on_ch(0, 1, C2, SOFT), on_ch(24_192, 1, G2, SOFT)],
        |k, p| {
            if k == switch {
                set_target(p, KeyboardTarget::B);
            }
        },
    );
    let f = fundamental(&l2[2 * 48_000..2 * 48_000 + 12_000], RATE);
    assert!(near(f, G2_HZ), "no note after the change: {f:.2} Hz");
    let s = q.synth().unwrap();
    assert_eq!((s.held(Voice::A), s.held(Voice::B)), (0, 1));
}

/// The accent follows the "Accent velocity" parameter: velocity 80 accents when the threshold
/// is 80, and 110 does not when it is 120.
#[test]
fn the_accent_threshold_follows_the_parameter() {
    let frames = 24_064;
    let with = |threshold: i32, velocity: u8| {
        let mut p = plugin_params(KeyboardTarget::A, threshold);
        render(&mut p, frames, &[on(0, C2, velocity), off(12_000, C2)]).0
    };
    let plain = with(100, 80);
    let accented = with(100, 110);
    assert!(max_diff(&plain, &accented, 0) > 0.01, "no accent at all");
    // at the boundary
    assert!(
        with(80, 80) == accented,
        "velocity 80 is not accented at 80"
    );
    assert!(
        with(81, 80) == plain,
        "velocity 80 is accented at threshold 81"
    );
    assert!(with(120, 110) == plain, "velocity 110 is accented at 120");
    assert!(
        with(127, 127) == accented,
        "velocity 127 is not accented at 127"
    );
    assert!(
        with(1, 1) == accented,
        "velocity 1 is not accented at threshold 1"
    );
    // the parameter's own default and range, and the target's default
    let defaults = KeyboardParams::default();
    assert_eq!(defaults.accent_velocity.value(), 100);
    assert!(matches!(
        defaults.accent_velocity.range(),
        IntRange::Linear { min: 1, max: 127 }
    ));
    assert_eq!(defaults.target.value(), KeyboardTarget::ByChannel);
}

/// The threshold applies to the key a release returns to, with the velocity it was pressed at.
#[test]
fn the_accent_of_a_returned_to_key_follows_its_own_velocity() {
    let frames = 36_096;
    let (t, u) = (12_000, 24_000);
    let run = |velocity: u8| {
        let events = [on(0, C2, velocity), on(t, G2, SOFT), off(u, G2)];
        play(KeyboardTarget::A, frames, &events)
    };
    let soft = run(SOFT);
    let hard = run(110);
    // identical until the first key's own accent (same as before Y): they already differ at 0
    assert!(
        max_diff(&soft, &hard, u) > 0.001,
        "the held key's accent is lost on the way back"
    );
}

/// The stack of held keys is 16 deep: with 20 keys down and all up again, the gate closes.
#[test]
fn more_keys_than_the_stack_holds_do_not_stick() {
    let frames = 3 * 48_000;
    // 20 keys down, then all up: the ones beyond the stack's depth were dropped, and every
    // key goes up without a stuck gate
    let mut events = Vec::new();
    for k in 0..20u8 {
        events.push(on(k as usize * 100, 36 + k, SOFT));
    }
    for k in 0..20u8 {
        events.push(off(10_000 + k as usize * 100, 36 + k));
    }
    let mut p = plugin_for(KeyboardTarget::A);
    let (out, _) = render(&mut p, frames, &events);
    assert!(rms(&out[2_000..9_000]) > 0.01);
    assert!(peak(&out[frames - 48_000..]) < 1e-4, "a key stuck");
    assert_eq!(p.synth().unwrap().held(Voice::A), 0);
}
