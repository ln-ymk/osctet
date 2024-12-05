//! Microtonal tracker with built-in subtractive/FM synth.

use std::error::Error;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender, Receiver};

use config::Config;
use fx::{FXSettings, GlobalFX};
use midir::{InitError, MidiInput, MidiInputConnection, MidiInputPort};
use fundsp::hacker32::*;
use cpal::{traits::{DeviceTrait, HostTrait, StreamTrait}, StreamConfig};
use module::{EventData, Module, TrackTarget};
use playback::Player;
use rfd::FileDialog;
use synth::{Key, KeyOrigin};
use macroquad::prelude::*;

pub mod pitch;
mod input;
mod config;
pub mod synth;
mod adsr;
pub mod fx;
pub mod ui;
pub mod module;
pub mod playback;

use input::MidiEvent;
use ui::instruments_tab::fix_patch_index;
use ui::pattern_tab::PatternEditor;

/// Application name, for window title, etc.
pub const APP_NAME: &str = "Osctet";
const MODULE_FILETYPE_NAME: &str = "Osctet module";
const MODULE_EXT: &str = "osctet";

const TABS: [&str; 3] = ["General", "Pattern", "Instruments"];

struct Midi {
    // Keep one input around for listing ports. If we need to connect, we'll
    // create a new input just for that (see Boddlnagg/midir#90).
    input: Option<MidiInput>,
    port_name: Option<String>,
    port_selection: Option<String>,
    conn: Option<MidiInputConnection<Sender<Vec<u8>>>>,
    rx: Option<Receiver<Vec<u8>>>,
    input_id: u16,
    rpn: (u8, u8),
    bend_range: f32,
}

impl Midi {
    fn new() -> Self {
        let mut m = Self {
            input: None,
            port_name: None,
            port_selection: None,
            conn: None,
            rx: None,
            input_id: 0,
            rpn: (0, 0),
            bend_range: 2.0,
        };
        m.input = m.new_input().ok();
        m
    }

    fn new_input(&mut self) -> Result<MidiInput, InitError> {
        self.input_id += 1;
        MidiInput::new(&format!("{} input #{}", APP_NAME, self.input_id))
    }

    fn selected_port(&self) -> Result<MidiInputPort, &'static str> {
        if let Some(selection) = &self.port_selection {
            if let Some(input) = &self.input {
                for port in input.ports() {
                    if let Ok(name) = input.port_name(&port) {
                        if name == *selection {
                            return Ok(port)
                        }
                    }
                }
                Err("Selected MIDI device not found")
            } else {
                Err("Could not open MIDI")
            }
        } else {
            Err("No MIDI device selected")
        }
    }
}

const MAIN_TAB_ID: &str = "main";
const TAB_GENERAL: usize = 0;
const TAB_PATTERN: usize = 1;
const TAB_INSTRUMENTS: usize = 2;

struct App {
    player: Player,
    octave: i8,
    midi: Midi,
    config: Config,
    module: Module,
    fx: GlobalFX,
    patch_index: Option<usize>, // if None, kit is selected
    ui: ui::UI,
    fullscreen: bool,
    pattern_editor: PatternEditor,
    instruments_scroll: f32,
    save_path: Option<PathBuf>,
}

impl App {
    fn new(seq: Sequencer, global_fx: GlobalFX, fx_settings: FXSettings) -> Self {
        let mut err: Option<Box<dyn Error>> = None;
        let config = match Config::load() {
            Ok(c) => c,
            Err(e) => {
                err = Some(e);
                Config::default()
            },
        };
        let mut midi = Midi::new();
        midi.port_selection = config.default_midi_input.clone();
        let module = Module::new(fx_settings);
        let mut app = App {
            player: Player::new(seq, module.tracks.len()),
            octave: 4,
            midi,
            config,
            module,
            fx: global_fx,
            patch_index: Some(0),
            ui: ui::UI::new(),
            fullscreen: false,
            pattern_editor: PatternEditor::new(),
            instruments_scroll: 0.0,
            save_path: None,
        };
        if let Some(err) = err {
            app.ui.report(err);
        }
        app
    }

    // TODO: switching tracks while keyjazzing could result in stuck notes
    // TODO: entering note input mode while keyjazzing could result in stuck notes
    // TODO: can keyjazzing mess up synth memory in a way that matters?
    fn keyjazz_track(&self) -> usize {
        if self.ui.get_tab(MAIN_TAB_ID) == Some(TAB_PATTERN) {
            self.pattern_editor.cursor_track()
        } else {
            0
        }
    }

