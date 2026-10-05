//! Speech recognition in the process: SenseVoice through sherpa-onnx, on CPU.
//!
//! The recognizer is loaded once at startup (a missing model is a startup error, not a failure
//! at the first voice message), and recognition runs on blocking threads, at most `workers` at a
//! time, so a burst of clips queues instead of starving the async runtime.

mod wav;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use qbot_media::{TranscribeError, Transcriber};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};
use tokio::sync::Semaphore;

pub use wav::{Pcm, WavError, parse_wav};

/// The files a SenseVoice bundle must contain.
pub const MODEL_FILE: &str = "model.int8.onnx";
pub const TOKENS_FILE: &str = "tokens.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsrConfig {
    /// The directory holding [`MODEL_FILE`] and [`TOKENS_FILE`].
    pub model_dir: PathBuf,
    /// `auto`, `zh`, `en`, `ja`, `ko` or `yue`.
    pub language: String,
    /// Threads the runtime uses inside one recognition.
    pub threads: i32,
    /// Recognitions that may run at once.
    pub workers: usize,
}

impl AsrConfig {
    /// SenseVoice from `<models_dir>/asr/sense-voice` (where `deploy/fetch_asr_model.sh` puts
    /// it), detecting the language, two threads, one recognition at a time.
    pub fn new(models_dir: &std::path::Path) -> Self {
        Self {
            model_dir: models_dir.join("asr").join("sense-voice"),
            language: "auto".into(),
            threads: 2,
            workers: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AsrError {
    #[error(
        "speech model file {path} is missing; download the SenseVoice bundle (model.int8.onnx and tokens.txt) into {dir} \
         (see deploy/fetch_asr_model.sh), or set asr.enabled = false"
    )]
    MissingModel { path: PathBuf, dir: PathBuf },
    #[error("the speech recognizer could not be created from {0}; the model files may be damaged")]
    Create(PathBuf),
}

pub struct SherpaTranscriber {
    recognizer: Arc<OfflineRecognizer>,
    permits: Arc<Semaphore>,
}

impl std::fmt::Debug for SherpaTranscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SherpaTranscriber").finish_non_exhaustive()
    }
}

fn require(dir: &Path, name: &str) -> Result<String, AsrError> {
    let path = dir.join(name);
    if path.is_file() {
        Ok(path.to_string_lossy().into_owned())
    } else {
        Err(AsrError::MissingModel {
            path,
            dir: dir.to_owned(),
        })
    }
}

impl SherpaTranscriber {
    /// Load the model. Blocking: call it at startup (or from `spawn_blocking`).
    pub fn load(cfg: &AsrConfig) -> Result<Self, AsrError> {
        let model = require(&cfg.model_dir, MODEL_FILE)?;
        let tokens = require(&cfg.model_dir, TOKENS_FILE)?;
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(model),
            language: Some(cfg.language.clone()),
            use_itn: true,
        };
        config.model_config.tokens = Some(tokens);
        config.model_config.num_threads = cfg.threads.max(1);
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| AsrError::Create(cfg.model_dir.clone()))?;
        Ok(Self {
            recognizer: Arc::new(recognizer),
            permits: Arc::new(Semaphore::new(cfg.workers.max(1))),
        })
    }
}

#[async_trait]
impl Transcriber for SherpaTranscriber {
    async fn transcribe(&self, wav: &[u8]) -> Result<String, TranscribeError> {
        let pcm = parse_wav(wav).map_err(|e| TranscribeError(e.to_string()))?;
        if pcm.samples.is_empty() {
            return Ok(String::new());
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| TranscribeError("the recognizer is shutting down".into()))?;
        let recognizer = Arc::clone(&self.recognizer);
        tokio::task::spawn_blocking(move || {
            let stream = recognizer.create_stream();
            stream.accept_waveform(pcm.sample_rate, &pcm.samples);
            recognizer.decode(&stream);
            stream
                .get_result()
                .map(|r| r.text.trim().to_owned())
                .unwrap_or_default()
        })
        .await
        .map_err(|e| TranscribeError(format!("recognition task failed: {e}")))
    }
}
