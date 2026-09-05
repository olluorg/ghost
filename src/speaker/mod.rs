//! Различение голосов внутри одного канала.
//!
//! Собеседников в звонке может быть несколько, а канал у них один. Разделить
//! их по каналу нельзя — только по голосу. Для каждой реплики считается
//! эмбеддинг (вектор, в котором близость означает «тот же человек»), и реплика
//! приписывается ближайшему уже известному голосу либо заводит новый.
//!
//! Работает по репликам целиком, а не покадрово: если двое говорят внутри
//! одной реплики, метка будет одна. Детектор режет по паузе, а смена
//! говорящего обычно паузой и сопровождается, поэтому на практике совпадает.

pub mod fbank;

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::Tensor;

use fbank::{Fbank, MEL_BINS};

pub struct Embedder {
    session: Session,
    fbank: Fbank,
    input: String,
}

impl Embedder {
    pub fn load(model: &std::path::Path) -> Result<Self> {
        let session = Session::builder()
            .context("сборка сессии ONNX")?
            .commit_from_file(model)
            .with_context(|| format!("не загрузилась модель {}", model.display()))?;

        // Имя входа у моделей разное, а ошибаться нельзя: спрашиваем у модели.
        let input = session
            .inputs()
            .first()
            .context("у модели нет входов")?
            .name()
            .to_string();

        Ok(Self { session, fbank: Fbank::new(), input })
    }

    /// Имена входов и выходов — нужны замеру, чтобы не гадать про модель.
    #[allow(dead_code)]
    pub fn names(&self) -> (Vec<String>, Vec<String>) {
        (
            self.session.inputs().iter().map(|i| i.name().to_string()).collect(),
            self.session.outputs().iter().map(|o| o.name().to_string()).collect(),
        )
    }

