use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use super::SAMPLE_RATE;

/// Пишет моно 16 кГц в 16-битный WAV. Только для проверки тракта на шаге 2 —
/// в обычной работе на диск ничего не попадает.
pub fn dump(dir: &Path, samples: &[f32]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{stamp}.wav"));

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec)?;
    for &s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    w.finalize()?;

    Ok(path)
}
