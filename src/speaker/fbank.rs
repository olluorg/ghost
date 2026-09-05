//! Фильтробанковые признаки в том виде, в каком их ждёт модель эмбеддингов.
//!
//! Параметры взяты из `preprocessor_config.json` модели: 16 кГц, 80 мел-полос,
//! окно 25 мс, шаг 10 мс, Хэмминг, snip_edges, без дизеринга. Всё остальное —
//! значения по умолчанию Kaldi, на котором модель обучалась: снятие постоянной
//! составляющей, предыскажение 0.97, степенной спектр, натуральный логарифм.
//!
//! Совпадение здесь не косметика: разойдись мы с эталоном — эмбеддинги
//! перестанут различать голоса, и понять это можно будет только по метрике.

use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

pub const MEL_BINS: usize = 80;

const SAMPLE_RATE: f32 = 16_000.0;
const FRAME_LEN: usize = 400; // 25 мс
const FRAME_SHIFT: usize = 160; // 10 мс
/// `round_to_power_of_two`: 400 округляется вверх до 512.
const FFT_SIZE: usize = 512;
const PREEMPHASIS: f32 = 0.97;
const LOW_FREQ: f32 = 20.0;

pub struct Fbank {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Для каждой мел-полосы: индекс первого бина и веса подряд.
    filters: Vec<(usize, Vec<f32>)>,
}

impl Fbank {
    pub fn new() -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);

        // Окно Хэмминга в варианте Kaldi: знаменатель N-1.
        let window = (0..FRAME_LEN)
            .map(|i| {
                0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME_LEN as f32 - 1.0)).cos()
            })
            .collect();

        Self { fft, window, filters: mel_filters() }
    }

    /// Число кадров при `snip_edges = true`: кадры не выходят за конец сигнала.
    pub fn frames(samples: usize) -> usize {
        if samples < FRAME_LEN {
            0
        } else {
            1 + (samples - FRAME_LEN) / FRAME_SHIFT
        }
    }

    /// Возвращает матрицу [кадры × 80] построчно, уже с вычтенным средним.
    pub fn compute(&self, pcm: &[f32]) -> Vec<f32> {
        let frames = Self::frames(pcm.len());
        let mut out = vec![0.0f32; frames * MEL_BINS];
        let mut buffer = vec![Complex32::default(); FFT_SIZE];
        let mut power = vec![0.0f32; FFT_SIZE / 2 + 1];

        for f in 0..frames {
            let start = f * FRAME_SHIFT;
            let mut frame: Vec<f32> = pcm[start..start + FRAME_LEN].to_vec();

            // Постоянная составляющая: Kaldi снимает её покадрово.
            let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
            for x in frame.iter_mut() {
                *x -= mean;
            }

            // Предыскажение идёт с конца, иначе затрутся ещё не использованные
            // отсчёты.
            for i in (1..FRAME_LEN).rev() {
                frame[i] -= PREEMPHASIS * frame[i - 1];
            }
            frame[0] -= PREEMPHASIS * frame[0];

            for (i, slot) in buffer.iter_mut().enumerate() {
                *slot = Complex32::new(frame.get(i).map_or(0.0, |x| x * self.window[i]), 0.0);
            }
            self.fft.process(&mut buffer);

            for (i, slot) in power.iter_mut().enumerate() {
                *slot = buffer[i].norm_sqr();
            }

            for (bin, (offset, weights)) in self.filters.iter().enumerate() {
                let energy: f32 =
                    weights.iter().enumerate().map(|(i, w)| w * power[offset + i]).sum();
                out[f * MEL_BINS + bin] = energy.max(f32::EPSILON).ln();
            }
        }

        subtract_mean(&mut out, frames);
        out
    }
}

/// Нормировка по среднему вдоль времени: убирает окраску канала и микрофона,
/// оставляя то, что отличает говорящих.
fn subtract_mean(data: &mut [f32], frames: usize) {
    if frames == 0 {
        return;
    }
    for bin in 0..MEL_BINS {
        let mean: f32 =
            (0..frames).map(|f| data[f * MEL_BINS + bin]).sum::<f32>() / frames as f32;
        for f in 0..frames {
            data[f * MEL_BINS + bin] -= mean;
        }
    }
}

fn to_mel(hz: f32) -> f32 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// Треугольные фильтры, равномерные по мел-шкале, без нормировки по площади —
/// как в Kaldi.
fn mel_filters() -> Vec<(usize, Vec<f32>)> {
    let nyquist = SAMPLE_RATE / 2.0;
    let bins = FFT_SIZE / 2 + 1;
    let step = SAMPLE_RATE / FFT_SIZE as f32;

    let (low, high) = (to_mel(LOW_FREQ), to_mel(nyquist));
    let delta = (high - low) / (MEL_BINS + 1) as f32;

    (0..MEL_BINS)
        .map(|bin| {
            let left = low + delta * bin as f32;
            let center = left + delta;
            let right = center + delta;

            let mut offset = 0;
            let mut weights = Vec::new();
            for k in 0..bins {
                let mel = to_mel(k as f32 * step);
                if mel <= left || mel >= right {
                    if weights.is_empty() {
                        continue;
                    }
                    break;
                }
                if weights.is_empty() {
                    offset = k;
                }
                weights.push(if mel <= center {
                    (mel - left) / (center - left)
                } else {
                    (right - mel) / (right - center)
                });
            }
            (offset, weights)
        })
        .collect()
}
