//! ReBirth VST: the OpenRB-338 engine as a VST3 instrument (nih-plug), played from a MIDI
//! keyboard. One plugin instance is one whole engine, and MIDI notes play the two TB-303s
//! (mono, legato notes slide; the "Keyboard target" parameter says which channel plays which)
//! with ReBirth's transport stopped; the drum machines, the effects and the mixer are there but
//! not reachable yet.
//!
//! The plugin is a thin layer over [`synth::Synth`], which holds no nih-plug type: the plugin
//! builds it in [`Plugin::initialize`] (where allocation is allowed) at the host's rate, and
//! [`Plugin::process`] splits the host's buffer at each note event's position, renders the
//! frames before it and hands the event to the synth. Nothing in `process` allocates, frees or
//! locks (`tests/no_alloc.rs`).

// `nih_export_vst3!` expands vst3-sys's `vtable!` macro, whose trailing semicolons trip this
// future-compatibility lint in this crate (nih-plug de421011, vst3-sys 0.1.0); nothing here
// can change them.
#![allow(semicolon_in_expressions_from_non_local_macros)]

pub mod synth;

use std::num::NonZeroU32;
use std::sync::Arc;

use nih_plug::prelude::*;

use synth::{Synth, Voice};

/// The plugin. [`Default`] makes it without an engine; [`Plugin::initialize`] builds one.
pub struct ReBirthVst {
    params: Arc<KeyboardParams>,
    synth: Option<Synth>,
    /// The target the last `process` saw: a change releases every key.
    target: KeyboardTarget,
}

/// Which 303 the keyboard plays.
///
/// The ids of the variants are stored in saved projects: never change them.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardTarget {
    /// MIDI channel 2 plays 303 #2, every other channel plays 303 #1.
    #[id = "by_channel"]
    #[name = "By channel"]
    ByChannel,
    /// Every channel plays 303 #1.
    #[id = "a"]
    #[name = "303 A"]
    A,
    /// Every channel plays 303 #2.
    #[id = "b"]
    #[name = "303 B"]
    B,
}

impl KeyboardTarget {
    /// The 303 that plays MIDI channel `channel` (nih-plug's 0-based: 0 is MIDI channel 1).
    pub fn voice(self, channel: u8) -> Voice {
        match self {
            KeyboardTarget::A => Voice::A,
            KeyboardTarget::B => Voice::B,
            // MIDI channel 10 (index 9) will be the drums (RV-04): it is not special-cased yet
            KeyboardTarget::ByChannel if channel == 1 => Voice::B,
            KeyboardTarget::ByChannel => Voice::A,
        }
    }
}

/// The plugin's parameters.
///
/// The `#[id]`s are permanent: hosts find the parameters in saved projects by them (as
/// [`Vst3Plugin::VST3_CLASS_ID`] finds the plugin). Never rename them.
#[derive(Params)]
pub struct KeyboardParams {
    /// Where the MIDI channels go.
    #[id = "target"]
    pub target: EnumParam<KeyboardTarget>,
    /// The velocity from which a note is accented.
    #[id = "accent_velocity"]
    pub accent_velocity: IntParam,
}

impl Default for KeyboardParams {
    fn default() -> Self {
        KeyboardParams {
            target: EnumParam::new("Keyboard target", KeyboardTarget::ByChannel),
            accent_velocity: IntParam::new(
                "Accent velocity",
                synth::ACCENT_VELOCITY as i32,
                IntRange::Linear { min: 1, max: 127 },
            ),
        }
    }
}

impl Default for ReBirthVst {
    fn default() -> Self {
        Self::with_params(KeyboardParams::default())
    }
}

impl ReBirthVst {
    /// A plugin with these parameters, as a host's state would restore them (the tests build
    /// non-default ones with it: nih-plug has no public setter for a parameter's value outside
    /// its own wrappers).
    pub fn with_params(params: KeyboardParams) -> Self {
        ReBirthVst {
            target: params.target.value(),
            params: Arc::new(params),
            synth: None,
        }
    }

    /// The parameters.
    pub fn keyboard_params(&self) -> &Arc<KeyboardParams> {
        &self.params
    }

    /// The synth, once [`Plugin::initialize`] has built it.
    pub fn synth(&self) -> Option<&Synth> {
        self.synth.as_ref()
    }
}

/// nih-plug's normalized velocity (0..1) as a MIDI velocity, at least 1 (a note-on is never
/// turned into a note-off by rounding).
pub fn midi_velocity(v: f32) -> u8 {
    (v * 127.0).round().clamp(1.0, 127.0) as u8
}

impl Plugin for ReBirthVst {
    const NAME: &'static str = "ReBirth VST";
    const VENDOR: &'static str = "OpenRB-338";
    const URL: &'static str = "https://github.com/SolidJoke/rebirth-vst";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _layout: &AudioIOLayout,
        config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        let rate = config.sample_rate.round() as u32;
        match Synth::new(rate) {
            Ok(s) => {
                context.set_latency_samples(s.latency());
                self.synth = Some(s);
                true
            }
            Err(e) => {
                nih_error!("ReBirth VST cannot run at {rate} Hz: {e}");
                false
            }
        }
    }

    fn reset(&mut self) {
        if let Some(s) = &mut self.synth {
            s.reset();
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let n = buffer.samples();
        let out = buffer.as_slice();
        let synth = match &mut self.synth {
            Some(s) if out.len() >= 2 => s,
            _ => {
                for ch in out.iter_mut() {
                    ch.fill(0.0);
                }
                return ProcessStatus::Normal;
            }
        };
        let (l, rest) = out.split_at_mut(1);
        let (l, r) = (&mut *l[0], &mut *rest[0]);
        // the parameters, read once per buffer (atomic loads): a new target releases every key
        // of both 303s, so no note is left sounding on a voice that no longer gets its note-off
        let target = self.params.target.value();
        if target != self.target {
            self.target = target;
            synth.all_notes_off();
        }
        synth.set_accent_velocity(self.params.accent_velocity.value().clamp(1, 127) as u8);
        let mut pos = 0;
        while let Some(ev) = context.next_event() {
            let at = (ev.timing() as usize).min(n);
            if at > pos {
                synth.render(&mut l[pos..at], &mut r[pos..at]);
                pos = at;
            }
            match ev {
                NoteEvent::NoteOn {
                    channel,
                    note,
                    velocity,
                    ..
                } => synth.note_on(target.voice(channel), note, midi_velocity(velocity)),
                NoteEvent::NoteOff { channel, note, .. } => {
                    synth.note_off(target.voice(channel), note)
                }
                NoteEvent::Choke { .. } => synth.all_notes_off(),
                _ => {}
            }
        }
        if pos < n {
            synth.render(&mut l[pos..n], &mut r[pos..n]);
        }
        // an instrument: the host keeps calling it while the notes decay
        ProcessStatus::KeepAlive
    }
}

impl Vst3Plugin for ReBirthVst {
    /// Never change it: hosts find the plugin in saved projects by it.
    const VST3_CLASS_ID: [u8; 16] = *b"OpenRB338RbVst01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth];
}

nih_export_vst3!(ReBirthVst);
