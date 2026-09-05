//! Нарезка непрерывного потока на реплики.
//!
//! Детектор речи (`earshot`) смотрит кадры по 16 мс и отдаёт вероятность.
//! Реплика начинается на первом речевом кадре и заканчивается, когда тишина
//! держится дольше `endpoint_ms` — именно этот момент и означает «собеседник
//! договорил», ради которого весь режим и делается.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};

use crate::config;
use crate::stt::Speaker;

use super::SAMPLE_RATE;

/// Детектор принимает кадры строго такой длины (16 мс при 16 кГц).
const FRAME: usize = 256;

pub struct Utterance {
    pub who: Speaker,
    pub pcm: Vec<f32>,
    /// Реплика ещё не закончена: это догадка, а не итог.
    pub partial: bool,
}

/// Флаги, которыми два сегментатора договариваются между собой.
#[derive(Clone)]
pub struct Gates {
    /// Включён ли режим постоянного прослушивания.
    pub listening: Arc<AtomicBool>,
    /// Прямо сейчас в канале собеседника идёт речь.
    pub loopback_speech: Arc<AtomicBool>,
}

impl Gates {
    pub fn new() -> Self {
        Self {
            listening: Arc::new(AtomicBool::new(false)),
            loopback_speech: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }

    pub fn toggle_listening(&self) -> bool {
        let next = !self.listening();
        self.listening.store(next, Ordering::Relaxed);
        next
    }
}

pub fn spawn(
    who: Speaker,
    tap: Receiver<Vec<f32>>,
    cfg: config::Shared,
    gates: Gates,
    out: Sender<Utterance>,
) {
    thread::spawn(move || {
        let mut detector = earshot::Detector::default();
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);
        let mut state = Segmenter::default();
        let mut last_voice = Instant::now();

        while let Ok(chunk) = tap.recv() {
            if !gates.listening() {
                state.reset(&gates, who);
                pending.clear();
                continue;
            }

            let (p, spec) = {
                let g = cfg.read().unwrap();
                (g.vad.clone(), g.speculation.clone())
            };
            pending.extend_from_slice(&chunk);

            let mut at = 0;
            while at + FRAME <= pending.len() {
                let frame = &pending[at..at + FRAME];
                at += FRAME;

                let mut voiced = detector.predict_f32(frame) >= p.threshold;

                // Эхо: без наушников микрофон слышит колонки и записал бы
                // собеседника как вашу реплику. Пока в канале собеседника идёт
                // речь, микрофон молчит.
                if who == Speaker::Me
                    && p.suppress_mic_while_loopback
                    && gates.loopback_speech.load(Ordering::Relaxed)
                {
                    voiced = false;
                }

                if voiced {
                    last_voice = Instant::now();
                }
                if let Some((pcm, partial)) = state.feed(frame, voiced, &p, &spec) {
                    if partial {
                        eprintln!(
                            "[ghost] догадка по {:.1} с речи",
                            pcm.len() as f32 / SAMPLE_RATE as f32
                        );
                    } else {
                        // Задержка от последнего звука речи до выдачи реплики:
                        // это и есть цена определения конца фразы.
                        eprintln!(
                            "[ghost] реплика {} · {:.1} с · конец определён за {} мс",
                            who.label(),
                            pcm.len() as f32 / SAMPLE_RATE as f32,
                            last_voice.elapsed().as_millis()
                        );
                    }
                    let _ = out.try_send(Utterance { who, pcm, partial });
                }
                state.publish(&gates, who);
            }
            pending.drain(..at);
        }
    });
}

#[derive(Default)]
struct Segmenter {
    speaking: bool,
    /// Сколько догадок уже отдано по текущей реплике и когда последнюю.
    guesses: u32,
    last_guess: Option<Instant>,
    /// Хвост тишины перед речью — иначе теряется начало первого слова.
    pre: VecDeque<f32>,
    utt: Vec<f32>,
    speech_samples: usize,
    silence_samples: usize,
}

