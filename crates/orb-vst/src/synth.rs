//! The keyboard synth: the engine of `orb-engine` with both TB-303s played by note events, at the
//! host's sample rate. It holds no plugin-framework type, so the tests drive it without a host.
//!
//! # Two mono voices with keyboard legato
//!
//! Each 303 is one [`Voice`]: mono, with a stack of the keys held on it ([`DEPTH`] deep; a key
//! pressed beyond that is dropped, and its note-off then finds nothing). The last key pressed
//! sounds. A key pressed over a held one, and a return to a held key when the sounding one goes
//! up, are legato: the 303 slides into the new pitch without a new note. The gate closes when the
//! last key goes up.
//!
//! In the engine a step's slide flag means "slide from this note into the next step"
//! ([`Tb303::note_on`] reads the previous step's flag, and stores the new one). A keyboard only
//! learns that when the second key arrives, so [`Keyboard303`] sets [`Tb303::slide`] right before
//! the legato note-on ([`Key::Slide`]), with the step's own slide byte at 0: the gate stays
//! open, the envelope is not triggered again, and the pitch glides to the new target. No
//! note-off is made for the key being left.
//!
//! # The transport stays stopped
//!
//! The engine is a song as the song constructor leaves it ([`Engine::new`]): pattern mode,
//! stopped, the 303s, the mixer and the master at their constructor values. Stopped, the
//! sequencer's tick does nothing ([`orb_engine::seq::Sequencer::tick`] returns at once), but
//! every block still runs every channel device's audio slot ([`Frame::block`]), and the 303's
//! audio (`Tb303::process`) does not look at the transport: it renders whatever its gate,
//! envelope and pitch hold. So a note-on made between two blocks sounds from the next block on,
//! with no change to the engine.
//!
//! # Reaching the 303
//!
//! The frame holds its channel devices as `Box<dyn ChannelDevice>`, which cannot be downcast to
//! the [`Tb303`] behind it. Channels 0 and 1 (the two 303s) are therefore [`Keyboard303`]: a
//! [`Tb303`] plus the consumer of a lock-free queue of note events, which it applies at the
//! start of its audio slot (the moment the sequencer's step would make them), then renders as
//! the 303 does. Every other slot is the 303's own.
//!
//! # Rate and timing
//!
//! The engine renders 64-frame blocks at 44.1 kHz ([`ENGINE_RATE`]); a host at another rate gets
//! them through rubato's asynchronous sinc resampler (the parameters of `orb-player`'s output),
//! one block per resampler chunk. The output is pulled: a block is rendered when the frames of
//! the last one are used up, so a note event takes effect at the next block rendered, at most
//! one block (64 engine frames, 1.45 ms) after its position in the host's buffer.
//!
//! The output is the frame's float master ([`Frame::block`]), not the int16 conversion of
//! [`Engine::render`]: the same samples divided by 32768, without the int16 clamp.
//!
//! # Real time
//!
//! [`Synth::new`] allocates everything; [`Synth::note_on`], [`Synth::note_off`],
//! [`Synth::render`] and [`Synth::reset`] neither allocate, free nor lock (`tests/no_alloc.rs`).
//!
//! [`Frame::block`]: orb_engine::frame::Frame::block

use std::sync::Arc;

use orb_engine::frame::{BLOCK, Block, BlockContext, ChannelDevice, Devices, Engine, SongDevice};
use orb_engine::samples::{Retired, SampleSet};
use orb_engine::seq::TrackMut;
use orb_engine::tables::Tables;
use orb_engine::tb303::Tb303;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// The engine's only rate.
pub const ENGINE_RATE: u32 = 44_100;
/// The MIDI note of the 303's step `{note 0, up 0, down 0}` at the default tune (64): the pitch
/// table's entry 3072 is `55 * 2^(3/12)` Hz = 65.41 Hz, C2 in scientific pitch (MIDI 60 = C4;
/// Ableton Live names it C1). `orb_engine::tables::tb303_pitch`.
pub const ORIGIN: u8 = 36;
/// The lowest note the 303 plays: a step with `down`, one octave below [`ORIGIN`].
pub const LOWEST: u8 = ORIGIN - 12;
/// The highest: `note 12` with `up`, two octaves above [`ORIGIN`].
pub const HIGHEST: u8 = ORIGIN + 24;
/// The default velocity from which a note is accented (the plugin's "Accent velocity").
pub const ACCENT_VELOCITY: u8 = 100;
/// The note events one render can queue for a 303 before it drops the rest.
const KEYS: usize = 256;
/// The keys held at once on one 303; the ones pressed beyond are dropped.
pub const DEPTH: usize = 16;

