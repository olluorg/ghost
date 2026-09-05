pub mod capture;
pub mod vad;
pub mod wav;

use std::collections::VecDeque;

/// Частота, к которой приводится всё аудио. Столько же ждёт whisper.
pub const SAMPLE_RATE: u32 = 16_000;

/// Глубина кольцевого буфера. Даёт возможность нажать клавишу уже ПОСЛЕ того,
/// как собеседник договорил: фраза всё ещё в памяти.
pub const RING_SECONDS: usize = 30;

/// Кольцевой буфер моно-сэмплов с монотонным счётчиком позиции.
///
/// Под мьютексом, а не lock-free: продюсер — наш собственный поток с блокирующим
/// ожиданием события WASAPI (~10 мс на итерацию), а не realtime-колбэк ОС с
/// жёстким дедлайном. Стоимость мьютекса здесь пренебрежима.
pub struct Ring {
    buf: VecDeque<f32>,
    cap: usize,
    /// Всего сэмплов, прошедших через буфер за время работы.
    written: u64,
}

impl Ring {
    pub fn new() -> Self {
        let cap = SAMPLE_RATE as usize * RING_SECONDS;
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
            written: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend(samples.iter().copied());
        self.written += samples.len() as u64;
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    /// Позиция за `ms` до конца, но не раньше начала записи.
    /// `back_off(0)` — текущий конец буфера.
    pub fn back_off(&self, ms: u64) -> u64 {
        let samples = SAMPLE_RATE as u64 * ms / 1000;
        self.written.saturating_sub(samples)
    }

    /// Сэмплы от абсолютной позиции `from` до конца.
    /// Если `from` уже вытеснено из буфера, отдаём с самого раннего доступного.
    pub fn since(&self, from: u64) -> Vec<f32> {
        let oldest = self.written - self.buf.len() as u64;
        let start = from.max(oldest);
        let skip = (start - oldest) as usize;
        self.buf.iter().skip(skip).copied().collect()
    }

    /// Пиковая амплитуда за последние `ms` — для индикатора уровня.
    pub fn peak(&self, ms: u64) -> f32 {
        let n = (SAMPLE_RATE as u64 * ms / 1000) as usize;
        let skip = self.buf.len().saturating_sub(n);
        self.buf
            .iter()
            .skip(skip)
            .fold(0.0f32, |acc, s| acc.max(s.abs()))
    }
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn буфер_отдаёт_записанное_с_позиции() {
        let mut ring = Ring::new();
        ring.push(&[1.0; 100]);
        let from = ring.back_off(0);
        ring.push(&[2.0; 50]);

        let tail = ring.since(from);
        assert_eq!(tail.len(), 50, "с отметки должно прийти только новое");
        assert!(tail.iter().all(|x| *x == 2.0));
    }

    #[test]
    fn отступ_назад_доклеивает_прошлое() {
        // На этом держится pre-roll: начало первого слова произносят
        // одновременно с нажатием клавиши.
        let mut ring = Ring::new();
        ring.push(&[1.0; SAMPLE_RATE as usize]);

        let from = ring.back_off(100);
        let tail = ring.since(from);

        assert_eq!(tail.len(), SAMPLE_RATE as usize / 10);
    }

    #[test]
    fn старое_вытесняется_но_не_ломает_чтение() {
        let mut ring = Ring::new();
        let from = ring.back_off(0);
        // Заполняем буфер с запасом: отметка уйдёт за его край.
        ring.push(&vec![1.0; SAMPLE_RATE as usize * (RING_SECONDS + 5)]);

        let tail = ring.since(from);
        assert_eq!(tail.len(), SAMPLE_RATE as usize * RING_SECONDS, "отдаём что осталось");
    }

    #[test]
    fn пик_считается_по_хвосту() {
        let mut ring = Ring::new();
        ring.push(&[0.9; SAMPLE_RATE as usize]);
        ring.push(&[0.1; SAMPLE_RATE as usize / 10]);

        assert!(ring.peak(50) < 0.2, "громкое было давно и в хвост не попало");
        assert!(ring.peak(2000) > 0.8);
    }
}
