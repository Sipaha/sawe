//! Bounded microphone utterances and interruptible speech playback.
//!
//! Device callbacks never wait for a consumer. A slow consumer stops capture with
//! an error instead of silently dropping words or accumulating unbounded audio.
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use cpal::{
    DeviceId, SampleFormat, SizedSample,
    traits::{DeviceTrait, StreamTrait},
};
use rodio::{Decoder, MixerDeviceSink, Player, Source, mixer::Mixer};

use crate::{EchoCanceller, audio_pipeline::open_output_stream, resolve_device};

const FRAME_SAMPLES: usize = 480; // 10ms at the AEC's required 48kHz
const WAV_RATE: u32 = 16_000;
const PRE_ROLL_FRAMES: usize = 20;
const START_FRAMES: usize = 3;
const MIN_VOICED_FRAMES: usize = 12;
const SILENCE_FRAMES: usize = 70;
const MAX_FRAMES: usize = 3_000; // 30 seconds, including pre-roll
const MAX_INPUT_CHUNK: usize = 4_096;
const MAX_PLAYBACK_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum VoiceCaptureEvent {
    SpeechStarted,
    /// Mono, 16kHz, signed PCM16 RIFF/WAV, no longer than 30 seconds.
    Utterance(Vec<u8>),
    Error(String),
    Stopped,
}

/// Owns one microphone stream. Dropping cancels without emitting a final utterance.
/// `finish` flushes an explicit recording, while continuous capture segments on
/// silence. Both modes enforce the same maximum utterance length.
pub struct VoiceCapture {
    events: Receiver<VoiceCaptureEvent>,
    pending_events: Arc<AtomicUsize>,
    finish: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl VoiceCapture {
    pub fn start(
        input_device: Option<DeviceId>,
        continuous: bool,
        echo_canceller: EchoCanceller,
    ) -> Result<Self> {
        let (events_tx, events) = mpsc::sync_channel(8);
        let pending_events = Arc::new(AtomicUsize::new(0));
        let events_tx = EventSender {
            sender: events_tx,
            pending: pending_events.clone(),
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let finish = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_finish = finish.clone();
        let worker_cancel = cancel.clone();
        let worker = thread::Builder::new()
            .name("voice-capture".into())
            .spawn(move || {
                let result = capture_worker(
                    input_device,
                    continuous,
                    echo_canceller,
                    &events_tx,
                    &ready_tx,
                    &worker_finish,
                    &worker_cancel,
                );
                if !worker_cancel.load(Ordering::Acquire) {
                    if let Err(error) = result {
                        events_tx.terminal(VoiceCaptureEvent::Error(format!("{error:#}")));
                    }
                    events_tx.terminal(VoiceCaptureEvent::Stopped);
                }
            })?;
        let mut capture = Self {
            events,
            pending_events,
            finish,
            cancel,
            worker: Some(worker),
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(message)) => {
                capture.stop();
                bail!(message)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                capture.stop();
                bail!("Microphone capture stopped during initialization")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A driver can block inside device initialization. It cannot be
                // killed safely; detach only this pending open, and cancel as
                // soon as it returns instead of blocking the UI in Drop.
                capture.cancel.store(true, Ordering::Release);
                capture.worker.take();
                log::warn!(
                    "Microphone initialization timed out; cancelling the pending device open"
                );
                bail!("Microphone initialization timed out; check the selected input device")
            }
        }
    }

    pub fn try_recv(&self) -> std::result::Result<VoiceCaptureEvent, TryRecvError> {
        let event = self.events.try_recv()?;
        self.pending_events.fetch_sub(1, Ordering::AcqRel);
        Ok(event)
    }

    pub fn finish(&self) {
        self.finish.store(true, Ordering::Release);
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if let Err(panic) = worker.join() {
                log::error!("Voice capture worker panicked: {panic:?}");
            }
        }
    }
}

