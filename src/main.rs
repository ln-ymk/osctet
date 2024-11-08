// disable console in windows release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ops::RangeInclusive;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::collections::VecDeque;

use config::Config;
use eframe::emath;
use midir::{InitError, MidiInput, MidiInputConnection, MidiInputPort};
use cpal::{traits::{DeviceTrait, HostTrait, StreamTrait}, StreamConfig};
use fundsp::hacker::*;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Ui};
use anyhow::{bail, Result};
use synth::{FilterType, Key, KeyOrigin, KeyTracking, PlayMode, Synth, Waveform};

mod pitch;
mod input;
mod config;
mod synth;
mod song;

const APP_NAME: &str = "Synth Tracker";

struct MessageBuffer {
    capacity: usize,
    messages: VecDeque<String>,
}

impl MessageBuffer {
    fn new(capacity: usize) -> Self {
        MessageBuffer {
            capacity,
            messages: VecDeque::new(),
        }
    }

    fn push(&mut self, msg: String) {
        self.messages.push_front(msg);
        self.messages.truncate(self.capacity);
    }

    fn report(&mut self, e: &impl std::fmt::Display) {
        self.push(format!("{}", e));
    }

    fn iter(&self) -> impl Iterator<Item = &'_ String> {
        self.messages.iter().rev()
    }
}

struct Midi {
    // Keep one input around for listing ports. If we need to connect, we'll
    // create a new input just for that (see Boddlnagg/midir#90).
    input: Option<MidiInput>,
    port_name: Option<String>,
    port_selection: Option<String>,
    conn: Option<MidiInputConnection<Sender<Vec<u8>>>>,
    rx: Option<Receiver<Vec<u8>>>,
    input_id: u16,
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
        };
        m.input = m.new_input().ok();
        m
    }

    fn new_input(&mut self) -> Result<MidiInput, InitError> {
        self.input_id += 1;
        MidiInput::new(&format!("{} input #{}", APP_NAME, self.input_id))
    }

    fn selected_port(&self) -> Option<MidiInputPort> {
        self.port_selection.as_ref().map(|selection| {
            self.input.as_ref().map(|input| {
                for port in input.ports() {
                    if let Ok(name) = input.port_name(&port) {
                        if name == *selection {
                            return Some(port)
                        }
                    }
                }
                None
            })?
        })?
    }
}

struct App {
    tuning: pitch::Tuning,
    messages: MessageBuffer,
    synth: Synth,
    seq: Sequencer,
    octave: i8,
    midi: Midi,
    config: Config,
    selected_osc: usize,
    global_fx: GlobalFX,
    song_editor: song::Editor,
}

impl App {
    fn new(synth: Synth, seq: Sequencer, global_fx: GlobalFX) -> Self {
        let mut messages = MessageBuffer::new(100);
        let config = match Config::load() {
            Ok(c) => c,
            Err(e) => {
                messages.push(format!("Could not load config: {}", &e));
                Config::default()
            },
        };
        let mut midi = Midi::new();
        midi.port_selection = config.default_midi_input.clone();
        App {
            tuning: pitch::Tuning::divide(2.0, 12, 1).unwrap(),
            messages,
            synth,
            seq,
            octave: 4,
            midi,
            config,
            selected_osc: 0,
            global_fx,
            song_editor: song::Editor::new(song::Song::new()),
        }
    }

    fn handle_ui_event(&mut self, evt: &egui::Event) {
        match evt {
            egui::Event::Key { physical_key, pressed, repeat, .. } => {
                if let Some(key) = physical_key {
                    if let Some(note) = input::note_from_key(key, &self.tuning, self.octave) {
                        if *pressed && !*repeat {
                            self.synth.note_on(Key {
                                origin: KeyOrigin::Keyboard,
                                channel: 0,
                                key: key.name().bytes().next().unwrap_or(0),
                            }, self.tuning.midi_pitch(&note), &mut self.seq);
                        } else if !*pressed {
                            self.synth.note_off(Key {
                                origin: KeyOrigin::Keyboard,
                                channel: 0,
                                key: key.name().bytes().next().unwrap_or(0),
                            }, &mut self.seq);
                        }
                    }
                }
            },
            _ => (),
        }
    }

