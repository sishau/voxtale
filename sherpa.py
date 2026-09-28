#!/usr/bin/env python3
# -*- coding: utf-8 -*-

import io
import os

import sherpa_onnx
import soundfile as sf


class sherpa:
    def __init__(self, conf):
        self.conf = conf
        self.tts_server = None

    def initialize(self):
        conf = self.conf
        model_dir = conf['model_folder']
        model_config = {
            'model': os.path.join(model_dir, conf['model']),
            'vocoder': os.path.join(model_dir, conf['vocoder']),
            'tokens': os.path.join(model_dir, conf['tokens']),
            'dict_dir': os.path.join(model_dir, conf['dict_dir']) if conf.get('dict_dir') else '',
        }
        lexicon = ",".join(os.path.join(model_dir, item.strip()) for item in conf['lexicon'].split(','))
        rule_fsts = ",".join(os.path.join(model_dir, item.strip()) for item in conf['rule_fsts'].split(','))

        matcha = sherpa_onnx.OfflineTtsMatchaModelConfig(
            acoustic_model=model_config['model'],
            vocoder=model_config['vocoder'],
            lexicon=lexicon,
            tokens=model_config['tokens'],
            data_dir=os.path.join(model_dir, conf['data_dir']) if conf.get('data_dir') else '',
            dict_dir=model_config['dict_dir'],
        )
        tts_config = sherpa_onnx.OfflineTtsConfig(
            model=sherpa_onnx.OfflineTtsModelConfig(
                matcha=matcha,
                provider="cpu",
                debug=False,
                num_threads=1,
            ),
            rule_fsts=rule_fsts,
            max_num_sentences=1,
        )
        if not tts_config.validate():
            raise ValueError("Please check your config")
        self.tts_server = sherpa_onnx.OfflineTts(tts_config)

    def synthesize(self, text: str):
        sid = self.conf.get("sid", 0)
        speed = self.conf.get("speed", 1.0)
        audio = self.tts_server.generate(text, sid=sid, speed=speed)
        if audio is None:
            return None
        # sherpa-onnx returns a result with empty samples and sample_rate=0
        # when the text cannot be converted to token IDs (e.g. an all-OOV line)
        if audio.samples is None or len(audio.samples) == 0 or audio.sample_rate <= 0:
            return None
        buffer = io.BytesIO()
        sf.write(buffer, audio.samples, samplerate=audio.sample_rate, format="WAV")
        buffer.seek(0)
        return buffer.read()
