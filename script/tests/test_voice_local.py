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
                except (FileNotFoundError, ProcessLookupError):
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

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux parent-death guard")
    def test_guard_exits_if_parent_died_before_guard_started(self):
        code = """
import os, subprocess, sys, time
env = {**os.environ, "SAWE_VOICE_PARENT_PID": str(os.getpid())}
child = subprocess.Popen([sys.executable, '-u', '-c',
    'import time,sys; time.sleep(.3); exec(sys.argv[1]); time.sleep(30)', sys.argv[1]], env=env)
print(child.pid, flush=True)
time.sleep(30)
"""
        parent = subprocess.Popen([sys.executable, "-u", "-c", code, voice.PARENT_GUARD], stdout=subprocess.PIPE, text=True)
        child_pid = None
        try:
            child_pid = int(parent.stdout.readline())
            parent.kill()
            parent.wait(timeout=3)
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                try:
                    if "State:\tZ" in Path(f"/proc/{child_pid}/status").read_text():
                        return
                except (FileNotFoundError, ProcessLookupError):
                    return
                time.sleep(.02)
            self.fail("Child survived when parent died before guard startup")
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

    def test_idle_watchdog_rechecks_expiry_after_acquiring_request_lock(self):
        runtime = object.__new__(voice.Runtime)
        runtime.args = SimpleNamespace(idle_seconds=1)
        runtime.child_lock = threading.RLock()
        runtime.closed = Mock()
        runtime.closed.wait.side_effect = [False, True]
        runtime.last_used = 0
        runtime.lock = Mock()
        def acquire(_blocking):
            runtime.last_used = time.monotonic()
            return True
        runtime.lock.acquire.side_effect = acquire
        runtime.stop_recognizer = Mock()
        runtime.reap_idle()
        runtime.stop_recognizer.assert_not_called()
        runtime.lock.release.assert_called_once()

    def test_keepalive_retains_idle_recognizer_without_loading_or_interrupting(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60))
            child = Mock()
            runtime.child = child
            runtime.last_used = 0
            runtime.closed.wait = Mock(side_effect=[False, True])
            try:
                runtime.keep_alive()
                runtime.reap_idle()
                child.terminate.assert_not_called()
                self.assertGreater(runtime.last_used, 0)
                self.assertIs(runtime.child, child)
                runtime.last_used -= 61
                runtime.closed.wait = Mock(side_effect=[False, True])
                runtime.reap_idle()
                child.terminate.assert_called_once()
                self.assertIsNone(runtime.child)
            finally:
                runtime.close()

    def test_keepalive_http_is_bounded_and_does_not_take_recognition_lock(self):
        runtime = SimpleNamespace(lock=threading.Lock(), speech_lock=threading.Lock(), keep_alive=Mock())
        server = voice.Server(("127.0.0.1", 0), runtime)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        def post(data, headers=None):
            return http.open(urllib.request.Request(f"http://127.0.0.1:{server.server_port}/v1/audio/keepalive",
                             data=data, headers=headers or {"Content-Type": "application/json"}))
        try:
            runtime.lock.acquire()
            with post(b"{}") as response:
                self.assertEqual(json.load(response), {"status": "ok"})
            runtime.keep_alive.assert_called_once()
            for data, headers, status in [(b"[]", None, 400), (b'{"input":"private"}', None, 400),
                                          (b"{}", {"Origin": "https://example.com"}, 403),
                                          (b"", None, 413)]:
                with self.subTest(status=status), self.assertRaises(urllib.error.HTTPError) as error:
                    post(data, headers)
                self.assertEqual(error.exception.code, status)
            runtime.keep_alive.assert_called_once()
        finally:
            runtime.lock.release()
            server.shutdown()
            server.server_close()

    def test_launch_delivered_between_timeout_and_cancel_is_reaped(self):
        class DeliveredAtTimeout(voice.Future):
            def result(self, timeout=None):
                child = super().result(timeout=timeout)
                if timeout and timeout > 0:
                    raise voice.FutureTimeout()
                return child
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60))
            future = DeliveredAtTimeout()
            child = None
            try:
                with patch.object(voice, "Future", return_value=future):
                    with self.assertRaisesRegex(RuntimeError, "launch timed out"):
                        runtime.spawn_owned([sys.executable, "-c", "import time; time.sleep(30)"])
                child = future.result(timeout=0)
                self.assertIsNotNone(child.poll(), "Delivered child survived caller timeout")
            finally:
                runtime.close()
                if child is not None and child.poll() is None:
                    child.kill()
                    child.wait()

    def test_idle_and_shutdown_terminate_owned_child(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=1))
            child = Mock()
            runtime.child = child
            runtime.stop_recognizer()
            child.terminate.assert_called_once()
            child.wait.assert_called_once_with(timeout=5)
            runtime.close()
            self.assertTrue(runtime.closed.is_set())

    def test_http_bounds_busy_errors_success_and_no_cors(self):
        runtime = SimpleNamespace(args=SimpleNamespace(model="small", idle_seconds=60), lock=threading.Lock(), speech_lock=threading.Lock(),
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
                health = json.load(response)
                self.assertEqual(health["model"], "small")
                self.assertEqual(health["engine"], "whisper.cpp")
                self.assertEqual(health["idle_seconds"], 60)
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

class GigaAMTests(unittest.TestCase):
    def runtime(self, root, idle_seconds=60):
        root = Path(root)
        (root / "venv/bin").mkdir(parents=True)
        (root / "venv/bin/python").symlink_to(sys.executable)
        models = root / "models" / voice.GIGAAM_MODEL
        models.mkdir(parents=True)
        for name in voice.GIGAAM_HASHES:
            (models / name).write_bytes(b"test model")
        return voice.Runtime(SimpleNamespace(runtime_dir=root, idle_seconds=idle_seconds,
            model=voice.GIGAAM_MODEL, backend="cpu", threads=4))

    FAKE_WORKER = voice.PARENT_GUARD + '''
import json, struct, sys, time
from pathlib import Path
mode = (Path(sys.argv[1]).parent.parent / "mode")
mode = mode.read_text() if mode.exists() else "ok"
def send(value):
    data = json.dumps(value).encode()
    sys.stdout.buffer.write(struct.pack("!I", len(data)) + data)
    sys.stdout.buffer.flush()
if mode == "no-ready":
    time.sleep(30)
if mode == "bad-ready":
    send({"ready": False})
else:
    send({"ready": True})
if mode == "no-read":
    time.sleep(30)
while True:
    header = sys.stdin.buffer.read(4)
    if not header:
        break
    size, = struct.unpack("!I", header)
    data = sys.stdin.buffer.read(size)
    if mode == "crash":
        sys.exit(1)
    if mode == "no-reply":
        time.sleep(30)
    if mode == "oversized":
        sys.stdout.buffer.write(struct.pack("!I", 65537)); sys.stdout.buffer.flush()
    elif mode == "bad-json":
        sys.stdout.buffer.write(struct.pack("!I", 1) + b"{"); sys.stdout.buffer.flush()
    elif mode == "bad-result":
        send({"text": 42})
    else:
        send({"text": "  проверка  "})
'''

    def test_cli_defaults_and_gigaam_rejects_cuda(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            argv = [str(SOURCE), "--runtime-dir", temp, "setup"]
            with patch.object(sys, "argv", argv), patch.object(voice, "setup") as setup:
                voice.main()
                args = setup.call_args.args[0]
                self.assertEqual((args.model, args.backend, args.threads, args.idle_seconds),
                                 (voice.GIGAAM_MODEL, "cpu", 4, 3600))
            with patch.object(sys, "argv", argv + ["--backend", "cuda"]), self.assertRaises(SystemExit):
                voice.main()

    def test_gigaam_setup_pins_files_without_whisper_cuda_or_hub(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            root = Path(temp)
            def download(url, target):
                target.write_bytes(b"fixture")
            args = SimpleNamespace(runtime_dir=root, model=voice.GIGAAM_MODEL, backend="cpu")
            with patch.dict(os.environ, clear=False), patch.object(voice, "run") as run, \
                    patch.object(voice, "download", side_effect=download) as fetch:
                voice.setup(args)
            commands = [[str(x) for x in call.args[0]] for call in run.call_args_list]
            self.assertFalse(any(cmd[0] in ("git", "cmake") for cmd in commands))
            packages = commands[-1]
            self.assertIn("onnx-asr==0.12.0", packages)
            self.assertIn("onnxruntime==1.31.0", packages)
            self.assertFalse(any("hub" in x or "gpu" in x for x in packages))
            urls = [call.args[0] for call in fetch.call_args_list]
            self.assertFalse(any("whisper" in url for url in urls))
            for name in voice.GIGAAM_HASHES:
                self.assertTrue(any(voice.GIGAAM_REV in url and url.endswith("/" + name) for url in urls))
            manifest = json.loads((root / "manifest.json").read_text())
            self.assertEqual(manifest["gigaam_revision"], voice.GIGAAM_REV)
            self.assertIn(voice.GIGAAM_MODEL + "/LICENSE.txt", manifest["sha256"])

    def test_gigaam_startup_rejects_corrupt_model_before_spawn(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            try:
                with patch.object(runtime, "spawn_owned") as spawn:
                    with self.assertRaisesRegex(RuntimeError, "missing/corrupt"):
                        runtime.transcribe(wav(), "ru")
                    spawn.assert_not_called()
            finally:
                runtime.close()

    def test_gigaam_language_rejected_before_worker_load(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            try:
                with patch.object(runtime, "gigaam") as load:
                    for language in ("en", "fr", ""):
                        with self.assertRaisesRegex(ValueError, "Russian only"):
                            runtime.transcribe(wav(), language)
                    load.assert_not_called()
            finally:
                runtime.close()

    def test_worker_reused_then_shutdown_reaps_and_closes_pipes(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            try:
                with patch.object(voice, "verify_download"), patch.object(voice, "GIGAAM_WORKER", self.FAKE_WORKER):
                    self.assertEqual(runtime.transcribe(wav(), "ru"), {"text": "проверка"})
                    child = runtime.child
                    self.assertEqual(runtime.transcribe(wav(), "auto"), {"text": "проверка"})
                    self.assertIs(runtime.child, child)
                    runtime.close()
                    self.assertIsNotNone(child.poll())
                    self.assertTrue(child.stdin.closed)
                    self.assertTrue(child.stdout.closed)
            finally:
                runtime.close()

    def test_worker_bad_readiness_and_timeout_reset_then_recover(self):
        for mode in ("bad-ready", "no-ready"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
                runtime = self.runtime(temp)
                Path(temp, "mode").write_text(mode)
                try:
                    with patch.object(voice, "verify_download"), patch.object(voice, "GIGAAM_WORKER", self.FAKE_WORKER), \
                            patch.object(voice, "WORKER_START_TIMEOUT", .1):
                        with self.assertRaisesRegex(RuntimeError, "startup failed"):
                            runtime.transcribe(wav(), "ru")
                        self.assertIsNone(runtime.child)
                        Path(temp, "mode").unlink()
                        self.assertEqual(runtime.transcribe(wav(), "ru"), {"text": "проверка"})
                finally:
                    runtime.close()

    def test_worker_io_failure_reset_then_recovery_including_blocked_write(self):
        for mode in ("crash", "no-read", "no-reply", "oversized", "bad-json", "bad-result"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
                runtime = self.runtime(temp)
                Path(temp, "mode").write_text(mode)
                try:
                    with patch.object(voice, "verify_download"), patch.object(voice, "GIGAAM_WORKER", self.FAKE_WORKER), \
                            patch.object(voice, "WORKER_REQUEST_TIMEOUT", .1):
                        start = time.monotonic()
                        with self.assertRaisesRegex(RuntimeError, "worker reset"):
                            runtime.transcribe(wav(60), "ru")
                        self.assertLess(time.monotonic() - start, 2)
                        self.assertIsNone(runtime.child)
                        Path(temp, "mode").unlink()
                        self.assertEqual(runtime.transcribe(wav(), "ru"), {"text": "проверка"})
                finally:
                    runtime.close()

    def test_cold_start_and_inference_share_the_request_deadline(self):
        read_fd, write_fd = os.pipe()
        response = b'{"text":"late valid response"}'
        os.write(write_fd, len(response).to_bytes(4, "big") + response)
        os.close(write_fd)
        clock = [100.0]
        with os.fdopen(read_fd, "rb", buffering=0) as output:
            child = SimpleNamespace(stdin=None, stdout=output)
            def startup(_deadline=None):
                clock[0] += .5
                return child
            def write(*_args):
                clock[0] += .31
            runtime = SimpleNamespace(gigaam=startup, closed=threading.Event(), stop_recognizer=Mock())
            with patch.object(voice.time, "monotonic", side_effect=lambda: clock[0]), \
                    patch.object(voice, "write_worker_frame", side_effect=write), \
                    patch.object(voice, "WORKER_REQUEST_TIMEOUT", .8):
                with self.assertRaisesRegex(RuntimeError, "worker reset"):
                    voice.Runtime.transcribe_gigaam(runtime, wav())
            runtime.stop_recognizer.assert_called_once()

    def test_worker_idle_unload_and_lazy_reload(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp, idle_seconds=1)
            try:
                with patch.object(voice, "verify_download"), patch.object(voice, "GIGAAM_WORKER", self.FAKE_WORKER):
                    self.assertIsNone(runtime.child)
                    runtime.transcribe(wav(), "ru")
                    child = runtime.child
                    runtime.last_used = 0
                    deadline = time.monotonic() + 2
                    while runtime.child is child and time.monotonic() < deadline:
                        time.sleep(.02)
                    self.assertIsNone(runtime.child)
                    self.assertIsNotNone(child.poll())
                    runtime.transcribe(wav(), "ru")
                    self.assertIsNot(runtime.child, child)
            finally:
                runtime.close()

    def test_shutdown_interrupts_worker_startup_without_waiting_full_deadline(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            Path(temp, "mode").write_text("no-ready")
            errors = []
            def request():
                try:
                    runtime.transcribe(wav(), "ru")
                except RuntimeError as exc:
                    errors.append(str(exc))
            try:
                with patch.object(voice, "verify_download"), patch.object(voice, "GIGAAM_WORKER", self.FAKE_WORKER):
                    thread = threading.Thread(target=request)
                    thread.start()
                    deadline = time.monotonic() + 2
                    while runtime.child is None and time.monotonic() < deadline:
                        time.sleep(.01)
                    self.assertIsNotNone(runtime.child)
                    start = time.monotonic()
                    runtime.close()
                    thread.join(2)
                    self.assertFalse(thread.is_alive())
                    self.assertLess(time.monotonic() - start, 2)
                    self.assertEqual(len(errors), 1)
                    self.assertIsNone(runtime.child)
            finally:
                runtime.close()

    def test_late_launch_after_caller_timeout_is_reaped(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            entered, release = threading.Event(), threading.Event()
            real_popen = subprocess.Popen
            children = []
            def slow_spawn(*args, **kwargs):
                child = real_popen(*args, **kwargs)
                children.append(child)
                entered.set()
                release.wait(2)
                return child
            try:
                with patch.object(voice.subprocess, "Popen", side_effect=slow_spawn), \
                        patch.object(voice, "LAUNCH_TIMEOUT", .05):
                    with self.assertRaisesRegex(RuntimeError, "launch timed out"):
                        runtime.spawn_owned([sys.executable, "-c", "import time; time.sleep(30)"])
                    self.assertTrue(entered.is_set())
                    release.set()
                    deadline = time.monotonic() + 2
                    while children[0].poll() is None and time.monotonic() < deadline:
                        time.sleep(.01)
                    self.assertIsNotNone(children[0].poll())
            finally:
                release.set()
                runtime.close()
                for child in children:
                    if child.poll() is None:
                        child.kill()
                        child.wait()

    def test_shutdown_reaps_inflight_launch_and_cancels_queued_launch(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            entered, release = threading.Event(), threading.Event()
            real_popen = subprocess.Popen
            children, errors = [], []
            def slow_spawn(*args, **kwargs):
                child = real_popen(*args, **kwargs)
                children.append(child)
                entered.set()
                release.wait(2)
                return child
            def request():
                try:
                    runtime.spawn_owned([sys.executable, "-c", "import time; time.sleep(30)"])
                except RuntimeError as exc:
                    errors.append(str(exc))
            threads = [threading.Thread(target=request) for _ in range(2)]
            try:
                with patch.object(voice.subprocess, "Popen", side_effect=slow_spawn):
                    threads[0].start()
                    self.assertTrue(entered.wait(2))
                    threads[1].start()
                    deadline = time.monotonic() + 2
                    while runtime.launches.qsize() == 0 and time.monotonic() < deadline:
                        time.sleep(.01)
                    self.assertEqual(runtime.launches.qsize(), 1)
                    runtime.close()
                    release.set()
                    for thread in threads:
                        thread.join(2)
                        self.assertFalse(thread.is_alive())
                    self.assertEqual(len(children), 1, "Queued process launched after shutdown")
                    self.assertIsNotNone(children[0].poll(), "Undelivered process survived shutdown")
                    self.assertEqual(len(errors), 2)
            finally:
                release.set()
                runtime.close()
                for thread in threads:
                    if thread.ident is not None:
                        thread.join(2)
                for child in children:
                    if child.poll() is None:
                        child.kill()
                        child.wait()

    def test_queued_launch_cannot_start_after_shutdown(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = self.runtime(temp)
            runtime.close()
            with patch.object(voice.subprocess, "Popen") as spawn:
                with self.assertRaisesRegex(RuntimeError, "shutting down"):
                    runtime.spawn_owned([sys.executable, "-c", "pass"])
                spawn.assert_not_called()

    def test_whisper_explicit_language_contract_is_retained(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temp:
            runtime = voice.Runtime(SimpleNamespace(runtime_dir=Path(temp), idle_seconds=60,
                                                   model="base", backend="cpu", threads=4))
            response = Mock()
            response.read.return_value = b'{"text":" English "}'
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            try:
                with patch.object(runtime, "whisper", return_value="http://127.0.0.1:1"), \
                        patch.object(voice.HTTP, "open", return_value=response):
                    self.assertEqual(runtime.transcribe(wav(), "en"), {"text": "English"})
            finally:
                runtime.close()


if __name__ == "__main__":
    unittest.main()
