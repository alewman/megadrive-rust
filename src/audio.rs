//! The console's analogue audio path: the FM and the PSG mixed and filtered.
//!
//! Transcribed from megadrive-python's `audio.py`. The two chips meet in an amplifier
//! whose shape differs by revision ("instead of the MD1's 1st order filter there's a
//! 2nd order filter", Kabuto, "The YM2612"; research). No source gives a corner
//! frequency, a mixing ratio or a distortion model, so this carries a mixing ratio
//! measured from the MDFourier recordings (hardware-captured), a one-pole low-pass for
//! `model1` and two cascaded poles for `model2` whose corner is fitted (an assumption),
//! a DC blocker for the coupling capacitor, and **no distortion model at all**.
//!
//! The YM2612 finishes a sample every 1,008 MCLK and the PSG every 240, a ratio of
//! exactly 21/5: the PSG is box-filtered down onto the FM's grid and the pair is then
//! resampled to the host's rate. The arithmetic is the reference's, operation for
//! operation, so the floats are the same.

use std::f64::consts::PI;

use crate::machine::Machine;

/// Master clocks per finished sample of each chip.
pub const MCLK_PER_FM_SAMPLE: i64 = 1008;
pub const MCLK_PER_PSG_SAMPLE: i64 = 240;

/// The PSG's level (0.0-4.0) per unit of the FM's summed DAC: **measured** on a real
/// Model 1 VA3 (`A-MD1UTVA3-M192.flac`; docs/worklog.md, milestone 4).
pub const PSG_GAIN: f64 = 422.0;

/// The analogue low-pass corner, in Hz, and its poles, by `Machine::audio_path`:
/// fitted, not measured.
pub fn filter(audio_path: &str) -> (f64, usize) {
    match audio_path {
        "model1" => (8000.0, 1),
        "model2" => (4500.0, 2),
        _ => (8000.0, 1),
    }
}

/// The coupling capacitor at the jack, an assumption like the low-pass corner.
pub const DC_BLOCK_HZ: f64 = 10.0;

/// Full scale: six FM channels at full volume sum to about 6,100 units.
pub const OUTPUT_SCALE: f64 = 4.0;

/// Mixes one machine's FM and PSG streams into stereo at `sample_rate`.
pub struct AudioPath {
    pub machine: Machine,
    pub sample_rate: u32,
    pub psg_gain: f64,
    pub cutoff: f64,
    pub poles: usize,
    psg_phase: i64,
    pending_psg: Vec<f64>,
    filter: Vec<(f64, f64)>,
    dc_in: (f64, f64),
    dc_out: (f64, f64),
    resample_phase: f64,
    previous: (f64, f64),
    have_previous: bool,
}

impl AudioPath {
    pub fn new(machine: Machine, sample_rate: u32) -> Self {
        let (cutoff, poles) = filter(machine.audio_path);
        AudioPath {
            machine,
            sample_rate,
            psg_gain: PSG_GAIN,
            cutoff,
            poles,
            psg_phase: 0,
            pending_psg: Vec::new(),
            filter: vec![(0.0, 0.0); poles],
            dc_in: (0.0, 0.0),
            dc_out: (0.0, 0.0),
            resample_phase: 0.0,
            previous: (0.0, 0.0),
            have_previous: false,
        }
    }

    pub fn reset(&mut self) {
        *self = AudioPath {
            psg_gain: self.psg_gain,
            cutoff: self.cutoff,
            poles: self.poles,
            ..AudioPath::new(self.machine, self.sample_rate)
        };
        self.filter = vec![(0.0, 0.0); self.poles];
    }

    pub fn fm_rate(&self) -> f64 {
        self.machine.mclk() as f64 / MCLK_PER_FM_SAMPLE as f64
    }

    /// One pole's coefficient for a corner at `cutoff`, at the FM rate.
    fn alpha(&self) -> f64 {
        let rc = 1.0 / (2.0 * PI * self.cutoff);
        let dt = 1.0 / self.fm_rate();
        dt / (rc + dt)
    }

    /// `fm` at the chip's rate and `psg` at the PSG's, to stereo at `sample_rate`.
    /// Leftovers of either stream are carried to the next call.
    pub fn mix(&mut self, fm: &[(i32, i32)], psg: &[f64]) -> Vec<(i32, i32)> {
        self.pending_psg.extend_from_slice(psg);
        let mut merged = Vec::with_capacity(fm.len());
        let mut psg_index = 0;
        let gain = self.psg_gain;
        for &(left, right) in fm {
            // Five FM samples take 21 PSG samples: an integer phase keeps the long-run
            // rate exact without a float accumulator.
            self.psg_phase += MCLK_PER_FM_SAMPLE;
            let mut level = 0.0;
            let mut taken = 0u32;
            while self.psg_phase >= MCLK_PER_PSG_SAMPLE && psg_index < self.pending_psg.len() {
                self.psg_phase -= MCLK_PER_PSG_SAMPLE;
                level += self.pending_psg[psg_index];
                psg_index += 1;
                taken += 1;
            }
            if taken != 0 {
                level /= f64::from(taken);
            } else {
                self.psg_phase = self.psg_phase.min(MCLK_PER_FM_SAMPLE);
                level = 0.0;
            }
            // The PSG is one signal mixed into both sides.
            merged.push((
                f64::from(left) + level * gain,
                f64::from(right) + level * gain,
            ));
        }
        self.pending_psg.drain(..psg_index);
        let filtered = self.filtered(merged);
        let blocked = self.dc_blocked(filtered);
        self.resample(blocked)
    }

    /// The coupling capacitor: a one-pole high-pass at [`DC_BLOCK_HZ`].
    fn dc_blocked(&mut self, samples: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
        let pole = 1.0 - 2.0 * PI * DC_BLOCK_HZ / self.fm_rate();
        let (mut in_l, mut in_r) = self.dc_in;
        let (mut out_l, mut out_r) = self.dc_out;
        let mut out = Vec::with_capacity(samples.len());
        for (left, right) in samples {
            out_l = left - in_l + pole * out_l;
            out_r = right - in_r + pole * out_r;
            in_l = left;
            in_r = right;
            out.push((out_l, out_r));
        }
        self.dc_in = (in_l, in_r);
        self.dc_out = (out_l, out_r);
        out
    }

    fn filtered(&mut self, mut samples: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
        let alpha = self.alpha();
        for pole in 0..self.poles {
            let (mut state_l, mut state_r) = self.filter[pole];
            for sample in &mut samples {
                state_l += alpha * (sample.0 - state_l);
                state_r += alpha * (sample.1 - state_r);
                *sample = (state_l, state_r);
            }
            self.filter[pole] = (state_l, state_r);
        }
        samples
    }

    /// Linear interpolation from the FM rate to `sample_rate`.
    fn resample(&mut self, samples: Vec<(f64, f64)>) -> Vec<(i32, i32)> {
        let step = self.fm_rate() / f64::from(self.sample_rate);
        let mut out = Vec::new();
        let mut phase = self.resample_phase;
        let mut previous = self.previous;
        let clamp = |x: f64| ((x * OUTPUT_SCALE) as i64).clamp(-32768, 32767) as i32;
        for sample in samples {
            if !self.have_previous {
                previous = sample;
                self.have_previous = true;
            }
            while phase < 1.0 {
                let left = previous.0 + (sample.0 - previous.0) * phase;
                let right = previous.1 + (sample.1 - previous.1) * phase;
                out.push((clamp(left), clamp(right)));
                phase += step;
            }
            phase -= 1.0;
            previous = sample;
        }
        self.resample_phase = phase;
        self.previous = previous;
        out
    }
}