    fn keyjazz_patch_index(&self) -> Option<usize> {
        match self.module.tracks[self.keyjazz_track()].target {
            TrackTarget::Global | TrackTarget::None => self.patch_index,
            TrackTarget::Kit => None,
            TrackTarget::Patch(i) => Some(i),
        }
    }

    // TODO: use most current vel/mod setting when keyjazzing in pattern
    fn handle_keys(&mut self) {
        let (pressed, released) = (get_keys_pressed(), get_keys_released());

        for key in released {
            if let Some(_) = input::note_from_key(key, &self.module.tuning, self.octave) {
                let key = Key {
                    origin: KeyOrigin::Keyboard,
                    channel: 0,
                    key: input::u8_from_key(key),
                };
                self.player.note_off(self.keyjazz_track(), key);
            }
        }

        let shift = is_key_down(KeyCode::LeftShift) || is_key_down(KeyCode::RightShift);
        let ctrl = is_key_down(KeyCode::LeftControl) || is_key_down(KeyCode::RightControl);
        for key in pressed {
            if ctrl {
                // TODO: undo/redo are silent right now, which could be confusing when
                //       things are being undone/redone offscreen. could either provide
                //       messages describing what's being done, or move view to location
                //       of changes
                match key {
                    KeyCode::E => self.render_and_save(),
                    KeyCode::N => self.new_module(), // TODO: prompt if unsaved
                    KeyCode::O => self.open_module(), // TODO: prompt if unsaved
                    KeyCode::S => if shift {
                        self.save_module_as();
                    } else {
                        self.save_module();
                    }
                    KeyCode::Y => if self.module.redo() {
                        self.player.update_synths(self.module.drain_track_history());
                        fix_patch_index(&mut self.patch_index, self.module.patches.len());
                    } else {
                        self.ui.report("Nothing to redo");
                    },
                    KeyCode::Z => if self.module.undo() {
                        self.player.update_synths(self.module.drain_track_history());
                        fix_patch_index(&mut self.patch_index, self.module.patches.len());
                    } else {
                        self.ui.report("Nothing to undo");
                    },
                    _ => (),
                }
            } else if let Some(note) = input::note_from_key(key, &self.module.tuning, self.octave) {
                self.ui.note_queue.push(EventData::Pitch(note));
                if !self.ui.accepting_note_input()
                    && !self.pattern_editor.in_digit_column(&self.ui) {
                    if let Some((patch, note)) =
                        self.module.map_input(self.keyjazz_patch_index(), note) {
                        let key = Key {
                            origin: KeyOrigin::Keyboard,
                            channel: 0,
                            key: input::u8_from_key(key),
                        };
                        let pitch = self.module.tuning.midi_pitch(&note);
                        self.player.note_on(self.keyjazz_track(), key, pitch, None, patch);
                    }
                }
            } else {
                match key {
                    KeyCode::F1 => self.ui.set_tab(MAIN_TAB_ID, 0),
                    KeyCode::F2 => self.ui.set_tab(MAIN_TAB_ID, 1),
                    KeyCode::F3 => self.ui.set_tab(MAIN_TAB_ID, 2),
                    KeyCode::F5 => self.player.play_from(0),
                    KeyCode::F7 => self.player.play_from(self.pattern_editor.cursor_tick()),
                    KeyCode::F8 => self.player.stop(),
                    KeyCode::F11 => {
                        self.fullscreen = !self.fullscreen;
                        set_fullscreen(self.fullscreen);
                    },
                    _ => (),
                }
            }
        }
    }

    fn midi_connect(&mut self) -> Result<MidiInputConnection<Sender<Vec<u8>>>, Box<dyn Error>> {
        match self.midi.selected_port() {
            Ok(port) => {
                match self.midi.new_input() {
                    Ok(mut input) => {
                        // ignore SysEx, time, and active sensing
                        input.ignore(midir::Ignore::All);
                        let (tx, rx) = channel();
                        self.midi.rx = Some(rx);
                        Ok(input.connect(
                            &port,
                            APP_NAME,
                            move |_, message, tx| {
                                // ignore the error here, it probably just means that the
                                // user changed ports
                                let _ = tx.send(message.to_vec());
                            },
                            tx,
                        )?)
                    },
                    Err(e) => Err(Box::new(e)),
                }
            },
            Err(e) => Err(e.into()),
        }
    }

