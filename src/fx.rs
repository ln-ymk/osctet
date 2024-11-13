// global FX

use fundsp::hacker::*;
use realseq::SequencerBackend;

use crate::synth::Parameter;

// serializable global FX settings
#[derive(Clone)]
pub struct FXSettings {
    pub reverb_amount: Parameter,
    pub predelay_time: f32,
    pub reverb_room_size: f32,
    pub reverb_time: f32,
    pub reverb_diffusion: f32,
    pub reverb_mod_speed: f32,
    pub reverb_damping: f32,
}

impl FXSettings {
    pub fn make_predelay(&self) -> Box<dyn AudioUnit> {
        Box::new(delay(self.predelay_time) | delay(self.predelay_time))
    }

    pub fn make_reverb(&self) -> Box<dyn AudioUnit> {
        Box::new(reverb2_stereo(
            self.reverb_room_size,
            self.reverb_time,
            self.reverb_diffusion,
            self.reverb_mod_speed,
            highshelf_hz(5000.0, 1.0, db_amp(-self.reverb_damping))))
    }
}

impl Default for FXSettings {
    fn default() -> Self {
        Self {
            reverb_amount: Parameter(shared(0.1)),
            predelay_time: 0.01,
            reverb_room_size: 20.0,
            reverb_time: 0.2,
            reverb_diffusion: 0.5,
            reverb_mod_speed: 0.5,
            reverb_damping: 3.0,
        } 
    }
}

// controls updates of global FX
pub struct GlobalFX {
    pub settings: FXSettings,
    pub net: Net,
    predelay_id: NodeId,
    reverb_id: NodeId,
}

impl GlobalFX {
    pub fn new(backend: SequencerBackend) -> Self {
        let settings: FXSettings = Default::default();
        let (predelay, predelay_id) = Net::wrap_id(settings.make_predelay());
        let (reverb, reverb_id) = Net::wrap_id(settings.make_reverb());

        Self {
            net: Net::wrap(Box::new(backend))
                >> (highpass_hz(1.0, 0.1) | highpass_hz(1.0, 0.1))
                >> (shape(Tanh(1.0)) | shape(Tanh(1.0)))
                >> (multipass::<U2>() & (var(&settings.reverb_amount.0) >> split::<U2>()) * (predelay >> reverb)),
            settings,
            predelay_id,
            reverb_id,
        }
    }

    pub fn commit_predelay(&mut self) {
        self.crossfade(self.predelay_id, self.settings.make_predelay());
    }

    pub fn commit_reverb(&mut self) {
        self.crossfade(self.reverb_id, self.settings.make_reverb());
    }

    fn crossfade(&mut self, id: NodeId, unit: Box<dyn AudioUnit>) {
        self.net.crossfade(id, Fade::Smooth, 0.1, unit);
        self.net.commit();
    }
}