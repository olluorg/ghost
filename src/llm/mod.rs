pub mod openai_compat;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::config;

#[derive(Clone, Debug)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// PNG снимка экрана. Со временем вытесняется: каждый снимок — это
    /// полторы тысячи токенов на КАЖДОМ последующем запросе.
    pub image: Option<Arc<Vec<u8>>>,
}

/// Расход одного запроса. Стоимость отдаёт сам провайдер — считать её по
/// прайсу самим значило бы разойтись с фактическим счётом.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cost: f64,
}

#[derive(Debug)]
pub enum Event {
    /// Пришёл первый токен: только он и характеризует ощущаемую задержку.
    First { after: Duration, draft: bool },
    Delta { text: String, draft: bool },
    Done {
        total: Duration,
        draft: bool,
        usage: Usage,
        /// Кто на самом деле ответил — основная модель или запасная. При
        /// разборе плохой подсказки первый вопрос: промах промпта или модели.
        model: String,
        /// Задержка до первого токена, миллисекунды.
        first_ms: u64,
    },
    /// Ответ получен запасной моделью: основная отказала.
    Fallback(String),
    /// Какие пункты подсказки уже произнесены и что стоит добавить.
    Coverage { covered: Vec<usize>, addendum: Option<String> },
    /// Готовый разбор сессии.
    Review(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum Status {
    Ready,
    Failed(String),
}

enum Job {
    Ask { label: String, text: String, manual: bool },
    /// Догадка по недоговорённой реплике: ответ нужен, но в историю он не
    /// пишется — иначе контекст забьётся черновиками к одному и тому же вопросу.
    Speculate { label: String, text: String, manual: bool },
    /// Реплика уходит в контекст, но ответа на неё не ждут.
    Note { label: String, text: String },
    Hint { text: String },
    Look { png: Vec<u8> },
    /// Сверка: что из подсказки уже произнесено вслух.
    Coverage { points: Vec<String>, said: String },
    /// Разбор прошедшей сессии по её расшифровке.
    Review { transcript: String },
    Reset,
}

pub struct Llm {
    jobs: Sender<Job>,
    pub events: Receiver<Event>,
    pub status: Arc<Mutex<Status>>,
    /// Счётчик поколений. Новый запрос увеличивает его, и запрос в полёте,
    /// увидев расхождение, прекращает читать поток: догадка к позапрошлому
    /// куску фразы никому не нужна.
    generation: Arc<AtomicU64>,
    /// Сколько реплик сейчас в контексте — чтобы это было видно, а не
    /// приходилось догадываться, помнит модель прошлый вопрос или уже нет.
    pub context_len: Arc<AtomicUsize>,
}

impl Llm {
    pub fn context_len(&self) -> usize {
        self.context_len.load(Ordering::Relaxed)
    }
}

impl Llm {
    /// `manual` выбирает профиль: сильную модель для того, что вы запросили
    /// сами, дешёвую — для потока постоянного прослушивания.
    pub fn ask(&self, manual: bool, label: String, text: String) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.jobs.try_send(Job::Ask { label, text, manual });
    }

    pub fn speculate(&self, manual: bool, label: String, text: String) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.jobs.try_send(Job::Speculate { label, text, manual });
    }

    /// Пополнить контекст, не запрашивая ответ.
    pub fn note(&self, label: String, text: String) {
        let _ = self.jobs.try_send(Job::Note { label, text });
    }

    /// Подсказка живого помощника. Ответа не ждём — она адресована человеку,
    /// а модели нужна лишь затем, чтобы не противоречить ей дальше.
    pub fn hint(&self, text: String) {
        let _ = self.jobs.try_send(Job::Hint { text });
    }

    /// Сверяет пункты подсказки с тем, что было сказано вслух. Историю не
    /// трогает: это служебный вопрос, а не часть разговора.
    pub fn check_coverage(&self, points: Vec<String>, said: String) {
        let _ = self.jobs.try_send(Job::Coverage { points, said });
    }

    /// Просит разобрать прошедший разговор. Идёт мимо истории: это взгляд со
    /// стороны, а не продолжение беседы.
    pub fn review(&self, transcript: String) {
        let _ = self.jobs.try_send(Job::Review { transcript });
    }

    pub fn look(&self, png: Vec<u8>) {
        let _ = self.jobs.try_send(Job::Look { png });
    }