    fn handle_midi(&mut self) {
        if let Some(rx) = &self.midi.rx {
            while let Ok(v) = rx.try_recv() {
                if let Some(evt) = MidiEvent::parse(&v) {
                    if let MidiEvent::NoteOn { key, velocity, .. } = evt {
                        self.ui.note_queue.push(EventData::Pitch(
                            input::note_from_midi(key, &self.module.tuning)));
                        let v = (velocity as f32 * 9.0 / 127.0).round() as u8;
                        self.ui.note_queue.push(EventData::Pressure(v));
                    }
                    if !self.ui.accepting_note_input() {
                        match evt {
                            MidiEvent::NoteOff { channel, key, .. } => {
                                let key = Key {
                                    origin: KeyOrigin::Midi,
                                    channel,
                                    key,
                                };
                                self.player.note_off(self.keyjazz_track(), key);
                            },
                            MidiEvent::NoteOn { channel, key, velocity } => {
                                if velocity != 0 {
                                    let note = input::note_from_midi(key, &self.module.tuning);
                                    if let Some((patch, note)) =
                                        self.module.map_input(self.keyjazz_patch_index(), note) {
                                        let key = Key {
                                            origin: KeyOrigin::Midi,
                                            channel,
                                            key,
                                        };
                                        let pitch = self.module.tuning.midi_pitch(&note);
                                        let pressure = velocity as f32 / 127.0;
                                        self.player.note_on(self.keyjazz_track(),
                                            key, pitch, Some(pressure), patch);
                                    }
                                } else {
                                    let key = Key {
                                        origin: KeyOrigin::Midi,
                                        channel,
                                        key,
                                    };
                                    self.player.note_off(self.keyjazz_track(), key);
                                }
                            },
                            MidiEvent::PolyPressure { channel, key, pressure } => {
                                if self.config.midi_send_pressure == Some(true) {
                                    self.player.poly_pressure(self.keyjazz_track(), Key {
                                        origin: KeyOrigin::Midi,
                                        channel,
                                        key,
                                    }, pressure as f32 / 127.0);
                                }
                            },
                            MidiEvent::Controller { channel, controller, value } => {
                                match controller {
                                    input::CC_MODULATION | input::CC_MACRO_MIN..=input::CC_MACRO_MAX => {
                                        self.player.modulate(self.keyjazz_track(),
                                            channel, value as f32 / 127.0);
                                    },
                                    input::CC_RPN_MSB => self.midi.rpn.0 = value,
                                    input::CC_RPN_LSB => self.midi.rpn.1 = value,
                                    input::CC_DATA_ENTRY_MSB => if self.midi.rpn == input::RPN_PITCH_BEND_SENSITIVITY {
                                        // set semitones
                                        self.midi.bend_range = self.midi.bend_range % 1.0 + value as f32;
                                    },
                                    input:: CC_DATA_ENTRY_LSB => if self.midi.rpn == input::RPN_PITCH_BEND_SENSITIVITY {
                                        // set cents
                                        self.midi.bend_range = self.midi.bend_range.round() + value as f32 / 100.0;
                                    },
                                    _ => (),
                                }
                            },
                            MidiEvent::ChannelPressure { channel, pressure } => {
                                if self.config.midi_send_pressure == Some(true) {
                                    self.player.channel_pressure(self.keyjazz_track(),
                                        channel, pressure as f32 / 127.0);
                                }
                            },
                            MidiEvent::Pitch { channel, bend } => {
                                self.player.pitch_bend(self.keyjazz_track(),
                                    channel, bend * self.midi.bend_range);
                            },
                        }
                    }
                }
            }
        }
    }

    fn check_midi_reconnect(&mut self) {
        if self.midi.port_selection.is_some() && self.midi.port_selection != self.midi.port_name {
            match self.midi_connect() {
                Ok(conn) => {
                    let old_conn = std::mem::replace(&mut self.midi.conn, Some(conn));
                    if let Some(c) = old_conn {
                        c.close();
                    }
                    self.midi.port_name = self.midi.port_selection.clone();
                    self.config.default_midi_input = self.midi.port_name.clone();
                    if let Err(e) = self.config.save() {
                        self.ui.report(e);
                    };
                },
                Err(e) => {
                    self.midi.port_selection = None;
                    self.ui.report(e);
                },
            }
        }
    }

    fn frame(&mut self) {
        if self.ui.accepting_keyboard_input() {
            self.player.clear_notes_with_origin(KeyOrigin::Keyboard);
        } else {
            self.handle_keys();
        }
        if self.ui.accepting_note_input() {
            self.player.clear_notes_with_origin(KeyOrigin::Midi);
        }
        self.handle_midi();
        self.check_midi_reconnect();
        self.process_ui();
        self.player.frame(&self.module, get_frame_time());
    }
    
