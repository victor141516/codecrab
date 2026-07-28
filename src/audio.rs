use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::{
    SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

pub(crate) struct AudioRecording {
    stream: Stream,
    samples: Arc<Mutex<Vec<i16>>>,
    error: Arc<Mutex<Option<String>>>,
    sample_rate: u32,
    channels: u16,
}

impl AudioRecording {
    pub(crate) fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no microphone input device is available")?;
        let supported = device
            .default_input_config()
            .context("cannot read the default microphone format")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));
        let stream = match sample_format {
            SampleFormat::F32 => {
                build_stream::<f32>(&device, &config, &samples, &error, |value| {
                    (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                })?
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, &samples, &error, |value| value)?
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, &samples, &error, |value| {
                    (value as i32 - 32_768) as i16
                })?
            }
            other => anyhow::bail!("unsupported microphone sample format: {other:?}"),
        };
        stream.play().context("cannot start microphone capture")?;
        Ok(Self {
            stream,
            samples,
            error,
            sample_rate: config.sample_rate.0,
            channels: config.channels,
        })
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>> {
        self.stream
            .pause()
            .context("cannot stop microphone capture")?;
        if let Some(error) = self
            .error
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone error state is unavailable"))?
            .take()
        {
            anyhow::bail!("microphone capture failed: {error}");
        }
        let samples = self
            .samples
            .lock()
            .map_err(|_| anyhow::anyhow!("recorded audio is unavailable"))?;
        if samples.is_empty() {
            anyhow::bail!("the microphone did not capture any audio");
        }
        pcm16_wav(&samples, self.sample_rate, self.channels)
    }

    pub(crate) fn waveform(&self, points: usize) -> Vec<u8> {
        let Ok(samples) = self.samples.lock() else {
            return vec![0; points];
        };
        waveform_levels(&samples, self.sample_rate, self.channels, points)
    }
}

fn waveform_levels(samples: &[i16], sample_rate: u32, channels: u16, points: usize) -> Vec<u8> {
    if points == 0 {
        return Vec::new();
    }
    let window_samples = usize::try_from(sample_rate)
        .unwrap_or(usize::MAX)
        .saturating_mul(usize::from(channels))
        .saturating_mul(2);
    let samples_per_point = window_samples.saturating_div(points).max(1);
    let available_points = samples.len().div_ceil(samples_per_point).min(points);
    let visible_samples = available_points.saturating_mul(samples_per_point);
    let start = samples.len().saturating_sub(visible_samples);
    let mut levels = vec![0; points.saturating_sub(available_points)];
    levels.extend(
        samples[start..]
            .chunks(samples_per_point)
            .take(available_points)
            .map(|chunk| {
                let peak = chunk
                    .iter()
                    .map(|sample| sample.unsigned_abs())
                    .max()
                    .unwrap_or(0);
                amplitude_level(peak)
            }),
    );
    levels.resize(points, 0);
    levels
}

fn amplitude_level(peak: u16) -> u8 {
    if peak < 128 {
        return 0;
    }
    let normalized = f64::from(peak) / 32_768.0;
    (normalized.sqrt() * 7.0).round().clamp(1.0, 7.0) as u8
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    samples: &Arc<Mutex<Vec<i16>>>,
    error: &Arc<Mutex<Option<String>>>,
    convert: impl Fn(T) -> i16 + Send + Sync + Copy + 'static,
) -> Result<Stream>
where
    T: cpal::SizedSample,
{
    let output = Arc::clone(samples);
    let stream_error = Arc::clone(error);
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if let Ok(mut output) = output.lock() {
                    output.extend(data.iter().copied().map(convert));
                }
            },
            move |capture_error| {
                if let Ok(mut error) = stream_error.lock() {
                    *error = Some(capture_error.to_string());
                }
            },
            None,
        )
        .context("cannot open the microphone input stream")
}

fn pcm16_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|length| u32::try_from(length).ok())
        .context("recording is too large")?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|rate| rate.checked_mul(2))
        .context("invalid microphone format")?;
    let block_align = channels
        .checked_mul(2)
        .context("invalid microphone channel count")?;
    let riff_len = 36_u32
        .checked_add(data_len)
        .context("recording is too large")?;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_samples_are_wrapped_in_a_valid_wav_container() {
        let wav = pcm16_wav(&[0, i16::MAX, i16::MIN], 48_000, 1).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
    }

    #[test]
    fn waveform_is_flat_without_audio_and_tracks_recent_peaks() {
        assert_eq!(waveform_levels(&[], 10, 1, 5), [0, 0, 0, 0, 0]);

        let levels = waveform_levels(&[0, 0, 8_192, 8_192, i16::MAX], 5, 1, 5);

        assert_eq!(levels[..2], [0, 0]);
        assert_eq!(levels[2], 0);
        assert!(levels[3] > 0);
        assert_eq!(levels[4], 7);
    }
}