impl Drop for VoiceCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_worker(
    input_device: Option<DeviceId>,
    continuous: bool,
    mut echo_canceller: EchoCanceller,
    events: &EventSender,
    ready: &SyncSender<std::result::Result<(), String>>,
    finish: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<()> {
    let (samples_tx, samples_rx) = mpsc::sync_channel(32);
    let failed = Arc::new(AtomicBool::new(false));
    let opened = open_capture(input_device, samples_tx, failed.clone());
    let (stream, rate) = match opened {
        Ok(value) => value,
        Err(error) => {
            if ready
                .send(Err(format!("Could not open microphone: {error:#}")))
                .is_err()
            {
                // The caller timed out while the device was opening.
                log::debug!("Microphone initialization receiver was cancelled");
            }
            return Ok(());
        }
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    if let Err(error) = stream.play() {
        if ready
            .send(Err(format!("Could not start microphone: {error}")))
            .is_err()
        {
            // The caller timed out while the device was opening.
            log::debug!("Microphone initialization receiver was cancelled");
        }
        return Ok(());
    }
    if ready.send(Ok(())).is_err() {
        // The caller timed out; drop the stream without starting capture work.
        return Ok(());
    }
    let mut processor = CaptureProcessor::new(rate, continuous);
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if failed.load(Ordering::Acquire) {
            bail!(
                "Microphone disconnected or capture could not keep up; please start recording again"
            );
        }
        if finish.load(Ordering::Acquire) {
            // Drop the callback stream first, then process its bounded pending data.
            drop(stream);
            while let Ok(samples) = samples_rx.try_recv() {
                if !processor.push(&samples, &mut echo_canceller, events)? {
                    break;
                }
            }
            processor.finish(events)?;
            return Ok(());
        }
        if !continuous && started.elapsed() >= Duration::from_secs(30) {
            processor.finish(events)?;
            return Ok(());
        }
        match samples_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(samples) => {
                if !processor.push(&samples, &mut echo_canceller, events)? {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("Microphone stream ended"),
        }
    }
}

fn open_capture(
    input_device: Option<DeviceId>,
    samples: SyncSender<Vec<f32>>,
    failed: Arc<AtomicBool>,
) -> Result<(cpal::Stream, u32)> {
    let device = resolve_device(input_device.as_ref(), true)?;
    let supported = device.default_input_config()?;
    let rate = supported.sample_rate();
    let config: cpal::StreamConfig = supported.clone().into();
    anyhow::ensure!(
        (8_000..=384_000).contains(&rate) && (1..=32).contains(&config.channels),
        "Unsupported microphone sample rate or channel count"
    );
    macro_rules! build {
        ($($format:ident => $sample:ty),* $(,)?) => {
            match supported.sample_format() {
                $(SampleFormat::$format => build_stream::<$sample>(&device, &config, samples, failed)?,)*
                other => bail!("Unsupported microphone format: {other}"),
            }
        }
    }
    let stream = build!(I8 => i8, I16 => i16, I24 => cpal::I24, I32 => i32, I64 => i64,
        U8 => u8, U16 => u16, U32 => u32, U64 => u64, F32 => f32, F64 => f64);
    Ok((stream, rate))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: SyncSender<Vec<f32>>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels);
    let stream_failed = failed.clone();
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data, _| {
                for chunk in data.chunks(MAX_INPUT_CHUNK * channels) {
                    let mono = chunk
                        .chunks_exact(channels)
                        .map(|frame| {
                            let sample = frame
                                .iter()
                                .map(|sample| sample.to_sample::<f32>())
                                .sum::<f32>()
                                / channels as f32;
                            if sample.is_finite() {
                                sample.clamp(-1.0, 1.0)
                            } else {
                                0.0
                            }
                        })
                        .collect();
                    if samples.try_send(mono).is_err() {
                        failed.store(true, Ordering::Release);
                        break;
                    }
                }
            },
            move |_| {
                stream_failed.store(true, Ordering::Release);
            },
            Some(Duration::from_secs(1)),
        )
        .context("Could not build microphone stream")
}

