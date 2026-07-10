pub mod dj;

/// Generate a join sound: 440Hz sine wave, ~0.3 seconds, 44100Hz mono, linear fade-out.
/// Returns i16 PCM samples.
pub fn generate_join_sound() -> Vec<i16> {
    const SAMPLE_RATE: f32 = 44100.0;
    const DURATION_SECS: f32 = 0.3;
    const FREQUENCY: f32 = 440.0;
    const AMPLITUDE: f32 = 16000.0;

    let num_samples = (SAMPLE_RATE * DURATION_SECS) as usize;
    let mut samples = Vec::with_capacity(num_samples);

    for i in 0..num_samples {
        let t = i as f32 / SAMPLE_RATE;
        let fade = 1.0 - (i as f32 / num_samples as f32);
        let sample = (2.0 * std::f32::consts::PI * FREQUENCY * t).sin() * AMPLITUDE * fade;
        samples.push(sample as i16);
    }

    samples
}

#[cfg(test)]
mod tests {
    use super::generate_join_sound;

    #[test]
    fn join_sound_has_expected_length() {
        // 0.3s at 44100 Hz mono.
        assert_eq!(generate_join_sound().len(), (44100.0 * 0.3) as usize);
    }

    #[test]
    fn join_sound_starts_at_zero_and_stays_in_range() {
        let s = generate_join_sound();
        assert_eq!(s[0], 0, "a sine starts at zero");
        assert!(s.iter().all(|&v| v.unsigned_abs() <= 16000), "within amplitude");
    }

    #[test]
    fn join_sound_fades_out() {
        let s = generate_join_sound();
        let peak_tail = s[s.len() - 100..].iter().map(|v| v.unsigned_abs()).max().unwrap();
        let peak_mid = s[s.len() / 2 - 50..s.len() / 2 + 50].iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(peak_tail < peak_mid, "tail {peak_tail} should be quieter than mid {peak_mid}");
    }
}
