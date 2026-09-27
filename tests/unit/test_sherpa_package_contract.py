"""Exercise real package configuration without loading or downloading model weights."""

import importlib
from types import SimpleNamespace
from unittest.mock import Mock

import sherpa_onnx

from qqbot.configuration import AsrCfg
from qqbot.providers.sherpa import SherpaAsr


def test_installed_sensevoice_factory_accepts_the_production_configuration(monkeypatch, tmp_path):
    (tmp_path / "model.int8.onnx").touch()
    (tmp_path / "tokens.txt").touch()
    stream = SimpleNamespace(accept_waveform=Mock(), result=SimpleNamespace(text=""))
    native = SimpleNamespace(create_stream=Mock(return_value=stream), decode_stream=Mock())
    constructor = Mock(return_value=native)
    module = importlib.import_module(sherpa_onnx.OfflineRecognizer.__module__)
    monkeypatch.setattr(module, "_Recognizer", constructor)
    recognizer = SherpaAsr._build(AsrCfg(model_dir=str(tmp_path), threads=3))
    (config,) = constructor.call_args.args
    assert recognizer.config is config
    assert config.model_config.num_threads == 3
    assert config.model_config.provider == "cpu"
    assert config.model_config.sense_voice.language == "auto"
    assert config.model_config.sense_voice.use_itn
    assert config.model_config.sense_voice.model == str(tmp_path / "model.int8.onnx")
    assert config.model_config.tokens == str(tmp_path / "tokens.txt")
    assert config.feat_config.sampling_rate == 16000
    native.decode_stream.assert_called_once_with(stream)
    stream.accept_waveform.assert_called_once()
