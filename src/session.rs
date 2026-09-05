//! Сессия — явно начатый и явно законченный разговор.
//!
//! Начало и конец ставит человек кнопкой, а не запуск программы: разбирать
//! потом нужно именно разговор, а не всё время, что оверлей провисел на
//! экране. Пока сессия не начата, писать нечего — и записи не возникает.
//!
//! Внутри сессии пишутся две вещи рядом: сведённое аудио разговора и
//! построчный журнал того, что было распознано и что ответила модель.
//! Разбирать жалобы «ответила не то» без записи невозможно: к моменту разбора
//! ни звука, ни текста уже нет.
//!
//! Всё остаётся на диске и никуда не уходит.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio::capture::Audio;
use crate::audio::SAMPLE_RATE;
use crate::config;

/// Итоги сессии. Считаются по ходу и ложатся рядом с записью: без них разбор
/// «как прошло» опирается на ощущения.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub started_ms: u128,
    /// Когда запись закончили. Ноль — сессия ещё идёт.
    pub ended_ms: u128,
    pub questions: u32,
    pub answers: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: f64,
}

impl Stats {
    /// Длительность. У законченной сессии она замирает: считать её от «сейчас»
    /// значило бы, что вчерашний разговор с каждым днём становится длиннее.
    pub fn minutes(&self) -> f64 {
        if self.started_ms == 0 {
            return 0.0;
        }
        let till = if self.ended_ms > 0 { self.ended_ms } else { now_ms() };
        till.saturating_sub(self.started_ms) as f64 / 60_000.0
    }
}

/// Идущая запись. Пока её нет, сессия не начата.
struct Live {
    dir: PathBuf,
    log: Option<BufWriter<File>>,
    stats: Stats,
    /// Просьба к дорожке звука закончить файл. Без явного знака подписка на
    /// звук живёт вечно, а WAV остаётся с недописанным заголовком.
    stop: Arc<AtomicBool>,
}

pub struct Session {
    root: PathBuf,
    live: Mutex<Option<Live>>,
}

impl Session {
    /// Ничего не пишет и ничего не создаёт: каталог появится, когда нажмут
    /// «начать».
    pub fn new(cfg: &config::Shared) -> Self {
        let root = PathBuf::from(&cfg.read().unwrap().session.dir);
        Self { root, live: Mutex::new(None) }
    }

    /// Сессия идёт прямо сейчас.
    pub fn active(&self) -> bool {
        self.live.lock().unwrap().is_some()
    }

    /// Каталог идущей сессии. Пусто — сессия не начата.
    pub fn dir(&self) -> Option<PathBuf> {
        self.live.lock().unwrap().as_ref().map(|l| l.dir.clone())
    }

    pub fn stats(&self) -> Stats {
        self.live.lock().unwrap().as_ref().map(|l| l.stats).unwrap_or_default()
    }

    /// Начинает запись: заводит каталог, журнал и дорожку звука.
    ///
    /// Возвращает каталог — его показывают человеку, чтобы он знал, куда всё
    /// легло, ещё до того, как разговор закончится.
    pub fn start(
        &self,
        cfg: &config::Shared,
        theirs: &Audio,
        mine: &Audio,
    ) -> anyhow::Result<PathBuf> {
        let settings = cfg.read().unwrap().session.clone();
        let mut live = self.live.lock().unwrap();
        // Повторное нажатие не должно рвать уже идущую запись.
        if let Some(l) = live.as_ref() {
            return Ok(l.dir.clone());
        }

        let dir = self.root.join(stamp());
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("каталог {} не создан: {e}", dir.display()))?;

        let log = match File::create(dir.join("log.jsonl")) {
            Ok(f) => Some(BufWriter::new(f)),
            Err(e) => {
                // Журнала нет — но звук пишется, и разговор всё равно стоит
                // вести: терять его целиком из-за одного файла неправильно.
                log::warn!("журнал сессии не создан: {e}");
                None
            }
        };

        write_meta(&dir, cfg, &device_name(theirs), &device_name(mine));

        let stop = Arc::new(AtomicBool::new(false));
        if settings.audio {
            spawn_audio(dir.join("audio.wav"), theirs, mine, Arc::clone(&stop));
        }

        *live = Some(Live {
            dir: dir.clone(),
            log,
            stats: Stats { started_ms: now_ms(), ..Stats::default() },
            stop,
        });
        drop(live);

