// disable console in windows release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::{channel, Sender, Receiver};
use std::error::Error;
use std::collections::VecDeque;

use midir::{InitError, MidiInput, MidiInputConnection, MidiInputPort};
use cpal::{traits::{DeviceTrait, HostTrait, StreamTrait}, StreamConfig};
use fundsp::hacker::*;
use eframe::egui;
use anyhow::{bail, Result};

pub mod pitch;
pub mod input;

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
    f: Shared,
    gate: Shared,
    octave: i8,
    midi: Midi,
}

impl App {
    fn new(f: Shared, gate: Shared) -> Self {
        let mut app = App {
            tuning: pitch::Tuning::divide(2.0, 12, 1).unwrap(),
            messages: MessageBuffer::new(100),
            f,
            gate,
            octave: 4,
            midi: Midi::new(),
        };
        app
    }

    fn handle_ui_event(&mut self, evt: &egui::Event) {
        match evt {
            egui::Event::Key { physical_key, pressed, .. } => {
                if let Some(key) = physical_key {
                    if let Some(note) = input::note_from_key(key, &self.tuning, self.octave) {
                        if *pressed {
                            self.messages.report(&note);
                            self.f.set(midi_hz(self.tuning.midi_pitch(&note)));
                            self.gate.set(1.0);
                        } else {
                            self.gate.set(0.0);
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
                    Ok(input) => {
                        let (tx, rx) = channel();
                        self.midi.rx = Some(rx);
                        Ok(input.connect(
                            &port,
                            APP_NAME,
                            move |_, message, tx| {
                                tx.send(message.to_vec());
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

    fn handle_midi(&self, message: &[u8]) {
        match message[0] & 0xf0 {
            0b10000000 => {
                // note off
                self.gate.set(0.0);
            },
            0b10010000 => {
                // note on
                let note = input::note_from_midi(message[1] as i8, &self.tuning);
                self.f.set(midi_hz(self.tuning.midi_pitch(&note)));
                self.gate.set(1.0);
            },
            _ => (),
        }
    }
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
        if let Some(rx) = &self.midi.rx {
            loop {
                match rx.try_recv() {
                    Ok(v) => {
                        self.handle_midi(&v);
                        self.messages.push(format!("Received MIDI message: {:?}", &v));
                    },
                    Err(_) => break,
                }
            }
        }

        // bottom panel
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            if self.midi.input.is_some() {
                egui::ComboBox::from_label("MIDI input port")
                    .selected_text(self.midi.port_name.clone().unwrap_or("".to_string()))
                    .show_ui(ui, |ui| {
                        let input = self.midi.input.as_ref().unwrap();
                        for p in input.ports() {
                            let name = input.port_name(&p).unwrap_or(String::from(""));
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
                        },
                        Err(e) => self.messages.report(&e),
                    }
                }
            }
        });

        // message panel
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in self.messages.iter() {
                    ui.label(line);
                }
            });
        });
    }
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
    let f = shared(440.0);
    let env_input = shared(0.0);
    let mut osc = (var(&f) >> follow(0.01) >> saw() * 0.2) >>
        moog_hz(1_000.0, 0.0) * (var(&env_input) >> adsr_live(0.1, 0.5, 0.5, 0.5));
    osc.set_sample_rate(config.sample_rate.0 as f64);
    let stream = device.build_output_stream(
        &config,
        move |data: &mut[f32], _: &cpal::OutputCallbackInfo| {
            // there's probably a better way to do this
            let mut i = 0;
            let len = data.len();
            while i < len {
                let (l, r) = osc.get_stereo();
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
            // This gives us image support:
            Ok(Box::new(App::new(f, env_input)))
        }),
    )
}