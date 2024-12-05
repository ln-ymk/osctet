use fundsp::hacker32::*;

use crate::{fx::GlobalFX, module::{EventData, Module, TrackEdit, TICKS_PER_BEAT}, synth::{Key, KeyOrigin, Patch, Synth}};

const DEFAULT_TEMPO: f32 = 120.0;
const LOOP_FADEOUT_TIME: f32 = 10.0;

// maximum values as written to pattern
const MAX_PRESSURE: u8 = 9;
const MAX_MODULATION: u8 = 9;

/// Handles module playback. In methods that take a `track` argument, 0 can
/// safely be used for keyjazz events (since track 0 will never sequence).
pub struct Player {
    pub seq: Sequencer,
    synths: Vec<Synth>, // one per track
    playing: bool,
    tick: u32,
    playtime: f64,
    tempo: f32,
    looped: bool,
}

impl Player {
    pub fn new(seq: Sequencer, num_tracks: usize) -> Self {
        Self {
            seq,
            synths: (0..=num_tracks).map(|i| Synth::new(i)).collect(),
            playing: false,
            tick: 0,
            playtime: 0.0, // not total playtime!
            tempo: DEFAULT_TEMPO,
            looped: false,
        }
    }

    pub fn reinit(&mut self, num_tracks: usize) {
        self.synths = (0..=num_tracks).map(|i| Synth::new(i)).collect();
        self.playing = false;
        self.tick = 0;
        self.playtime = 0.0;
        self.tempo = DEFAULT_TEMPO;
        self.looped = false;
    }

    pub fn get_tick(&self) -> u32 {
        self.tick
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.clear_notes_with_origin(KeyOrigin::Pattern);
    }

    pub fn play(&mut self) {
        self.playing = true;
        self.playtime = 0.0;
        self.looped = false;
    }

    pub fn play_from(&mut self, tick: u32) {
        // TODO: calulcate correct memory if tick is nonzero
        for synth in self.synths.iter_mut() {
            synth.reset_memory();
        }
        self.tempo = DEFAULT_TEMPO;
        self.tick = tick;
        self.play();
    }

    pub fn update_synths(&mut self, edits: Vec<TrackEdit>) {
        for edit in edits {
            match edit {
                TrackEdit::Insert(i) => self.synths.insert(i, Synth::new(i)),
                TrackEdit::Remove(i) => { self.synths.remove(i); }
            }
        }
    }

    pub fn note_on(&mut self, track: usize, key: Key,
        pitch: f32, pressure: Option<f32>, patch: &Patch
    ) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.note_on(key, pitch, pressure, patch, &mut self.seq);
        }
    }

    pub fn note_off(&mut self, track: usize, key: Key) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.note_off(key, &mut self.seq);
        }
    }

    pub fn poly_pressure(&mut self, track: usize, key: Key, pressure: f32) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.poly_pressure(key, pressure);
        }
    }

    pub fn modulate(&mut self, track: usize, channel: u8, depth: f32) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.modulate(channel, depth);
        }
    }

    pub fn channel_pressure(&mut self, track: usize, channel: u8, pressure: f32) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.channel_pressure(channel, pressure);
        }
    }

    pub fn pitch_bend(&mut self, track: usize, channel: u8, bend: f32) {
        if let Some(synth) = self.synths.get_mut(track) {
            synth.pitch_bend(channel, bend);
        }
    }

    pub fn clear_notes_with_origin(&mut self, origin: KeyOrigin) {
        for synth in self.synths.iter_mut() {
            synth.clear_notes_with_origin(&mut self.seq, origin);
        }
    }

    pub fn frame(&mut self, module: &Module, dt: f32) {
        if !self.playing {
            return
        }

        self.playtime += dt as f64;
        let prev_tick = self.tick;
        self.tick += interval_ticks(self.playtime, self.tempo);
        self.playtime -= tick_interval(self.tick - prev_tick, self.tempo);

        for (track_i, track) in module.tracks.iter().enumerate() {
            for (channel_i, channel) in track.channels.iter().enumerate() {
                for event in &channel.events {
                    if event.tick >= prev_tick && event.tick < self.tick {
                        self.handle_event(&event.data, module, track_i, channel_i);
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, data: &EventData, module: &Module,
        track: usize, channel: usize
    ) {
        let key = Key {
            origin: KeyOrigin::Pattern,
            channel: channel as u8,
            key: 0,
        };

        match *data {
            EventData::Pitch(note) => {
                if let Some((patch, note)) = module.map_note(note, track) {
                    let pitch = module.tuning.midi_pitch(&note);
                    self.note_on(track, key, pitch, None, patch);
                }
            }
            EventData::Pressure(v) => {
                self.channel_pressure(track, channel as u8, v as f32 / MAX_PRESSURE as f32);
            }
            EventData::Modulation(v) => {
                self.modulate(track, channel as u8, v as f32 / MAX_MODULATION as f32);
            }
            EventData::NoteOff => {
                self.note_off(track, key);
            }
            EventData::Tempo(t) => self.tempo = t,
            EventData::RationalTempo(n, d) => self.tempo *= n as f32 / d as f32,
            EventData::End => if let Some(tick) = module.find_loop_start(self.tick) {
                self.go_to(tick);
                self.looped = true;
            } else {
                self.stop();
            },
            EventData::Loop => (),
        }
    }

    fn go_to(&mut self, tick: u32) {
        self.tick = tick;
    } 
}

fn interval_ticks(dt: f64, tempo: f32) -> u32 {
    (dt * tempo as f64 / 60.0 * TICKS_PER_BEAT as f64).round() as u32
}

fn tick_interval(dtick: u32, tempo: f32) -> f64 {
    dtick as f64 / tempo as f64 * 60.0 / TICKS_PER_BEAT as f64
}

/// Renders module to PCM. Loops forever if module is missing END!
pub fn render(module: &Module) -> Wave {
    let sample_rate = 44100;
    let mut wave = Wave::new(2, sample_rate as f64);
    let mut seq = Sequencer::new(false, 2);
    seq.set_sample_rate(sample_rate as f64);
    let mut fx = GlobalFX::new(seq.backend(), &module.fx);
    let fadeout_gain = shared(1.0);
    fx.net = fx.net * (var(&fadeout_gain) | var(&fadeout_gain));
    fx.net.set_sample_rate(sample_rate as f64);
    let mut player = Player::new(seq, module.tracks.len());
    let mut backend = BlockRateAdapter::new(Box::new(fx.net.backend()));
    let block_size = 64;
    let dt = block_size as f32 / sample_rate as f32;
    let mut time_since_loop = 0.0;

    // TODO: render would probably be faster if we called player.frame() only
    //       when there's a new event. benchmark this
    player.play();
    while player.playing && time_since_loop < LOOP_FADEOUT_TIME {
        player.frame(module, dt);
        for _ in 0..block_size {
            wave.push(backend.get_stereo());
        }
        if player.looped {
            fadeout_gain.set(1.0 - (time_since_loop / LOOP_FADEOUT_TIME));
            time_since_loop += dt;
        }
    }

    wave
}