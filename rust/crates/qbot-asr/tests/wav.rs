#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_asr::{WavError, parse_wav};

fn wav(
    format: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    data: &[u8],
    extra_chunk: bool,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&format.to_le_bytes());
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&rate.to_le_bytes());
    body.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
    body.extend_from_slice(&(channels * bits / 8).to_le_bytes());
    body.extend_from_slice(&bits.to_le_bytes());
    if extra_chunk {
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&4u32.to_le_bytes());
        // An even-sized chunk, as encoders write. (hound does not skip the pad byte of an
        // odd-sized one, so such a file is reported as unreadable and its clip stays a bare marker.)
        body.extend_from_slice(&[1, 2, 3, 4]);
    }
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn pcm(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn mono_pcm16_becomes_floats_at_its_own_rate() {
    let parsed = parse_wav(&wav(1, 1, 16_000, 16, &pcm(&[0, 16384, -32768]), false)).unwrap();
    assert_eq!(parsed.sample_rate, 16_000);
    assert_eq!(parsed.samples, [0.0, 0.5, -1.0]);
}

#[test]
fn stereo_is_mixed_down_and_other_chunks_are_skipped() {
    let parsed = parse_wav(&wav(
        1,
        2,
        44_100,
        16,
        &pcm(&[16384, -16384, 8192, 8192]),
        true,
    ))
    .unwrap();
    assert_eq!(parsed.sample_rate, 44_100);
    assert_eq!(parsed.samples, [0.0, 0.25]);
}

#[test]
fn a_declared_size_past_the_end_takes_what_is_there() {
    let mut bytes = wav(1, 1, 16_000, 16, &pcm(&[1, 2, 3, 4]), false);
    bytes.truncate(bytes.len() - 3); // cut mid-sample
    let parsed = parse_wav(&bytes).unwrap();
    assert_eq!(parsed.samples.len(), 2, "the half sample is dropped");
}

#[test]
fn unsupported_and_broken_input_is_an_error_not_silence() {
    assert!(matches!(
        parse_wav(b"not a wav at all"),
        Err(WavError::Invalid(_))
    ));
    assert!(matches!(
        parse_wav(&wav(3, 1, 16_000, 32, &[0; 8], false)),
        Err(WavError::Unsupported {
            bits: 32,
            format: "float"
        })
    ));
    assert!(matches!(
        parse_wav(&wav(1, 1, 16_000, 8, &[0; 8], false)),
        Err(WavError::Unsupported { bits: 8, .. })
    ));
    assert!(matches!(
        parse_wav(b"RIFF\x04\0\0\0WAVE"),
        Err(WavError::Invalid(_))
    ));
}
