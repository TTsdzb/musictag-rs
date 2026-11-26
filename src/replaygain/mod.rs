use std::collections::HashMap;
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use ebur128::{EbuR128, Mode};
use rayon::prelude::*;
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::core::models::{ReplayGainData, RgScanMode};
use crate::core::traits::{ReplayGainScanner, ScanProgressCallback};

/// EBU R128 loudness and sample-peak scanner backed by `symphonia` and `ebur128`.
#[derive(Debug, Clone)]
pub struct EbuR128Scanner {
    cancel_flag: Arc<AtomicBool>,
}

impl Default for EbuR128Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl EbuR128Scanner {
    pub fn new() -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_cancel_flag(cancel_flag: Arc<AtomicBool>) -> Self {
        Self { cancel_flag }
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel_flag)
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }
}

impl ReplayGainScanner for EbuR128Scanner {
    fn scan_tracks(
        &self,
        paths: &[PathBuf],
        mode: RgScanMode,
        target_lufs: f64,
        clip_prevent: bool,
    ) -> Result<Vec<(PathBuf, ReplayGainData)>> {
        self.scan_tracks_with_progress(paths, mode, target_lufs, clip_prevent, None)
    }

    fn scan_tracks_with_progress(
        &self,
        paths: &[PathBuf],
        mode: RgScanMode,
        target_lufs: f64,
        clip_prevent: bool,
        on_progress: Option<ScanProgressCallback<'_>>,
    ) -> Result<Vec<(PathBuf, ReplayGainData)>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        if self.cancel_flag.load(Ordering::Relaxed) {
            bail!("ReplayGain scan cancelled");
        }

        let total = paths.len();
        let completed = AtomicUsize::new(0);

        let mut analyzed: Vec<(usize, PathBuf, EbuR128, f64)> = paths
            .par_iter()
            .enumerate()
            .map(|(idx, path)| {
                if self.cancel_flag.load(Ordering::Relaxed) {
                    bail!("ReplayGain scan cancelled");
                }
                let (ebu, peak) = analyze_file_ebur128(path, &self.cancel_flag)?;
                let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                if let Some(cb) = on_progress {
                    cb(done, total, path);
                }
                Ok((idx, path.clone(), ebu, peak))
            })
            .collect::<Result<Vec<_>>>()?;

        if self.cancel_flag.load(Ordering::Relaxed) {
            bail!("ReplayGain scan cancelled");
        }

        analyzed.sort_by_key(|(idx, _, _, _)| *idx);

        let album_map = if mode == RgScanMode::AlbumAndTrack {
            compute_album_gains(&analyzed, target_lufs, clip_prevent)
        } else {
            HashMap::new()
        };

        let mut output = Vec::with_capacity(analyzed.len());
        for (idx, path, ebu, peak) in analyzed {
            let loudness = extract_track_loudness(&ebu);
            let gain_db = compute_gain_db(loudness, target_lufs, peak, clip_prevent);

            let (album_gain, album_peak) = if let Some((ag, ap)) = album_map.get(&idx) {
                (Some(ag.clone()), Some(ap.clone()))
            } else {
                (None, None)
            };

            output.push((
                path,
                ReplayGainData {
                    track_gain: Some(format_gain_db(gain_db)),
                    track_peak: Some(format!("{peak:.6}")),
                    album_gain,
                    album_peak,
                },
            ));
        }

        Ok(output)
    }
}

fn compute_album_gains(
    analyzed: &[(usize, PathBuf, EbuR128, f64)],
    target_lufs: f64,
    clip_prevent: bool,
) -> HashMap<usize, (String, String)> {
    let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (pos, (_, path, _, _)) in analyzed.iter().enumerate() {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        by_dir.entry(dir).or_default().push(pos);
    }

    let mut result = HashMap::with_capacity(analyzed.len());
    for group_positions in by_dir.into_values() {
        let album_peak = group_positions
            .iter()
            .map(|&pos| analyzed[pos].3)
            .fold(0.0_f64, f64::max);

        let mut album_loudness =
            EbuR128::loudness_global_multiple(group_positions.iter().map(|&pos| &analyzed[pos].2))
                .unwrap_or(-f64::INFINITY);

        if !album_loudness.is_finite() {
            let finite_tracks: Vec<f64> = group_positions
                .iter()
                .map(|&pos| extract_track_loudness(&analyzed[pos].2))
                .filter(|v| v.is_finite())
                .collect();
            if !finite_tracks.is_empty() {
                album_loudness =
                    finite_tracks.iter().sum::<f64>() / finite_tracks.len() as f64;
            }
        }

        let album_gain_db =
            compute_gain_db(album_loudness, target_lufs, album_peak, clip_prevent);
        let gain_str = format_gain_db(album_gain_db);
        let peak_str = format!("{album_peak:.6}");

        for pos in group_positions {
            let orig_idx = analyzed[pos].0;
            result.insert(orig_idx, (gain_str.clone(), peak_str.clone()));
        }
    }

    result
}

