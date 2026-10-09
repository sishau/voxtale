#!/usr/bin/env python3
# -*- coding: utf-8 -*-

import io
import os
import re

import sherpa_onnx
import soundfile as sf


class sherpa:
    def __init__(self, conf):
        self.conf = conf
        self.tts_server = None

    def initialize(self):
        conf = self.conf
        model_dir = conf['model_folder']
        lexicon = ",".join(os.path.join(model_dir, item.strip()) for item in conf['lexicon'].split(','))
        rule_fsts = ",".join(os.path.join(model_dir, item.strip()) for item in conf['rule_fsts'].split(','))
        num_threads = int(conf.get('num_threads', 1))

        if conf.get('voices'):
            # kokoro architecture: model.onnx + voices.bin + dual lexicon + espeak-ng-data
            kokoro = sherpa_onnx.OfflineTtsKokoroModelConfig(
                model=os.path.join(model_dir, conf['model']),
                voices=os.path.join(model_dir, conf['voices']),
                lexicon=lexicon,
                tokens=os.path.join(model_dir, conf['tokens']),
                data_dir=os.path.join(model_dir, conf['data_dir']) if conf.get('data_dir') else '',
                dict_dir=os.path.join(model_dir, conf['dict_dir']) if conf.get('dict_dir') else '',
            )
            model_config = sherpa_onnx.OfflineTtsModelConfig(
                kokoro=kokoro,
                provider="cpu",
                debug=False,
                num_threads=num_threads,
            )
        else:
            # matcha architecture: acoustic model + vocoder
            matcha = sherpa_onnx.OfflineTtsMatchaModelConfig(
                acoustic_model=os.path.join(model_dir, conf['model']),
                vocoder=os.path.join(model_dir, conf['vocoder']),
                lexicon=lexicon,
                tokens=os.path.join(model_dir, conf['tokens']),
                data_dir=os.path.join(model_dir, conf['data_dir']) if conf.get('data_dir') else '',
                dict_dir=os.path.join(model_dir, conf['dict_dir']) if conf.get('dict_dir') else '',
            )
            model_config = sherpa_onnx.OfflineTtsModelConfig(
                matcha=matcha,
                provider="cpu",
                debug=False,
                num_threads=num_threads,
            )

        tts_config = sherpa_onnx.OfflineTtsConfig(
            model=model_config,
            rule_fsts=rule_fsts,
            max_num_sentences=1,
        )
        if not tts_config.validate():
            raise ValueError("Please check your config")
        self.tts_server = sherpa_onnx.OfflineTts(tts_config)

    # quote characters leak into the espeak English path and get spoken
    # as spurious syllables (e.g. closing quotes at sentence ends)
    _QUOTE_CHARS = ''.join(chr(c) for c in (0x201c, 0x201d, 0x2018, 0x2019, 0x22, 0x27, 0x300c, 0x300d, 0x300e, 0x300f, 0xab, 0xbb, 0x2039, 0x203a))

    # decorative brackets (lenticular/fullwidth) carry no prosody and only
    # leak into the espeak path; runs of 2+ punctuation chars make espeak-ng
    # vocalize an "empty clause" artifact (~0.7s, e.g. trailing '】' after
    # '!', or '！？' mid-text); punctuation at chunk head/tail does the same
    _BRACKETS = ''.join(chr(c) for c in (0x3010, 0x3011, 0xFF08, 0xFF09,
                                         0x300A, 0x300B, 0x3008, 0x3009,
                                         0x3014, 0x3015))
    _PUNCT = ''.join(chr(c) for c in (0xFF0C, 0x3002, 0xFF01, 0xFF1F, 0xFF1B,
                                      0xFF1A, 0x3001, 0x2026, 0x2014, 0xB7)) \
        + ',.!?:;()[]<>{}~@#^&*+=|/'
    _ENDER = ''.join(chr(c) for c in (0x3002, 0xFF01, 0xFF1F, 0x2026)) + '!?'

    @staticmethod
    def _clean_punct(text: str) -> str:
        text = text.translate(str.maketrans('', '', sherpa._BRACKETS))
        # collapse punct runs to their last char (it governs the contour)
        text = re.sub('[' + re.escape(sherpa._PUNCT) + ']{2,}',
                      lambda m: m.group(0)[-1], text)
        text = text.lstrip(sherpa._PUNCT)
        if not re.search('[' + sherpa._ENDER + ']$', text):
            text = text.rstrip(sherpa._PUNCT)
        return text

    # 无停顿长串会让模型整体加速(实测 ~17%): kokoro 只在 .!?; 处分句, 逗号不分句,
    # 连续无停顿 token 序列的 duration 预测整体偏短 (表征为"默认语速慢、超长才加速")。
    # 超过 _RUN_LIMIT 字的无停顿片段自动插逗号, 优先插在虚词后 (贴近自然句读),
    # 找不到虚词才按长度硬切; 标点良好的文本零改动
    _RUN_LIMIT = 16
    _SOFT_BREAK_AFTER = set('的了着呢吧吗啊是也都很又便就还才却和与或但而地得把被向从对让'
                            '上中下里内外前后间们个这那')

    @staticmethod
    def _split_run(run: str) -> str:
        """在单个无停顿片段内按 ~_RUN_LIMIT 字插入逗号。"""
        def alnum(c):
            return c.isascii() and c.isalnum()

        limit = sherpa._RUN_LIMIT
        parts, pos = [], 0
        while len(run) - pos > limit:
            window = run[pos:pos + limit]
            # 从窗口尾部向回找虚词, 在其后下刀 (k>=2 保证前段至少 3 字)
            cut = None
            for k in range(len(window) - 1, 1, -1):
                if window[k] in sherpa._SOFT_BREAK_AFTER:
                    cut = k + 1
                    break
            if cut is None:
                cut = limit
            # 避免把 ASCII 单词/数字拦腰切断: 切点落在字母数字段中间则退到段首
            while 0 < cut < len(run) - pos and alnum(run[pos + cut - 1]) and alnum(run[pos + cut]):
                cut -= 1
            parts.append(run[pos:pos + cut])
            parts.append('，')
            pos += cut
        parts.append(run[pos:])
        return ''.join(parts)

    @classmethod
    def _soft_break(cls, text: str) -> str:
        punct = cls._PUNCT + '，。！？；：、…—·“”‘’《》〈〉（）【】〔〕\u3000 \t\n'
        out = []
        i, n = 0, len(text)
        while i < n:
            j = i
            while j < n and text[j] not in punct:
                j += 1
            run = text[i:j]
            out.append(run if len(run) <= cls._RUN_LIMIT else cls._split_run(run))
            if j < n:
                out.append(text[j])
            i = j + 1
        return ''.join(out)

    def synthesize(self, text: str):
        text = text.translate(str.maketrans('', '', self._QUOTE_CHARS))
        # full-width colon is spoken as the word 'colon' by espeak when the
        # chunk contains ASCII (e.g. Q币); a comma keeps the prosodic pause
        text = text.replace(chr(0xFF1A), chr(0xFF0C))
        text = self._clean_punct(text)
        # 无标点长串整体加速: 超长无停顿片段自动补句读逗号 (须在清洗后做,
        # 括号删除/标点收敛会改变片段边界)
        text = self._soft_break(text)
        if not text:
            return None
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