    fn midi_connect(&mut self, ctx: egui::Context) -> Result<MidiInputConnection<Sender<Vec<u8>>>> {
        match self.midi.selected_port() {
            Some(port) => {
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
                                // ignore the error here, it probably just means that the user
                                // changed ports
                                let _ = tx.send(message.to_vec());
                                ctx.request_repaint();
                            },
                            tx,
                        )?)
                    },
                    Err(e) => bail!(e),
                }
            },
            None => bail!("no MIDI port selected")
        }
    }

    fn handle_midi(&mut self) {
        if let Some(rx) = &self.midi.rx {
            loop {
                match rx.try_recv() {
                    Ok(v) => {
                        // FIXME: invalid MIDI could crash this
                        // FIXME: this shouldn't all be inlined here
                        match v[0] & 0xf0 {
                            0b10000000 => {
                                // note off
                                self.synth.note_off(Key{
                                    origin: KeyOrigin::Midi,
                                    channel: v[0] & 0xf,
                                    key: v[1],
                                }, &mut self.seq);
                            },
                            0b10010000 => {
                                // note on, unless velocity is zero
                                if v[2] != 0 {
                                    let note = input::note_from_midi(v[1] as i8, &self.tuning);
                                    self.synth.note_on(Key {
                                        origin: KeyOrigin::Midi,
                                        channel: v[0] & 0xf,
                                        key: v[1],
                                    }, self.tuning.midi_pitch(&note), &mut self.seq);
                                } else {
                                    self.synth.note_off(Key {
                                        origin: KeyOrigin::Midi,
                                        channel: v[0] & 0xf,
                                        key: v[1],
                                    }, &mut self.seq);
                                }
                            },
                            _ => (),
                        }
                    },
                    Err(_) => break,
                }
            }
        }
    }
}

fn shared_slider(ui: &mut Ui, var: &Shared, range: RangeInclusive<f32>, text: &str, log: bool) {
    let mut val = var.value();
    ui.add(egui::Slider::new(&mut val, range).text(text).logarithmic(log));
    if val != var.value() {
        var.set_value(val);
    }
}