/// A 303 of the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voice {
    /// TB-303 #1 (channel 0 of the frame).
    A,
    /// TB-303 #2 (channel 1 of the frame).
    B,
}

/// The 303 step `[note, slide, accent, up, down, rest]` for MIDI note `note` at `velocity`:
/// the note folded by octaves into [`LOWEST`]..=[`HIGHEST`], then split into the step's note
/// (0..12) and its octave switches. No slide; accent from [`ACCENT_VELOCITY`].
pub fn step_for(note: u8, velocity: u8) -> [u8; 6] {
    step_with(note, velocity, ACCENT_VELOCITY)
}

/// [`step_for`] with the accent threshold `accent`: accented from that velocity on.
pub fn step_with(note: u8, velocity: u8, accent: u8) -> [u8; 6] {
    let mut n = note as i32;
    while n < LOWEST as i32 {
        n += 12;
    }
    while n > HIGHEST as i32 {
        n -= 12;
    }
    let s = n - ORIGIN as i32;
    let (key, up, down) = if s < 0 {
        (s + 12, 0, 1)
    } else if s > 12 {
        (s - 12, 1, 0)
    } else {
        (s, 0, 0)
    };
    let accent = (velocity >= accent) as u8;
    [key as u8, 0, accent, up, down, 0]
}

/// A note event for the 303.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// `Tb303::note_on` with this step.
    On([u8; 6]),
    /// `Tb303::note_on` as a legato note: the slide flag is set first, so the gate stays open,
    /// the envelope is not triggered and the pitch glides (see the module doc).
    Slide([u8; 6]),
    /// `Tb303::note_off`.
    Off,
}

/// The keys held on one 303, oldest first, with their velocities. A fixed array: nothing here
/// allocates.
#[derive(Debug, Clone, Copy)]
struct Held {
    keys: [(u8, u8); DEPTH],
    len: usize,
}

impl Held {
    const EMPTY: Held = Held {
        keys: [(0, 0); DEPTH],
        len: 0,
    };

    /// The key that sounds: the last pressed of the ones held.
    fn top(&self) -> Option<(u8, u8)> {
        self.len.checked_sub(1).map(|i| self.keys[i])
    }

    /// Removes `note`; whether it was held.
    fn remove(&mut self, note: u8) -> bool {
        let Some(i) = self.keys[..self.len].iter().position(|k| k.0 == note) else {
            return false;
        };
        self.keys.copy_within(i + 1..self.len, i);
        self.len -= 1;
        true
    }

    /// Presses `note`; false (and nothing done) when [`DEPTH`] keys are held.
    fn push(&mut self, note: u8, velocity: u8) -> bool {
        if self.len == DEPTH {
            return false;
        }
        self.keys[self.len] = (note, velocity);
        self.len += 1;
        true
    }
}

/// One 303 as the keyboard plays it: the keys held, and the queue to its [`Keyboard303`].
struct VoiceState {
    held: Held,
    keys: rtrb::Producer<Key>,
}

impl VoiceState {
    fn note_on(&mut self, note: u8, velocity: u8, accent: u8) {
        // pressing a note already held (a host that lost its note-off) moves it to the top; the
        // gate is open and no note-off came, so it is legato (a slide to the same pitch)
        let legato = self.held.top().is_some();
        let mut next = self.held;
        next.remove(note);
        if !next.push(note, velocity) {
            return;
        }
        let step = step_with(note, velocity, accent);
        let key = if legato {
            Key::Slide(step)
        } else {
            Key::On(step)
        };
        if self.keys.push(key).is_ok() {
            self.held = next;
        }
    }

    fn note_off(&mut self, note: u8, accent: u8) {
        let mut next = self.held;
        if !next.remove(note) {
            return;
        }
        if self.held.top().map(|k| k.0) != Some(note) {
            // not the sounding key: it only leaves the stack
            self.held = next;
            return;
        }
        // the sounding key went up: back to the key held before it, or the gate closes
        let key = match next.top() {
            Some((n, v)) => Key::Slide(step_with(n, v, accent)),
            None => Key::Off,
        };
        if self.keys.push(key).is_ok() {
            self.held = next;
        }
    }

    fn all_notes_off(&mut self) {
        if self.keys.push(Key::Off).is_ok() {
            self.held = Held::EMPTY;
        }
    }
}

/// A channel of the frame that holds a TB-303: it first plays the queued note events (see the
/// module doc). Every slot but the audio is the 303's.
struct Keyboard303 {
    tb: Tb303,
    keys: rtrb::Consumer<Key>,
}