        self.note("session", "сессия", "начало");
        log::info!("сессия начата: {}", dir.display());
        eprintln!("[ghost] сессия начата: {}", dir.display());
        Ok(dir)
    }

    /// Заканчивает запись: дописывает журнал, кладёт итоги и закрывает звук.
    ///
    /// Возвращает каталог и итоги — их показывают сразу после нажатия, чтобы
    /// было видно, что запись действительно сохранена.
    pub fn stop(&self) -> Option<(PathBuf, Stats)> {
        self.note("session", "сессия", "конец");

        let mut live = self.live.lock().unwrap();
        let mut l = live.take()?;
        l.stats.ended_ms = now_ms();
        // Звук закрывается сам: дорожка увидит знак, допишет заголовок WAV и
        // отпустит подписку.
        l.stop.store(true, Ordering::Relaxed);
        if let Some(file) = l.log.as_mut() {
            let _ = file.flush();
        }
        drop(l.log.take());
        write_stats(&l.dir, &l.stats);

        log::info!("сессия закончена: {}", l.dir.display());
        eprintln!("[ghost] сессия закончена: {}", l.dir.display());
        Some((l.dir.clone(), l.stats))
    }

    pub fn count_question(&self) {
        if let Some(l) = self.live.lock().unwrap().as_mut() {
            l.stats.questions += 1;
        }
    }

    /// Ответ получен: копим расход и складываем пару в банк.
    pub fn count_answer(&self, tokens_in: u64, tokens_out: u64, cost: f64) {
        if let Some(l) = self.live.lock().unwrap().as_mut() {
            l.stats.answers += 1;
            l.stats.tokens_in += tokens_in;
            l.stats.tokens_out += tokens_out;
            l.stats.cost += cost;
        }
    }

    /// Пара «вопрос — ответ» в общий банк.
    ///
    /// Банк один на все сессии: его смысл в том, чтобы к десятому разговору
    /// видеть, что спрашивают чаще всего и что вы на это отвечали.
    pub fn bank(&self, who: &str, question: &str, answer: &str) {
        if question.trim().is_empty() || answer.trim().is_empty() {
            return;
        }
        let Some(id) = self
            .live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.dir.file_name().unwrap_or_default().to_string_lossy().into_owned())
        else {
            return;
        };
        let path = self.root.join("bank.jsonl");
        let line = format!(
            "{{\"ms\":{},\"session\":\"{}\",\"who\":\"{}\",\"q\":\"{}\",\"a\":\"{}\"}}\n",
            now_ms(),
            escape(&id),
            escape(who),
            escape(question),
            escape(answer)
        );
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Помечает в банке последнюю пару как негодную.
    ///
    /// Отдельной строкой, а не правкой прежней: банк — это append-only журнал,
    /// и дописать в него дешевле и надёжнее, чем искать и переписывать пару.
    /// При чтении банка пометка привязывается к паре по тексту вопроса.
    pub fn bank_rate(&self, who: &str, question: &str) {
        if question.trim().is_empty() {
            return;
        }
        let Some(id) = self
            .live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.dir.file_name().unwrap_or_default().to_string_lossy().into_owned())
        else {
            return;
        };
        let path = self.root.join("bank.jsonl");
        let line = format!(
            "{{\"ms\":{},\"session\":\"{}\",\"who\":\"{}\",\"q\":\"{}\",\"rate\":\"мимо\"}}\n",
            now_ms(),
            escape(&id),
            escape(who),
            escape(question),
        );
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Кладёт итоги рядом с записью: они нужны и после перезапуска.
    pub fn save_stats(&self) {
        let guard = self.live.lock().unwrap();
        let Some(l) = guard.as_ref() else { return };
        write_stats(&l.dir, &l.stats);
    }

    /// Строка журнала. Формат — по строке JSON на событие: его одинаково легко
    /// читать глазами и разбирать скриптом.
    pub fn note(&self, kind: &str, who: &str, text: &str) {
        let Ok(mut guard) = self.live.lock() else { return };
        let Some(file) = guard.as_mut().and_then(|l| l.log.as_mut()) else { return };

        let line = format!(
            "{{\"ms\":{},\"kind\":\"{}\",\"who\":\"{}\",\"text\":\"{}\"}}\n",
            now_ms(),
            escape(kind),
            escape(who),
            escape(text)
        );
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

fn write_stats(dir: &Path, s: &Stats) {
    let json = format!(
        "{{\"started_ms\":{},\"ended_ms\":{},\"minutes\":{:.1},\"questions\":{},\"answers\":{},\
\"tokens_in\":{},\"tokens_out\":{},\"cost\":{:.6}}}\n",
        s.started_ms,
        s.ended_ms,
        s.minutes(),
        s.questions,
        s.answers,
        s.tokens_in,
        s.tokens_out,
        s.cost
    );
    let _ = std::fs::write(dir.join("stats.json"), json);
}

/// Обстановка, в которой шёл разговор: модели, промпт, пороги, устройства.
///
/// Без неё запись через неделю не о чем спросить. Подсказки зависят от
/// промпта, брифа и модели, а всё это меняется от сессии к сессии: без снимка
/// настроек нельзя ни сравнить две записи между собой, ни понять, что именно
/// привело к плохому ответу. Файл кладётся один раз, в начале — правки по ходу
/// разговора сюда не попадают намеренно, это снимок старта.
fn write_meta(dir: &Path, cfg: &config::Shared, theirs: &str, mine: &str) {
    let g = cfg.read().unwrap();
    let meta = serde_json::json!({
        "ghost": env!("CARGO_PKG_VERSION"),
        "started_ms": now_ms(),
        "llm": {
            "base_url": g.llm.base_url,
            "manual": g.llm.manual,
            "auto": g.llm.auto,
            "vision_model": g.llm.vision_model,
        },
        // Промпт целиком, а не путь к файлу: файл к моменту разбора уже правят.
        "prompt": g.prompt(),
        "glossary": g.glossary(),
        "input": g.input,
        "vad": g.vad,
        "speculation": g.speculation,
        "coverage": g.coverage,
        "stt": g.stt,
        "voices": g.voices,
        "devices": { "theirs": theirs, "mine": mine },
    });
    drop(g);

    match serde_json::to_string_pretty(&meta) {
        Ok(text) => {
            let _ = std::fs::write(dir.join("meta.json"), text);
        }
        Err(e) => log::warn!("обстановка сессии не записана: {e}"),
    }
}

/// Как устройство называется в системе. Имя из настроек может быть пустым
/// («по умолчанию»), а разбирать запись надо, зная, что именно её писало.
fn device_name(audio: &Audio) -> String {
    match &*audio.status.lock().unwrap() {
        crate::audio::capture::Status::Running { device } => device.clone(),
        crate::audio::capture::Status::Starting => "запускается".into(),
        crate::audio::capture::Status::Failed(e) => format!("не открылось: {e}"),
    }
}

/// Пишет разговор двумя дорожками: собеседник слева, вы справа.
///
/// Стерео, а не сведённое моно. Сведение слушать удобно, но по нему уже нельзя
/// ни прогнать распознавание отдельно по каждому каналу, ни перенастроить
/// нарезку, ни проверить различение голосов — а ради этого запись и ведётся.
/// Цена — двойной размер файла.
///
/// Дорожка живёт ровно столько, сколько идёт сессия: по знаку `stop` она
/// дописывает заголовок WAV и отпускает подписку на звук.
fn spawn_audio(path: PathBuf, theirs: &Audio, mine: &Audio, stop: Arc<AtomicBool>) {
    let (a, b) = (theirs.subscribe(), mine.subscribe());

    thread::spawn(move || {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = match hound::WavWriter::create(&path, spec) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[ghost] аудио сессии не пишется: {e}");
                return;
            }
        };

        let mut buf_a: Vec<f32> = Vec::new();
        let mut buf_b: Vec<f32> = Vec::new();
        let mut last_flush = Instant::now();

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Ждём с оглядкой: в тишине звук может не приходить вовсе, а знак
            // об окончании должен сработать всё равно.
            crossbeam_channel::select! {
                recv(a) -> chunk => match chunk { Ok(c) => buf_a.extend(c), Err(_) => break },
                recv(b) -> chunk => match chunk { Ok(c) => buf_b.extend(c), Err(_) => break },
                default(Duration::from_millis(250)) => {}
            }

            // Замолчавшее устройство не должно останавливать запись второго.
            const STALL: usize = SAMPLE_RATE as usize / 2;
            if buf_a.len() > STALL && buf_b.is_empty() {
                buf_b.resize(buf_a.len(), 0.0);
            }
            if buf_b.len() > STALL && buf_a.is_empty() {
                buf_a.resize(buf_b.len(), 0.0);
            }

            let n = buf_a.len().min(buf_b.len());
            if n == 0 {
                continue;
            }
            for sample in interleave(&buf_a[..n], &buf_b[..n]) {
                let _ = writer.write_sample(sample);
            }
            buf_a.drain(..n);
            buf_b.drain(..n);

            // Заголовок WAV несёт длину, и без периодического сброса файл
            // после аварийного завершения оказался бы нечитаемым.
            if last_flush.elapsed() >= Duration::from_secs(3) {
                last_flush = Instant::now();
                let _ = writer.flush();
            }
        }

        // Заголовок дописывается здесь: без этого длина в файле остаётся от
        // последнего сброса, а хвост записи пропадает.
        if let Err(e) = writer.finalize() {
            eprintln!("[ghost] аудио сессии не закрыто: {e}");
        }
    });
}

