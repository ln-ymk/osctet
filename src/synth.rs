use core::f64;
use std::collections::HashMap;

use fundsp::hacker::*;

#[derive(PartialEq, Eq, Hash)]
pub enum KeyOrigin {
    Keyboard,
    Midi,
    Pattern,
}

#[derive(PartialEq, Eq, Hash)]
pub struct Key {
    pub origin: KeyOrigin,
    pub channel: u8,
    pub key: u8,
}

#[derive(PartialEq, Clone, Copy)]
pub enum PlayMode {
    Poly,
    Mono,
    SingleTrigger,
}

impl PlayMode {
    pub const VARIANTS: [PlayMode; 3] = [Self::Poly, Self::Mono, Self::SingleTrigger];

    pub fn name(&self) -> &str {
        match self {
            Self::Poly => "Poly",
            Self::Mono => "Mono",
            Self::SingleTrigger => "Single trigger",
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum Waveform {
    Sawtooth,
    Pulse,
    Triangle,
    Sine,
}

impl Waveform {
    pub const VARIANTS: [Waveform; 4] = [Self::Sawtooth, Self::Pulse, Self::Triangle, Self::Sine];

    pub fn name(&self) -> &str {
        match self {
            Self::Sawtooth => "Sawtooth",
            Self::Pulse => "Pulse",
            Self::Triangle => "Triangle",
            Self::Sine => "Sine",
        }
    }

    fn make_net(&self, freq: &Shared, glide_time: f32, duty: &Shared) -> Net {
        // have to compensate for different volumes. the sine is so loud!
        match self {
            Self::Sawtooth => Net::wrap(Box::new(var(freq) >> follow(glide_time) >> saw())),
            Self::Pulse => Net::wrap(Box::new((var(freq) >> follow(glide_time) | var(duty)) >> pulse())),
            Self::Triangle => Net::wrap(Box::new(var(freq) >> follow(glide_time) >> triangle() * 2.0)),
            Self::Sine => Net::wrap(Box::new(var(freq) >> follow(glide_time) >> sine() * 0.5)),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum FilterType {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

impl FilterType {
    pub const VARIANTS: [FilterType; 4] = [Self::Lowpass, Self::Highpass, Self::Bandpass, Self::Notch];
    
    pub fn name(&self) -> &str {
        match self {
            Self::Lowpass => "Lowpass",
            Self::Highpass => "Highpass",
            Self::Bandpass => "Bandpass",
            Self::Notch => "Notch",
        }
    }
}

pub struct Synth {
    pub oscs: [Oscillator; 1],
    pub filter: Filter,
    pub play_mode: PlayMode,
    pub glide_time: f32,
    voices: HashMap<Key, Voice>,
}

impl Synth {
    pub fn new() -> Self {
        Self {
            oscs: [Oscillator::new()],
            filter: Filter::new(),
            play_mode: PlayMode::Poly,
            glide_time: 0.05,
            voices: HashMap::new(),
        }
    }

    pub fn note_on(&mut self, key: Key, pitch: f32, seq: &mut Sequencer) {
        match self.play_mode {
            PlayMode::Poly => {
                self.voices.insert(key, Voice::new(pitch, self.glide_time, &self.oscs, &self.filter, seq));
            },
            PlayMode::Mono => {
                for voice in self.voices.values_mut() {
                    voice.off(seq);
                }
                self.voices.insert(key, Voice::new(pitch, self.glide_time, &self.oscs, &self.filter, seq));
            },
            PlayMode::SingleTrigger => {
                if self.voices.is_empty() {
                    self.voices.insert(key, Voice::new(pitch, self.glide_time, &self.oscs, &self.filter, seq));
                } else {
                    let voice = self.voices.drain().map(|(_, v)| v).next().unwrap();
                    voice.freq.set(midi_hz(pitch));
                    self.voices.insert(key, voice);
                }
            },
        }
    }

    pub fn note_off(&mut self, key: Key, seq: &mut Sequencer) {
        if let Some(voice) = self.voices.remove(&key) {
            voice.off(seq);
        }
    }
}

pub struct Oscillator {
    pub level: Shared,
    pub duty: Shared,
    pub env: ADSR,
    pub waveform: Waveform,
}

impl Oscillator {
    fn new() -> Self {
        Self {
            level: shared(0.5),
            duty: shared(0.5),
            env: ADSR::new(),
            waveform: Waveform::Sawtooth,
        }
    }
}

pub struct Filter {
    pub filter_type: FilterType,
    pub cutoff: Shared,
    pub resonance: Shared,
    pub key_tracking: Shared,
    pub env: ADSR,
    pub env_level: Shared,
}

impl Filter {
    fn new() -> Self {
        Self {
            cutoff: shared(440.0),
            resonance: shared(0.1),
            key_tracking: shared(0.0),
            filter_type: FilterType::Lowpass,
            env: ADSR::new(),
            env_level: shared(0.0),
        }
    }

    fn make_net(&self, note_freq: &Shared) -> Net {
        // TODO: envelope
        (pass() | var(&self.cutoff) * (var(note_freq) * (1.0/261.6) * var(&self.key_tracking) + 1.0) | var(&self.resonance)) >> match self.filter_type {
            FilterType::Lowpass => Net::wrap(Box::new(lowpass())),
            FilterType::Highpass => Net::wrap(Box::new(highpass())),
            FilterType::Bandpass => Net::wrap(Box::new(bandpass())),
            FilterType::Notch => Net::wrap(Box::new(notch())),
        }
    }
}

pub struct ADSR {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl ADSR {
    fn new() -> Self {
        Self {
            attack: 0.01,
            decay: 1.0,
            sustain: 1.0,
            release: 0.01,
        }
    }
}

struct Voice {
    freq: Shared,
    gate: Shared,
    release_time: f32,
    event_id: EventId,
}

impl Voice {
    fn new(pitch: f32, glide_time: f32, oscs: &[Oscillator], filter: &Filter, seq: &mut Sequencer) -> Self {
        let freq = shared(midi_hz(pitch));
        let gate = shared(1.0);
        let f = |i: usize| {
            (oscs[i].waveform.make_net(&freq, glide_time, &oscs[i].duty)) * var(&oscs[i].level) *
                (var(&gate) >> adsr_live(oscs[i].env.attack, oscs[i].env.decay, oscs[i].env.sustain, oscs[i].env.release)) >>
                filter.make_net(&freq)
        };
        let unit = f(0);
        Self {
            freq,
            gate,
            release_time: oscs[0].env.release,
            event_id: seq.push_relative(0.0, f64::INFINITY, Fade::Smooth, 0.0, 0.0, Box::new(unit)),
        }
    }

    fn off(&self, seq: &mut Sequencer) {
        self.gate.set(0.0);
        seq.edit_relative(self.event_id, self.release_time as f64, 0.0);
    }
}