    pub fn reset(&self) {
        let _ = self.jobs.try_send(Job::Reset);
    }
}

pub fn spawn(cfg: config::Shared, api_key: String) -> Llm {
    let (jobs_tx, jobs_rx) = bounded::<Job>(4);
    let (ev_tx, ev_rx) = bounded::<Event>(256);
    let status = Arc::new(Mutex::new(Status::Ready));
    let context_len = Arc::new(AtomicUsize::new(0));
    let generation = Arc::new(AtomicU64::new(0));

    thread::spawn({
        let status = Arc::clone(&status);
        let context_len = Arc::clone(&context_len);
        let generation = Arc::clone(&generation);
        move || {
            let base_url = cfg.read().unwrap().llm.base_url.clone();
            let client = match openai_compat::Client::new(&base_url, &api_key) {
                Ok(c) => c,
                Err(e) => {
                    *status.lock().unwrap() = Status::Failed(format!("{e:#}"));
                    return;
                }
            };
            let mut turns: Vec<Turn> = Vec::new();

            while let Ok(job) = jobs_rx.recv() {
                match job {
                    Job::Reset => {
                        turns.clear();
                        context_len.store(0, Ordering::Relaxed);
                        eprintln!("[ghost] контекст очищен");
                    }
                    Job::Ask { label, text, manual } => {
                        let turn = Turn {
                            role: Role::User,
                            text: format!("{label}: {text}"),
                            image: None,
                        };
                        run(&client, &cfg, &mut turns, turn, &ev_tx, &context_len, &generation, Mode::final_(manual));
                    }
                    Job::Speculate { label, text, manual } => {
                        let turn = Turn {
                            role: Role::User,
                            // Пометка обязательна: без неё модель начнёт просить
                            // договорить фразу вместо того, чтобы отвечать.
                            text: format!("<реплика status=\"partial\">{label}: {text}</реплика>"),
                            image: None,
                        };
                        // Работаем на копии истории: догадка устареет через
                        // секунду, а контекст засорила бы навсегда.
                        let mut scratch = turns.clone();
                        run(
                            &client,
                            &cfg,
                            &mut scratch,
                            turn,
                            &ev_tx,
                            &context_len,
                            &generation,
                            Mode { manual, draft: true },
                        );
                    }
                    Job::Note { label, text } => {
                        turns.push(Turn {
                            role: Role::User,
                            text: format!("{label}: {text}"),
                            image: None,
                        });
                        let max_turns = cfg.read().unwrap().llm.history_turns;
                        trim(&mut turns, max_turns);
                        context_len.store(turns.len(), Ordering::Relaxed);
                    }
                    Job::Hint { text } => {
                        turns.push(Turn {
                            role: Role::User,
                            text: format!("помощник подсказывает: {text}"),
                            image: None,
                        });
                        let max_turns = cfg.read().unwrap().llm.history_turns;
                        trim(&mut turns, max_turns);
                        context_len.store(turns.len(), Ordering::Relaxed);
                    }
                    Job::Review { transcript } => {
                        eprintln!("[ghost] разбор сессии: {} символов", transcript.len());
                        review(&client, &cfg, &transcript, &ev_tx);
                    }
                    Job::Coverage { points, said } => {
                        eprintln!("[ghost] сверка: {} пунктов против «{said}»", points.len());
                        coverage(&client, &cfg, &points, &said, &ev_tx);
                    }
                    Job::Look { png } => {
                        let prompt = cfg.read().unwrap().llm.screen_prompt.clone();
                        let turn = Turn {
                            role: Role::User,
                            text: prompt,
                            image: Some(Arc::new(png)),
                        };
                        // Снимок экрана — действие руками, значит и профиль
                        // ручной.
                        run(&client, &cfg, &mut turns, turn, &ev_tx, &context_len, &generation, Mode::final_(true));
                    }
                }
            }
        }
    });

    Llm { jobs: jobs_tx, events: ev_rx, status, generation, context_len }
}

/// Что за запрос: чей профиль брать и пишется ли ответ в историю.
#[derive(Clone, Copy)]
struct Mode {
    manual: bool,
    draft: bool,
}