impl Segmenter {
    /// Отдаёт кусок речи: догадку по ещё не законченной реплике либо саму
    /// реплику, когда она закончилась.
    fn feed(
        &mut self,
        frame: &[f32],
        voiced: bool,
        p: &config::Vad,
        spec: &config::Speculation,
    ) -> Option<(Vec<f32>, bool)> {
        let ms = |v: u64| (SAMPLE_RATE as u64 * v / 1000) as usize;

        if !self.speaking {
            if !voiced {
                self.pre.extend(frame.iter().copied());
                while self.pre.len() > ms(p.pre_roll_ms) {
                    self.pre.pop_front();
                }
                return None;
            }
            self.speaking = true;
            self.utt = self.pre.drain(..).collect();
            self.speech_samples = 0;
            self.silence_samples = 0;
            self.guesses = 0;
            self.last_guess = None;
        }

        self.utt.extend_from_slice(frame);
        if voiced {
            self.speech_samples += FRAME;
            self.silence_samples = 0;
        } else {
            self.silence_samples += FRAME;
        }

        let ended = self.silence_samples >= ms(p.endpoint_ms);
        let overlong = self.utt.len() >= SAMPLE_RATE as usize * p.max_utterance_s as usize;
        if !ended && !overlong {
            return self.guess(spec).map(|pcm| (pcm, true));
        }

        self.speaking = false;
        let utt = std::mem::take(&mut self.utt);
        let enough = self.speech_samples >= ms(p.min_speech_ms);
        self.speech_samples = 0;
        self.silence_samples = 0;

        // Кашель, щелчок мыши, короткое «ага» — распознавать нечего.
        enough.then_some((utt, false))
    }

    /// Закрывает недоговорённую реплику в конце записи.
    ///
    /// В живом потоке речь заканчивает пауза; у файла паузы в конце может не
    /// быть — тогда хвост так и остался бы в буфере. Отдаётся, только если
    /// речи набралось достаточно, по тому же правилу, что и обычное закрытие.
    fn flush(&mut self, p: &config::Vad) -> Option<Vec<f32>> {
        if !self.speaking {
            return None;
        }
        self.speaking = false;
        let enough = self.speech_samples >= (SAMPLE_RATE as usize) * p.min_speech_ms as usize / 1000;
        let utt = std::mem::take(&mut self.utt);
        self.speech_samples = 0;
        self.silence_samples = 0;
        enough.then_some(utt)
    }

    /// Догадка по недоговорённой реплике. Условия все сразу: набралось
    /// достаточно речи, прошло достаточно времени с прошлой догадки и потолок
    /// на реплику не исчерпан.
    fn guess(&mut self, spec: &config::Speculation) -> Option<Vec<f32>> {
        if !spec.enabled || self.guesses >= spec.max_per_utterance {
            return None;
        }
        let speech_ms = self.speech_samples as u64 * 1000 / SAMPLE_RATE as u64;
        if speech_ms < spec.min_speech_ms {
            return None;
        }
        if let Some(last) = self.last_guess {
            if last.elapsed().as_millis() < spec.interval_ms as u128 {
                return None;
            }
        }

        self.guesses += 1;
        self.last_guess = Some(Instant::now());
        Some(self.utt.clone())
    }

    fn publish(&self, gates: &Gates, who: Speaker) {
        if who == Speaker::Them {
            gates.loopback_speech.store(self.speaking, Ordering::Relaxed);
        }
    }

    fn reset(&mut self, gates: &Gates, who: Speaker) {
        if self.speaking || !self.utt.is_empty() {
            self.speaking = false;
            self.utt.clear();
            self.pre.clear();
            self.speech_samples = 0;
            self.silence_samples = 0;
            self.guesses = 0;
            self.last_guess = None;
            self.publish(gates, who);
        }
    }
}