fn make_reverb(r: f32, t: f32, d: f32, m: f32) -> Box<dyn AudioUnit> {
    Box::new(reverb2_stereo(r, t, d, m, highshelf_hz(5000.0, 1.0, db_amp(-1.0))))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // process UI input
        ctx.input(|input| {
            for evt in input.events.iter() {
                self.handle_ui_event(evt);
            }
        });

        // process MIDI input
        self.handle_midi();

        // bottom panel
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            if self.midi.input.is_some() {
                egui::ComboBox::from_label("MIDI input port")
                    .selected_text(self.midi.port_name.clone().unwrap_or("(none)".to_string()))
                    .show_ui(ui, |ui| {
                        let input = self.midi.input.as_ref().unwrap();
                        for p in input.ports() {
                            let name = input.port_name(&p).unwrap_or(String::from("(unknown)"));
                            ui.selectable_value(&mut self.midi.port_selection, Some(name.clone()), name);
                        }
                    });
                if self.midi.port_selection.is_some() && self.midi.port_selection != self.midi.port_name {
                    match self.midi_connect(ctx.clone()) {
                        Ok(conn) => {
                            let old_conn = std::mem::replace(&mut self.midi.conn, Some(conn));
                            if let Some(c) = old_conn {
                                c.close();
                            }
                            self.midi.port_name = self.midi.port_selection.clone();
                            self.midi.port_name.as_ref().inspect(|name| {
                                self.messages.push(format!("Connected to {} for MIDI input", name));
                            });
                            self.config.default_midi_input = self.midi.port_name.clone();
                            if let Err(e) = self.config.save() {
                                self.messages.push(format!("Error saving config: {}", e));
                            };
                        },
                        Err(e) => {
                            self.midi.port_selection = None;
                            self.messages.report(&e);
                        },
                    }
                }
            }
        });

        // instrument panel
        egui::SidePanel::left("left_panel").resizable(false).show(ctx, |ui| {
            // global controls
            {
                let mut commit = false;
                let fx = &mut self.global_fx;
                shared_slider(ui, &fx.reverb_amount, 0.0..=1.0, "Reverb level", false);
                let (predelay_time, room_size, time, diffusion, mod_speed) =
                    (fx.predelay_time, fx.reverb_room_size, fx.reverb_time, fx.reverb_diffusion, fx.reverb_mod_speed);
                ui.add(egui::Slider::new(&mut fx.predelay_time, 0.0..=0.1).text("Predelay"));
                ui.add(egui::Slider::new(&mut fx.reverb_room_size, 10.0..=30.0).text("Room size"));
                ui.add(egui::Slider::new(&mut fx.reverb_time, 0.1..=10.0).text("Time").logarithmic(true));
                ui.add(egui::Slider::new(&mut fx.reverb_diffusion, 0.0..=1.0).text("Diffusion"));
                ui.add(egui::Slider::new(&mut fx.reverb_mod_speed, 0.0..=1.0).text("Mod speed"));
                if predelay_time != fx.predelay_time {
                    fx.net.crossfade(fx.predelay_id, Fade::Smooth, 0.1,
                        Box::new(delay(predelay_time) | delay(predelay_time)));
                    commit = true;
                }
                if room_size != fx.reverb_room_size || time != fx.reverb_time ||
                    diffusion != fx.reverb_diffusion || mod_speed != fx.reverb_mod_speed {
                    fx.net.crossfade(fx.reverb_id, Fade::Smooth, 0.1, make_reverb(room_size, time, diffusion, mod_speed));
                    commit = true;
                }
                if commit {
                    fx.net.commit();
                }
                ui.separator();
            }

            // play mode control
            egui::ComboBox::from_label("Play mode")
                .selected_text(self.synth.play_mode.name())
                .show_ui(ui, |ui| {
                    for variant in PlayMode::VARIANTS {
                        ui.selectable_value(&mut self.synth.play_mode, variant, variant.name());
                    }
                });

            // glide time slider
            // FIXME: this doesn't update voices that are already playing
            ui.add(egui::Slider::new(&mut self.synth.glide_time, 0.0..=0.5).text("Glide"));

            // oscillator controls
            ui.separator();
            ui.horizontal(|ui| {
                for i in 0..self.synth.oscs.len() {
                    ui.selectable_value(&mut self.selected_osc, i, format!("Osc {}", i + 1));
                }
            });
            let osc = &mut self.synth.oscs[self.selected_osc];
            shared_slider(ui, &osc.level, 0.0..=1.0, "Level", false);
            ui.add_enabled_ui(osc.waveform == Waveform::Pulse, |ui| {
                shared_slider(ui, &osc.duty, 0.0..=1.0, "Duty", false);
            });
            egui::ComboBox::from_label("Waveform")
                .selected_text(osc.waveform.name())
                .show_ui(ui, |ui| {
                    for variant in Waveform::VARIANTS {
                        ui.selectable_value(&mut osc.waveform, variant, variant.name());
                    }
                });
            ui.add(egui::Slider::new(&mut osc.env.attack, 0.0..=10.0).text("Attack").logarithmic(true));
            ui.add(egui::Slider::new(&mut osc.env.decay, 0.01..=10.0).text("Decay").logarithmic(true));
            ui.add(egui::Slider::new(&mut osc.env.sustain, 0.0..=1.0).text("Sustain"));
            ui.add(egui::Slider::new(&mut osc.env.release, 0.01..=10.0).text("Release").logarithmic(true));
            ui.separator();

            ui.add(egui::Label::new("Filter"));
            egui::ComboBox::from_label("Type")
                .selected_text(self.synth.filter.filter_type.name())
                .show_ui(ui, |ui| {
                    for variant in FilterType::VARIANTS {
                        ui.selectable_value(&mut self.synth.filter.filter_type, variant, variant.name());
                    }
                });
            shared_slider(ui, &self.synth.filter.cutoff, 20.0..=20_000.0, "Cutoff", true);
            shared_slider(ui, &self.synth.filter.resonance, 0.1..=1.0, "Resonance", false);
            egui::ComboBox::from_label("Key tracking")
                .selected_text(self.synth.filter.key_tracking.name())
                .show_ui(ui, |ui| {
                    for variant in KeyTracking::VARIANTS {
                        ui.selectable_value(&mut self.synth.filter.key_tracking, variant, variant.name());
                    }
                });
            shared_slider(ui, &self.synth.filter.env_level, -1.0..=7.0, "Envelope level", false);
            ui.add(egui::Slider::new(&mut self.synth.filter.env.attack, 0.0..=10.0).text("Attack").logarithmic(true));
            ui.add(egui::Slider::new(&mut self.synth.filter.env.decay, 0.01..=10.0).text("Decay").logarithmic(true));
            ui.add(egui::Slider::new(&mut self.synth.filter.env.sustain, 0.0..=1.0).text("Sustain"));
            ui.add(egui::Slider::new(&mut self.synth.filter.env.release, 0.01..=10.0).text("Release").logarithmic(true));

            // message area
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in self.messages.iter() {
                    ui.label(line);
                }
            });
        });

        // song panel
        egui::CentralPanel::default().show(ctx, |ui| {
            let line_height = 12.0;
            let font = FontId::monospace(line_height);
            let fg_color = Color32::BLACK;
            let cursor_color = Color32::GRAY;
            let char_width = 8.0;
            let channel_width = char_width * 10.0;
            let column_offsets = [0.0, char_width * 3.0, char_width * 5.0, char_width * 7.0];

            let (response, painter) = ui.allocate_painter(ui.available_size_before_wrap(), Sense::click());

            let cursor = &self.song_editor.cursor;
            let cursor_x = response.rect.min.x + 
                channel_width * cursor.channel as f32 +
                column_offsets[cursor.column as usize] +
                char_width * cursor.char as f32;
            let cursor_y = response.rect.min.y + line_height * 2.0 + cursor.tick as f32; // TODO
            let cursor_rect = Rect {
                min: Pos2 { x: cursor_x , y: cursor_y },
                max: Pos2 { x: cursor_x + char_width, y: cursor_y + line_height, },
            };
            painter.rect_filled(cursor_rect, 0.0, cursor_color);

            for (i, channel) in self.song_editor.song.channels.iter().enumerate() {
                let pos = Pos2 {
                    x: channel_width * i as f32,
                    y: 0.0
                };
                painter.text(response.rect.min + pos.to_vec2(),
                    Align2::LEFT_TOP, format!("Channel {}", i + 1), font.clone(), fg_color);
            }

            if let Some(pointer_pos) = response.interact_pointer_pos() {
                // TODO: update cursor position
            }
        });
    }
}