/// Перемежает две дорожки в кадры стерео: собеседник слева, вы справа.
///
/// Порядок здесь не косметика: по нему при разборе определяют, кто говорил, и
/// перепутанные местами каналы означают перепутанные роли во всей записи.
fn interleave(theirs: &[f32], mine: &[f32]) -> Vec<i16> {
    let pcm = |v: f32| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    theirs.iter().zip(mine).flat_map(|(&l, &r)| [pcm(l), pcm(r)]).collect()
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Имя каталога сессии. Без внешних зависимостей на календарь: секунды от
/// эпохи сортируются так же, а разобрать их при разборе несложно.
fn stamp() -> String {
    format!("{}", SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Прошедшая сессия на диске.
pub struct Entry {
    pub id: String,
    pub dir: PathBuf,
    pub stats: Option<Stats>,
    /// Сколько шла — берётся из записи, а не считается от «сейчас».
    pub minutes: f64,
    /// Разбор уже сделан и лежит рядом.
    pub reviewed: bool,
    /// В журнале нет ни одной реплики: разбирать нечего. Такие записи копятся
    /// сами — включили и не поговорили, — и список из них не читается.
    pub empty: bool,
}

impl Entry {
    /// Читаемая дата, по местному времени.
    ///
    /// Каталог назван секундами от эпохи — переводим сами, календарной
    /// зависимости в проекте нет. Поправка на пояс обязательна: время реплик в
    /// ленте ставится местное (`GetLocalTime`), и разговор в три часа дня,
    /// показанный в списке как полдень, — это двое часов в одном окне.
    pub fn when(&self) -> String {
        let Ok(secs) = self.id.parse::<i64>() else {
            return self.id.clone();
        };
        moment(secs + local_offset_secs())
    }
}

/// Секунды от эпохи в «ГГГГ-ММ-ДД чч:мм».
fn moment(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, rest / 3600, (rest % 3600) / 60)
}

/// Поправка на часовой пояс в секундах — разница между местными и всемирными
/// часами системы.
///
/// Берётся текущая, а не та, что была во время разговора: запись, сделанная до
/// перевода часов, съедет на час. Ради одного часа на границе перевода тащить
/// разбор правил перехода в этот файл незачем — но знать об этом стоит.
fn local_offset_secs() -> i64 {
    use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let (local, utc) = unsafe { (GetLocalTime(), GetSystemTime()) };
    epoch_secs(&local) - epoch_secs(&utc)
}

fn epoch_secs(t: &windows::Win32::Foundation::SYSTEMTIME) -> i64 {
    days_from_civil(t.wYear as i64, t.wMonth as u32, t.wDay as u32) * 86_400
        + i64::from(t.wHour) * 3600
        + i64::from(t.wMinute) * 60
        + i64::from(t.wSecond)
}

/// Календарная дата в дни от эпохи — обратная к [`civil`], тот же алгоритм.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Дни от эпохи в календарную дату (алгоритм Хиннанта).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = era * 400 + yoe + i64::from(m <= 2);
    (y, m, d)
}

/// Список записанных сессий, свежие первыми.
pub fn list(root: &std::path::Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|dir| {
            let id = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let stats = read_stats(&dir);
            Entry {
                minutes: stats.map(|(_, m)| m).unwrap_or(0.0),
                stats: stats.map(|(s, _)| s),
                reviewed: dir.join("review.md").exists(),
                empty: transcript(&dir).trim().is_empty(),
                id,
                dir,
            }
        })
        .collect();
    // Имя каталога — секунды от эпохи, поэтому обычная сортировка строк
    // совпадает с хронологической.
    out.sort_by(|a, b| b.id.cmp(&a.id));
    out
}

