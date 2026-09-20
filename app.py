#! /usr/bin/env python
# -*- coding: utf-8 -*-

import os
import threading
import yaml
from flask import Flask, request, Response, render_template, session
from flask_socketio import SocketIO, emit

from text import text
from sherpa import sherpa
from logger import logger

project_folder = os.path.dirname(os.path.abspath(__file__))

with open(os.path.join(project_folder, 'config', 'config.yaml'), 'r', encoding='utf-8') as f:
    config = yaml.safe_load(f)

SERVER = text(config['server'])
TTS = sherpa(config['tts'])

SERVER.initialize()
logger.info("Server initialized")
TTS.initialize()
logger.info("TTS initialized")

# Independent HTTP /tts stream (not shared with socket sessions)
tts_gen = SERVER.GenText()

# Per-socket-connection text generator (keyed by request.sid)
session_gens = {}

# Serialize generator advancement per session (socketio handles events in threads)
session_locks = {}

app = Flask(__name__)
app.config['SECRET_KEY'] = '~heartale!'
socketio = SocketIO(app)


def _next_audio(gen):
    """Pull one text chunk from the generator and synthesize it. Returns None on end."""
    while True:
        try:
            gen_text = next(gen)
        except StopIteration:
            return None
        audio = TTS.synthesize(gen_text["text"])
        if audio is None:
            logger.warning(f"TTS returned empty audio, skip chunk chapter{gen_text['chapterIndex']} position{gen_text['position']}")
            continue
        gen_text["audio"] = audio
        return gen_text


def _session_gen():
    sid = request.sid
    gen = session_gens.get(sid)
    if gen is None:
        gen = SERVER.GenText()
        session_gens[sid] = gen
    return gen


def _session_lock():
    sid = request.sid
    lock = session_locks.get(sid)
    if lock is None:
        lock = threading.Lock()
        session_locks[sid] = lock
    return lock


@socketio.on('request_next_audio')
def request_next_audio():
    with _session_lock():
        gen_text = _next_audio(_session_gen())
    if gen_text is None:
        emit("audio_end")
        return
    logger.debug(f"Sending audio chapter{gen_text['chapterIndex']} position{gen_text['position']}")
    emit("audio_data", {
        "chapterIndex": gen_text["chapterIndex"],
        "position": gen_text["position"],
        "audio": gen_text["audio"],
        "text": gen_text["text"],
    })


@socketio.on('chunk_played')
def handle_chunk_played(data):
    SERVER.save_book_progress(data["chapterIndex"], data["position"])
    logger.debug(f"Progress saved chapter{data['chapterIndex']} position{data['position']}")


@socketio.on('connect')
def handle_connect():
    app.logger.info('Client connected')


@socketio.on('disconnect')
def handle_disconnect():
    session_gens.pop(request.sid, None)
    session_locks.pop(request.sid, None)
    SERVER.flush_progress()
    app.logger.info('Client disconnected')


@socketio.on('text_sync')
def handle_text_sync(checked):
    session['text_sync'] = checked
    if not checked:
        emit("text_data", "")


@app.route('/index')
def index():
    return render_template('index.html')


@app.route('/tts')
def tts():
    global tts_gen
    gen_text = _next_audio(tts_gen)
    if gen_text is None:
        return Response(status=204)
    logger.debug(f"TTS audio chapter{gen_text['chapterIndex']} position{gen_text['position']}")
    SERVER.save_book_progress(gen_text["chapterIndex"], gen_text["position"])
    return Response(gen_text["audio"], mimetype='audio/wav')


@app.route('/save', methods=['GET', 'POST'])
def save():
    index = request.args.get('index')
    position = request.args.get('pos')
    if index is not None and position is not None:
        SERVER.save_book_progress(int(index), int(position))
    return Response("success", status=200)


if __name__ == '__main__':
    socketio.run(app, debug=False, host='0.0.0.0', port=28081, allow_unsafe_werkzeug=True)