impl Mode {
    fn final_(manual: bool) -> Self {
        Self { manual, draft: false }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    client: &openai_compat::Client,
    cfg: &config::Shared,
    turns: &mut Vec<Turn>,
    turn: Turn,
    ev_tx: &Sender<Event>,
    context_len: &AtomicUsize,
    generation: &AtomicU64,
    mode: Mode,
) {
    let draft = mode.draft;
    let mine = generation.load(Ordering::SeqCst);
    // Настройки читаются на каждом ходе: правки в режиме настройки должны
    // действовать сразу, без перезапуска.
    let (llm, system) = {
        let guard = cfg.read().unwrap();
        (guard.llm.clone(), guard.prompt())
    };
    let profile = llm.profile(mode.manual).clone();

    turns.push(turn);
    // Лимиты берутся у истории, а не у профиля: история одна на разговор, и
    // резать её по мерке того, кто ответил последним, значит терять контекст
    // тем, у кого мерка длиннее.
    trim(turns, llm.history_turns);
    forget_images(turns, llm.history_images);

    // Снимок в истории делает текстовую модель бесполезной: она его не увидит
    // и станет отвечать вслепую. Поэтому весь ход идёт через модель со зрением.
    let with_image = turns.iter().any(|t| t.image.is_some());
    let req = openai_compat::Req {
        model: if with_image { llm.vision_model.clone() } else { profile.model.clone() },
        max_tokens: if with_image { llm.vision_max_tokens } else { profile.max_tokens },
        // У моделей со зрением рассуждение обычно принудительное, а запрет
        // его выключить возвращается ошибкой 400.
        reasoning: if with_image { None } else { Some(profile.reasoning) },
    };

    let started = Instant::now();
    let mut first_sent = false;
    let mut first_ms = 0u64;
    let mut answer = String::new();
    let mut model_used = req.model.clone();

    let mut fallback_used = false;
    let outcome = client.stream(&req, &system, turns, |delta| {
        // Запрос устарел: пришёл следующий кусок фразы или сама реплика.
        if generation.load(Ordering::SeqCst) != mine {
            return false;
        }
        if !first_sent {
            first_sent = true;
            first_ms = started.elapsed().as_millis() as u64;
            let _ = ev_tx.send(Event::First { after: started.elapsed(), draft });
        }
        answer.push_str(&delta);
        let _ = ev_tx.send(Event::Delta { text: delta, draft });
        true
    });

    // Токены кончились, лимит запросов, провайдер лёг — на живой сессии это
    // выглядит как «модель молчит». Пробуем запасную, но только если ни одного
    // токена ещё не пришло: иначе получим склейку двух разных ответов.
    let outcome = match outcome {
        Err(e) if !first_sent && !profile.fallback_model.is_empty()
            && profile.fallback_model != req.model =>
        {
            eprintln!("[ghost] основная модель отказала ({e:#}), перехожу на запасную");
            fallback_used = true;
            model_used = profile.fallback_model.clone();
            let spare = openai_compat::Req {
                model: profile.fallback_model.clone(),
                max_tokens: req.max_tokens,
                reasoning: Some(profile.reasoning),
            };
            client.stream(&spare, &system, turns, |delta| {
                if generation.load(Ordering::SeqCst) != mine {
                    return false;
                }
                if !first_sent {
                    first_sent = true;
                    let _ = ev_tx.send(Event::First { after: started.elapsed(), draft });
                }
                answer.push_str(&delta);
                let _ = ev_tx.send(Event::Delta { text: delta, draft });
                true
            })
        }
        other => other,
    };

    let usage = outcome.as_ref().ok().copied();
    match outcome {
        Ok(_) => {
            if fallback_used {
                let _ = ev_tx.send(Event::Fallback(profile.fallback_model.clone()));
            }
            // Догадку в историю не пишем: она устареет через секунду, а
            // контекст засорит навсегда.
            if !draft {
                turns.push(Turn { role: Role::Assistant, text: answer, image: None });
                context_len.store(turns.len(), Ordering::Relaxed);
            }
            let _ = ev_tx.send(Event::Done {
                total: started.elapsed(),
                draft,
                usage: usage.unwrap_or_default(),
                model: model_used,
                first_ms,
            });
        }
        Err(e) => {
            // Неудачный ход не оставляем в истории: иначе следующий запрос
            // уйдёт с висящим вопросом.
            turns.pop();
            context_len.store(turns.len(), Ordering::Relaxed);
            let _ = ev_tx.send(Event::Failed(format!("{e:#}")));
        }
    }
}

/// Оставляет картинки только у последних `max_images` снимков.
///
/// Без этого каждый новый вопрос тащил бы за собой все снимки за сессию: и по
/// деньгам, и по задержке это растёт линейно и очень быстро.
fn forget_images(turns: &mut [Turn], max_images: usize) {
    let mut seen = 0;
    for turn in turns.iter_mut().rev() {
        if turn.image.is_some() {
            seen += 1;
            if seen > max_images {
                turn.image = None;
                turn.text = "(снимок экрана вытеснен из контекста)".into();
            }
        }
    }
}

/// Разбор прошедшего разговора.
///
/// Модель здесь не суфлёр, а сторонний наблюдатель, поэтому и системный промпт
/// другой: продолжать разговор не нужно, нужно назвать, что получилось и что
/// стоит поправить.
fn review(
    client: &openai_compat::Client,
    cfg: &config::Shared,
    transcript: &str,
    ev_tx: &Sender<Event>,
) {
    let profile = cfg.read().unwrap().llm.manual.clone();

    let system = "Ты разбираешь прошедший разговор по его расшифровке. Пиши коротко и по делу, без вступлений и похвал. Строго такая структура:\n\n## Как прошло\nдва-три предложения по существу\n\n## Получилось\n- до трёх пунктов\n\n## Не получилось\n- до трёх пунктов, с примером из расшифровки\n\n## Над чем поработать\n- до трёх конкретных действий к следующему разу\n\nВ расшифровке есть ошибки распознавания — не придирайся к формулировкам, разбирай смысл. Если данных мало, так и скажи, а не выдумывай.";
    let question = format!("Расшифровка разговора:\n\n{transcript}");

    let req = openai_compat::Req {
        model: profile.model.clone(),
        max_tokens: 1200,
        reasoning: Some(profile.reasoning),
    };
    let turns = [Turn { role: Role::User, text: question, image: None }];

    let mut reply = String::new();
    if client
        .stream(&req, system, &turns, |delta| {
            reply.push_str(&delta);
            true
        })
        .is_err()
    {
        let _ = ev_tx.send(Event::Review("разбор не получился: модель не ответила".into()));
        return;
    }
    let _ = ev_tx.send(Event::Review(reply));
}

/// Отдельный короткий запрос: какие пункты уже прозвучали.
///
/// Идёт мимо истории и мимо системного промпта суфлёра — здесь нужен не совет,
/// а разбор. Ответ в одну строку: разбирать его надёжнее, чем JSON, который
/// модель норовит обернуть в пояснения.
fn coverage(
    client: &openai_compat::Client,
    cfg: &config::Shared,
    points: &[String],
    said: &str,
    ev_tx: &Sender<Event>,
) {
    let profile = cfg.read().unwrap().llm.auto.clone();

    let numbered: String = points
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}. {p}\n", i + 1))
        .collect();
    let system = "Ты сверяешь подсказку с тем, что человек уже произнёс вслух. Пункт считается закрытым, если мысль прозвучала своими словами — дословного совпадения не требуется. Не закрывай пункт, если названа только тема, но не сама суть. Отвечай ровно одной строкой вида:\ncovered=1,3 | add=что стоит добавить одной короткой фразой\nЕсли закрытых нет — covered= пусто. Если добавлять нечего — add=-. Ничего кроме этой строки.";
    let question = format!("Пункты подсказки:\n{numbered}\nСказано вслух: «{said}»");