fn analyze_file_ebur128(path: &Path, cancel_flag: &AtomicBool) -> Result<(EbuR128, f64)> {
    let file =
        File::open(path).with_context(|| format!("failed to open audio file {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("failed to probe audio container {}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| format.default_track())
        .with_context(|| format!("no decodable audio stream in {}", path.display()))?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .with_context(|| format!("unsupported audio codec in {}", path.display()))?;

    let mut ebur: Option<EbuR128> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut last_spec: Option<SignalSpec> = None;

    loop {
        if cancel_flag.load(Ordering::Relaxed) {
            bail!("ReplayGain scan cancelled");
        }

        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(ref err)) if err.kind() == ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("demuxing failed while scanning {}", path.display()));
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(ref err)) if err.kind() == ErrorKind::UnexpectedEof => {
                break;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("decoding failed while scanning {}", path.display()));
            }
        };

        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }

        let spec = *decoded.spec();
        let channels = spec.channels.count() as u32;
        let rate = spec.rate;
        if channels == 0 || rate == 0 {
            continue;
        }

        if let Some(ref mut analyzer) = ebur {
            if analyzer.channels() != channels || analyzer.rate() != rate {
                analyzer
                    .change_parameters(channels, rate)
                    .map_err(|err| anyhow::anyhow!("ebur128 parameter change failed: {err}"))?;
            }
        } else {
            ebur = Some(
                EbuR128::new(channels, rate, Mode::I | Mode::SAMPLE_PEAK)
                    .map_err(|err| anyhow::anyhow!("ebur128 init failed: {err}"))?,
            );
        }

        let needed_samples = frames * channels as usize;
        let recreate_buf = match &sample_buf {
            Some(sb) => sb.capacity() < needed_samples || last_spec != Some(spec),
            None => true,
        };
        if recreate_buf {
            sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
            last_spec = Some(spec);
        }

        let sb = sample_buf.as_mut().expect("sample buffer initialized");
        sb.copy_interleaved_ref(decoded);
        ebur.as_mut()
            .expect("ebur128 initialized")
            .add_frames_f32(sb.samples())
            .map_err(|err| anyhow::anyhow!("ebur128 add_frames failed: {err}"))?;
    }

    let analyzer = ebur
        .with_context(|| format!("no audio frames decoded from {}", path.display()))?;

    let mut max_peak = 0.0_f64;
    for ch in 0..analyzer.channels() {
        let ch_peak = analyzer
            .sample_peak(ch)
            .map_err(|err| anyhow::anyhow!("ebur128 sample_peak failed: {err}"))?;
        if ch_peak > max_peak {
            max_peak = ch_peak;
        }
    }

    Ok((analyzer, max_peak))
}

fn extract_track_loudness(ebu: &EbuR128) -> f64 {
    if let Ok(global) = ebu.loudness_global() {
        if global.is_finite() {
            return global;
        }
    }
    if let Ok(momentary) = ebu.loudness_momentary() {
        if momentary.is_finite() {
            return momentary;
        }
    }
    -f64::INFINITY
}

fn compute_gain_db(loudness_lufs: f64, target_lufs: f64, peak: f64, clip_prevent: bool) -> f64 {
    if !loudness_lufs.is_finite() {
        return 0.0;
    }
    let mut gain = target_lufs - loudness_lufs;
    if clip_prevent && peak > 0.0 {
        let max_gain = -20.0 * peak.log10();
        if gain > max_gain {
            gain = max_gain;
        }
    }
    if gain.abs() < 0.005 { 0.0 } else { gain }
}