struct GlobalFX {
    predelay_id: NodeId,
    predelay_time: f32,
    reverb_id: NodeId,
    reverb_room_size: f32,
    reverb_time: f32,
    reverb_diffusion: f32,
    reverb_mod_speed: f32,
    reverb_amount: Shared,
    net: Net,
}

fn main() -> eframe::Result {
    // init audio
    let host = cpal::default_host();
    let device = host.default_output_device()
        .expect("no output device available");
    let mut configs = device.supported_output_configs()
        .expect("error querying output configs");
    let config: StreamConfig = configs.next()
        .expect("no supported output config")
        .with_max_sample_rate()
        .into();
    let synth = Synth::new();
    let mut seq = Sequencer::new(false, 1);
    seq.set_sample_rate(config.sample_rate.0 as f64);

    let predelay_time = 0.01;
    let reverb_room_size = 20.0;
    let reverb_time = 1.0;
    let reverb_diffusion = 0.5;
    let reverb_mod_speed = 0.5;
    let (reverb, reverb_id) = Net::wrap_id(
        make_reverb(reverb_room_size, reverb_time, reverb_diffusion, reverb_mod_speed));
    let (predelay, predelay_id) = Net::wrap_id(Box::new(delay(predelay_time) | delay(predelay_time)));
    let reverb_amount = shared(0.5);
    let mut net = Net::wrap(Box::new(seq.backend())) >>
        highpass_hz(5.0, 0.1) >> shape(Tanh(1.0)) >> split::<U2>() >>
        (multipass::<U2>() & (var(&reverb_amount) >> split::<U2>()) * (predelay >> reverb));
    let mut backend = BlockRateAdapter::new(Box::new(net.backend()));
    let global_fx = GlobalFX {
        predelay_time,
        predelay_id,
        reverb_id,
        reverb_room_size,
        reverb_time,
        reverb_diffusion,
        reverb_mod_speed,
        reverb_amount,
        net,
    };
    
    let stream = device.build_output_stream(
        &config,
        move |data: &mut[f32], _: &cpal::OutputCallbackInfo| {
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
    ).unwrap();
    stream.play().unwrap();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_cc| {
            Ok(Box::new(App::new(synth, seq, global_fx)))
        }),
    )
}