    /// Эмбеддинг реплики. Вектор нормирован, поэтому близость — просто
    /// скалярное произведение.
    pub fn embed(&mut self, pcm16k: &[f32]) -> Result<Vec<f32>> {
        let frames = Fbank::frames(pcm16k.len());
        // Меньше девяти кадров модель не принимает, да и судить по такому
        // огрызку не о чем.
        if frames < 9 {
            anyhow::bail!("слишком короткая реплика: {frames} кадров");
        }

        let feats = self.fbank.compute(pcm16k);
        let tensor = Tensor::from_array(([1, frames, MEL_BINS], feats))
            .context("тензор признаков")?;

        let outputs = self
            .session
            .run(ort::inputs![self.input.as_str() => tensor])
            .context("прогон модели")?;
        let (_, data) = outputs[0].try_extract_tensor::<f32>().context("чтение выхода")?;

        Ok(normalize(data.to_vec()))
    }
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

/// Косинусная близость нормированных векторов.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Известный голос: центр облака его эмбеддингов.
struct Voice {
    centroid: Vec<f32>,
    count: u32,
    label: String,
}

/// Онлайновая кластеризация голосов.
///
/// Реплика приписывается ближайшему известному голосу, если близость выше
/// порога, иначе заводит новый. Порог подбирается по разрыву между «тот же
/// человек» и «другой»: на синтетических голосах он был 0.92 против 0.50.
pub struct Registry {
    threshold: f32,
    max: usize,
    voices: Vec<Voice>,
    /// Голос владельца, набранный с микрофона. Канал собеседника иногда несёт
    /// его же эхо из колонок — так его видно.
    own: Option<Vec<f32>>,
}

impl Registry {
    pub fn new(threshold: f32, max: usize) -> Self {
        Self { threshold, max, voices: Vec::new(), own: None }
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    #[allow(dead_code)]
    pub fn forget(&mut self) {
        self.voices.clear();
    }

    #[allow(dead_code)]
    pub fn known(&self) -> usize {
        self.voices.len()
    }

    /// Запоминает голос владельца. Отдельного знакомства не требуется: всё,
    /// что приходит с микрофона, по определению его.
    pub fn learn_own(&mut self, embedding: &[f32]) {
        self.own = Some(match self.own.take() {
            Some(prev) => normalize(prev.iter().zip(embedding).map(|(a, b)| a * 0.8 + b * 0.2).collect()),
            None => embedding.to_vec(),
        });
    }

    /// Метка говорящего для реплики из канала собеседника.
    pub fn label(&mut self, embedding: &[f32]) -> String {
        if let Some(own) = &self.own {
            if similarity(own, embedding) >= self.threshold {
                return "ваш голос".into();
            }
        }

        let best = self
            .voices
            .iter()
            .enumerate()
            .map(|(i, v)| (i, similarity(&v.centroid, embedding)))
            .max_by(|a, b| a.1.total_cmp(&b.1));

        if let Some((i, score)) = best {
            if score >= self.threshold {
                let voice = &mut self.voices[i];
                // Скользящее среднее: центр уточняется, но одна нетипичная
                // реплика его не утаскивает.
                let weight = 1.0 / (voice.count + 1) as f32;
                voice.centroid = normalize(
                    voice
                        .centroid
                        .iter()
                        .zip(embedding)
                        .map(|(a, b)| a * (1.0 - weight) + b * weight)
                        .collect(),
                );
                voice.count += 1;
                return voice.label.clone();
            }
        }

        // Потолок нужен, чтобы шум и обрывки не наплодили десяток «собеседников».
        if self.voices.len() >= self.max {
            return "собеседник".into();
        }

        let label = format!("собеседник {}", self.voices.len() + 1);
        self.voices.push(Voice { centroid: embedding.to_vec(), count: 1, label: label.clone() });
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Вектор, «похожий» на базовый: близость задаётся долей примеси.
    fn vector(seed: usize, noise: f32) -> Vec<f32> {
        let base: Vec<f32> = (0..64).map(|i| ((i * 7 + seed * 31) % 13) as f32 - 6.0).collect();
        normalize(base.iter().enumerate().map(|(i, v)| v + noise * i as f32).collect())
    }

    #[test]
    fn один_голос_не_плодит_собеседников() {
        let mut reg = Registry::new(0.62, 6);
        let a = vector(1, 0.0);
        let b = vector(1, 0.02);

        assert_eq!(reg.label(&a), "собеседник 1");
        assert_eq!(reg.label(&b), "собеседник 1", "тот же голос — та же метка");
        assert_eq!(reg.known(), 1);
    }

    #[test]
    fn разные_голоса_различаются() {
        let mut reg = Registry::new(0.62, 6);
        assert_eq!(reg.label(&vector(1, 0.0)), "собеседник 1");
        assert_eq!(reg.label(&vector(9, 0.0)), "собеседник 2");
        assert_eq!(reg.known(), 2);
    }

    #[test]
    fn свой_голос_узнаётся_без_знакомства() {
        // Микрофон — это владелец по определению, отдельная процедура не нужна.
        let mut reg = Registry::new(0.62, 6);
        let mine = vector(3, 0.0);
        reg.learn_own(&mine);

        assert_eq!(reg.label(&mine), "ваш голос");
        assert_eq!(reg.known(), 0, "своё эхо не должно заводить собеседника");
    }

    #[test]
    fn потолок_не_даёт_расплодить_метки() {
        // Иначе шум и обрывки за час разговора наплодят десяток «собеседников».
        let mut reg = Registry::new(0.62, 2);
        for seed in [1, 9, 21, 33] {
            reg.label(&vector(seed, 0.0));
        }
        assert_eq!(reg.known(), 2);
        assert_eq!(reg.label(&vector(41, 0.0)), "собеседник");
    }

    #[test]
    fn близость_нормированных_векторов_в_пределах() {
        let a = vector(1, 0.0);
        assert!((similarity(&a, &a) - 1.0).abs() < 1e-4, "сам с собой — единица");
        assert!(similarity(&a, &vector(9, 0.0)) < 0.99);
    }

    /// Проверка на настоящей модели. Запускать: cargo test -- --ignored
    /// Нужны файл модели и заранее записанные образцы двух голосов.
    #[test]
    #[ignore]
    fn эмбеддинги_разделяют_реальные_голоса() {
        let model = std::path::Path::new("models/spk-resnet34.onnx");
        if !model.exists() {
            eprintln!("модели нет, проверка пропущена");
            return;
        }
        let mut embedder = Embedder::load(model).expect("модель не загрузилась");

        if !std::path::Path::new("dumps/spk/irina_1.wav").exists() {
            eprintln!("образцов голосов нет, проверка пропущена");
            return;
        }
        let read = |name: &str| -> Vec<f32> {
            let reader = hound::WavReader::open(format!("dumps/spk/{name}.wav")).unwrap();
            reader.into_samples::<i16>().filter_map(Result::ok).map(|s| s as f32 / 32768.0).collect()
        };

        let a1 = embedder.embed(&read("irina_1")).unwrap();
        let a2 = embedder.embed(&read("irina_2")).unwrap();
        let b1 = embedder.embed(&read("zira_1")).unwrap();

        let same = similarity(&a1, &a2);
        let other = similarity(&a1, &b1);
        assert!(same > 0.8, "один голос должен быть близок: {same}");
        assert!(other < 0.7, "разные голоса должны расходиться: {other}");
        assert!(same - other > 0.2, "разрыв должен быть уверенным");
    }
}
