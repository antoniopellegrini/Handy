use anyhow::Result;
use hound::{WavReader, WavSpec, WavWriter};
use log::debug;
use std::path::Path;

/// Read a WAV file and return normalised f32 samples.
pub fn read_wav_samples<P: AsRef<Path>>(file_path: P) -> Result<Vec<f32>> {
    let reader = WavReader::open(file_path.as_ref())?;
    let samples = reader
        .into_samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
        .collect::<Result<Vec<f32>, _>>()?;
    Ok(samples)
}

/// Verify a WAV file by reading it back and checking the sample count.
pub fn verify_wav_file<P: AsRef<Path>>(file_path: P, expected_samples: usize) -> Result<()> {
    let reader = WavReader::open(file_path.as_ref())?;
    let actual_samples = reader.len() as usize;
    if actual_samples != expected_samples {
        anyhow::bail!(
            "WAV sample count mismatch: expected {}, got {}",
            expected_samples,
            actual_samples
        );
    }
    Ok(())
}

/// Save audio samples as a WAV file
pub fn save_wav_file<P: AsRef<Path>>(file_path: P, samples: &[f32]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(file_path.as_ref(), spec)?;

    // Convert f32 samples to i16 for WAV
    for sample in samples {
        let sample_i16 = (sample * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    debug!("Saved WAV file: {:?}", file_path.as_ref());
    Ok(())
}

/// Sample rate every transcription engine in Handy expects.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Encode mono 16 kHz f32 samples into an in-memory 16-bit WAV.
///
/// The network transcription paths need a WAV *buffer*, not a file: the client
/// uploads one as a multipart body and the server may hand one back for
/// debugging. Mirrors [`save_wav_file`]'s format exactly so both sides agree.
pub fn encode_wav_bytes(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut buffer = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = WavWriter::new(&mut buffer, spec)?;
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            writer.write_sample((clamped * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(buffer.into_inner())
}

/// Decode an arbitrary in-memory WAV into mono 16 kHz f32 samples.
///
/// Unlike [`read_wav_samples`], which assumes Handy's own 16 kHz mono 16-bit
/// output, this accepts whatever a third-party client uploads: any bit depth,
/// int or float, any channel count, any sample rate. Channels are averaged and
/// the result is resampled to [`TARGET_SAMPLE_RATE`].
pub fn decode_wav_bytes(bytes: &[u8]) -> Result<Vec<f32>> {
    let mut reader = WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();

    if spec.channels == 0 {
        anyhow::bail!("WAV declares zero channels");
    }

    // Normalise every supported encoding to f32 in [-1, 1].
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, _) => {
            reader.samples::<f32>().collect::<Result<Vec<f32>, _>>()?
        }
        (hound::SampleFormat::Int, bits) => {
            // hound yields sign-extended i32 for every integer width, so one
            // divisor derived from the declared depth covers 8/16/24/32-bit.
            let scale = (1i64 << (bits - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<Vec<f32>, _>>()?
        }
    };

    let mono = downmix_to_mono(&interleaved, spec.channels);
    Ok(resample_linear(mono, spec.sample_rate, TARGET_SAMPLE_RATE))
}

/// Average interleaved channels into a single channel.
fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }
    let channels = channels as usize;
    interleaved
        .chunks(channels)
        // A trailing partial frame is averaged over the samples actually
        // present rather than being dropped or padded with silence.
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Linear-interpolation resample.
///
/// The recording path uses [`FrameResampler`] (rubato, sinc) because it runs
/// per-frame on live audio and quality there is worth the cost. This one runs
/// once on a whole finished upload, where linear interpolation is inaudible to
/// an ASR model and avoids constructing a rubato pipeline per request.
fn resample_linear(samples: Vec<f32>, from_hz: u32, to_hz: u32) -> Vec<f32> {
    if from_hz == to_hz || samples.is_empty() {
        return samples;
    }

    let ratio = to_hz as f64 / from_hz as f64;
    let out_len = ((samples.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src = i as f64 / ratio;
        let left = src.floor() as usize;
        let frac = (src - left as f64) as f32;
        let a = samples[left.min(samples.len() - 1)];
        let b = samples[(left + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_bytes_round_trip_preserves_samples() {
        let original: Vec<f32> = (0..1600).map(|i| (i as f32 / 100.0).sin() * 0.5).collect();
        let decoded = decode_wav_bytes(&encode_wav_bytes(&original).unwrap()).unwrap();

        assert_eq!(decoded.len(), original.len());
        for (a, b) in original.iter().zip(decoded.iter()) {
            // 16-bit quantisation is the only loss in the round trip.
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn stereo_44100_is_downmixed_and_resampled() {
        // Two channels of constant, differing amplitude: the average is exact,
        // so resampling is the only thing under test.
        let spec = WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buffer = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut writer = WavWriter::new(&mut buffer, spec).unwrap();
            for _ in 0..44_100 {
                writer.write_sample((0.5 * i16::MAX as f32) as i16).unwrap();
                writer.write_sample((0.1 * i16::MAX as f32) as i16).unwrap();
            }
            writer.finalize().unwrap();
        }

        let decoded = decode_wav_bytes(&buffer.into_inner()).unwrap();

        // One second in, one second out at the target rate.
        assert!(
            decoded.len().abs_diff(TARGET_SAMPLE_RATE as usize) <= 1,
            "got {} samples",
            decoded.len()
        );
        // Interior samples avoid the linear interpolator's edge clamping.
        for &sample in &decoded[100..decoded.len() - 100] {
            assert!((sample - 0.3).abs() < 1e-2, "got {sample}");
        }
    }

    #[test]
    fn rejects_non_wav_bytes() {
        assert!(decode_wav_bytes(b"this is not a wav file at all").is_err());
    }
}
