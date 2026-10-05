#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Runs the real recognizer. Needs the SenseVoice bundle, so it only runs when
//! `QBOT_ASR_MODEL_DIR` points at a directory holding `model.int8.onnx`, `tokens.txt` and
//! `test_wavs/{en,zh}.wav` (the layout of the published bundle); otherwise it says so and passes.

use std::path::PathBuf;
use std::sync::Arc;

use qbot_asr::{AsrConfig, AsrError, SherpaTranscriber};
use qbot_media::Transcriber;

fn model_dir() -> Option<PathBuf> {
    match std::env::var("QBOT_ASR_MODEL_DIR") {
        Ok(dir) => Some(PathBuf::from(dir)),
        Err(_) => {
            eprintln!("SKIPPED: QBOT_ASR_MODEL_DIR is not set");
            None
        }
    }
}

fn config(dir: PathBuf, workers: usize) -> AsrConfig {
    AsrConfig {
        model_dir: dir,
        language: "auto".into(),
        threads: 2,
        workers,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_speech_is_recognised_in_english_and_chinese() {
    let Some(dir) = model_dir() else { return };
    let asr = SherpaTranscriber::load(&config(dir.clone(), 1)).unwrap();
    let en = asr
        .transcribe(&std::fs::read(dir.join("test_wavs/en.wav")).unwrap())
        .await
        .unwrap()
        .to_lowercase();
    assert!(en.contains("the tribal chieftain"), "{en}");
    let zh = asr
        .transcribe(&std::fs::read(dir.join("test_wavs/zh.wav")).unwrap())
        .await
        .unwrap();
    assert!(zh.contains('\u{5F00}'), "expected Chinese text, got {zh:?}"); // "open"
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clips_queue_behind_the_worker_limit() {
    let Some(dir) = model_dir() else { return };
    let asr = Arc::new(SherpaTranscriber::load(&config(dir.clone(), 1)).unwrap());
    let wav = Arc::new(std::fs::read(dir.join("test_wavs/en.wav")).unwrap());
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let (asr, wav) = (asr.clone(), wav.clone());
            tokio::spawn(async move { asr.transcribe(&wav).await.unwrap() })
        })
        .collect();
    for task in tasks {
        assert!(task.await.unwrap().to_lowercase().contains("tribal"));
    }
}

#[tokio::test]
async fn silence_and_garbage_are_handled_without_panicking() {
    let Some(dir) = model_dir() else { return };
    let asr = SherpaTranscriber::load(&config(dir, 1)).unwrap();
    assert!(asr.transcribe(b"definitely not audio").await.is_err());
    let silence: Vec<u8> = {
        let data = vec![0u8; 16_000 * 2];
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&32_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    };
    let text = asr.transcribe(&silence).await.unwrap();
    assert!(
        text.trim().is_empty() || text.chars().count() < 10,
        "silence should not produce a sentence: {text:?}"
    );
}

#[test]
fn a_missing_model_is_a_typed_startup_error_naming_the_file() {
    let dir = std::env::temp_dir().join("qbot-asr-no-model");
    let error = SherpaTranscriber::load(&config(dir.clone(), 1)).unwrap_err();
    assert!(
        matches!(&error, AsrError::MissingModel { path, .. } if path.ends_with("model.int8.onnx")),
        "{error}"
    );
    assert!(error.to_string().contains("fetch_asr_model.sh"));
}