    let req = openai_compat::Req {
        model: profile.model.clone(),
        max_tokens: 200,
        reasoning: Some(false),
    };
    let turns = [Turn { role: Role::User, text: question, image: None }];

    let mut reply = String::new();
    if client.stream(&req, system, &turns, |delta| {
        reply.push_str(&delta);
        true
    })
    .is_err()
    {
        return;
    }

    let (covered, addendum) = parse_coverage(&reply, points.len());
    eprintln!("[ghost] сверка: ответ {reply:?} -> закрыто {covered:?}, добавить {addendum:?}");
    let _ = ev_tx.send(Event::Coverage { covered, addendum });
}

fn parse_coverage(reply: &str, total: usize) -> (Vec<usize>, Option<String>) {
    let line = reply.lines().find(|l| l.contains("covered=")).unwrap_or(reply);

    let covered = line
        .split("covered=")
        .nth(1)
        .map(|rest| rest.split('|').next().unwrap_or(""))
        .map(|list| {
            list.split(',')
                .filter_map(|n| n.trim().parse::<usize>().ok())
                // Нумерация в ответе с единицы, у нас — с нуля.
                .filter(|n| *n >= 1 && *n <= total)
                .map(|n| n - 1)
                .collect()
        })
        .unwrap_or_default();

    let addendum = line
        .split("add=")
        .nth(1)
        .map(|t| t.trim().trim_matches('«').trim_matches('»').trim())
        .filter(|t| !t.is_empty() && *t != "-")
        .map(str::to_string);

    (covered, addendum)
}