impl SongDevice for Keyboard303 {
    fn step(&mut self, tick: i32, shuffled: i32, tempo: i32) {
        self.tb.step(tick, shuffled, tempo);
    }
    fn relocate(&mut self, tick: i32, now: i32) {
        self.tb.relocate(tick, now);
    }
    fn start(&mut self, now: i32) {
        self.tb.start(now);
    }
    fn stop(&mut self) {
        self.tb.stop();
    }
    fn reset(&mut self) {
        self.tb.reset();
    }
    fn silence(&mut self) {
        self.tb.silence();
    }
    fn set_song_mode(&mut self, on: bool) {
        self.tb.set_song_mode(on);
    }
    fn data(&mut self, payload: &[u8], song_mode: bool) {
        self.tb.data(payload, song_mode);
    }
    fn set_param(&mut self, ctrl: u8, value: u8) {
        self.tb.set_param(ctrl, value);
    }
    fn param_message(&mut self, ctrl: u8, value: u8) {
        self.tb.param_message(ctrl, value);
    }
    fn set_sound_model(&mut self, vintage: bool) {
        self.tb.set_sound_model(vintage);
    }
    fn init_track(&mut self, track: &mut TrackMut) {
        self.tb.init_track(track);
    }
    fn live(&self) -> &[u8] {
        self.tb.live()
    }
    fn pattern_params(&self) -> &[u8] {
        self.tb.pattern_params()
    }
    fn saved_block(&self, song_mode: bool) -> &[u8] {
        self.tb.saved_block(song_mode)
    }
}

impl ChannelDevice for Keyboard303 {
    fn process(&mut self, bus: &mut Block, ctx: &BlockContext) {
        while let Ok(k) = self.keys.pop() {
            match k {
                Key::On(step) => self.tb.note_on(step),
                Key::Slide(step) => {
                    // the slide flag of the step being left; `note_on` stores the new step's
                    // own (0) afterwards, so only this transition slides
                    self.tb.slide = true;
                    self.tb.note_on(step);
                }
                Key::Off => self.tb.note_off(),
            }
        }
        self.tb.process(bus, ctx);
    }
    fn playing_pattern(&self) -> Option<u8> {
        self.tb.playing_pattern()
    }
    fn patterns_mut(&mut self) -> Option<&mut [u8]> {
        self.tb.patterns_mut()
    }
    fn stack_word(&self) -> Option<i32> {
        ChannelDevice::stack_word(&self.tb)
    }
    fn set_samples(&mut self, samples: &Arc<SampleSet>, retired: &mut Retired) {
        self.tb.set_samples(samples, retired);
    }
}

/// The resampler from the engine's rate to the host's, one engine block per chunk.
struct Resample {
    rs: Async<f32>,
    /// One engine block, interleaved.
    input: Box<[f32; 2 * BLOCK]>,
}

impl Resample {
    fn new(rate: u32) -> Result<Self, String> {
        let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2)
            .oversampling_factor(256)
            .interpolation(SincInterpolationType::Cubic);
        let rs = Async::<f32>::new_sinc(
            rate as f64 / ENGINE_RATE as f64,
            1.1,
            &params,
            BLOCK,
            2,
            FixedAsync::Input,
        )
        .map_err(|e| format!("the resampler: {e}"))?;
        Ok(Resample {
            rs,
            input: Box::new([0.0; 2 * BLOCK]),
        })
    }
}

/// The engine played from a keyboard, at the host's rate (see the module doc).
pub struct Synth {
    engine: Box<Engine>,
    /// TB-303 #1 and #2, in the order of [`Voice`].
    voices: [VoiceState; 2],
    /// The velocity from which a note is accented.
    accent: u8,
    rate: u32,
    resample: Option<Resample>,
    /// The frames at the host rate not yet handed out, interleaved: `out[2*pos..2*len]`.
    out: Box<[f32]>,
    len: usize,
    pos: usize,
    l: Block,
    r: Block,
    /// Render calls whose resampler failed (and that output silence instead).
    errors: u64,
}

impl Synth {
    /// A synth for a host at `rate` Hz (resampled unless 44100).
    pub fn new(rate: u32) -> Result<Self, String> {
        if rate == 0 {
            return Err("a sample rate of 0".into());
        }
        let mut devices = Devices::implemented(None);
        let mut voice = |channel: usize| {
            let (keys, rx) = rtrb::RingBuffer::new(KEYS);
            devices.channels[channel] = Box::new(Keyboard303 {
                tb: Tb303::new(),
                keys: rx,
            });
            VoiceState {
                held: Held::EMPTY,
                keys,
            }
        };
        let voices = [voice(0), voice(1)];
        let engine = Box::new(Engine::new(Arc::new(Tables::new()), devices));
        let resample = if rate == ENGINE_RATE {
            None
        } else {
            Some(Resample::new(rate)?)
        };
        let frames = match &resample {
            Some(r) => r.rs.output_frames_max(),
            None => BLOCK,
        };
        Ok(Synth {
            engine,
            voices,
            accent: ACCENT_VELOCITY,
            rate,
            resample,
            out: vec![0.0; 2 * frames].into_boxed_slice(),
            len: 0,
            pos: 0,
            l: [0.0; BLOCK],
            r: [0.0; BLOCK],
            errors: 0,
        })
    }

