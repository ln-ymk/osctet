use core::f64;
use std::{collections::HashMap, fmt::Display};

use rand::prelude::*;
use fundsp::hacker::*;

pub const MAX_ENVS: usize = 4;
pub const MAX_OSCS: usize = 4;

const KEY_TRACKING_REF_FREQ: f32 = 261.6;
const SEMITONE_RATIO: f32 = 1.059463;

#[derive(PartialEq, Eq, Hash, Clone)]
pub enum KeyOrigin {
    Keyboard,
    Midi,
    Pattern,
}

#[derive(PartialEq, Eq, Hash, Clone)]
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

    fn make_net(&self, settings: &Settings, vars: &VoiceVars, osc: &Oscillator, index: usize, fm_oscs: Net) -> Net {
        let base = var(&vars.freq)
            * var(&osc.freq_ratio)
            * var_fn(&osc.fine_pitch, |x| pow(SEMITONE_RATIO, x))
            >> follow(settings.glide_time)
            * ((settings.dsp_component(vars, ModTarget::Pitch(index)) >> shape_fn(|x| pow(4.0, x))))
            * ((settings.dsp_component(vars, ModTarget::FinePitch(index)) >> shape_fn(|x| pow(SEMITONE_RATIO, x))))
            * (1.0 + fm_oscs * 100.0);

        // have to compensate for different volumes. the sine is so loud!
        match self {
            Self::Sawtooth => Net::wrap(Box::new(base >> saw())),
            Self::Pulse => {
                let duty_mod = settings.dsp_component(vars, ModTarget::Duty(index));
                Net::wrap(Box::new((base | var(&osc.duty) + duty_mod >> follow(0.01)) >> pulse()))
            },
            Self::Triangle => Net::wrap(Box::new(base >> triangle())),
            Self::Sine => Net::wrap(Box::new(base >> sine() * 0.5)),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum FilterType {
    Ladder,
    Lowpass,
    Highpass,
    Bandpass,
}

impl FilterType {
    pub const VARIANTS: [FilterType; 4] = [Self::Ladder, Self::Lowpass, Self::Highpass, Self::Bandpass];
    
    pub fn name(&self) -> &str {
        match self {
            Self::Ladder => "Ladder",
            Self::Lowpass => "Lowpass",
            Self::Highpass => "Highpass",
            Self::Bandpass => "Bandpass",
        }
    }
}

pub struct Synth {
    pub settings: Settings,
    voices: HashMap<Key, Voice>,
    bend_memory: [f32; 16],
    mod_memory: f32,
}

impl Synth {
    pub fn new() -> Self {
        Self {
            settings: Settings {
                oscs: vec![Oscillator::new()],
                envs: vec![ADSR::new()],
                filter: Filter::new(),
                play_mode: PlayMode::Poly,
                glide_time: 0.05,
                mod_matrix: vec![Modulation {
                    source: ModSource::Envelope(0),
                    target: ModTarget::Gain,
                    depth: shared(0.0),
                }, Modulation {
                    source: ModSource::Pressure,
                    target: ModTarget::Gain,
                    depth: shared(0.0),
                }],
            },
            voices: HashMap::new(),
            bend_memory: [0.0; 16],
            mod_memory: 0.0,
        }
    }

    pub fn note_on(&mut self, key: Key, pitch: f32, pressure: f32, seq: &mut Sequencer) {
        let bend = if key.origin == KeyOrigin::Midi {
            self.bend_memory[key.channel as usize]
        } else {
            0.0
        };
        let insert_voice = match self.settings.play_mode {
            PlayMode::Poly => true,
            PlayMode::Mono => {
                for voice in self.voices.values_mut() {
                    voice.off(seq);
                }
                true
            },
            PlayMode::SingleTrigger => {
                if self.voices.is_empty() {
                    true
                } else {
                    let voice = self.voices.drain().map(|(_, v)| v).next()
                        .expect("voices confirmed non-empty");
                    voice.vars.freq.set(midi_hz(pitch));
                    self.voices.insert(key.clone(), voice);
                    false
                }
            },
        };
        if insert_voice {
           self.voices.insert(key, Voice::new(pitch, bend, pressure, self.mod_memory, &self.settings, seq));
        }
    }

    pub fn note_off(&mut self, key: Key, seq: &mut Sequencer) {
        if let Some(voice) = self.voices.remove(&key) {
            voice.off(seq);
        }
    }

    pub fn pitch_bend(&mut self, channel: u8, bend: f32) {
        self.bend_memory[channel as usize] = bend;
        for (key, voice) in self.voices.iter_mut() {
            if key.origin == KeyOrigin::Midi && key.channel == channel {
                voice.vars.freq.set(midi_hz(voice.base_pitch + bend));
            }
        }
    }

    pub fn poly_pressure(&mut self, key: Key, pressure: f32) {
        self.voices.get(&key).inspect(|v| v.vars.pressure.set(pressure));
    }

    pub fn channel_pressure(&mut self, channel: u8, pressure: f32) {
        for (key, voice) in self.voices.iter_mut() {
            if key.origin == KeyOrigin::Midi && key.channel == channel {
                voice.vars.pressure.set(pressure);
            }
        }
    }

    pub fn modulate(&mut self, depth: f32) {
        self.mod_memory = depth;
        for (key, voice) in self.voices.iter_mut() {
            if key.origin == KeyOrigin::Midi {
                voice.vars.modulation.set(depth);
            }
        }
    }
}

pub struct Settings {
    pub oscs: Vec<Oscillator>,
    pub envs: Vec<ADSR>,
    pub filter: Filter,
    pub play_mode: PlayMode,
    pub glide_time: f32,
    pub mod_matrix: Vec<Modulation>,
}

impl Settings {
    fn dsp_component(&self, vars: &VoiceVars, target: ModTarget) -> Net {
        let mut net = Net::wrap(Box::new(constant(if target.is_additive() { 0.0 } else { 1.0 })));
        for m in &self.mod_matrix {
            if m.target == target {
                if target.is_additive() {
                    net = net + m.dsp_component(&self, &vars);
                } else {
                    net = net * m.dsp_component(&self, &vars);
                }
            }
        }
        net
    }

    pub fn mod_sources(&self) -> Vec<ModSource> {
        let mut v = vec![ModSource::Pitch, ModSource::Pressure, ModSource::Modulation, ModSource::Random];
        for i in 0..self.envs.len() {
            v.push(ModSource::Envelope(i));
        }
        v
    }

    pub fn mod_targets(&self) -> Vec<ModTarget> {
        let mut v = vec![ModTarget::Gain, ModTarget::FilterCutoff];
        for i in 0..self.oscs.len() {
            v.push(ModTarget::Level(i));
            v.push(ModTarget::Pitch(i));
            v.push(ModTarget::FinePitch(i));
            v.push(ModTarget::Duty(i));
        }
        v
    }

    pub fn outputs(&self, osc_index: usize) -> Vec<OscOutput> {
        if osc_index == 0 {
            vec![OscOutput::Mix(0)]
        } else {
            (0..osc_index).flat_map(|i| vec![
                OscOutput::Mix(i),
                OscOutput::AM(i),
                OscOutput::FM(i),
            ]).collect()
        }
    }

    pub fn remove_osc(&mut self, i: usize) {
        if i < self.oscs.len() {
            self.oscs.remove(i);

            // update outputs for new osc indices
            for (j, osc) in self.oscs.iter_mut().enumerate() {
                if j == 0 {
                    osc.output = OscOutput::Mix(0);
                } else {
                    match osc.output {
                        OscOutput::Mix(n) if n > i => osc.output = OscOutput::Mix(n - 1),
                        OscOutput::AM(n) if n > i => osc.output = OscOutput::AM(n - 1),
                        OscOutput::FM(n) if n > i => osc.output = OscOutput::FM(n - 1),
                        _ => (),
                    }
                }
            }

            // update mod matrix for new osc indices
            self.mod_matrix.retain(|m| m.target.osc() != Some(i));
            for m in self.mod_matrix.iter_mut() {
                if let Some(n) = m.target.osc() {
                    if n > i {
                        m.target = ModTarget::Duty(n - 1);
                    }
                }
            }
        }
    }

    pub fn remove_env(&mut self, i: usize) {
        if i < self.envs.len() {
            self.envs.remove(i);

            // update mod matrix for new env indices
            self.mod_matrix.retain(|m| m.source != ModSource::Envelope(i));
            for m in self.mod_matrix.iter_mut() {
                if let ModSource::Envelope(n) = m.source {
                    if n > i {
                        m.source = ModSource::Envelope(n - 1);
                    }
                }
            }
        }
    }

    fn make_osc(&self, i: usize, vars: &VoiceVars) -> Net {
        // FIXME: right now, output can sound different depending on the order oscs are mixed in.
        //        this is because of pseudorandom phase based on node location in its network.
        //        this should be fixable in the next published version of the crate.
        let mut mixed_oscs = Net::new(0, 1);
        let mut am_oscs = Net::wrap(Box::new(constant(1.0)));
        let mut fm_oscs = Net::new(0, 1);
        for (j, osc) in self.oscs.iter().enumerate() {
            if j > i {
                if osc.output == OscOutput::Mix(i) {
                    mixed_oscs = mixed_oscs + self.make_osc(j, vars);
                } else if osc.output == OscOutput::AM(i) {
                    am_oscs = am_oscs * self.make_osc(j, vars);
                } else if osc.output == OscOutput::FM(i) {
                    fm_oscs = fm_oscs + self.make_osc(j, vars);
                }
            }
        }

        (self.oscs[i].waveform.make_net(self, &vars, &self.oscs[i], i, fm_oscs))
            * (var(&self.oscs[i].level) >> follow(0.01))
            * self.dsp_component(&vars, ModTarget::Level(i))
            * am_oscs
            + mixed_oscs
    }
}

pub struct Oscillator {
    pub level: Shared,
    pub duty: Shared,
    pub freq_ratio: Shared,
    pub fine_pitch: Shared,
    pub waveform: Waveform,
    pub output: OscOutput,
}

impl Oscillator {
    pub fn new() -> Self {
        Self {
            level: shared(0.5),
            duty: shared(0.5),
            freq_ratio: shared(1.0),
            fine_pitch: shared(0.0),
            waveform: Waveform::Sine,
            output: OscOutput::Mix(0),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum OscOutput {
    Mix(usize),
    AM(usize),
    FM(usize),
}

impl Display for OscOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Mix(i) if *i == 0 => "Mix",
            Self::Mix(i) => &format!("Mix to osc {}", i + 1),
            Self::AM(i) => &format!("RM osc {}", i + 1),
            Self::FM(i) => &format!("FM osc {}", i + 1),
        };
        f.write_str(s)
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum KeyTracking {
    None,
    Partial,
    Full,
}

impl KeyTracking {
    pub const VARIANTS: [KeyTracking; 3] = [Self::None, Self::Partial, Self::Full];

    pub fn name(&self) -> &str {
        match self {
            Self::None => "None",
            Self::Partial => "Partial",
            Self::Full => "Full",
        }
    }
}

pub struct Filter {
    pub filter_type: FilterType,
    pub cutoff: Shared,
    pub resonance: Shared,
    pub key_tracking: KeyTracking,
}

impl Filter {
    fn new() -> Self {
        Self {
            cutoff: shared(20_000.0),
            resonance: shared(0.1),
            key_tracking: KeyTracking::None,
            filter_type: FilterType::Ladder,
        }
    }

    fn make_net(&self, settings: &Settings, vars: &VoiceVars) -> Net {
        // FIXME: partial key tracking uses linear math, when it should be logarithmic
        let kt = match self.key_tracking {
            KeyTracking::None => Net::wrap(Box::new(constant(1.0))),
            KeyTracking::Partial => Net::wrap(Box::new((var(&vars.freq) + KEY_TRACKING_REF_FREQ) * 0.5 * (1.0/KEY_TRACKING_REF_FREQ))),
            KeyTracking::Full => Net::wrap(Box::new(var(&vars.freq) * (1.0/KEY_TRACKING_REF_FREQ))),
        };
        let cutoff_mod = settings.dsp_component(vars, ModTarget::FilterCutoff) >> shape_fn(|x| pow(4.0, x));
        let f = match self.filter_type {
            FilterType::Ladder => Net::wrap(Box::new(moog())),
            FilterType::Lowpass => Net::wrap(Box::new(lowpass())),
            FilterType::Highpass => Net::wrap(Box::new(highpass())),
            FilterType::Bandpass => Net::wrap(Box::new(bandpass())),
        };
        (pass() | var(&self.cutoff) * kt * cutoff_mod | var(&self.resonance)) >> f
    }
}

pub struct ADSR {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl ADSR {
    pub fn new() -> Self {
        Self {
            attack: 0.01,
            decay: 1.0,
            sustain: 1.0,
            release: 0.01,
        }
    }

    fn make_node(&self, gate: &Shared) -> An<Pipe<Var, EnvelopeIn<f32, impl FnMut(f32, &numeric_array::NumericArray<f32, typenum::UInt<typenum::UTerm, typenum::B1>>) -> f32 + Clone, typenum::UInt<typenum::UTerm, typenum::B1>, f32>>> {
        var(gate) >> adsr_live(self.attack, self.decay, self.sustain, self.release)
    }
}

pub struct Modulation {
    pub source: ModSource,
    pub target: ModTarget,
    pub depth: Shared,
}

impl Modulation {
    pub fn default() -> Self {
        Self {
            source: ModSource::Modulation,
            target: ModTarget::FilterCutoff,
            depth: shared(0.0),
        }
    }

    fn dsp_component(&self, settings: &Settings, vars: &VoiceVars) -> Net {
        let net = match self.source {
            ModSource::Pitch => Net::wrap(Box::new(var_fn(&vars.freq,|f| dexerp(20.0, 5000.0, f)))),
            ModSource::Pressure => Net::wrap(Box::new(var(&vars.pressure))),
            ModSource::Modulation => Net::wrap(Box::new(var(&vars.modulation))),
            ModSource::Random => Net::wrap(Box::new(constant(random::<f32>()))),
            ModSource::Envelope(i) => match settings.envs.get(i) {
                Some(env) => Net::wrap(Box::new(env.make_node(&vars.gate))),
                None => Net::wrap(Box::new(zero())),
            },
        };
        if self.target.is_additive() {
            net * var(&self.depth)
        } else {
            net
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum ModSource {
    Pitch,
    Pressure,
    Modulation,
    Random,
    Envelope(usize),
}

impl Display for ModSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Pitch => "Pitch",
            Self::Pressure => "Pressure",
            Self::Modulation => "Mod wheel",
            Self::Random => "Random",
            Self::Envelope(i) => &format!("Envelope {}", i + 1),
        };
        f.write_str(s)
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum ModTarget {
    Gain,
    Level(usize),
    Pitch(usize),
    FinePitch(usize),
    Duty(usize),
    FilterCutoff,
}

impl ModTarget {
    pub fn is_additive(&self) -> bool {
        match *self  {
            ModTarget::Gain | ModTarget::Level(_) => false,
            _ => true,
        }
    }

    fn osc(&self) -> Option<usize> {
        match *self {
            ModTarget::Level(n) | ModTarget::Pitch(n) |
                ModTarget::FinePitch(n) | ModTarget::Duty(n) => Some(n),
            _ => None,
        }
    }
}

impl Display for ModTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Gain => "Gain",
            Self::Level(n) => &format!("Osc {} level", n + 1),
            Self::Pitch(n) => &format!("Osc {} pitch", n + 1),
            Self::FinePitch(n) => &format!("Osc {} fine pitch", n + 1),
            Self::Duty(n) => &format!("Osc {} duty", n + 1),
            Self::FilterCutoff => "Filter cutoff",
        };
        f.write_str(s)
    }
}

struct Voice {
    vars: VoiceVars,
    base_pitch: f32,
    release_time: f32,
    event_id: EventId,
}

impl Voice {
    fn new(pitch: f32, bend: f32, pressure: f32, modulation: f32, settings: &Settings, seq: &mut Sequencer) -> Self {
        let gate = shared(1.0);
        let vars = VoiceVars {
            freq: shared(midi_hz(pitch + bend)),
            gate,
            pressure: shared(pressure),
            modulation: shared(modulation),
        };
        let net = (settings.make_osc(0, &vars) >> settings.filter.make_net(&settings, &vars))
            * settings.dsp_component(&vars, ModTarget::Gain);
        Self {
            vars,
            base_pitch: pitch,
            release_time: settings.envs.iter().map(|env| env.release).fold(0.0, f32::max),
            event_id: seq.push_relative(0.0, f64::INFINITY, Fade::Smooth, 0.0, 0.0, Box::new(net)),
        }
    }

    fn off(&self, seq: &mut Sequencer) {
        self.vars.gate.set(0.0);
        seq.edit_relative(self.event_id, self.release_time as f64, 0.01);
    }
}

struct VoiceVars {
    freq: Shared,
    pressure: Shared,
    modulation: Shared,
    gate: Shared,
}