/// Обрезает историю с начала, сохраняя её структуру.
///
/// Наивное удаление по одному оставляет первым ход ассистента — ответ на
/// вопрос, которого в контексте уже нет. Модель считывает это как обрывок
/// чужого разговора, поэтому режем строго парами и никогда не оставляем
/// ассистента в начале.
fn trim(turns: &mut Vec<Turn>, max_turns: usize) {
    let max = max_turns.max(2);
    while turns.len() > max {
        turns.remove(0);
        if matches!(turns.first(), Some(t) if matches!(t.role, Role::Assistant)) {
            turns.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, text: &str) -> Turn {
        Turn { role, text: text.into(), image: None }
    }

    #[test]
    fn ответ_сверки_разбирается() {
        let (covered, add) = parse_coverage("covered=1,3 | add=упомяните сроки", 4);
        assert_eq!(covered, vec![0, 2], "нумерация в ответе с единицы, у нас с нуля");
        assert_eq!(add.as_deref(), Some("упомяните сроки"));
    }

    #[test]
    fn пустая_сверка_и_прочерк() {
        let (covered, add) = parse_coverage("covered= | add=-", 3);
        assert!(covered.is_empty());
        assert!(add.is_none(), "прочерк означает «добавить нечего»");
    }

    #[test]
    fn номера_вне_диапазона_отбрасываются() {
        // Модель иногда называет пункт, которого нет: он не должен обращаться
        // в панику или в чужой индекс.
        let (covered, _) = parse_coverage("covered=0,2,9 | add=-", 3);
        assert_eq!(covered, vec![1]);
    }

    #[test]
    fn многословный_ответ_всё_равно_разбирается() {
        let reply = "Вот разбор:\ncovered=2 | add=назовите сложность\nНадеюсь, помог.";
        let (covered, add) = parse_coverage(reply, 3);
        assert_eq!(covered, vec![1]);
        assert_eq!(add.as_deref(), Some("назовите сложность"));
    }

    #[test]
    fn ответ_без_формата_ничего_не_ломает() {
        let (covered, add) = parse_coverage("не понял вопроса", 3);
        assert!(covered.is_empty());
        assert!(add.is_none());
    }

    #[test]
    fn обрезка_не_оставляет_ответ_без_вопроса() {
        // Ровно та ошибка, из-за которой модель отвечала с оглядкой на обрывок
        // чужого разговора.
        let mut turns: Vec<Turn> = (0..4)
            .flat_map(|i| {
                [
                    turn(Role::User, &format!("вопрос {i}")),
                    turn(Role::Assistant, &format!("ответ {i}")),
                ]
            })
            .collect();

        trim(&mut turns, 5);

        assert!(turns.len() <= 5);
        assert!(
            matches!(turns.first().unwrap().role, Role::User),
            "первым в истории обязан быть вопрос, а не ответ"
        );
    }

    #[test]
    fn обрезка_не_трогает_короткую_историю() {
        let mut turns = vec![turn(Role::User, "а"), turn(Role::Assistant, "б")];
        trim(&mut turns, 20);
        assert_eq!(turns.len(), 2);
    }

    #[test]
    fn снимки_вытесняются_кроме_последних() {
        let png = || Some(Arc::new(vec![0u8; 4]));
        let mut turns = vec![
            Turn { role: Role::User, text: "первый".into(), image: png() },
            turn(Role::Assistant, "ответ"),
            Turn { role: Role::User, text: "второй".into(), image: png() },
        ];

        forget_images(&mut turns, 1);

        assert!(turns[0].image.is_none(), "старый снимок должен уйти из контекста");
        assert!(turns[0].text.contains("вытеснен"), "и это должно быть видно в тексте");
        assert!(turns[2].image.is_some(), "последний снимок остаётся");
    }
}
