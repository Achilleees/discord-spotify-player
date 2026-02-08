use crate::audio_bridge::AudioBridge;
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::decoder::AudioPacket;
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct DspConfig {
    pub preamp_db: f32,
    pub bass_boost_db: f32,
    pub treble_boost_db: f32,
}

impl DspConfig {
    pub fn new(preamp_db: f32, bass_boost_db: f32, treble_boost_db: f32) -> Self {
        Self {
            preamp_db,
            bass_boost_db,
            treble_boost_db,
        }
    }

    fn enabled(&self) -> bool {
        self.preamp_db.abs() > f32::EPSILON
            || self.bass_boost_db.abs() > f32::EPSILON
            || self.treble_boost_db.abs() > f32::EPSILON
    }
}

#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn new() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn set_low_shelf(&mut self, sample_rate: f32, freq: f32, gain_db: f32) {
        if gain_db.abs() <= f32::EPSILON {
            *self = Self::new();
            return;
        }

        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let sqrt_a = a.sqrt();
        let alpha = sin_w0 / 2.0 * ((a + 1.0 / a) * 2.0).sqrt();

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;

        self.set_coefficients(b0, b1, b2, a0, a1, a2);
    }

    fn set_high_shelf(&mut self, sample_rate: f32, freq: f32, gain_db: f32) {
        if gain_db.abs() <= f32::EPSILON {
            *self = Self::new();
            return;
        }

        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let sqrt_a = a.sqrt();
        let alpha = sin_w0 / 2.0 * ((a + 1.0 / a) * 2.0).sqrt();

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;

        self.set_coefficients(b0, b1, b2, a0, a1, a2);
    }

    fn set_coefficients(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

/// Custom audio sink that sends audio to Discord via the AudioBridge
pub struct DiscordSink {
    bridge: Arc<AudioBridge>,
    scratch: Vec<f32>,
    dsp_enabled: bool,
    preamp_gain: f32,
    low_l: Biquad,
    low_r: Biquad,
    high_l: Biquad,
    high_r: Biquad,
    start_instant: Option<std::time::Instant>,
    frames_sent: u64,
}

impl DiscordSink {
    pub fn new(bridge: Arc<AudioBridge>, dsp: DspConfig) -> Self {
        let mut low_l = Biquad::new();
        let mut low_r = Biquad::new();
        let mut high_l = Biquad::new();
        let mut high_r = Biquad::new();
        let enabled = dsp.enabled();
        if enabled {
            let sample_rate = SAMPLE_RATE as f32;
            low_l.set_low_shelf(sample_rate, 80.0, dsp.bass_boost_db);
            low_r.set_low_shelf(sample_rate, 80.0, dsp.bass_boost_db);
            high_l.set_high_shelf(sample_rate, 8000.0, dsp.treble_boost_db);
            high_r.set_high_shelf(sample_rate, 8000.0, dsp.treble_boost_db);
        }

        Self {
            bridge,
            scratch: Vec::new(),
            dsp_enabled: enabled,
            preamp_gain: 10.0_f32.powf(dsp.preamp_db / 20.0),
            low_l,
            low_r,
            high_l,
            high_r,
            start_instant: None,
            frames_sent: 0,
        }
    }
}

impl Sink for DiscordSink {
    fn start(&mut self) -> SinkResult<()> {
        tracing::debug!("spotify sink started");
        self.bridge.clear();
        self.start_instant = None;
        self.frames_sent = 0;
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        tracing::debug!("spotify sink stopped");
        self.bridge.clear();
        self.start_instant = None;
        self.frames_sent = 0;
        Ok(())
    }

    fn write(
        &mut self,
        packet: AudioPacket,
        _converter: &mut librespot_playback::convert::Converter,
    ) -> SinkResult<()> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        match packet {
            AudioPacket::Samples(samples) => {
                let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 5 || count.is_multiple_of(200) {
                    tracing::debug!(
                        target: "audio_stream",
                        samples = samples.len(),
                        "sink write"
                    );
                }

                let n = samples.len();
                if self.scratch.len() < n {
                    self.scratch.resize(n, 0.0);
                }

                if self.dsp_enabled {
                    let gain = self.preamp_gain;
                    // Process stereo frames (L/R pairs) to avoid per-sample branch.
                    let frame_count = n / 2;
                    for i in 0..frame_count {
                        let li = i * 2;
                        let mut l = samples[li] as f32 * gain;
                        let mut r = samples[li + 1] as f32 * gain;
                        l = self.low_l.process(l);
                        l = self.high_l.process(l);
                        r = self.low_r.process(r);
                        r = self.high_r.process(r);
                        self.scratch[li] = l.clamp(-1.0, 1.0);
                        self.scratch[li + 1] = r.clamp(-1.0, 1.0);
                    }
                    // Handle trailing sample if odd count (shouldn't happen with stereo).
                    if n % 2 != 0 {
                        self.scratch[n - 1] = (samples[n - 1] as f32 * gain).clamp(-1.0, 1.0);
                    }
                } else {
                    for (dst, src) in self.scratch[..n].iter_mut().zip(samples.iter()) {
                        *dst = (*src as f32).clamp(-1.0, 1.0);
                    }
                }

                self.bridge.push_samples(&self.scratch[..n]);

                // Pace Spotify decode to real-time to avoid rapid skipping.
                let frames_out = (n / NUM_CHANNELS as usize) as u64;
                let start = *self.start_instant.get_or_insert_with(|| {
                    self.frames_sent = 0;
                    std::time::Instant::now()
                });
                self.frames_sent = self.frames_sent.saturating_add(frames_out);
                let target = start
                    + std::time::Duration::from_secs_f64(
                        self.frames_sent as f64 / SAMPLE_RATE as f64,
                    );
                let now = std::time::Instant::now();
                if target > now {
                    let remaining = target - now;
                    if remaining > std::time::Duration::from_millis(2) {
                        std::thread::sleep(remaining - std::time::Duration::from_millis(1));
                        while std::time::Instant::now() < target {
                            std::hint::spin_loop();
                        }
                    } else {
                        std::thread::yield_now();
                    }
                }
                Ok(())
            }
            AudioPacket::Raw(_) => {
                // Raw audio data - not used in our decode pipeline
                Ok(())
            }
        }
    }
}
