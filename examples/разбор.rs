//! Переразбор записанной сессии текущими настройками.
//!
//! Записи ведутся ради того, чтобы улучшать распознавание и нарезку не на
//! следующем живом разговоре, а на сотне уже случившихся. Этот инструмент
//! берёт `sessions/<id>/audio.wav`, прогоняет каждый канал через тот же
//! сегментатор и тот же whisper, что и вживую, — но с настройками из
//! `config.toml` прямо сейчас, — и показывает, что получилось бы, будь они
//! такими во время разговора.
//!
//!   cargo run --release --example разбор -- sessions/<id>
//!
//! Запись стерео: собеседник слева, вы справа (см. `session::spawn_audio`).
//! Старые записи моно — тогда разбираем одной дорожкой и говорим об этом.

use std::path::{Path, PathBuf};
use std::time::Instant;

use ghost::audio::vad::segment_offline;
use ghost::audio::SAMPLE_RATE;
use ghost::config::Config;
use ghost::session;
use ghost::stt::{local::Whisper, Transcriber};

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).map(PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!("аргумент 1: каталог сессии, например sessions/1788560260")
    })?;

    let cfg = Config::load(Path::new(ghost::config::PATH));
    let wav = dir.join("audio.wav");
    let (theirs, mine, channels) = read_channels(&wav)?;

    eprintln!(
        "запись {}: {:.1} с, {} канал{}",
        dir.display(),
        theirs.len() as f32 / SAMPLE_RATE as f32,
        channels,
        if channels == 1 { "" } else { "а" }
    );
    if channels == 1 {
        eprintln!("моно (старая запись): разбираю одной дорожкой, роли не разделяю");
    }

    let mut whisper = Whisper::load(
        Path::new(&cfg.stt.model_path),
        &cfg.stt.language,
        num_threads(),
    )?;
    whisper.set_hints(cfg.glossary(), cfg.stt.clone());

    let started = Instant::now();
    let mut lines = 0;
    let mut doubtful = 0;
    let sides: &[(&str, &[f32])] = if channels == 1 {
        &[("запись", &theirs)]
    } else {
        &[("собеседник", &theirs), ("я", &mine)]
    };

    for (who, pcm) in sides {
        let utterances = segment_offline(pcm, &cfg.vad);
        eprintln!("\n{who}: реплик {}", utterances.len());
        for utt in &utterances {
            let text = whisper.transcribe(utt)?.text;
            if text.trim().is_empty() {
                continue;
            }
            lines += 1;
            doubtful += text.matches('\u{2039}').count();
            println!("{who}: {text}");
        }
    }

    eprintln!(
        "\nитог: строк {lines}, слов под сомнением {doubtful}, за {:.1} с",
        started.elapsed().as_secs_f32()
    );

    // С чем сравнивать: что распозналось тогда, во время разговора.
    let then = session::read_log(&dir)
        .into_iter()
        .filter(|(kind, _, _)| kind == "said")
        .count();
    if then > 0 {
        eprintln!("во время разговора было строк: {then} (сейчас {lines})");
    }
    Ok(())
}

/// Читает запись, разводя каналы. Стерео — два, моно — один продублирован
/// в оба возвращаемых вектора, чтобы вызывающему не разбирать этот случай.
fn read_channels(wav: &Path) -> anyhow::Result<(Vec<f32>, Vec<f32>, u16)> {
    let mut reader = hound::WavReader::open(wav)
        .map_err(|e| anyhow::anyhow!("{} не открылся: {e}", wav.display()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().filter_map(Result::ok).map(|s| s as f32 * scale).collect()
        }
        hound::SampleFormat::Float => {
            reader.samples::<f32>().filter_map(Result::ok).collect()
        }
    };

    if spec.channels == 2 {
        let mut theirs = Vec::with_capacity(samples.len() / 2);
        let mut mine = Vec::with_capacity(samples.len() / 2);
        for pair in samples.chunks_exact(2) {
            theirs.push(pair[0]);
            mine.push(pair[1]);
        }
        Ok((theirs, mine, 2))
    } else {
        Ok((samples.clone(), samples, 1))
    }
}

/// То же правило, что и в рабочем распознавании: около числа P-ядер.
fn num_threads() -> usize {
    std::thread::available_parallelism().map(|n| (n.get() / 2).clamp(4, 8)).unwrap_or(4)
}
