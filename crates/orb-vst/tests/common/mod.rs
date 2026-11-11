//! A stand-in host: it initializes the plugin at a sample rate and runs its `process` on
//! 128-frame buffers (the buffer of the planned Live check), feeding it note events at their
//! positions. Nothing of a real host is needed.

#![allow(dead_code)]

use std::cell::Cell;

use nih_plug::prelude::*;
use orb_vst::{KeyboardTarget, ReBirthVst};

/// The stable id of a target value (the one a host's saved state holds).
pub fn target_id(target: KeyboardTarget) -> &'static str {
    match target {
        KeyboardTarget::ByChannel => "by_channel",
        KeyboardTarget::A => "a",
        KeyboardTarget::B => "b",
    }
}

/// Switches the target as the host does, between two `process` calls. nih-plug has no safe
/// public setter outside its wrappers: the type-erased parameter behind `as_ptr` has one, by
/// the stable id.
pub fn set_target(p: &ReBirthVst, target: KeyboardTarget) {
    let ptr = p.keyboard_params().target.as_ptr();
    let ParamPtr::EnumParam(inner) = ptr else {
        panic!("the target is not an enum parameter");
    };
    // SAFETY: the pointer is to the parameter inside `p`, which outlives this call
    assert!(unsafe { &*inner }.set_from_id(target_id(target)));
}

/// The host's buffer, in frames.
pub const HOST_BUFFER: usize = 128;

/// The [`InitContext`] of the stand-in host: it keeps the latency the plugin reports.
pub struct Init {
    pub latency: Cell<Option<u32>>,
}

impl InitContext<ReBirthVst> for Init {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Vst3
    }
    fn execute(&self, _task: ()) {}
    fn set_latency_samples(&self, samples: u32) {
        self.latency.set(Some(samples));
    }
    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// The [`ProcessContext`] of one host buffer: the events whose positions fall in it.
pub struct Process<'a> {
    pub events: &'a [NoteEvent<()>],
    pub next: usize,
}

impl ProcessContext<ReBirthVst> for Process<'_> {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Vst3
    }
    fn execute_background(&self, _task: ()) {}
    fn execute_gui(&self, _task: ()) {}
    fn transport(&self) -> &Transport {
        // the plugin does not read the host's transport (and nih-plug offers no constructor)
        unimplemented!("the plugin does not read the host's transport")
    }
    fn next_event(&mut self) -> Option<NoteEvent<()>> {
        let e = self.events.get(self.next).copied();
        self.next += 1;
        e
    }
    fn send_event(&mut self, _event: NoteEvent<()>) {}
    fn set_latency_samples(&self, _samples: u32) {}
    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// A plugin initialized at `rate` Hz, and the latency it reported.
pub fn plugin(rate: u32) -> (ReBirthVst, Option<u32>) {
    plugin_with(ReBirthVst::default(), rate)
}

/// `p` initialized at `rate` Hz, and the latency it reported.
pub fn plugin_with(mut p: ReBirthVst, rate: u32) -> (ReBirthVst, Option<u32>) {
    let mut init = Init {
        latency: Cell::new(None),
    };
    let config = BufferConfig {
        sample_rate: rate as f32,
        min_buffer_size: None,
        max_buffer_size: HOST_BUFFER as u32,
        process_mode: ProcessMode::Realtime,
    };
    let layout = ReBirthVst::AUDIO_IO_LAYOUTS[0];
    assert!(p.initialize(&layout, &config, &mut init));
    p.reset();
    (p, init.latency.get())
}

/// A note-on at frame `at` of the render, on MIDI channel 1 (nih-plug's channel 0).
pub fn on(at: usize, note: u8, velocity: u8) -> (usize, NoteEvent<()>) {
    on_ch(at, 1, note, velocity)
}

/// A note-off at frame `at`, on MIDI channel 1.
pub fn off(at: usize, note: u8) -> (usize, NoteEvent<()>) {
    off_ch(at, 1, note)
}

/// A note-on on MIDI channel `channel` (1-based, as a musician counts them).
pub fn on_ch(at: usize, channel: u8, note: u8, velocity: u8) -> (usize, NoteEvent<()>) {
    let e = NoteEvent::NoteOn {
        timing: 0,
        voice_id: None,
        channel: channel - 1,
        note,
        velocity: velocity as f32 / 127.0,
    };
    (at, e)
}

/// A note-off on MIDI channel `channel` (1-based).
pub fn off_ch(at: usize, channel: u8, note: u8) -> (usize, NoteEvent<()>) {
    let e = NoteEvent::NoteOff {
        timing: 0,
        voice_id: None,
        channel: channel - 1,
        note,
        velocity: 0.0,
    };
    (at, e)
}

/// The host's buffers for one render: the output channels and the events of each buffer, made
/// before any processing so that [`Host::buffer`] does not allocate.
pub struct Host {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    /// Per host buffer, its events with their timing inside it.
    pub events: Vec<Vec<NoteEvent<()>>>,
}

