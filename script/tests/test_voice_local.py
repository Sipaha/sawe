"""Boundary and lifecycle tests; no model downloads or external network."""
import importlib.machinery
import importlib.util
import io
import json
import os
import subprocess
import time
from pathlib import Path
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import urllib.error
import urllib.request
import wave

SOURCE = Path(__file__).resolve().parents[1] / "voice-local"
loader = importlib.machinery.SourceFileLoader("voice_local", str(SOURCE))
spec = importlib.util.spec_from_loader(loader.name, loader)
voice = importlib.util.module_from_spec(spec)
loader.exec_module(voice)
SCRATCH = Path("/home/spk/.spk/sawe/ss/Sawe1/.tmp")


def wav(seconds=1, rate=16000):
    output = io.BytesIO()
    with wave.open(output, "wb") as file:
        file.setparams((1, 2, rate, 0, "NONE", ""))
        file.writeframes(b"\0\0" * int(seconds * rate))
    return output.getvalue()


class VoiceTests(unittest.TestCase):
    def test_wav_duration_format_and_truncation_bounds(self):
        voice.validate_wav(wav())
        for audio in (b"broken", wav(.01), wav(61), wav(rate=22050), wav()[:-8]):
            with self.subTest(size=len(audio)), self.assertRaises(ValueError):
                voice.validate_wav(audio)

    def test_multipart_requires_unique_file(self):
        body = b'--x\r\nContent-Disposition: form-data; name="file"; filename="x.wav"\r\n\r\nabc\r\n--x--\r\n'
        self.assertEqual(voice.multipart("multipart/form-data; boundary=x", body)["file"], b"abc")
        with self.assertRaises(ValueError):
            voice.multipart("application/json", body)
        with self.assertRaises(ValueError):
            voice.multipart("multipart/form-data; boundary=x", body.replace(b'--x--', body[:-7] + b'--x--'))

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux parent-death guard")
    def test_linux_killed_sidecar_does_not_orphan_engine(self):
        parent_code = """
import subprocess, sys, time
child = subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.executable, '-u', '-c',
    'import time; print("ready", flush=True); time.sleep(30)'], stdout=subprocess.PIPE, text=True)
assert child.stdout.readline().strip() == 'ready'
print(child.pid, flush=True)
time.sleep(30)
"""
        parent = subprocess.Popen([sys.executable, "-u", "-c", parent_code, voice.EXEC_GUARD],
                                  stdout=subprocess.PIPE, text=True)
        child_pid = None
        try:
            child_pid = int(parent.stdout.readline())
            parent.kill()
            parent.wait(timeout=3)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                try:
                    status = Path(f"/proc/{child_pid}/status").read_text()
                    if "State:\tZ" in status:
                        return
                except FileNotFoundError:
                    return
                time.sleep(.02)
            self.fail("Inference process survived killed sidecar")
        finally:
            if parent.poll() is None:
                parent.kill()
                parent.wait()
            parent.stdout.close()
            if child_pid:
                try:
                    os.kill(child_pid, 9)
                except ProcessLookupError:
                    pass

    def test_tts_limits_before_spawning(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60, threads=2))
            try:
                for payload in ({}, {"input": ""}, {"input": "x" * 4001}, {"input": "hi", "response_format": "mp3"}):
                    with self.assertRaises(ValueError):
                        runtime.speech(payload)
                self.assertEqual(list(Path(temp).iterdir()), [])
            finally:
                runtime.close()

    def test_model_survives_short_lived_request_thread(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60))
            def request():
                runtime.child = runtime.spawn_owned([sys.executable, "-c", voice.EXEC_GUARD,
                    sys.executable, "-u", "-c", 'import time; print("ready", flush=True); time.sleep(30)'],
                    stdout=subprocess.PIPE, text=True)
                self.assertEqual(runtime.child.stdout.readline().strip(), "ready")
            thread = threading.Thread(target=request)
            try:
                thread.start()
                thread.join(3)
                self.assertFalse(thread.is_alive())
                child = runtime.child
                time.sleep(.1)
                self.assertIsNone(child.poll(), "Model was killed with HTTP handler thread")
                runtime.close()
                self.assertIsNotNone(child.poll())
                child.stdout.close()
            finally:
                runtime.close()

    def test_transcription_gate_blocks_late_tts_spawn(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60, threads=2))
            try:
                runtime.begin_transcription()
                with patch.object(voice.subprocess, "Popen") as spawn:
                    with self.assertRaises(voice.EngineBusy):
                        runtime.speech({"input": "answer raced with microphone"})
                    spawn.assert_not_called()
                self.assertEqual(list(Path(temp).iterdir()), [])
                runtime.end_transcription()
                self.assertFalse(runtime.transcribing)
            finally:
                runtime.close()

    def test_checksum_detects_corruption(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            file = Path(temp) / "ggml-base-q5_1.bin"
            file.write_bytes(b"incomplete model")
            with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
                voice.verify_download(file)

    def test_idle_watchdog_releases_model(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=1))
            child = Mock()
            runtime.child = child
            runtime.last_used = 0
            try:
                runtime.watchdog.join(1.2)
                child.terminate.assert_called_once()
                self.assertIsNone(runtime.child)
            finally:
                runtime.close()

    def test_idle_and_shutdown_terminate_owned_child(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=1))
            child = Mock()
            runtime.child = child
            runtime.stop_whisper()
            child.terminate.assert_called_once()
            child.wait.assert_called_once_with(timeout=5)
            runtime.close()
            self.assertTrue(runtime.closed.is_set())

    def test_http_bounds_busy_errors_success_and_no_cors(self):
        runtime = SimpleNamespace(args=SimpleNamespace(model="small"), lock=threading.Lock(), speech_lock=threading.Lock(),
                                  begin_transcription=Mock(), end_transcription=Mock(),
                                  transcribe=Mock(return_value={"text": "Проверка"}),
                                  speech=Mock(return_value=wav()), last_used=0)
        server = voice.Server(("127.0.0.1", 0), runtime)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        def post(path, body, headers=None):
            return http.open(urllib.request.Request(base + path, data=body, headers=headers or {}))
        try:
            with http.open(base + "/health") as response:
                self.assertEqual(json.load(response)["model"], "small")
                self.assertIsNone(response.headers.get("Access-Control-Allow-Origin"))
            for path, data, code in (("/wrong", b"{}", 404), ("/v1/audio/speech", b"[1]", 400),
                                     ("/v1/audio/speech", b"", 413)):
                with self.assertRaises(urllib.error.HTTPError) as error:
                    post(path, data)
                self.assertEqual(error.exception.code, code)
            with self.assertRaises(urllib.error.HTTPError) as error:
                post("/v1/audio/speech", b'{}', {"Origin": "https://example.com"})
            self.assertEqual(error.exception.code, 403)
            runtime.speech_lock.acquire()
            try:
                with self.assertRaises(urllib.error.HTTPError) as error:
                    post("/v1/audio/speech", b'{}')
                self.assertEqual(error.exception.code, 429)
            finally:
                runtime.speech_lock.release()
            # Run an actual overlapping HTTP speech request; STT interrupts it.
            tts_started, tts_canceled = threading.Event(), threading.Event()
            def slow_tts(payload):
                tts_started.set()
                if not tts_canceled.wait(3):
                    raise RuntimeError("STT failed to interrupt TTS")
                return wav()
            runtime.speech.side_effect = slow_tts
            runtime.begin_transcription.side_effect = tts_canceled.set
            tts_result = []
            def request_speech():
                with post("/v1/audio/speech", b'{"input":"long answer"}') as response:
                    tts_result.append(response.status)
            tts_thread = threading.Thread(target=request_speech)
            tts_thread.start()
            self.assertTrue(tts_started.wait(2))
            body = b'--x\r\nContent-Disposition: form-data; name="file"; filename="a.wav"\r\n\r\nabc\r\n--x--\r\n'
            with post("/v1/audio/transcriptions", body, {"Content-Type": "multipart/form-data; boundary=x"}) as response:
                self.assertEqual(json.load(response)["text"], "Проверка")
            runtime.begin_transcription.assert_called_once()
            tts_thread.join(3)
            self.assertEqual(tts_result, [200])
            runtime.speech.side_effect = None
            with post("/v1/audio/speech", b'{"input":"hello"}') as response:
                self.assertEqual(response.headers["Content-Type"], "audio/wav")
                voice.validate_wav(response.read())
            runtime.speech.side_effect = RuntimeError("private engine details")
            with self.assertRaises(urllib.error.HTTPError) as error:
                post("/v1/audio/speech", b'{"input":"hello"}')
            self.assertEqual(error.exception.code, 503)
            self.assertNotIn(b"private engine details", error.exception.read())
        finally:
            server.shutdown()
            server.server_close()
            thread.join(2)


if __name__ == "__main__":
    unittest.main()