    fn process_ui(&mut self) {
        self.ui.start_frame();

        self.bottom_panel();

        match self.ui.tab_menu(MAIN_TAB_ID, &TABS) {
            TAB_GENERAL => ui::general_tab::draw(&mut self.ui, &mut self.module,
                &mut self.fx),
            TAB_PATTERN => ui::pattern_tab::draw(&mut self.ui, &mut self.module,
                &mut self.player, &mut self.pattern_editor),
            TAB_INSTRUMENTS => ui::instruments_tab::draw(&mut self.ui, &mut self.module,
                &mut self.patch_index, &mut self.instruments_scroll),
            _ => panic!("bad tab value"),
        }

        self.ui.end_frame();
    }
    
    fn bottom_panel(&mut self) {
        self.ui.start_bottom_panel();

        if self.midi.input.is_some() {
            let s = if let Some(name) = &self.midi.port_name {
                &name
            } else {
                "(none)"
            };
            if let Some(i) = self.ui.combo_box("midi_input", "MIDI input", s,
                || input_names(self.midi.input.as_ref().unwrap())) {
                self.midi.port_selection = input_names(self.midi.input.as_ref().unwrap()).get(i).cloned();
            }

            let mut v = self.config.midi_send_pressure.unwrap_or(true);
            if self.ui.checkbox("Use aftertouch", &mut v) {
                self.config.midi_send_pressure = Some(v);
                if let Err(e) = self.config.save() {
                    self.ui.report(e);
                }
            }
        } else {
            self.ui.label("No MIDI device");
        }

        if let Some(n) = self.ui.edit_box("Division", 3,
            self.pattern_editor.beat_division.to_string()) {
            match n.parse::<u8>() {
                Ok(n) => self.pattern_editor.beat_division = n,
                Err(e) => self.ui.report(e),
            }
        }
        
        self.ui.end_bottom_panel();
    }
    
    fn render_and_save(&mut self) {
        if self.module.ends() {
            if let Some(path) = FileDialog::new()
                .add_filter("WAV file", &["wav"])
                .save_file() {
                match playback::render(&self.module).save_wav16(path) {
                    Ok(_) => self.ui.report("Wrote WAV."),
                    Err(e) => self.ui.report(e),
                }
            }
        } else {
            self.ui.report("Module must have END event to export")
        }
    }

    fn new_module(&mut self) {
        self.load_module(Module::new(Default::default()));
        self.save_path = None;
    }

    fn save_module(&mut self) {
        if let Some(path) = &self.save_path {
            if let Err(e) = self.module.save(path) {
                self.ui.report(e);
            }
        } else {
            self.save_module_as();
        }
    }

    fn save_module_as(&mut self) {
        if let Some(path) = FileDialog::new()
            .add_filter(MODULE_FILETYPE_NAME, &[MODULE_EXT])
            .save_file() {
            if let Err(e) = self.module.save(&path) {
                self.ui.report(e);
            } else {
                self.save_path = Some(path);
            }
        }
    }

    fn open_module(&mut self) {
        if let Some(path) = FileDialog::new()
            .add_filter(MODULE_FILETYPE_NAME, &[MODULE_EXT])
            .pick_file() {
            match Module::load(&path) {
                Ok(module) => {
                    self.load_module(module);
                    self.save_path = Some(path);
                },
                Err(e) => self.ui.report(e),
            }
        }
    }

    fn load_module(&mut self, module: Module) {
        self.module = module;
        self.player.reinit(self.module.tracks.len());
        self.fx.reinit(&self.module.fx);
    }
}

fn input_names(input: &MidiInput) -> Vec<String> {
    input.ports().into_iter()
        .map(|p| input.port_name(&p).unwrap_or(String::from("(unknown)")))
        .collect()
}

/// Application entry point.
pub async fn run() -> Result<(), Box<dyn Error>> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("could not open audio output device")?;

    let config: StreamConfig = device.supported_output_configs()?
        .next()
        .ok_or("could not find audio output config")?
        .with_max_sample_rate()
        .into();

    let mut seq = Sequencer::new(false, 2);
    seq.set_sample_rate(config.sample_rate.0 as f64);

    let fx_settings: FXSettings = Default::default();
    let mut global_fx = GlobalFX::new(seq.backend(), &fx_settings);
    global_fx.net.set_sample_rate(config.sample_rate.0 as f64);
    let mut backend = BlockRateAdapter::new(Box::new(global_fx.net.backend()));
    
    let stream = device.build_output_stream(
        &config,move |data: &mut[f32], _: &cpal::OutputCallbackInfo| {
            // there's probably a better way to do this
            let mut i = 0;
            let len = data.len();
            while i < len {
                let (l, r) = backend.get_stereo();
                data[i] = l;
                data[i+1] = r;
                i += 2;
            }
        },
        move |err| {
            eprintln!("stream error: {}", err);
        },
        None
    )?;
    stream.play()?;

    let mut app = App::new(seq, global_fx, fx_settings);

    loop {
        app.frame();
        next_frame().await
    }
}