    /// The host's rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The latency reported to the host, in frames at its rate: the resampler's delay (half its
    /// sinc length at the output rate), 0 at 44.1 kHz. The pull adds 0 to 1 block of jitter to
    /// a note event, which is not reported.
    pub fn latency(&self) -> u32 {
        match &self.resample {
            Some(r) => r.rs.output_delay() as u32,
            None => 0,
        }
    }

    /// The engine's transport is playing (the song's play flag). Never set by this synth.
    pub fn transport_playing(&self) -> bool {
        self.engine.playing()
    }

    /// Render calls whose resampler failed.
    pub fn errors(&self) -> u64 {
        self.errors
    }

    /// Sets the velocity from which a note is accented (it applies to the notes played from
    /// now on, and to the held key a release returns to).
    pub fn set_accent_velocity(&mut self, velocity: u8) {
        self.accent = velocity;
    }

    /// MIDI note-on (`velocity` 1..127; 0 is a note-off) on `voice`: the 303 plays the note
    /// from the next block on. Mono, last note wins; pressed over a held key, it slides.
    pub fn note_on(&mut self, voice: Voice, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off(voice, note);
            return;
        }
        let accent = self.accent;
        self.voices[voice as usize].note_on(note, velocity, accent);
    }

    /// MIDI note-off on `voice`: a key that is not sounding only leaves the stack; the sounding
    /// one returns to the key held before it (a slide), or closes the gate when it is the last.
    pub fn note_off(&mut self, voice: Voice, note: u8) {
        let accent = self.accent;
        self.voices[voice as usize].note_off(note, accent);
    }

    /// How many keys are held on `voice`.
    pub fn held(&self, voice: Voice) -> usize {
        self.voices[voice as usize].held.len
    }

    /// The note that sounds on `voice` (the last pressed of the keys held), if any.
    pub fn sounding(&self, voice: Voice) -> Option<u8> {
        self.voices[voice as usize].held.top().map(|k| k.0)
    }

    /// Closes both 303s' gates and forgets every held key.
    pub fn all_notes_off(&mut self) {
        for v in &mut self.voices {
            v.all_notes_off();
        }
    }

    /// Drops the frames not yet handed out and the resampler's history, and closes the gates.
    pub fn reset(&mut self) {
        self.all_notes_off();
        self.len = 0;
        self.pos = 0;
        if let Some(r) = &mut self.resample {
            r.rs.reset();
        }
    }

    /// Fills `l` and `r` (the same length) with the next frames at the host's rate.
    pub fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let frames = l.len().min(r.len());
        let mut i = 0;
        while i < frames {
            if self.pos == self.len {
                self.pos = 0;
                self.len = 0;
                match self.next_block() {
                    Some(n) => self.len = n,
                    None => {
                        self.errors += 1;
                        l[i..frames].fill(0.0);
                        r[i..frames].fill(0.0);
                        return;
                    }
                }
                continue;
            }
            let n = (self.len - self.pos).min(frames - i);
            for k in 0..n {
                l[i + k] = self.out[2 * (self.pos + k)];
                r[i + k] = self.out[2 * (self.pos + k) + 1];
            }
            self.pos += n;
            i += n;
        }
    }

    /// One engine block into `out` at the host's rate; the frames, or `None` when the resampler
    /// refuses.
    fn next_block(&mut self) -> Option<usize> {
        if self.engine.frame.block(&mut self.l, &mut self.r).is_err() {
            // only the PCF faults, and no channel is routed to it here
            self.l.fill(0.0);
            self.r.fill(0.0);
        }
        let Some(rs) = &mut self.resample else {
            for k in 0..BLOCK {
                self.out[2 * k] = self.l[k];
                self.out[2 * k + 1] = self.r[k];
            }
            return Some(BLOCK);
        };
        for k in 0..BLOCK {
            rs.input[2 * k] = self.l[k];
            rs.input[2 * k + 1] = self.r[k];
        }
        let max = self.out.len() / 2;
        let inp = InterleavedSlice::new(&rs.input[..], 2, BLOCK).ok()?;
        let mut outp = InterleavedSlice::new_mut(&mut self.out[..], 2, max).ok()?;
        let (_, written) = rs.rs.process_into_buffer(&inp, &mut outp, None).ok()?;
        (written > 0).then_some(written)
    }
}
