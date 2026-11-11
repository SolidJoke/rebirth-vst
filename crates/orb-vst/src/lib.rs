//! ReBirth VST: the OpenRB-338 engine as a VST3 instrument (nih-plug), played from a MIDI
//! keyboard. Spike RV-02: one plugin instance is one whole engine, and MIDI notes play TB-303 #1
//! with ReBirth's transport stopped; the 303 #2, the drum machines, the effects and the mixer
//! are there but not reachable yet.
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

use synth::Synth;

/// The plugin. [`Default`] makes it without an engine; [`Plugin::initialize`] builds one.
pub struct ReBirthVst {
    params: Arc<NoParams>,
    synth: Option<Synth>,
}

/// No automatable parameter yet.
#[derive(Params, Default)]
pub struct NoParams {}

impl Default for ReBirthVst {
    fn default() -> Self {
        ReBirthVst {
            params: Arc::new(NoParams::default()),
            synth: None,
        }
    }
}

impl ReBirthVst {
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
        let mut pos = 0;
        while let Some(ev) = context.next_event() {
            let at = (ev.timing() as usize).min(n);
            if at > pos {
                synth.render(&mut l[pos..at], &mut r[pos..at]);
                pos = at;
            }
            match ev {
                NoteEvent::NoteOn { note, velocity, .. } => {
                    synth.note_on(note, midi_velocity(velocity))
                }
                NoteEvent::NoteOff { note, .. } => synth.note_off(note),
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
