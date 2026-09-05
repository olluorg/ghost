//! Замер абсолютного времени распознавания.
//!
//! whisper всегда кодирует окно в 30 секунд, поэтому стоимость почти не зависит
//! от длины реплики — мерить надо секунды, а не отношение к длительности аудио.
//!
//! cargo run --release --example bench -- <модель.bin> <файл.wav>

use std::time::Instant;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let model = args.next().expect("аргумент 1: путь к модели");
    let wav = args.next().expect("аргумент 2: путь к wav");

    let reader = hound::WavReader::open(&wav).expect("wav не открылся");
    let spec = reader.spec();
    let ints: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
    let mut pcm = vec![0.0f32; ints.len()];
    whisper_rs::convert_integer_to_float_audio(&ints, &mut pcm).unwrap();
    let audio_secs = pcm.len() as f32 / spec.sample_rate as f32;

    let threads: i32 = std::env::var("BENCH_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    let t0 = Instant::now();
    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
        .expect("модель не загрузилась");
    let mut state = ctx.create_state().unwrap();
    let load = t0.elapsed().as_secs_f32();

    println!("\nмодель {model}");
    println!("аудио {audio_secs:.2} с · загрузка {load:.2} с · потоков {threads}");

    for run in 1..=3 {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("ru"));
        params.set_n_threads(threads);
        params.set_translate(false);
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let t = Instant::now();
        state.full(params, &pcm).expect("full");
        let took = t.elapsed().as_secs_f32();

        let mut text = String::new();
        for seg in state.as_iter() {
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
        }
        println!("  прогон {run}: {took:.2} с  |{}", text.trim());
    }
}