impl Host {
    /// `frames` frames (a multiple of [`HOST_BUFFER`]) with `events` (frame, event), sorted.
    pub fn new(frames: usize, events: &[(usize, NoteEvent<()>)]) -> Self {
        assert_eq!(frames % HOST_BUFFER, 0);
        let buffers = frames / HOST_BUFFER;
        let mut per = vec![Vec::new(); buffers];
        for &(at, e) in events {
            let t = (at % HOST_BUFFER) as u32;
            let e = match e {
                NoteEvent::NoteOn {
                    voice_id,
                    channel,
                    note,
                    velocity,
                    ..
                } => NoteEvent::NoteOn {
                    timing: t,
                    voice_id,
                    channel,
                    note,
                    velocity,
                },
                NoteEvent::NoteOff {
                    voice_id,
                    channel,
                    note,
                    velocity,
                    ..
                } => NoteEvent::NoteOff {
                    timing: t,
                    voice_id,
                    channel,
                    note,
                    velocity,
                },
                other => other,
            };
            per[at / HOST_BUFFER].push(e);
        }
        Host {
            l: vec![0.0; frames],
            r: vec![0.0; frames],
            events: per,
        }
    }

    pub fn buffers(&self) -> usize {
        self.events.len()
    }

    /// Runs the plugin's `process` on host buffer `k`, inside `around` (which must call the
    /// function it is given once): the buffer's setup, which allocates, stays outside it.
    pub fn buffer(&mut self, p: &mut ReBirthVst, k: usize, around: impl FnOnce(&mut dyn FnMut())) {
        let at = k * HOST_BUFFER;
        let l = &mut self.l[at..at + HOST_BUFFER];
        let r = &mut self.r[at..at + HOST_BUFFER];
        let mut buffer = Buffer::default();
        // SAFETY: both slices outlive `buffer` and have HOST_BUFFER frames
        unsafe {
            buffer.set_slices(HOST_BUFFER, |s| {
                s.push(l);
                s.push(r);
            });
        }
        let mut aux = AuxiliaryBuffers {
            inputs: &mut [],
            outputs: &mut [],
        };
        let mut ctx = Process {
            events: &self.events[k],
            next: 0,
        };
        around(&mut || {
            p.process(&mut buffer, &mut aux, &mut ctx);
        });
    }
}

/// Renders `frames` frames of `p` with `events`, buffer by buffer; returns (left, right).
pub fn render(
    p: &mut ReBirthVst,
    frames: usize,
    events: &[(usize, NoteEvent<()>)],
) -> (Vec<f32>, Vec<f32>) {
    let mut host = Host::new(frames, events);
    for k in 0..host.buffers() {
        host.buffer(p, k, |f| f());
    }
    (host.l, host.r)
}

/// Root mean square of `x`.
pub fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len() as f64).sqrt()
}

/// Largest absolute value of `x`.
pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, &v| m.max(v.abs()))
}

/// The fundamental of `x` at `rate` Hz, between 40 and 200 Hz: the lag of the autocorrelation's
/// maximum.
pub fn pitch(x: &[f32], rate: u32) -> f64 {
    let (lo, hi) = (rate as usize / 200, rate as usize / 40);
    let n = x.len() - hi;
    let corr = |lag: usize| -> f64 { (0..n).map(|i| x[i] as f64 * x[i + lag] as f64).sum::<f64>() };
    let c: Vec<(usize, f64)> = (lo..=hi).map(|lag| (lag, corr(lag))).collect();
    let best = c.iter().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap().0;
    rate as f64 / best as f64
}

/// The first frame whose absolute value exceeds `threshold`.
pub fn onset(x: &[f32], threshold: f32) -> Option<usize> {
    x.iter().position(|v| v.abs() > threshold)
}

/// The fundamental of `x` at `rate` Hz, between 40 and 200 Hz: the first maximum of the
/// autocorrelation within 90 % of the best one. [`pitch`] takes the best, which for a 303 note
/// above 80 Hz is often the octave below (the second period correlates as well as the first).
pub fn fundamental(x: &[f32], rate: u32) -> f64 {
    let (lo, hi) = (rate as usize / 200, rate as usize / 40);
    let n = x.len() - hi;
    let corr = |lag: usize| -> f64 { (0..n).map(|i| x[i] as f64 * x[i + lag] as f64).sum::<f64>() };
    let c: Vec<f64> = (lo..=hi).map(corr).collect();
    let best = c.iter().cloned().fold(f64::MIN, f64::max);
    (1..c.len() - 1)
        .find(|&i| c[i] >= 0.9 * best && c[i] >= c[i - 1] && c[i] >= c[i + 1])
        .map_or(0.0, |i| rate as f64 / (lo + i) as f64)
}
