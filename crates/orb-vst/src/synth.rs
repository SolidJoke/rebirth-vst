//! The keyboard synth: the engine of `orb-engine` with TB-303 #1 played by note events, at the
//! host's sample rate. It holds no plugin-framework type, so the tests drive it without a host.
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
//! the [`Tb303`] behind it. Channel 0 is therefore [`Keyboard303`]: a [`Tb303`] plus the
//! consumer of a lock-free queue of note events, which it applies at the start of its audio
//! slot (the moment the sequencer's step would make them), then renders as the 303 does. Every
//! other slot is the 303's own.
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
/// The velocity from which a note is accented (provisional; RV-03 refines it).
pub const ACCENT_VELOCITY: u8 = 100;
/// The note events one render can queue for the 303 before it drops the rest.
const KEYS: usize = 256;

/// The 303 step `[note, slide, accent, up, down, rest]` for MIDI note `note` at `velocity`:
/// the note folded by octaves into [`LOWEST`]..=[`HIGHEST`], then split into the step's note
/// (0..12) and its octave switches. No slide; accent from [`ACCENT_VELOCITY`].
pub fn step_for(note: u8, velocity: u8) -> [u8; 6] {
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
    let accent = (velocity >= ACCENT_VELOCITY) as u8;
    [key as u8, 0, accent, up, down, 0]
}

/// A note event for the 303.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// `Tb303::note_on` with this step.
    On([u8; 6]),
    /// `Tb303::note_off`.
    Off,
}

/// Channel 0 of the frame: a TB-303 that first plays the queued note events (see the module
/// doc). Every slot but the audio is the 303's.
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
    keys: rtrb::Producer<Key>,
    /// The note that opened the 303's gate last, until its note-off.
    held: Option<u8>,
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
        let (keys, rx) = rtrb::RingBuffer::new(KEYS);
        let mut devices = Devices::implemented(None);
        devices.channels[0] = Box::new(Keyboard303 {
            tb: Tb303::new(),
            keys: rx,
        });
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
            keys,
            held: None,
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

    /// MIDI note-on (`velocity` 1..127; 0 is a note-off): the 303 plays the note from the next
    /// block on. Mono, last note wins, no slide.
    pub fn note_on(&mut self, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off(note);
            return;
        }
        if self.keys.push(Key::On(step_for(note, velocity))).is_ok() {
            self.held = Some(note);
        }
    }

    /// MIDI note-off: closes the 303's gate when `note` is the one that opened it.
    pub fn note_off(&mut self, note: u8) {
        if self.held == Some(note) && self.keys.push(Key::Off).is_ok() {
            self.held = None;
        }
    }

    /// Closes the 303's gate whatever note opened it.
    pub fn all_notes_off(&mut self) {
        if self.keys.push(Key::Off).is_ok() {
            self.held = None;
        }
    }

    /// Drops the frames not yet handed out and the resampler's history, and closes the gate.
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
