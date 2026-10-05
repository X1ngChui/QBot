//! WAV decoding for the recogniser: 16-bit PCM, any rate, any channel count, mixed to mono floats.

use std::io::Cursor;

#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub sample_rate: i32,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WavError {
    #[error("not a readable WAV file: {0}")]
    Invalid(String),
    #[error("only 16-bit integer PCM audio is supported (got {bits} bits, {format})")]
    Unsupported { bits: u16, format: &'static str },
}

pub fn parse_wav(bytes: &[u8]) -> Result<Pcm, WavError> {
    let mut reader =
        hound::WavReader::new(Cursor::new(bytes)).map_err(|e| WavError::Invalid(e.to_string()))?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(WavError::Unsupported {
            bits: spec.bits_per_sample,
            format: if spec.sample_format == hound::SampleFormat::Float {
                "float"
            } else {
                "integer"
            },
        });
    }
    let channels = usize::from(spec.channels.max(1));
    // A clip cut short mid-stream yields the samples before the cut rather than nothing.
    let samples: Vec<i16> = reader.samples::<i16>().map_while(Result::ok).collect();
    let mono = samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().map(|s| f32::from(*s)).sum::<f32>() / channels as f32 / 32768.0)
        .collect();
    Ok(Pcm {
        sample_rate: i32::try_from(spec.sample_rate)
            .map_err(|_| WavError::Invalid("sample rate".into()))?,
        samples: mono,
    })
}
