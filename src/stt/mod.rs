pub mod local;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

/// Кто говорил. Расшифровка без этой пометки бесполезна: реплика собеседника
/// требует ответа, а собственная — только уточняет контекст.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Them,
    Me,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Them => "собеседник",
            Speaker::Me => "я",
        }
    }
}

/// Распознаватель речи.
///
/// Реализаций по замыслу две (§7 SPEC): локальная пакетная сейчас и облачная
/// потоковая в проде. Поэтому трейт заводится сразу, а не «отрефакторим потом».
/// Результат распознавания одного куска.
pub struct Transcript {
    pub text: String,
    /// Whisper считает, что дальше говорит другой человек. Точнее нашего
    /// порога тишины, поэтому склейка вопроса на этом обрывается.
    pub speaker_turn: bool,
}

pub trait Transcriber: Send {
    fn transcribe(&mut self, pcm16k: &[f32]) -> anyhow::Result<Transcript>;
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Кусок взят в работу. Подробности не нужны: их несёт `Done`.
    Started,
    Done {
        who: Speaker,
        auto: bool,
        /// Кто именно говорил: «собеседник 1», «собеседник 2», «я». Без
        /// различения голосов — просто сторона канала.
        label: String,
        partial: bool,
        text: String,
        /// Распознавание считает, что реплика закончена сменой говорящего.
        speaker_turn: bool,
        took: Duration,
        audio_secs: f32,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum Status {
    Loading,
    Ready { model: String, threads: usize },
    Failed(String),
}

pub struct Stt {
    jobs: Sender<Job>,
    pub events: Receiver<Event>,
    pub status: Arc<Mutex<Status>>,
}

/// `auto` отличает реплику, нарезанную детектором речи, от той, что записана
/// удержанием клавиши: на первую отвечать нужно не всегда.
struct Job {
    who: Speaker,
    auto: bool,
    /// Реплика ещё не закончена: ответ по ней будет черновиком.
    partial: bool,
    pcm: Vec<f32>,
}

impl Stt {
    /// Кладёт кусок на распознавание. Никогда не блокирует UI-поток.
    pub fn submit(&self, who: Speaker, auto: bool, partial: bool, pcm: Vec<f32>) {
        let _ = self.jobs.try_send(Job { who, auto, partial, pcm });
    }
}

pub fn spawn(cfg: crate::config::Shared, model: PathBuf, language: String) -> Stt {
    // В постоянном режиме реплики идут подряд, поэтому очередь глубже одной:
    // на GPU распознавание занимает доли секунды и успевает за разговором.
    let (jobs_tx, jobs_rx) = bounded::<Job>(4);
    let (ev_tx, ev_rx) = bounded::<Event>(16);
    let status = Arc::new(Mutex::new(Status::Loading));

    thread::spawn({
        let status = Arc::clone(&status);
        move || {
            let mut voices = Voices::load(&cfg);
            let threads = threads();
            let mut engine = match local::Whisper::load(&model, &language, threads) {
                Ok(w) => {
                    let name = model
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    eprintln!("[ghost] whisper загружен: {name}, потоков {threads}");
                    *status.lock().unwrap() = Status::Ready { model: name, threads };
                    w
                }
                Err(e) => {
                    eprintln!("[ghost] whisper: {e:#}");
                    *status.lock().unwrap() = Status::Failed(format!("{e:#}"));
                    return;
                }
            };

            while let Ok(Job { who, auto, partial, pcm }) = jobs_rx.recv() {
                let audio_secs = pcm.len() as f32 / crate::audio::SAMPLE_RATE as f32;
                let _ = ev_tx.try_send(Event::Started);

                // Голос определяется до распознавания: он нужен и тогда, когда
                // текст окажется пустым.
                let label = voices.label(&cfg, who, &pcm);

                // Глоссарий и пороги читаются на каждом куске: правка
                // обстановки должна действовать сразу, без перезапуска.
                {
                    let guard = cfg.read().unwrap();
                    engine.set_hints(guard.glossary(), guard.stt.clone());
                }

                let t0 = Instant::now();
                let event = match engine.transcribe(&pcm) {
                    Ok(t) => Event::Done {
                        who,
                        auto,
                        label,
                        partial,
                        text: t.text,
                        speaker_turn: t.speaker_turn,
                        took: t0.elapsed(),
                        audio_secs,
                    },
                    Err(e) => Event::Failed(format!("{e:#}")),
                };
                let _ = ev_tx.try_send(event);
            }
        }
    });

    Stt { jobs: jobs_tx, events: ev_rx, status }
}

/// Различение голосов. Отсутствие модели не должно ломать распознавание,
/// поэтому вся подсистема необязательна и молча выключается.
struct Voices {
    embedder: Option<crate::speaker::Embedder>,
    registry: crate::speaker::Registry,
}

impl Voices {
    fn load(cfg: &crate::config::Shared) -> Self {
        let v = cfg.read().unwrap().voices.clone();
        let registry = crate::speaker::Registry::new(v.threshold, v.max_voices);

        if !v.enabled {
            return Self { embedder: None, registry };
        }
        let embedder = match crate::speaker::Embedder::load(std::path::Path::new(&v.model_path)) {
            Ok(e) => {
                eprintln!("[ghost] различение голосов включено: {}", v.model_path);
                Some(e)
            }
            Err(e) => {
                eprintln!("[ghost] различение голосов выключено: {e:#}");
                None
            }
        };
        Self { embedder, registry }
    }

    fn label(&mut self, cfg: &crate::config::Shared, who: Speaker, pcm: &[f32]) -> String {
        let Some(embedder) = self.embedder.as_mut() else {
            return who.label().to_string();
        };
        let Ok(embedding) = embedder.embed(pcm) else {
            // Слишком короткий кусок — судить не о чем.
            return who.label().to_string();
        };

        match who {
            // Микрофон — это владелец по определению, знакомиться отдельно не
            // нужно: каждая его реплика уточняет профиль.
            Speaker::Me => {
                self.registry.learn_own(&embedding);
                who.label().to_string()
            }
            Speaker::Them => {
                self.registry.set_threshold(cfg.read().unwrap().voices.threshold);
                self.registry.label(&embedding)
            }
        }
    }
}

/// На гибридных Intel держим число потоков около количества P-ядер: раскладывать
/// работу ещё и на E-ядра невыгодно из-за ожидания отстающих потоков.
fn threads() -> usize {
    thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(4, 8))
        .unwrap_or(4)
}
