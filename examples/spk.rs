//! Проверка, что эмбеддинги вообще различают голоса.
//!
//! Считает матрицу близостей по размеченным записям. Смысл прост: пары одного
//! говорящего должны стоять заметно выше пар разных. Если разрыва нет —
//! признаки разошлись с эталоном и различать голоса нечем.
//!
//! cargo run --release --example spk -- <модель.onnx> <файл.wav>...

use ghost::speaker::{similarity, Embedder};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let model = args.next().expect("аргумент 1: модель .onnx");
    let files: Vec<String> = args.collect();
    assert!(files.len() >= 2, "нужно хотя бы два файла");

    let mut embedder = Embedder::load(std::path::Path::new(&model))?;
    let (inputs, outputs) = embedder.names();
    println!("входы: {inputs:?}  выходы: {outputs:?}\n");

    let mut names = Vec::new();
    let mut vectors = Vec::new();
    for path in &files {
        let pcm = read_wav(path)?;
        let t = std::time::Instant::now();
        let vector = embedder.embed(&pcm)?;
        println!(
            "{:10} {:5.1} с · {} чисел · {} мс",
            short(path),
            pcm.len() as f32 / 16_000.0,
            vector.len(),
            t.elapsed().as_millis()
        );
        names.push(short(path));
        vectors.push(vector);
    }

    println!("\nблизость:");
    print!("{:>10}", "");
    for n in &names {
        print!("{n:>10}");
    }
    println!();
    for (i, a) in vectors.iter().enumerate() {
        print!("{:>10}", names[i]);
        for b in vectors.iter() {
            print!("{:>10.3}", similarity(a, b));
        }
        println!();
    }

    // Считаем разрыв: он и решает, годится ли всё это в работу.
    let (mut same, mut other) = (Vec::new(), Vec::new());
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let speaker = |n: &str| n.split('_').next().unwrap_or("").to_string();
            let s = similarity(&vectors[i], &vectors[j]);
            if speaker(&names[i]) == speaker(&names[j]) {
                same.push(s);
            } else {
                other.push(s);
            }
        }
    }
    let avg = |v: &Vec<f32>| if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 };
    let worst_same = same.iter().cloned().fold(f32::MAX, f32::min);
    let best_other = other.iter().cloned().fold(f32::MIN, f32::max);
    println!(
        "\nодин голос: среднее {:.3}, худшая пара {:.3}\nразные:     среднее {:.3}, лучшая пара {:.3}\nразрыв: {:.3}",
        avg(&same),
        worst_same,
        avg(&other),
        best_other,
        worst_same - best_other
    );

    Ok(())
}

fn short(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_wav(path: &str) -> anyhow::Result<Vec<f32>> {
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    anyhow::ensure!(spec.sample_rate == 16_000, "нужен 16 кГц, а не {}", spec.sample_rate);
    Ok(reader
        .into_samples::<i16>()
        .filter_map(Result::ok)
        .map(|s| s as f32 / 32768.0)
        .collect())
}
