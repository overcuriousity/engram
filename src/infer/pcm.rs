//! A recording, as a speech model wants it: sixteen thousand mono samples a
//! second, in ±1.
//!
//! The phone's own microphone door already records exactly that, as 16-bit
//! PCM in a WAV, so for it this is a read and a scale. Everything else here is
//! for whatever else reaches the route: another rate, two channels, float
//! samples. A compressed container is refused in words — there is no codec in
//! this build, by design.

use crate::error::{Error, Result};

/// The rate Whisper hears at.
pub const RATE: u32 = 16_000;

fn refused(why: impl std::fmt::Display) -> Error {
    Error::Validation(format!(
        "this device transcribes PCM WAV recordings (audio/wav): {why}"
    ))
}

pub fn samples_16k(audio: &[u8], mime: &str) -> Result<Vec<f32>> {
    // The type as declared, without parameters: `audio/wav; codecs=1` is a WAV.
    let kind = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        kind.as_str(),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave"
    ) {
        return Err(refused(format!("this one says it is {kind}")));
    }
    let mut reader = hound::WavReader::new(std::io::Cursor::new(audio))
        .map_err(|e| refused(format!("it could not be read as one ({e})")))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(refused("its header names no channels or no rate"));
    }

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| refused(format!("its samples could not be read ({e})")))?,
        hound::SampleFormat::Int => {
            // Full scale for the declared width, so 8, 16, 24 and 32 bits all land in ±1.
            let scale = (1i64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| refused(format!("its samples could not be read ({e})")))?
        }
    };

    let channels = spec.channels as usize;
    let mono: Vec<f32> = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    if spec.sample_rate == RATE || mono.is_empty() {
        return Ok(mono);
    }
    resample(mono, spec.sample_rate)
}

fn resample(mono: Vec<f32>, from: u32) -> Result<Vec<f32>> {
    use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
    use rubato::{Fft, FixedSync, Resampler};

    let failed = |e: &dyn std::fmt::Display| Error::Inference {
        role: "transcribe",
        detail: format!("could not bring {from} Hz to {RATE} Hz: {e}"),
    };
    let mut resampler = Fft::<f32>::new(from as usize, RATE as usize, 1024, 1, FixedSync::Input)
        .map_err(|e| failed(&e))?;
    let frames = mono.len();
    let input = vec![mono];
    let adapter = SequentialSliceOfVecs::new(&input, 1, frames).map_err(|e| failed(&e))?;
    let out = resampler
        .process_all(&adapter, frames, None)
        .map_err(|e| failed(&e))?;
    Ok(out.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, format: hound::SampleFormat, frames: &[Vec<f32>]) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: if format == hound::SampleFormat::Float {
                32
            } else {
                16
            },
            sample_format: format,
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut bytes, spec).unwrap();
            for frame in frames {
                for s in frame {
                    match format {
                        hound::SampleFormat::Float => w.write_sample(*s).unwrap(),
                        hound::SampleFormat::Int => w.write_sample((*s * 32767.0) as i16).unwrap(),
                    }
                }
            }
            w.finalize().unwrap();
        }
        bytes.into_inner()
    }

    fn tone(rate: u32, hz: f32, secs: f32) -> Vec<Vec<f32>> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| vec![0.5 * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin()])
            .collect()
    }

    /// Upward zero crossings a second: a tone's frequency, without an FFT.
    fn hz(samples: &[f32], rate: u32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        crossings as f32 * rate as f32 / samples.len() as f32
    }

    #[test]
    fn what_the_phone_records_comes_back_sample_for_sample() {
        let frames = vec![vec![0.0], vec![0.5], vec![-0.5], vec![0.25]];
        let got = samples_16k(
            &wav(16_000, 1, hound::SampleFormat::Int, &frames),
            "audio/wav",
        )
        .unwrap();
        assert_eq!(got.len(), 4);
        for (g, f) in got.iter().zip(&frames) {
            assert!((g - f[0]).abs() < 1e-3, "{g} vs {}", f[0]);
        }
    }

    #[test]
    fn two_channels_are_averaged_to_one() {
        let frames = vec![vec![0.5, -0.5], vec![0.5, 0.25]];
        let got = samples_16k(
            &wav(16_000, 2, hound::SampleFormat::Int, &frames),
            "audio/x-wav",
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        assert!(
            got[0].abs() < 1e-3 && (got[1] - 0.375).abs() < 1e-3,
            "{got:?}"
        );
    }

    #[test]
    fn a_faster_recording_is_slowed_and_its_pitch_kept() {
        let got = samples_16k(
            &wav(
                48_000,
                1,
                hound::SampleFormat::Int,
                &tone(48_000, 440.0, 1.0),
            ),
            "audio/wav",
        )
        .unwrap();
        assert!((got.len() as f32 - 16_000.0).abs() < 160.0, "{}", got.len());
        let f = hz(&got, RATE);
        assert!((f - 440.0).abs() < 9.0, "{f} Hz");
        assert!(got.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn a_slower_recording_is_stretched_and_its_pitch_kept() {
        let got = samples_16k(
            &wav(8_000, 1, hound::SampleFormat::Int, &tone(8_000, 440.0, 1.0)),
            "audio/wav",
        )
        .unwrap();
        assert!((got.len() as f32 - 16_000.0).abs() < 160.0, "{}", got.len());
        assert!((hz(&got, RATE) - 440.0).abs() < 9.0);
    }

    #[test]
    fn float_samples_are_read_as_they_are() {
        let got = samples_16k(
            &wav(
                16_000,
                1,
                hound::SampleFormat::Float,
                &[vec![0.125], vec![-1.0]],
            ),
            "audio/wav; codecs=3",
        )
        .unwrap();
        assert_eq!(got, vec![0.125, -1.0]);
    }

    #[test]
    fn a_compressed_container_is_refused_in_words() {
        let e = samples_16k(b"\x1aE\xdf\xa3", "audio/webm;codecs=opus").unwrap_err();
        assert!(
            matches!(&e, Error::Validation(m) if m.contains("audio/webm") && m.contains("PCM WAV")),
            "{e}"
        );
    }

    #[test]
    fn something_that_only_says_it_is_a_wav_is_refused_too() {
        assert!(matches!(
            samples_16k(b"not a riff", "audio/wav"),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn an_empty_recording_is_no_samples() {
        assert!(
            samples_16k(&wav(44_100, 1, hound::SampleFormat::Int, &[]), "audio/wav")
                .unwrap()
                .is_empty()
        );
    }
}