/// Убирает записи, в которых не осталось ни одной реплики.
///
/// Возвращает, сколько каталогов убрано и сколько байт освободилось: удаление
/// без отчёта выглядит как «кнопка ничего не сделала».
///
/// `keep` — идущая сессия: её каталог уже создан и пока пуст, но трогать его
/// нельзя, в него прямо сейчас пишут.
pub fn remove_empty(entries: &[Entry], keep: Option<&Path>) -> (usize, u64) {
    let mut count = 0;
    let mut bytes = 0;
    for entry in entries.iter().filter(|e| e.empty) {
        if Some(entry.dir.as_path()) == keep {
            continue;
        }
        let size = dir_size(&entry.dir);
        match std::fs::remove_dir_all(&entry.dir) {
            Ok(()) => {
                count += 1;
                bytes += size;
            }
            Err(e) => log::warn!("запись {} не убрана: {e}", entry.dir.display()),
        }
    }
    if count > 0 {
        log::info!("убрано пустых записей: {count}, освобождено {bytes} байт");
    }
    (count, bytes)
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn read_stats(dir: &std::path::Path) -> Option<(Stats, f64)> {
    let text = std::fs::read_to_string(dir.join("stats.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let minutes = v["minutes"].as_f64().unwrap_or(0.0);
    Some((Stats {
        started_ms: v["started_ms"].as_u64().unwrap_or(0) as u128,
        ended_ms: v["ended_ms"].as_u64().unwrap_or(0) as u128,
        questions: v["questions"].as_u64().unwrap_or(0) as u32,
        answers: v["answers"].as_u64().unwrap_or(0) as u32,
        tokens_in: v["tokens_in"].as_u64().unwrap_or(0),
        tokens_out: v["tokens_out"].as_u64().unwrap_or(0),
        cost: v["cost"].as_f64().unwrap_or(0.0),
    }, minutes))
}

/// Событие журнала: что за строка, кто сказал, что именно.
pub fn read_log(dir: &std::path::Path) -> Vec<(String, String, String)> {
    let Ok(text) = std::fs::read_to_string(dir.join("log.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|v| {
            (
                v["kind"].as_str().unwrap_or("").to_string(),
                v["who"].as_str().unwrap_or("").to_string(),
                v["text"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Разговор одной строкой на реплику — то, что уходит на разбор.
///
/// Отметки начала, конца, пауз и сбоев в расшифровку не идут: они нужны файлу,
/// а не модели, и в разборе выглядели бы репликой участника. Человеку они
/// видны в журнале сессии, где у сбоев свой цвет.
pub fn transcript(dir: &std::path::Path) -> String {
    read_log(dir)
        .into_iter()
        .filter(|(kind, _, _)| !matches!(kind.as_str(), "session" | "error" | "meta"))
        .map(|(kind, who, text)| {
            if kind == "llm" {
                format!("подсказка: {text}\n")
            } else {
                format!("{who}: {text}\n")
            }
        })
        .collect()
}

/// Пара из банка с оценкой.
pub struct Pair {
    pub who: String,
    pub question: String,
    pub answer: String,
    /// Подсказку отметили негодной. Ради этой пометки разбор записей и делают
    /// в первую очередь: она указывает, где модель промахивается.
    pub missed: bool,
}

/// Накопленный банк пар «вопрос — ответ», свежие первыми.
///
/// Оценки лежат в том же файле отдельными строками (см. [`Session::bank_rate`])
/// и здесь привязываются к паре по сессии и тексту вопроса: пара, к которой
/// пришла отметка, помечается негодной, а не показывается пустой строкой.
/// Отметка может стоять в файле и раньше, и позже своей пары, поэтому сперва
/// собираем все отметки, а затем размечаем пары.
pub fn read_bank(root: &std::path::Path, limit: usize) -> Vec<Pair> {
    let Ok(text) = std::fs::read_to_string(root.join("bank.jsonl")) else {
        return Vec::new();
    };

    // Ключ — сессия и вопрос вместе: один и тот же вопрос в разных разговорах
    // это разные пары, и отметка на одном не красит другой.
    let mut missed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut rows: Vec<(String, Pair)> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let session = v["session"].as_str().unwrap_or("").to_string();
        let question = v["q"].as_str().unwrap_or("").to_string();
        if v.get("rate").is_some() {
            missed.insert((session, question));
            continue;
        }
        rows.push((
            session,
            Pair {
                who: v["who"].as_str().unwrap_or("").to_string(),
                answer: v["a"].as_str().unwrap_or("").to_string(),
                missed: false,
                question,
            },
        ));
    }

    let mut pairs: Vec<Pair> = rows
        .into_iter()
        .map(|(session, mut pair)| {
            pair.missed = missed.contains(&(session, pair.question.clone()));
            pair
        })
        .collect();
    pairs.reverse();
    pairs.truncate(limit);
    pairs
}

#[cfg(test)]
mod tests_dates {
    use super::*;

    #[test]
    fn дата_сессии_читается_из_имени_каталога() {
        let entry = |id: &str| Entry {
            id: id.into(),
            dir: PathBuf::new(),
            stats: None,
            minutes: 0.0,
            reviewed: false,
            empty: true,
        };
        // Перевод считается без поправки на пояс: её в `when` добавляют
        // отдельно, а машина, на которой идут тесты, может стоять где угодно.
        assert_eq!(moment(1788523200), "2026-09-04 12:00");
        // Високосный день не должен съезжать.
        assert_eq!(moment(1709164800), "2024-02-29 00:00");
        // Каталог, созданный руками, показываем как есть, а не падаем.
        assert_eq!(entry("черновик").when(), "черновик");
    }

    #[test]
    fn перевод_даты_обратим() {
        // На обратном переводе держится поправка на часовой пояс: она считается
        // как разница местных и всемирных часов системы, а те приходят
        // календарной датой, а не секундами.
        for days in [-25_000_i64, -1, 0, 1, 19_000, 20_336, 40_000] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y:04}-{m:02}-{d:02}");
        }
    }

    #[test]
    fn поправка_на_пояс_правдоподобна() {
        // Пояса кратны минуте и не выходят за ±14 часов. Ошибка в переводе
        // дат вылезла бы здесь сразу, а не через сутки на глаз.
        let offset = local_offset_secs();
        assert_eq!(offset % 60, 0, "пояс не кратен минуте: {offset}");
        assert!(offset.abs() <= 14 * 3600, "невозможный пояс: {offset}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn строка_журнала_остаётся_разбираемой() {
        // Кавычки и переводы строк в расшифровке ломали бы весь файл.
        let text = escape("он сказал \"да\"\nи ушёл\\");
        let line = format!("{{\"text\":\"{text}\"}}");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("строка должна быть JSON");
        assert_eq!(parsed["text"], "он сказал \"да\"\nи ушёл\\");
    }

    #[test]
    fn управляющие_символы_не_попадают_в_журнал() {
        let text = escape("до\u{7}после");
        assert!(!text.contains('\u{7}'));
    }

    #[test]
    fn оценка_мимо_привязывается_к_своей_паре() {
        // Единственная оценка в записи должна найти свою пару даже если
        // отметка легла в файл позже, а тот же вопрос в другой сессии её не
        // подхватывает.
        let root = std::env::temp_dir().join(format!("ghost-банк-{}", now_ms()));
        std::fs::create_dir_all(&root).unwrap();
        let bank = root.join("bank.jsonl");
        let lines = concat!(
            r#"{"ms":1,"session":"a","who":"я","q":"вопрос","a":"ответ"}"#, "
",
            r#"{"ms":2,"session":"b","who":"я","q":"вопрос","a":"другой ответ"}"#, "
",
            r#"{"ms":3,"session":"a","who":"я","q":"вопрос","rate":"мимо"}"#, "
",
        );
        std::fs::write(&bank, lines).unwrap();

        let pairs = read_bank(&root, 100);
        assert_eq!(pairs.len(), 2, "строка оценки — не отдельная пара");
        let a = pairs.iter().find(|p| p.answer == "ответ").unwrap();
        let b = pairs.iter().find(|p| p.answer == "другой ответ").unwrap();
        assert!(a.missed, "пара из своей сессии отмечена");
        assert!(!b.missed, "тот же вопрос в другой сессии не задет");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn до_начала_сессии_на_диск_ничего_не_ложится() {
        // Оверлей висит на экране постоянно, и пока сессию не начали, ни
        // каталога, ни банка появляться не должно.
        let root = std::env::temp_dir().join(format!("ghost-idle-{}", stamp()));
        let session = Session { root: root.clone(), live: Mutex::new(None) };

        session.note("said", "я", "проверка");
        session.count_question();
        session.count_answer(10, 20, 0.5);
        session.bank("я", "вопрос", "ответ");
        session.save_stats();

        assert!(!session.active());
        assert_eq!(session.dir(), None);
        assert_eq!(session.stats().questions, 0);
        assert!(!root.exists(), "каталог сессии не должен появляться до начала");
    }

    #[test]
    fn запись_идёт_двумя_дорожками_в_известном_порядке() {
        // Слева собеседник, справа вы. Перепутанные каналы означали бы
        // перепутанные роли во всей записи, а заметить это можно только на слух
        // и уже после разговора.
        let frames = interleave(&[1.0, -1.0], &[0.0, 0.0]);
        assert_eq!(frames.len(), 4, "на кадр — два отсчёта");
        assert_eq!(frames[0], i16::MAX);
        assert_eq!(frames[1], 0);
        assert_eq!(frames[2], -i16::MAX);
        assert_eq!(frames[3], 0);
        // Сложение двух громких каналов раньше уходило за предел типа.
        assert_eq!(interleave(&[9.0], &[-9.0]), vec![i16::MAX, -i16::MAX]);
    }

    #[test]
    fn обстановка_разговора_ложится_рядом_с_записью() {
        // Через неделю по записи надо понять, каким промптом и какой моделью
        // получены эти подсказки: и то и другое меняется от сессии к сессии.
        let dir = std::env::temp_dir().join(format!("ghost-мета-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = config::Config::default();
        cfg.llm.manual.model = "проверочная/модель".into();
        let cfg: config::Shared = std::sync::Arc::new(std::sync::RwLock::new(cfg));

        write_meta(&dir, &cfg, "динамики", "микрофон");

        let text = std::fs::read_to_string(dir.join("meta.json")).expect("мета должна лечь рядом");
        let v: serde_json::Value = serde_json::from_str(&text).expect("мета должна быть JSON");
        assert_eq!(v["llm"]["manual"]["model"], "проверочная/модель");
        assert_eq!(v["devices"]["theirs"], "динамики");
        assert!(!v["prompt"].as_str().unwrap_or_default().is_empty(), "промпт нужен целиком");
        assert!(v["vad"]["threshold"].is_number());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn уборка_не_трогает_ни_разговор_ни_идущую_запись() {
        // Пустые записи копятся сами: включили и не поговорили. Убирать их
        // можно, но ошибиться здесь нельзя — удаление необратимо.
        let root = std::env::temp_dir().join(format!("ghost-уборка-{}", now_ms()));
        let make = |name: &str, log: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("log.jsonl"), log).unwrap();
            dir
        };
        let сказанное =
            "{\"ms\":1,\"kind\":\"said\",\"who\":\"я\",\"text\":\"вопрос\"}\n";
        let отметки =
            "{\"ms\":1,\"kind\":\"session\",\"who\":\"сессия\",\"text\":\"начало\"}\n";

        let разговор = make("1", сказанное);
        let пустая = make("2", отметки);
        let идущая = make("3", отметки);

        let entries = list(&root);
        assert_eq!(entries.len(), 3);
        let (done, _) = remove_empty(&entries, Some(&идущая));

        assert_eq!(done, 1, "убрать полагалось ровно одну запись");
        assert!(разговор.exists(), "запись с репликами трогать нельзя");
        assert!(идущая.exists(), "в идущую сессию прямо сейчас пишут");
        assert!(!пустая.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn отметки_сессии_не_считаются_разговором() {
        // Начало, конец и паузы есть в каждой записи. Если считать их
        // содержимым, пустых записей не станет вовсе и уборка потеряет смысл.
        let dir = std::env::temp_dir().join(format!("ghost-отметки-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("log.jsonl"),
            "{\"ms\":1,\"kind\":\"session\",\"who\":\"авто\",\"text\":\"пауза\"}\n",
        )
        .unwrap();

        assert!(transcript(&dir).trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn законченная_сессия_не_удлиняется_со_временем() {
        // Итоги читают и через неделю после разговора: длительность должна
        // браться из записи, а не считаться от «сейчас».
        let done = Stats { started_ms: 1_000_000, ended_ms: 1_600_000, ..Stats::default() };
        assert!((done.minutes() - 10.0).abs() < 1e-9);
        // У неначатой сессии длительности нет вовсе.
        assert_eq!(Stats::default().minutes(), 0.0);
    }
}