/// Оффлайн-нарезка готовой записи тем же сегментатором, что и живой поток.
///
/// Ради переразбора сессий: пороги нарезки подбираются на сотне настоящих
/// разговоров, а не на следующем живом. Упреждающие догадки выключены — они
/// нужны, чтобы черновик появился раньше, а при разборе спешить некуда.
pub fn segment_offline(pcm: &[f32], p: &config::Vad) -> Vec<Vec<f32>> {
    let spec = config::Speculation { enabled: false, ..Default::default() };
    let mut detector = earshot::Detector::default();
    let mut seg = Segmenter::default();
    let mut out = Vec::new();

    let mut at = 0;
    while at + FRAME <= pcm.len() {
        let frame = &pcm[at..at + FRAME];
        at += FRAME;
        let voiced = detector.predict_f32(frame) >= p.threshold;
        if let Some((utt, partial)) = seg.feed(frame, voiced, p, &spec) {
            if !partial {
                out.push(utt);
            }
        }
    }
    if let Some(utt) = seg.flush(p) {
        out.push(utt);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vad() -> config::Vad {
        config::Vad { endpoint_ms: 500, min_speech_ms: 300, pre_roll_ms: 100, ..Default::default() }
    }

    fn spec(enabled: bool) -> config::Speculation {
        config::Speculation {
            enabled,
            min_speech_ms: 300,
            interval_ms: 0,
            max_per_utterance: 2,
        }
    }

    /// Кадров на N миллисекунд речи или тишины.
    fn frames(ms: u64) -> usize {
        (SAMPLE_RATE as usize * ms as usize / 1000).div_ceil(FRAME)
    }

    /// Прогоняет через сегментатор отрезок с заданной «озвученностью».
    fn feed(state: &mut Segmenter, voiced: bool, ms: u64, p: &config::Vad, s: &config::Speculation)
        -> Vec<(Vec<f32>, bool)>
    {
        let frame = [0.0f32; FRAME];
        (0..frames(ms)).filter_map(|_| state.feed(&frame, voiced, p, s)).collect()
    }

    #[test]
    fn реплика_закрывается_паузой() {
        let (p, s) = (vad(), spec(false));
        let mut state = Segmenter::default();

        assert!(feed(&mut state, true, 1000, &p, &s).is_empty(), "пока говорят — не отдаём");
        let out = feed(&mut state, false, 700, &p, &s);

        assert_eq!(out.len(), 1, "пауза длиннее порога закрывает реплику");
        assert!(!out[0].1, "это законченная реплика, а не догадка");
        assert!(
            out[0].0.len() > SAMPLE_RATE as usize,
            "в реплику должна войти вся речь"
        );
    }

    #[test]
    fn короткий_звук_отбрасывается() {
        // Кашель, щелчок мыши, короткое «ага»: распознавать там нечего, а
        // запрос к модели стоил бы денег и внимания.
        let (p, s) = (vad(), spec(false));
        let mut state = Segmenter::default();

        feed(&mut state, true, 120, &p, &s);
        let out = feed(&mut state, false, 700, &p, &s);

        assert!(out.is_empty());
    }

    #[test]
    fn короткая_пауза_не_рвёт_реплику() {
        // Пауза для вдоха ничего не заканчивает.
        let (p, s) = (vad(), spec(false));
        let mut state = Segmenter::default();

        feed(&mut state, true, 600, &p, &s);
        assert!(feed(&mut state, false, 200, &p, &s).is_empty(), "200 мс — это ещё не конец");
        feed(&mut state, true, 600, &p, &s);
        let out = feed(&mut state, false, 700, &p, &s);

        assert_eq!(out.len(), 1, "обе половины должны прийти одной репликой");
    }

    #[test]
    fn догадка_приходит_до_конца_фразы() {
        let (p, s) = (vad(), spec(true));
        let mut state = Segmenter::default();

        let out = feed(&mut state, true, 1200, &p, &s);
        assert!(!out.is_empty(), "упреждение обязано сработать, не дожидаясь паузы");
        assert!(out.iter().all(|(_, partial)| *partial));
        assert!(out.len() <= s.max_per_utterance as usize, "потолок догадок соблюдается");
    }

    #[test]
    fn выключенное_упреждение_молчит() {
        let (p, s) = (vad(), spec(false));
        let mut state = Segmenter::default();
        assert!(feed(&mut state, true, 3000, &p, &s).is_empty());
    }

    #[test]
    fn слишком_длинная_речь_принудительно_закрывается() {
        // Предохранитель: без него реплика без пауз росла бы бесконечно.
        let p = config::Vad { max_utterance_s: 1, ..vad() };
        let s = spec(false);
        let mut state = Segmenter::default();

        let out = feed(&mut state, true, 1500, &p, &s);
        assert_eq!(out.len(), 1);
        assert!(!out[0].1, "принудительное закрытие даёт законченную реплику");
    }
}
