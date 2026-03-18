/// Generate a join sound: 440Hz sine wave, ~0.3 seconds, 44100Hz mono, linear fade-out.
/// Returns i16 PCM samples.
pub fn generate_join_sound() -> Vec<i16> {
    const SAMPLE_RATE: f32 = 44100.0;
    const DURATION_SECS: f32 = 0.3;
    const FREQUENCY: f32 = 440.0;
    const AMPLITUDE: f32 = 16000.0; // ~50% of i16::MAX

    let num_samples = (SAMPLE_RATE * DURATION_SECS) as usize;
    let mut samples = Vec::with_capacity(num_samples);

    for i in 0..num_samples {
        let t = i as f32 / SAMPLE_RATE;
        let fade = 1.0 - (i as f32 / num_samples as f32); // linear fade out
        let sample = (2.0 * std::f32::consts::PI * FREQUENCY * t).sin() * AMPLITUDE * fade;
        samples.push(sample as i16);
    }

    samples
}