struct CaptureProcessor {
    resampler: Resampler,
    frame: Vec<f32>,
    segmenter: Segmenter,
}

impl CaptureProcessor {
    fn new(rate: u32, continuous: bool) -> Self {
        Self {
            resampler: Resampler::new(rate),
            frame: Vec::with_capacity(FRAME_SAMPLES),
            segmenter: Segmenter::new(continuous),
        }
    }

    fn push(
        &mut self,
        samples: &[f32],
        echo: &mut EchoCanceller,
        events: &EventSender,
    ) -> Result<bool> {
        let Self {
            resampler,
            frame,
            segmenter,
        } = self;
        let mut keep_going = true;
        let mut error = None;
        resampler.push(samples, |sample| {
            if !keep_going || error.is_some() {
                return;
            }
            frame.push(sample);
            if frame.len() == FRAME_SAMPLES {
                let mut stereo = [0i16; FRAME_SAMPLES * 2];
                for (sample, pair) in frame.iter().zip(stereo.chunks_exact_mut(2)) {
                    pair.fill((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                }
                if let Err(err) = echo.process_stream(&mut stereo) {
                    error = Some(err);
                    return;
                }
                let mono: Vec<i16> = stereo
                    .chunks_exact(6)
                    .map(|samples| (samples.iter().map(|&s| i32::from(s)).sum::<i32>() / 6) as i16)
                    .collect();
                for event in segmenter.push(&mono) {
                    if let Err(err) = send_event(events, event) {
                        error = Some(err);
                        return;
                    }
                }
                keep_going = !segmenter.done;
                frame.clear();
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        Ok(keep_going)
    }

    fn finish(&mut self, events: &EventSender) -> Result<()> {
        // Preserve the final incomplete 10ms block; it is too short for AEC.
        if !self.frame.is_empty() {
            let mono: Vec<i16> = self
                .frame
                .chunks(3)
                .map(|chunk| {
                    (chunk.iter().sum::<f32>() / chunk.len() as f32 * i16::MAX as f32) as i16
                })
                .collect();
            for event in self.segmenter.push(&mono) {
                send_event(events, event)?;
            }
            self.frame.clear();
        }
        if let Some(event) = self.segmenter.finish() {
            send_event(events, event)?;
        }
        Ok(())
    }
}

// Reserve two queue slots for Error/Stopped so overload is observable.
struct EventSender {
    sender: SyncSender<VoiceCaptureEvent>,
    pending: Arc<AtomicUsize>,
}

impl EventSender {
    fn enqueue(&self, event: VoiceCaptureEvent) -> Result<()> {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = self.sender.try_send(event) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(error).context("Voice event receiver is unavailable");
        }
        Ok(())
    }

    fn terminal(&self, event: VoiceCaptureEvent) {
        if let Err(error) = self.enqueue(event) {
            // Expected if the consumer dropped capture during worker shutdown.
            log::debug!("Voice terminal event was not delivered: {error:#}");
        }
    }
}

fn send_event(events: &EventSender, event: VoiceCaptureEvent) -> Result<()> {
    anyhow::ensure!(
        events.pending.load(Ordering::Acquire) < 6,
        "Voice event consumer is not keeping up; recording stopped"
    );
    events.enqueue(event)
}

/// Streaming box-filter rate conversion. Integer time units avoid drift at 44.1kHz;
/// averaging input intervals also avoids simple sample-skipping on downsampling.
struct Resampler {
    input_rate: u32,
    filled: u32,
    sum: f64,
}

impl Resampler {
    fn new(input_rate: u32) -> Self {
        Self {
            input_rate,
            filled: 0,
            sum: 0.0,
        }
    }
    fn push(&mut self, input: &[f32], mut output: impl FnMut(f32)) {
        for &sample in input {
            let mut remaining = 48_000;
            while remaining > 0 {
                let take = remaining.min(self.input_rate - self.filled);
                self.sum += f64::from(sample) * f64::from(take);
                self.filled += take;
                remaining -= take;
                if self.filled == self.input_rate {
                    output((self.sum / f64::from(self.input_rate)) as f32);
                    self.filled = 0;
                    self.sum = 0.0;
                }
            }
        }
    }
}

struct Segmenter {
    continuous: bool,
    pre_roll: VecDeque<Vec<i16>>,
    recording: Vec<i16>,
    noise_floor: f32,
    start_frames: usize,
    voiced_frames: usize,
    silent_frames: usize,
    frames: usize,
    done: bool,
}

impl Segmenter {
    fn new(continuous: bool) -> Self {
        Self {
            continuous,
            pre_roll: VecDeque::new(),
            recording: Vec::new(),
            noise_floor: 0.001,
            start_frames: 0,
            voiced_frames: 0,
            silent_frames: 0,
            frames: 0,
            done: false,
        }
    }

    fn push(&mut self, frame: &[i16]) -> Vec<VoiceCaptureEvent> {
        if self.done {
            return Vec::new();
        }
        let mut events = Vec::new();
        let energy = (frame
            .iter()
            .map(|s| (f32::from(*s) / 32768.0).powi(2))
            .sum::<f32>()
            / frame.len().max(1) as f32)
            .sqrt();
        let voiced = energy > (self.noise_floor * 3.5).max(0.008);
        if self.recording.is_empty() {
            self.pre_roll.push_back(frame.to_vec());
            while self.pre_roll.len() > PRE_ROLL_FRAMES {
                self.pre_roll.pop_front();
            }
            self.start_frames = if voiced { self.start_frames + 1 } else { 0 };
            if !voiced {
                self.noise_floor = self.noise_floor * 0.98 + energy * 0.02;
            }
            if self.start_frames >= START_FRAMES {
                self.frames = self.pre_roll.len();
                self.recording.extend(self.pre_roll.drain(..).flatten());
                self.voiced_frames = self.start_frames;
                self.silent_frames = 0;
                events.push(VoiceCaptureEvent::SpeechStarted);
            }
        } else {
            self.recording.extend_from_slice(frame);
            self.frames += 1;
            if voiced {
                self.voiced_frames += 1;
                self.silent_frames = 0;
            } else {
                self.silent_frames += 1;
            }
            if self.frames >= MAX_FRAMES
                || (self.continuous && self.silent_frames >= SILENCE_FRAMES)
            {
                if let Some(event) = self.finish() {
                    events.push(event);
                }
                if !self.continuous {
                    self.done = true;
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Option<VoiceCaptureEvent> {
        let recording = std::mem::take(&mut self.recording);
        let voiced = self.voiced_frames;
        self.pre_roll.clear();
        self.start_frames = 0;
        self.voiced_frames = 0;
        self.silent_frames = 0;
        self.frames = 0;
        (voiced >= MIN_VOICED_FRAMES).then(|| VoiceCaptureEvent::Utterance(pcm_wav(&recording)))
    }
}

fn pcm_wav(samples: &[i16]) -> Vec<u8> {
    let bytes = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&WAV_RATE.to_le_bytes());
    wav.extend_from_slice(&(WAV_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Owns the output device and queue. Cancellation never affects an agent task.
pub struct CancellablePlayback {
    control: PlaybackControl,
    _output: Option<MixerDeviceSink>,
}

struct PlaybackControl {
    player: Player,
    cancelled: Arc<AtomicBool>,
}

impl PlaybackControl {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.player.stop();
    }

    fn is_finished(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.player.empty()
    }
}

impl Drop for PlaybackControl {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl CancellablePlayback {
    pub fn start(
        wav: Vec<u8>,
        output_device: Option<DeviceId>,
        echo_canceller: EchoCanceller,
    ) -> Result<Self> {
        let source = decode_speech(wav)?;
        let (output, mixer) = open_output_stream(output_device, echo_canceller)?;
        Ok(Self::with_source(source, &mixer, Some(output)))
    }

    /// Prefer the mixer's shared `Audio` output when sound effects also use AEC.
    /// Its owner must keep the output stream alive for the duration of playback.
    pub fn start_on_mixer(wav: Vec<u8>, mixer: Mixer) -> Result<Self> {
        Ok(Self::with_source(decode_speech(wav)?, &mixer, None))
    }

    fn with_source(
        source: Decoder<Cursor<Vec<u8>>>,
        mixer: &Mixer,
        output: Option<MixerDeviceSink>,
    ) -> Self {
        let (player, queue) = Player::new();
        player.append(source);
        let cancelled = Arc::new(AtomicBool::new(false));
        // The outer guard checks each sample, independent of Player's periodic
        // control polling. The mixer may retain at most a resampling frame.
        let queue = rodio::source::UniformSourceIterator::new(
            queue,
            crate::CHANNEL_COUNT,
            crate::SAMPLE_RATE,
        );
        mixer.add(InterruptibleSource {
            inner: queue,
            cancelled: cancelled.clone(),
        });
        Self {
            control: PlaybackControl { player, cancelled },
            _output: output,
        }
    }

    pub fn cancel(&self) {
        self.control.cancel();
    }

    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }
}

struct InterruptibleSource<S> {
    inner: S,
    cancelled: Arc<AtomicBool>,
}

impl<S: Source> Iterator for InterruptibleSource<S> {
    type Item = f32;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cancelled.load(Ordering::Acquire) {
            None
        } else {
            self.inner.next()
        }
    }
}

impl<S: Source> Source for InterruptibleSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

fn decode_speech(wav: Vec<u8>) -> Result<Decoder<Cursor<Vec<u8>>>> {
    anyhow::ensure!(
        wav.len() <= MAX_PLAYBACK_BYTES,
        "Speech audio exceeds the playback size limit"
    );
    Decoder::new(Cursor::new(wav)).context("Could not decode speech audio")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source;

    fn frames(segmenter: &mut Segmenter, count: usize, amplitude: i16) -> Vec<VoiceCaptureEvent> {
        let frame: Vec<i16> = (0..160)
            .map(|n| if n % 2 == 0 { amplitude } else { -amplitude })
            .collect();
        (0..count).flat_map(|_| segmenter.push(&frame)).collect()
    }

    #[test]
    fn silence_and_short_clicks_are_not_utterances() {
        let mut segmenter = Segmenter::new(true);
        assert!(frames(&mut segmenter, 500, 40).is_empty());
        assert!(frames(&mut segmenter, 2, 4_000).is_empty());
        assert!(frames(&mut segmenter, 100, 0).is_empty());
        assert!(segmenter.finish().is_none());
        assert!(segmenter.pre_roll.len() <= PRE_ROLL_FRAMES);
    }

    #[test]
    fn continuous_speech_has_preroll_hangover_and_rearms() {
        let mut segmenter = Segmenter::new(true);
        assert!(frames(&mut segmenter, 50, 0).is_empty());
        let speech = frames(&mut segmenter, 20, 4_000);
        assert_eq!(speech.len(), 1);
        assert!(matches!(speech[0], VoiceCaptureEvent::SpeechStarted));
        assert!(frames(&mut segmenter, SILENCE_FRAMES - 1, 0).is_empty());
        let ended = frames(&mut segmenter, 1, 0);
        let VoiceCaptureEvent::Utterance(wav) = &ended[0] else {
            panic!("Expected utterance")
        };
        // Pre-roll retains 17 silent frames plus the 3 onset frames.
        assert_eq!(wav.len(), 44 + (17 + 20 + SILENCE_FRAMES) * 160 * 2);
        assert!(
            frames(&mut segmenter, 15, 4_000)
                .iter()
                .any(|event| matches!(event, VoiceCaptureEvent::SpeechStarted))
        );
        assert!(matches!(
            segmenter.finish(),
            Some(VoiceCaptureEvent::Utterance(_))
        ));
    }

    #[test]
    fn manual_recording_waits_for_finish_and_preserves_pauses() {
        let mut segmenter = Segmenter::new(false);
        assert!(matches!(
            frames(&mut segmenter, 20, 4_000)[0],
            VoiceCaptureEvent::SpeechStarted
        ));
        assert!(frames(&mut segmenter, SILENCE_FRAMES * 2, 0).is_empty());
        assert!(frames(&mut segmenter, 20, 4_000).is_empty());
        assert!(matches!(
            segmenter.finish(),
            Some(VoiceCaptureEvent::Utterance(_))
        ));
        assert!(segmenter.finish().is_none());
    }

    #[test]
    fn continuous_and_manual_recordings_are_bounded() {
        for continuous in [false, true] {
            let mut segmenter = Segmenter::new(continuous);
            let events = frames(&mut segmenter, MAX_FRAMES * 2 + 20, 4_000);
            let utterances: Vec<_> = events
                .iter()
                .filter_map(|event| {
                    if let VoiceCaptureEvent::Utterance(wav) = event {
                        Some(wav)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(utterances.len(), if continuous { 2 } else { 1 });
            for wav in utterances {
                assert!(wav.len() <= 44 + WAV_RATE as usize * 30 * 2);
            }
            assert!(segmenter.recording.len() <= WAV_RATE as usize * 30);
            assert_eq!(segmenter.done, !continuous);
        }
    }

    #[test]
    fn wav_roundtrips_with_real_decoder() {
        let input = [i16::MIN, -8_000, 0, 8_000, i16::MAX];
        let wav = pcm_wav(&input);
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize,
            wav.len() - 8
        );
        let decoder = Decoder::new(Cursor::new(wav)).unwrap();
        assert_eq!(decoder.channels().get(), 1);
        assert_eq!(decoder.sample_rate().get(), WAV_RATE);
        let decoded: Vec<_> = decoder.collect();
        assert_eq!(decoded.len(), input.len());
        for (actual, expected) in decoded.iter().zip(input) {
            assert!((*actual - f32::from(expected) / 32768.0).abs() < 0.0001);
        }
    }

    #[test]
    fn resampling_is_chunk_independent_and_has_no_44100_drift() {
        for rate in [8_000, 16_000, 44_100, 48_000, 96_000, 192_000] {
            let input: Vec<_> = (0..rate).map(|n| (n as f32 * 0.01).sin() * 0.1).collect();
            let mut all = Vec::new();
            Resampler::new(rate).push(&input, |sample| all.push(sample));
            let mut split = Vec::new();
            let mut resampler = Resampler::new(rate);
            for chunk in input.chunks(137) {
                resampler.push(chunk, |sample| split.push(sample));
            }
            assert_eq!(all.len(), 48_000, "Input sample rate {rate}");
            assert_eq!(all, split);
            assert!(
                all.iter()
                    .all(|sample| sample.is_finite() && sample.abs() <= 0.101)
            );
        }
    }

    #[test]
    fn dropping_capture_joins_without_waiting_for_microphone_data() {
        let (tx, events) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = joined.clone();
        let worker = thread::spawn(move || {
            while !worker_cancel.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_millis(1));
            }
            worker_joined.store(true, Ordering::Release);
            drop(tx);
        });
        let capture = VoiceCapture {
            events,
            pending_events: Arc::new(AtomicUsize::new(0)),
            finish: Arc::new(AtomicBool::new(false)),
            cancel,
            worker: Some(worker),
        };
        drop(capture);
        assert!(joined.load(Ordering::Acquire));
    }

    #[test]
    fn invalid_playback_fails_before_opening_output_device() {
        assert!(CancellablePlayback::start(vec![1, 2, 3], None, EchoCanceller::default()).is_err());
    }
    #[test]
    fn playback_cancellation_and_drop_stop_the_owned_source() {
        for drop_control in [false, true] {
            let (mixer, mut source) = rodio::mixer::mixer(rodio::nz!(1), rodio::nz!(48_000));
            // Match the shared output stream's permanent silence source. Without
            // it, dropping playback can end the mixer before a silent tail.
            mixer.add(rodio::source::Zero::new(rodio::nz!(1), rodio::nz!(48_000)));
            let control =
                CancellablePlayback::start_on_mixer(pcm_wav(&[1_000; WAV_RATE as usize]), mixer)
                    .unwrap();
            assert!(!control.is_finished());
            assert!(source.by_ref().take(100).any(|sample| sample != 0.0));
            if drop_control {
                drop(control);
            } else {
                control.cancel();
                assert!(control.is_finished());
            }
            let tail: Vec<_> = source.take(480).collect();
            assert_eq!(tail.len(), 480);
            assert!(
                tail.iter().skip(48).all(|sample| *sample == 0.0),
                "Stopped source must be silent within 1ms, including resampler lookahead"
            );
        }
    }

    #[test]
    fn interruption_does_not_wait_for_low_rate_wav_resampling() {
        let mut wav = pcm_wav(&[1_000; 2]);
        wav[24..28].copy_from_slice(&1u32.to_le_bytes());
        wav[28..32].copy_from_slice(&2u32.to_le_bytes());
        let (mixer, mut source) = rodio::mixer::mixer(crate::CHANNEL_COUNT, crate::SAMPLE_RATE);
        mixer.add(rodio::source::Zero::new(
            crate::CHANNEL_COUNT,
            crate::SAMPLE_RATE,
        ));
        let playback = CancellablePlayback::start_on_mixer(wav, mixer).unwrap();
        assert!(source.by_ref().take(100).any(|sample| sample != 0.0));
        playback.cancel();
        assert!(source.take(96).skip(2).all(|sample| sample == 0.0));
    }

    #[test]
    fn playback_guard_stops_before_reading_another_sample() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut source = InterruptibleSource {
            inner: rodio::source::SineWave::new(440.0),
            cancelled: cancelled.clone(),
        };
        assert!(source.next().is_some());
        cancelled.store(true, Ordering::Release);
        assert!(source.next().is_none());
        assert!(source.next().is_none());
    }

    #[test]
    fn playback_reports_natural_completion() {
        let (mixer, source) = rodio::mixer::mixer(rodio::nz!(1), rodio::nz!(16_000));
        let control = CancellablePlayback::start_on_mixer(pcm_wav(&[1_000; 160]), mixer).unwrap();
        assert!(!control.is_finished());
        let tail: Vec<_> = source.take(2_000).collect();
        assert!(tail.iter().rev().take(160).all(|sample| *sample == 0.0));
        assert!(control.is_finished());
    }

    #[test]
    fn overloaded_event_queue_still_delivers_error_and_stopped() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let events = EventSender {
            sender,
            pending: Arc::new(AtomicUsize::new(0)),
        };
        for _ in 0..6 {
            send_event(&events, VoiceCaptureEvent::SpeechStarted).unwrap();
        }
        assert!(send_event(&events, VoiceCaptureEvent::SpeechStarted).is_err());
        events.terminal(VoiceCaptureEvent::Error("Slow consumer".into()));
        events.terminal(VoiceCaptureEvent::Stopped);
        let queued: Vec<_> = receiver.try_iter().collect();
        assert_eq!(queued.len(), 8);
        assert!(matches!(queued[6], VoiceCaptureEvent::Error(_)));
        assert!(matches!(queued[7], VoiceCaptureEvent::Stopped));
    }
}