fn format_gain_db(gain_db: f64) -> String {
    format!("{gain_db:+.2} dB")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn write_sine_wav(
        path: &Path,
        sample_rate: u32,
        duration_secs: f32,
        freq_hz: f32,
        amplitude: f32,
    ) {
        let channels: u16 = 2;
        let bits_per_sample: u16 = 16;
        let total_frames = (sample_rate as f32 * duration_secs) as usize;
        let data_bytes = (total_frames * usize::from(channels) * 2) as u32;
        let riff_size = 36 + data_bytes;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        let block_align = channels * (bits_per_sample / 8);

        let mut buf = Vec::with_capacity(44 + data_bytes as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&riff_size.to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());

        let step = 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
        for i in 0..total_frames {
            let sample = ((i as f32 * step).sin() * amplitude * f32::from(i16::MAX))
                .round()
                .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            let bytes = sample.to_le_bytes();
            buf.extend_from_slice(&bytes);
            buf.extend_from_slice(&bytes);
        }

        std::fs::write(path, buf).unwrap();
    }

    #[test]
    fn scans_wav_track_and_album_loudness_and_peak() {
        let dir = tempdir().unwrap();
        let album_a = dir.path().join("album_a");
        let album_b = dir.path().join("album_b");
        std::fs::create_dir_all(&album_a).unwrap();
        std::fs::create_dir_all(&album_b).unwrap();

        let loud_wav = album_a.join("01_loud.wav");
        let quiet_wav = album_a.join("02_quiet.wav");
        let other_wav = album_b.join("01_other.wav");

        // Half-scale 440 Hz sine (~ -6.67 LUFS, peak ~ 0.5) & quarter-scale (~ -12.69 LUFS, peak ~ 0.25)
        write_sine_wav(&loud_wav, 44100, 1.5, 440.0, 0.5);
        write_sine_wav(&quiet_wav, 44100, 1.5, 440.0, 0.25);
        write_sine_wav(&other_wav, 44100, 1.5, 440.0, 0.125);

        let scanner = EbuR128Scanner::new();
        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let progress_ref = Arc::clone(&progress_events);
        let cb = move |done: usize, total: usize, p: &Path| {
            progress_ref
                .lock()
                .unwrap()
                .push((done, total, p.to_path_buf()));
        };

        let paths = vec![loud_wav.clone(), quiet_wav.clone(), other_wav.clone()];
        let results = scanner
            .scan_tracks_with_progress(&paths, RgScanMode::AlbumAndTrack, -18.0, true, Some(&cb))
            .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(progress_events.lock().unwrap().len(), 3);

        // Verify order preserved
        assert_eq!(results[0].0, loud_wav);
        assert_eq!(results[1].0, quiet_wav);
        assert_eq!(results[2].0, other_wav);

        // Check expected gain/peak values
        let rg_loud = &results[0].1;
        let rg_quiet = &results[1].1;
        let rg_other = &results[2].1;

        let parse_db = |s: &Option<String>| -> f64 {
            s.as_deref()
                .unwrap()
                .trim_end_matches(" dB")
                .parse::<f64>()
                .unwrap()
        };
        let parse_peak = |s: &Option<String>| -> f64 { s.as_deref().unwrap().parse::<f64>().unwrap() };

        let loud_gain = parse_db(&rg_loud.track_gain);
        let quiet_gain = parse_db(&rg_quiet.track_gain);
        let other_gain = parse_db(&rg_other.track_gain);

        // Half amplitude vs quarter amplitude is ~6 dB difference; quarter vs eighth is ~6 dB.
        assert!((loud_gain - (-11.33)).abs() < 0.5, "loud_gain={loud_gain}");
        assert!((quiet_gain - (-5.31)).abs() < 0.5, "quiet_gain={quiet_gain}");
        assert!((other_gain - 0.71).abs() < 0.5, "other_gain={other_gain}");
        assert!(rg_other.track_gain.as_deref().unwrap().starts_with('+'));

        assert!((parse_peak(&rg_loud.track_peak) - 0.5).abs() < 0.002);
        assert!((parse_peak(&rg_quiet.track_peak) - 0.25).abs() < 0.002);
        assert!((parse_peak(&rg_other.track_peak) - 0.125).abs() < 0.002);

        // Album A tracks share identical album gain & max album peak (~0.5), distinct from Album B
        assert_eq!(rg_loud.album_gain, rg_quiet.album_gain);
        assert_eq!(rg_loud.album_peak, rg_quiet.album_peak);
        assert!((parse_peak(&rg_loud.album_peak) - 0.5).abs() < 0.002);
        assert_ne!(rg_loud.album_gain, rg_other.album_gain);
        assert_eq!(rg_other.album_gain, rg_other.track_gain);
    }

    #[test]
    fn clip_prevent_and_cancel_flag_work() {
        let dir = tempdir().unwrap();
        let wav_path = dir.path().join("near_full.wav");
        // Peak ~ 0.90 (-0.915 dBFS -> max positive gain without clipping is +0.915 dB)
        // With target_lufs = +10.0, raw gain would be ~ +11 dB, clipped down to ~ +0.92 dB!
        write_sine_wav(&wav_path, 44100, 1.2, 440.0, 0.9);

        let scanner = EbuR128Scanner::new();
        let unclipped = scanner
            .scan_tracks(&[wav_path.clone()], RgScanMode::TrackOnly, 10.0, false)
            .unwrap();
        assert!(unclipped[0].1.album_gain.is_none());
        let raw_gain: f64 = unclipped[0]
            .1
            .track_gain
            .as_deref()
            .unwrap()
            .trim_end_matches(" dB")
            .parse()
            .unwrap();
        assert!(raw_gain > 5.0);

        let clipped = scanner
            .scan_tracks(&[wav_path.clone()], RgScanMode::TrackOnly, 10.0, true)
            .unwrap();
        let clamped_gain: f64 = clipped[0]
            .1
            .track_gain
            .as_deref()
            .unwrap()
            .trim_end_matches(" dB")
            .parse()
            .unwrap();
        assert!(
            (clamped_gain - 0.92).abs() < 0.1,
            "expected clamped gain ~ +0.92 dB, got {clamped_gain}"
        );

        // Cancel flag aborts scan cleanly
        let cancel = Arc::new(AtomicBool::new(true));
        let cancelled_scanner = EbuR128Scanner::with_cancel_flag(cancel);
        assert!(
            cancelled_scanner
                .scan_tracks(&[wav_path], RgScanMode::TrackOnly, -18.0, true)
                .is_err()
        );
    }
}
