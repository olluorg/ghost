pub mod board;
pub mod layered;
pub mod settings;
pub mod tray;
pub mod win;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use egui_commonmark::CommonMarkCache;
use image::ImageEncoder;

use crate::audio::capture::{Audio, Status as AudioStatus};
use crate::audio::vad::{Gates, Utterance};
use crate::audio::wav;
use crate::config;
use crate::hotkeys::Event as Key;
use crate::llm::{Event as LlmEvent, Llm, Status as LlmStatus};
use crate::remote::{Remote, Tunnel};
use crate::session::Session;
use crate::stt::{Event as SttEvent, Speaker, Status as SttStatus, Stt};
use settings::Settings;
use win::Affinity;

/// Потолок ленты. Смысл не в экономии памяти, а в том, чтобы прокрутка
/// оставалась осмысленной: глубже никто не листает.
const MAX_CHAT: usize = 200;

/// Сколько живёт подсказка в шапке. Постоянная строка только отнимает место
/// у ответов, а прочитать её нужно один раз.
const NOTE_TTL: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Author {
    Them,
    Me,
    Screen,
    Helper,
    Model,
}

impl Author {
    fn of(who: Speaker) -> Self {
        match who {
            Speaker::Them => Author::Them,
            Speaker::Me => Author::Me,
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Author::Them => egui::Color32::from_rgb(150, 190, 230),
            Author::Me => egui::Color32::from_rgb(140, 190, 150),
            Author::Screen => egui::Color32::from_rgb(190, 160, 220),
            Author::Helper => egui::Color32::from_rgb(120, 210, 200),
            Author::Model => egui::Color32::from_rgb(225, 195, 130),
        }
    }
}

struct Msg {
    author: Author,
    /// Подпись сверху. Для реплик это метка говорящего, а не просто сторона
    /// канала: собеседников в звонке может быть несколько.
    title: String,
    text: String,
    /// Ответ ещё дописывается потоком.
    streaming: bool,
    /// Ответ по недоговорённой реплике: его сменит следующая догадка или финал.
    draft: bool,
    /// Пузырь ждёт нового ответа, но старый ещё показывается. Первый же токен
    /// его заменит — не раньше, иначе панель мигнёт пустотой.
    stale: bool,
    /// Какие строки ответа уже произнесены вслух. Пусто — сверки ещё не было.
    covered: Vec<bool>,
    /// Что стоит добавить к уже сказанному.
    addendum: Option<String>,
    /// Местное время появления — подпись в углу карточки.
    time: String,
}

/// Одна дорожка: свой диалог с моделью и своя лента.
///
/// Контексты раздельные намеренно: если бы ручные и автоматические реплики
/// шли в одну историю, каждая дорожка отвечала бы с оглядкой на чужие ходы,
/// и сравнивать их было бы бессмысленно.
struct Track {
    llm: Llm,
    chat: Vec<Msg>,
    timing: Option<String>,
    /// Индекс реплики, по которой сейчас идёт разговор: догадки обновляют её
    /// на месте, а не плодят новые сообщения.
    open: Option<usize>,
    /// Последний законченный ответ: за ним следим, что из него произнесено.
    tracked: Option<usize>,
    /// Пришедший разбор сессии: он не часть разговора и в ленту не идёт —
    /// его забирают настройки.
    review: Option<String>,
    /// Сколько пунктов отслеживаемой подсказки уже отмечено в истории как
    /// произнесённые. Сверка приходит по нескольку раз на одну подсказку, и
    /// без этого счётчика история заполнялась бы одним и тем же.
    spoken_noted: usize,
}

impl Track {
    fn new(llm: Llm) -> Self {
        Self {
            llm,
            chat: Vec::new(),
            timing: None,
            open: None,
            tracked: None,
            review: None,
            spoken_noted: 0,
        }
    }

    fn push(&mut self, author: Author, title: String, text: String, streaming: bool) {
        self.chat.push(Msg {
            author,
            title,
            text,
            streaming,
            draft: false,
            stale: false,
            covered: Vec::new(),
            addendum: None,
            time: win::now_hhmm(),
        });
        if self.chat.len() > MAX_CHAT {
            self.chat.remove(0);
            // Индексы съезжают вместе с лентой.
            self.open = self.open.and_then(|i| i.checked_sub(1));
            self.tracked = self.tracked.and_then(|i| i.checked_sub(1));
        }
    }

    /// Открывает пару «реплика + ответ» или обновляет уже открытую: догадки по
    /// одной и той же фразе не должны множить сообщения.
    fn open_pair(&mut self, author: Author, title: String, text: String) {
        match self.open {
            Some(i) if i + 1 < self.chat.len() => {
                self.chat[i].title = title;
                self.chat[i].text = text;
                self.chat[i + 1].stale = true;
                self.chat[i + 1].streaming = true;
            }
            _ => {
                self.push(author, title, text, false);
                self.push(Author::Model, "подсказка".into(), String::new(), true);
                self.open = Some(self.chat.len() - 2);
            }
        }
    }

    /// Закрывает открытое сообщение модели. `fallback` подставляется, если не
    /// пришло ни одного токена — пустой пузырь выглядел бы как зависание.
    fn finish_streaming(&mut self, fallback: Option<String>) {
        if let Some(msg) = self.chat.last_mut().filter(|m| m.streaming) {
            msg.streaming = false;
            if msg.text.trim().is_empty() {
                msg.text = fallback.unwrap_or_else(|| "(пустой ответ)".into());
            }
        }
    }

    /// Кладёт в историю то, что человек уже произнёс из подсказки.
    ///
    /// Сверка это и так считает — но до сих пор её ответ уходил только в
    /// галочки на экране. Модель об этом не знала и на следующем ходе
    /// предлагала ровно то же самое, что человек только что сказал вслух.
    ///
    /// Пишется только прирост: сверка идёт по нескольку раз за подсказку, и
    /// каждый её ответ засорял бы историю повтором.
    fn note_spoken(&mut self) {
        let Some(msg) = self.tracked.and_then(|i| self.chat.get(i)) else {
            return;
        };
        let spoken: Vec<&str> = msg
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .zip(msg.covered.iter())
            .filter(|(_, covered)| **covered)
            .map(|(line, _)| line)
            .collect();
        if spoken.len() <= self.spoken_noted {
            return;
        }
        self.spoken_noted = spoken.len();
        self.llm.note("я".into(), format!("из подсказки уже сказал вслух: {}", spoken.join(" ")));
    }

    /// Возвращает завершённый ответ, если он только что дописался: его надо
    /// отдать в журнал и помощнику.
    fn drain(&mut self) -> Option<Answered> {
        let mut finished = None;
        while let Ok(event) = self.llm.events.try_recv() {
            match event {
                LlmEvent::First { after, draft } => {
                    // Первый токен — единственное, что определяет ощущаемую
                    // задержку: дальше пользователь уже читает.
                    self.timing = Some(format!(
                        "{}первый токен через {:.2} с",
                        if draft { "черновик: " } else { "" },
                        after.as_secs_f32()
                    ));
                }
                LlmEvent::Delta { text, draft } => {
                    if let Some(msg) = self.chat.last_mut().filter(|m| m.streaming) {
                        if msg.stale {
                            msg.text.clear();
                            msg.stale = false;
                        }
                        msg.draft = draft;
                        msg.text.push_str(&text);
                    }
                }
                LlmEvent::Done { total, draft, usage, model, first_ms } => {
                    // Черновик оставляем открытым: его сменит следующая догадка
                    // или финальный ответ.
                    if !draft {
                        self.finish_streaming(None);
                        self.open = None;
                        finished = self.chat.last().map(|m| Answered {
                            text: m.text.clone(),
                            usage,
                            model: model.clone(),
                            first_ms,
                        });
                        // Следим за свежим ответом: сверять сказанное со
                        // старым бессмысленно.
                        self.tracked = self.chat.len().checked_sub(1);
                        self.spoken_noted = 0;
                    }
                    if let Some(t) = &self.timing {
                        self.timing = Some(format!("{t} · всего {:.2} с", total.as_secs_f32()));
                    }
                }
                LlmEvent::Coverage { covered, addendum } => {
                    if self.tracked.is_none() {
                        eprintln!("[ghost] сверка пришла, но подсказка не отслеживается");
                    }
                    if let Some(msg) = self.tracked.and_then(|i| self.chat.get_mut(i)) {
                        let lines = msg.text.lines().filter(|l| !l.trim().is_empty()).count();
                        let mut marks = vec![false; lines];
                        for i in covered {
                            if let Some(slot) = marks.get_mut(i) {
                                *slot = true;
                            }
                        }
                        msg.covered = marks;
                        msg.addendum = addendum;
                    }
                    self.note_spoken();
                }
                LlmEvent::Review(text) => {
                    self.review = Some(text);
                }
                LlmEvent::Fallback(model) => {
                    self.timing = Some(format!("основная модель отказала, отвечает {model}"));
                }
                LlmEvent::Failed(e) => {
                    self.finish_streaming(Some(format!("не ответила: {e}")));
                    eprintln!("[ghost] модель: {e}");
                }
            }
        }
        finished
    }

    fn reset(&mut self) {
        self.chat.clear();
        self.timing = None;
        self.open = None;
        self.tracked = None;
        self.llm.reset();
    }
}

/// Завершённый ответ модели со всем, что нужно журналу: сам текст, расход,
/// кто ответил и за сколько пришёл первый токен.
struct Answered {
    text: String,
    usage: crate::llm::Usage,
    model: String,
    first_ms: u64,
}

/// Собираемый вопрос. Детектор режет речь по паузам, а пауза для вдоха ничего
/// не заканчивает: без сборки модель отвечает на обрывок.
struct Merge {
    /// Канал, из которого идёт речь. Именно он, а не метка голоса, решает,
    /// продолжается ли реплика.
    who: Speaker,
    label: String,
    text: String,
    auto: bool,
    since: Instant,
    /// Фраза выглядит законченной: есть точка или знак вопроса на конце.
    /// Незаконченной даём больше времени — пауза на размышление длиннее паузы
    /// для вдоха.
    finished: bool,
    /// Метка, которая уже один раз пришла вместо нашей. Смену говорящего
    /// признаём только со второго раза.
    doubt: Option<String>,
}

impl Merge {
    /// Продолжает ли пришедший кусок собираемую реплику.
    ///
    /// Метка голоса приходит от различения голосов и иногда ошибается на
    /// отдельном куске. Раньше одной такой ошибки в середине фразы хватало,
    /// чтобы вопрос разорвался надвое, а первая половина немедленно ушла в
    /// модель. Поэтому смену говорящего признаём только когда новая метка
    /// пришла дважды подряд: настоящий собеседник говорит дольше одного куска,
    /// а сбой различения — нет.
    fn accepts(&mut self, who: Speaker, label: &str, auto: bool) -> bool {
        if who != self.who || !auto || !self.auto {
            return false;
        }
        if label == self.label {
            self.doubt = None;
            return true;
        }
        match self.doubt.take() {
            Some(seen) if seen == label => false,
            _ => {
                self.doubt = Some(label.to_string());
                true
            }
        }
    }
}

/// Знаки, которыми whisper заканчивает завершённую мысль.
fn looks_finished(text: &str) -> bool {
    text.trim_end().ends_with(['.', '?', '!', '\u{2026}'])
}

struct Hold {
    who: Speaker,
    from: u64,
    pressed: Instant,
}

pub struct Ghost {
    cfg: config::Shared,
    /// Кэш разметки: egui_commonmark разбирает markdown заново на каждом кадре,
    /// без кэша это заметно на длинных ответах.
    md: CommonMarkCache,
    settings: Settings,

    affinity: Affinity,
    last_check: Instant,
    restores: u32,

    keys: Receiver<Key>,
    theirs: Audio,
    mine: Audio,
    stt: Stt,
    gates: Gates,
    utterances: Receiver<Utterance>,
    remote: Remote,
    session: Session,
    /// Незаконченный вопрос: куски одного говорящего собираются, пока он не
    /// замолчит окончательно.
    merge: Option<Merge>,
    /// Что вы произнесли с момента появления подсказки.
    said: String,
    said_at: Instant,
    /// Текст, для которого сверка уже отправлена: один и тот же не переспрашиваем.
    said_checked: String,
    /// Последний заданный вопрос и кто его задал — для банка пар.
    last_question: String,
    last_asker: String,

    /// Единственная лента: ручной ввод и постоянный режим идут в неё вместе.
    /// Разделять их визуально смысла не оказалось — сравнивать больше нечего,
    /// а место делить приходилось.
    track: Track,

    hold: Option<Hold>,
    busy: bool,
    /// Окно можно двигать и растягивать, не заходя в настройки.
    move_mode: bool,
    /// Фактически применённое состояние «мышь внутрь».
    interactive: bool,
    note: Option<String>,
    /// Когда подсказка сменилась. Отслеживаем сравнением, а не при каждой
    /// записи: мест, где она ставится, слишком много.
    note_shown: Option<String>,
    note_at: Instant,
    /// Строка ручного ввода: сюда же попадает распознанное по клавише, чтобы
    /// вопрос можно было поправить до отправки.
    input_text: String,
    /// Поле ввода сейчас держит курсор. Нужно, чтобы понимать, кому
    /// адресован Ctrl+Enter.
    input_focused: bool,
    focus_input: bool,
    input_h: f32,
    /// Открытый выбор модели или устройства. Живёт прямо в полосе, на месте
    /// строки ввода: всплывающий список пришлось бы либо обрезать краем окна,
    /// либо раздвигать окна под него — и то и другое плохо.
    choosing: Option<Chooser>,
    /// Пора закрываться.
    quit: bool,
    /// Оверлей спрятан целиком — по значку в трее.
    hidden: bool,
    /// Значок в области уведомлений. Снимается сам при выходе.
    tray: Option<tray::Tray>,
    /// Маска прошлого кадра — только чтобы не писать в журнал одно и то же.
    mask: Mask,
    /// Последнее описание окна из журнала: пишем только при расхождении.
    window_state: String,
    /// Окно сейчас ловит мышь. Хранится, чтобы не трогать стиль каждый кадр.
    catching: bool,
    /// Когда снять себя самого по `GHOST_SELFSHOT`. Пусто — не просили или
    /// уже сняли. Отсчёт от запуска, а не от кадра: кадры идут неравномерно,
    /// и «через N кадров» означало разное время на разных машинах.
    selfshot_at: Option<Instant>,
    /// Табло: отдельное окно, которое только показывает.
    board: Option<board::Board>,
    /// Общая с eframe видеокарта — на ней же рисуется табло.
    wgpu: Option<eframe::egui_wgpu::RenderState>,
    /// Высота полосы управления, считается по факту отрисовки.
    strip_h: f32,
    /// Куда полосу уже поставили: чтобы не двигать её каждый кадр.
    placed: Option<(f32, f32, f32, f32)>,
    /// Следующий пришедший кадр надо ещё и сохранить в файл.
    want_disk_shot: bool,
    /// Кадр у системы уже запрошен и мы ждём его. Без этого запрос уходил бы
    /// каждый кадр, пока снимок не придёт, и получилась бы очередь снимков.
    shot_asked: bool,
    /// Высота подвала с прошлого кадра. Знать её заранее нельзя, а делить
    /// окно надо до отрисовки — за кадр запаздывания при изменении размера
    /// глазом не заметно.
    footer_h: f32,
    /// Последняя ссылка, уехавшая в буфер обмена. Нужна, чтобы не копировать
    /// одно и то же каждый кадр и чтобы поймать подмену локальной на публичную.
    copied: Option<String>,
    /// Кнопка завершения нажата один раз и ждёт подтверждения. Промах по ней
    /// посреди разговора обрывал бы запись, а вернуть её уже нельзя.
    confirm_stop: Option<Instant>,
    /// Что сломано прямо сейчас и уже записано в журнал сессии. Нужно, чтобы
    /// не писать одну и ту же беду каждый кадр и чтобы заметить, когда она
    /// прошла.
    troubles: std::collections::HashMap<&'static str, String>,
}

impl Ghost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: config::Shared,
        keys: Receiver<Key>,
        theirs: Audio,
        mine: Audio,
        stt: Stt,
        llm: Llm,
        gates: Gates,
        utterances: Receiver<Utterance>,
        remote: Remote,
        session: Session,
    ) -> Self {
        install_symbol_font(&cc.egui_ctx);
        // Выделение текста мышью отключено во всём окне. Дело не в удобстве:
        // при наведении на подпись egui ставит курсор-балку, а при протяжке
        // держит её до конца жеста — уже после того, как мы возвращаем курсор
        // к обычному виду. Курсор рисуется системой и попадает в запись
        // экрана, поэтому любая его реакция выдаёт, что под ним что-то есть.
        cc.egui_ctx.all_styles_mut(|s| s.interaction.selectable_labels = false);
        let affinity = win::harden(cc);
        eprintln!("[ghost] display affinity: {affinity:?}");
        Self {
            cfg,
            md: CommonMarkCache::default(),
            settings: Settings::default(),
            affinity,
            last_check: Instant::now(),
            restores: 0,
            keys,
            theirs,
            mine,
            stt,
            gates,
            utterances,
            remote,
            session,
            merge: None,
            said: String::new(),
            said_at: Instant::now(),
            said_checked: String::new(),
            last_question: String::new(),
            last_asker: String::new(),
            track: Track::new(llm),
            hold: None,
            busy: false,
            move_mode: false,
            interactive: false,
            note: None,
            note_shown: None,
            note_at: Instant::now(),
            input_text: String::new(),
            input_focused: false,
            focus_input: false,
            input_h: 52.0,
            choosing: None,
            quit: false,
            hidden: false,
            tray: None,
            mask: Mask::default(),
            window_state: String::new(),
            catching: true,
            selfshot_at: std::env::var_os("GHOST_SELFSHOT")
                .map(|_| Instant::now() + Duration::from_millis(1500)),
            board: cc.wgpu_render_state.as_ref().and_then(|rs| {
                let made = board::Board::new(&rs.device);
                match &made {
                    Some(b) => {
                        // Табло тоже не должно попадать в запись экрана —
                        // именно на нём и написаны подсказки. Отладочный флаг
                        // снимает это, иначе табло нечем сфотографировать.
                        if std::env::var_os("GHOST_SHOW_IN_CAPTURE").is_none() {
                            win::exclude_from_capture(b.hwnd());
                        }
                        log::info!("табло создано");
                    }
                    None => log::warn!("табло не создалось"),
                }
                made
            }),
            wgpu: cc.wgpu_render_state.clone(),
            strip_h: 120.0,
            placed: None,
            want_disk_shot: false,
            shot_asked: false,
            footer_h: 96.0,
            copied: None,
            confirm_stop: None,
            troubles: std::collections::HashMap::new(),
        }
    }

    /// Сбои — в журнал сессии.
    ///
    /// Без этого при разборе виден вопрос без ответа и непонятно, почему:
    /// человек промолчал, распознавание не сработало или отвалилась модель.
    /// Пишется только смена состояния: и поломка, и то, что она прошла.
    fn note_troubles(&mut self) {
        if !self.session.active() {
            return;
        }
        let failures = [
            (
                "распознавание",
                match &*self.stt.status.lock().unwrap() {
                    SttStatus::Failed(e) => Some(e.clone()),
                    _ => None,
                },
            ),
            (
                "модель",
                match &*self.track.llm.status.lock().unwrap() {
                    LlmStatus::Failed(e) => Some(e.clone()),
                    _ => None,
                },
            ),
            ("звук собеседника", audio_failure(&self.theirs)),
            ("мой микрофон", audio_failure(&self.mine)),
        ];

        for (who, failure) in failures {
            match failure {
                Some(text) => {
                    if self.troubles.get(who) != Some(&text) {
                        self.troubles.insert(who, text.clone());
                        self.session.note("error", who, &text);
                    }
                }
                None => {
                    if self.troubles.remove(who).is_some() {
                        self.session.note("error", who, "снова работает");
                    }
                }
            }
        }
    }

    /// Начинает сессию: с этого мгновения пишется звук и журнал, а окно
    /// начинает слушать и отвечать.
    fn start_session(&mut self) {
        if self.session.active() {
            return;
        }
        match self.session.start(&self.cfg, &self.theirs, &self.mine) {
            Ok(dir) => {
                // Беды, начавшиеся до сессии, в её журнале ещё не отмечены:
                // забываем, что уже писали, и записываем заново.
                self.troubles.clear();
                // Автоматический режим включается вместе с сессией: разговор
                // начинается с первой же фразы, и помнить в этот момент про
                // ещё одну кнопку человеку нечем.
                let auto = self.cfg.read().unwrap().input.auto;
                self.gates.listening.store(auto, std::sync::atomic::Ordering::Relaxed);
                self.note = Some(format!(
                    "сессия начата · {} · пишем в {}",
                    if auto { "авто-режим" } else { "авто-режим на паузе" },
                    dir.display()
                ));
            }
            Err(e) => {
                log::error!("сессия не начата: {e:#}");
                self.note = Some(format!("сессия не начата: {e}"));
            }
        }
    }

    /// Начать сессию или закончить её — одним действием, за кнопкой и за
    /// клавишей.
    ///
    /// Завершение спрашивает подтверждение: нажатие второй раз в течение
    /// [`STOP_CONFIRM`]. Начало — нет, начать лишнюю сессию не жалко.
    fn toggle_session(&mut self) {
        if !self.session.active() {
            self.start_session();
            return;
        }
        if self.confirm_stop.is_some_and(|at| at.elapsed() < STOP_CONFIRM) {
            self.stop_session();
        } else {
            self.confirm_stop = Some(Instant::now());
            self.note = Some("ещё раз — завершить сессию".into());
        }
    }

    /// Пауза и возврат автоматического режима.
    ///
    /// Пауза касается только модели: она перестаёт слышать и отвечать сама.
    /// Запись сессии при этом идёт дальше — на паузу ставят, чтобы не мешала
    /// подсказка, а не чтобы вырезать кусок из записи.
    fn toggle_auto(&mut self) {
        let on = self.gates.toggle_listening();
        if !on {
            // Недособранная реплика к разговору после паузы не относится:
            // отправить её значило бы ответить на то, что уже неактуально.
            self.merge = None;
        }
        // Пауза пишется в журнал: иначе при разборе будет кусок звука без
        // единой реплики, и не отличить молчание от паузы и от отвалившегося
        // распознавания.
        self.session.note("session", "авто", if on { "продолжение" } else { "пауза" });
        self.note = Some(if on {
            "авто-режим".into()
        } else {
            "авто-режим на паузе · запись идёт".into()
        });
        eprintln!("[ghost] авто-режим: {on}");
    }

    /// Завершает сессию: дописывает файлы и возвращает окно в состояние, где
    /// доступны только настройки.
    fn stop_session(&mut self) {
        let Some((dir, stats)) = self.session.stop() else { return };
        // Слушать после конца разговора незачем: иначе следующая сессия
        // началась бы с чужого хвоста в контексте.
        self.gates.listening.store(false, std::sync::atomic::Ordering::Relaxed);
        self.hold = None;
        self.merge = None;
        self.confirm_stop = None;
        self.note = Some(format!(
            "сессия сохранена: {} · {:.0} мин · вопросов {}",
            dir.display(),
            stats.minutes(),
            stats.questions
        ));
    }

    fn source_of(&self, who: Speaker) -> &Audio {
        match who {
            Speaker::Them => &self.theirs,
            Speaker::Me => &self.mine,
        }
    }

    fn on_down(&mut self, who: Speaker) {
        if self.hold.is_some() {
            return;
        }
        let pre_roll = self.cfg.read().unwrap().input.pre_roll_ms;
        let from = self.source_of(who).ring.lock().unwrap().back_off(pre_roll);
        self.hold = Some(Hold { who, from, pressed: Instant::now() });
    }

    fn on_up(&mut self, who: Speaker) {
        let Some(hold) = self.hold.take_if(|h| h.who == who) else {
            return;
        };
        let min_hold = Duration::from_millis(self.cfg.read().unwrap().input.min_hold_ms);
        if hold.pressed.elapsed() < min_hold {
            return; // это был шорткат активного приложения
        }

        let samples = self.source_of(who).ring.lock().unwrap().since(hold.from);

        if std::env::var_os("GHOST_DUMP_AUDIO").is_some() {
            match wav::dump(&PathBuf::from("dumps"), &samples) {
                Ok(p) => eprintln!("[ghost] дамп: {}", p.display()),
                Err(e) => eprintln!("[ghost] дамп не удался: {e:#}"),
            }
        }

        self.busy = true;
        self.stt.submit(who, false, false, samples);
    }

    fn on_screen(&mut self) {
        let started = Instant::now();

        let target = {
            let g = self.cfg.read().unwrap();
            crate::screen::Target::parse(&g.llm.screen_target)
        };

        match crate::screen::capture(1568, target) {
            Ok(shot) => {
                let took = started.elapsed().as_millis();
                let text = format!(
                    "{} · {}×{}, {} КБ, за {took} мс",
                    shot.source,
                    shot.width,
                    shot.height,
                    shot.png.len() / 1024
                );
                let track = &mut self.track;
                track.push(Author::Screen, "экран".into(), text, false);
                track.push(Author::Model, "подсказка".into(), String::new(), true);
                track.llm.look(shot.png);
            }
            Err(e) => {
                self.note = Some(format!("снимок не удался: {e:#}"));
                eprintln!("[ghost] снимок: {e:#}");
            }
        }
    }

    /// Подсказки живого помощника с другого устройства.
    fn drain_remote(&mut self) {
        while let Ok(text) = self.remote.messages.try_recv() {
            // До начала сессии подсказывать нечему: разговора нет, и в ленту
            // это легло бы без записи, к которой потом не вернуться.
            if !self.session.active() {
                eprintln!("[ghost] помощник до начала сессии, пропущено: {text}");
                continue;
            }
            eprintln!("[ghost] помощник: {text}");
            // В ручную дорожку: это подсказка человеку, а не ответ модели.
            self.track.push(Author::Helper, "помощник".into(), text.clone(), false);
            self.track.llm.hint(text);
        }
    }

    /// Реплики, нарезанные детектором речи в постоянном режиме.
    ///
    /// Канал вычитывается всегда, даже до начала сессии: нарезка идёт
    /// постоянно, и брошенная очередь через минуту отдала бы в распознавание
    /// разговор, которого никто не начинал.
    fn drain_utterances(&mut self) {
        while let Ok(utt) = self.utterances.try_recv() {
            if !self.session.active() {
                continue;
            }
            self.busy = true;
            self.stt.submit(utt.who, true, utt.partial, utt.pcm);
        }
    }

    /// Забирает готовый снимок окна и кладёт его на диск.
    /// Забирает готовый кадр и отдаёт его окну, а по требованию — ещё и на диск.
    ///
    /// Кадр приходит на следующем кадре после запроса — это и есть цена
    /// попиксельной прозрачности: одна перерисовка задержки, около 50 мс.
    fn present_frame(&mut self, ctx: &egui::Context) {
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = shot else {
            // Кадр не приходит сам: его надо попросить. Раньше этой просьбы не
            // было вовсе, и снимок молча не получался — ни по клавише, ни по
            // GHOST_SELFSHOT.
            if self.want_disk_shot && !self.shot_asked {
                self.shot_asked = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::UserData::default(),
                ));
            }
            return;
        };

        let [w, h] = image.size;
        self.shot_asked = false;
        if !std::mem::take(&mut self.want_disk_shot) {
            return;
        }

        let dir = std::path::PathBuf::from(&self.cfg.read().unwrap().dev.dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("{stamp}.png"));

        let result = std::fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|()| {
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(
                    image.as_raw(),
                    w as u32,
                    h as u32,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| e.to_string())?;
            std::fs::write(&path, png).map_err(|e| e.to_string())
        });

        self.note = Some(match result {
            Ok(()) => format!("снимок окна: {}", path.display()),
            Err(e) => format!("снимок не сохранён: {e}"),
        });
        eprintln!("[ghost] {}", self.note.as_deref().unwrap_or(""));
    }

    /// Мышь и фокус пускаются внутрь, только пока это нужно. Состояние
    /// выставляется в одном месте: разбросанные вызовы рассинхронизировались
    /// при выходе, и окно оставалось перехватывающим клики.
    fn sync_interactive(&mut self, ctx: &egui::Context) {
        let want = self.move_mode || self.settings.open;
        if want == self.interactive {
            return;
        }
        self.interactive = want;
        win::set_interactive(want);
        if want {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn visible_note(&self) -> Option<&str> {
        self.note.as_deref().filter(|_| self.note_at.elapsed() < NOTE_TTL)
    }

    /// Новая подсказка — счёт сказанного начинается заново.
    fn start_tracking(&mut self) {
        self.said.clear();
        self.said_checked.clear();
    }

    /// Сверяет пункты подсказки со сказанным, когда вы договорили.
    fn check_coverage_if_ready(&mut self) {
        let cfg = self.cfg.read().unwrap().coverage.clone();
        if !cfg.enabled || self.said.trim().is_empty() || self.said == self.said_checked {
            return;
        }
        if self.said_at.elapsed() < Duration::from_millis(cfg.debounce_ms) {
            return;
        }

        let track = &self.track;
        let Some(msg) = track.tracked.and_then(|i| track.chat.get(i)) else {
            return;
        };
        let points: Vec<String> = msg
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if points.is_empty() {
            return;
        }

        eprintln!("[ghost] отправляю сверку по {} пунктам", points.len());
        self.said_checked = self.said.clone();
        track.llm.check_coverage(points, self.said.clone());
    }

    /// Отправляет то, что набрано или наговорено в строку ввода.
    fn submit_input(&mut self) {
        let text = self.input_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.session.active() {
            self.note = Some("сначала начните сессию".into());
            return;
        }
        self.input_text.clear();

        // Незаконченная сборка чужой реплики к нашему вопросу отношения не
        // имеет: отправляем её, чтобы она не приклеилась следом.
        self.flush_merge();

        self.session.note("said", "я", &text);
        self.remote.publish("said", "я", &text);
        self.remember_question("я", &text);
        self.track.open_pair(Author::Me, "я".into(), text.clone());
        self.track.push(Author::Model, "подсказка".into(), String::new(), true);
        self.track.llm.ask(true, "я".into(), text);
    }

    /// Отмечает последнюю подсказку негодной.
    ///
    /// Единственная оценка в записи. Ложится в журнал строкой рядом с
    /// ответом, а в общий банк — пометкой к паре: там подсказки за все сессии,
    /// и по этой отметке потом видно, на каких вопросах модель промахивается.
    /// Разбирать и переразбирать записи есть смысл в первую очередь по ним.
    fn rate_miss(&mut self) {
        if !self.session.active() {
            self.note = Some("сначала начните сессию".into());
            return;
        }
        let Some(msg) = self.track.tracked.and_then(|i| self.track.chat.get(i)) else {
            self.note = Some("нечего отмечать: подсказки ещё не было".into());
            return;
        };
        // Первые слова ответа — чтобы при разборе журнала было видно, какую
        // именно подсказку забраковали, не сверяясь по времени.
        let head: String = msg.text.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
        self.session.note("rate", "я", &format!("мимо: {head}"));
        self.session.bank_rate(&self.last_asker, &self.last_question);
        self.note = Some("отмечено: подсказка мимо".into());
        eprintln!("[ghost] подсказка отмечена негодной");
    }

    fn publish_answer(&self, a: &Answered) {
        // Обстоятельства ответа — не в сам текст, а отдельной строкой рядом:
        // текст читают вслух, а модель и задержка нужны только при разборе.
        self.session.note("llm", "подсказка", &a.text);
        self.session.note(
            "meta",
            &a.model,
            &format!(
                "токенов {}→{}, ${:.4}, первый через {} мс",
                a.usage.input, a.usage.output, a.usage.cost, a.first_ms
            ),
        );
        self.remote.publish("llm", "подсказка", &a.text);
    }

    /// Отправляет собранный вопрос, когда говорящий замолчал окончательно.
    fn flush_merge_if_ready(&mut self) {
        let vad = self.cfg.read().unwrap().vad.clone();
        let ready = self.merge.as_ref().is_some_and(|m| {
            let window = if m.finished { vad.merge_ms } else { vad.merge_open_ms };
            m.since.elapsed() >= Duration::from_millis(window)
        });
        if ready {
            self.flush_merge();
        }
    }

    fn flush_merge(&mut self) {
        let Some(m) = self.merge.take() else { return };
        self.remember_question(&m.label, &m.text);
        let track = &mut self.track;
        track.llm.ask(!m.auto, m.label, m.text);
    }

    /// Запоминает вопрос, чтобы после ответа положить пару в банк.
    fn remember_question(&mut self, who: &str, text: &str) {
        self.last_asker = who.to_string();
        self.last_question = text.to_string();
        self.session.count_question();
    }

    fn drain_stt(&mut self) {
        while let Ok(event) = self.stt.events.try_recv() {
            match event {
                SttEvent::Started => {}
                SttEvent::Done { who, auto, label, partial, text, speaker_turn, took, audio_secs } => {
                    self.busy = false;
                    // Пока реплика распознавалась, режим успели поставить на
                    // паузу. Отвечать на неё поздно: человек нажал паузу
                    // именно для того, чтобы подсказки не появлялись.
                    if auto && !self.gates.listening() {
                        continue;
                    }
                    if text.is_empty() {
                        // В постоянном режиме пустых кусков много: шум, вздох,
                        // щелчок. Засорять ими ленту незачем.
                        if !auto && !partial {
                            let track = &mut self.track;
                            track.push(
                                Author::of(who),
                                label,
                                "(речь не распознана)".into(),
                                false,
                            );
                        }
                        continue;
                    }

                    // Догадка по недоговорённой фразе: обновляем ту же пару
                    // сообщений и просим черновик, не трогая историю.
                    if partial {
                        // Проверка та же, что и для законченной реплики: иначе
                        // собственная речь порождала черновики, хотя отвечать
                        // на неё никто не просил.
                        let wanted = {
                            let v = self.cfg.read().unwrap().vad.clone();
                            match who {
                                Speaker::Them => v.answer_them,
                                Speaker::Me => v.answer_me,
                            }
                        };
                        if !wanted {
                            continue;
                        }
                        let track = &mut self.track;
                        track.open_pair(Author::of(who), label.clone(), text.clone());
                        track.llm.speculate(!auto, label, text);
                        continue;
                    }

                    eprintln!("[ghost] {label} ({}): {text}", if auto { "авто" } else { "ручной" });
                    self.session.note("said", &label, &text);
                    self.remote.publish("said", &label, &text);

                    // Своя речь копится отдельно: по ней сверяется, что из
                    // подсказки уже прозвучало.
                    if who == Speaker::Me {
                        if !self.said.is_empty() {
                            self.said.push(' ');
                        }
                        self.said.push_str(&text);
                        self.said_at = Instant::now();
                    }

                    let answer = if auto {
                        let v = self.cfg.read().unwrap().vad.clone();
                        match who {
                            Speaker::Them => v.answer_them,
                            Speaker::Me => v.answer_me,
                        }
                    } else {
                        // Удержание клавиши — это всегда явный запрос ответа.
                        true
                    };

                    let stt_line = format!(
                        "аудио {audio_secs:.1} с · {:.1} с на распознавание",
                        took.as_secs_f32()
                    );
                    let track = &mut self.track;
                    track.timing = Some(stt_line);

                    if !answer {
                        // Ответа не ждём, но модель должна знать, что было
                        // сказано: иначе предложит то же самое.
                        track.push(Author::of(who), label.clone(), text.clone(), false);
                        track.open = None;
                        track.llm.note(label, text);
                        continue;
                    }

                    // Записанное по клавише не уходит в модель сразу: сначала
                    // оно попадает в строку ввода, где можно поправить
                    // искажённое слово или дописать мысль.
                    if !auto {
                        if !self.input_text.trim().is_empty() {
                            self.input_text.push(' ');
                        }
                        self.input_text.push_str(&text);
                        // Курсор ставим сразу: записанное почти всегда хочется
                        // поправить, а не отправлять как есть.
                        self.focus_input = true;
                        win::focus();
                        continue;
                    }

                    // Кусок того же говорящего продолжает вопрос, а не начинает
                    // новый: дособираем и ждём дальше.
                    // Проверка отдельно от ветвления: `accepts` меняет
                    // состояние сборки, а в охране образца изменяемое
                    // заимствование запрещено.
                    let continues =
                        self.merge.as_mut().is_some_and(|m| m.accepts(who, &label, auto));
                    match self.merge.as_mut() {
                        Some(m) if continues => {
                            m.text.push(' ');
                            m.text.push_str(&text);
                            m.since = Instant::now();
                            m.finished = looks_finished(&m.text);
                            let merged = m.text.clone();
                            let track = &mut self.track;
                            track.open_pair(Author::of(who), label, merged);
                        }
                        _ => {
                            // Заговорил другой — прошлый вопрос закончен.
                            self.flush_merge();
                            let track = &mut self.track;
                            track.open_pair(Author::of(who), label.clone(), text.clone());
                            let finished = looks_finished(&text);
                            self.merge = Some(Merge {
                                who,
                                label,
                                text,
                                auto,
                                since: Instant::now(),
                                finished,
                                doubt: None,
                            });
                        }
                    }

                    // Распознавание считает, что дальше говорит другой:
                    // это точнее нашего порога тишины, ждать больше нечего.
                    if speaker_turn {
                        self.flush_merge();
                    }
                }
                SttEvent::Failed(e) => {
                    self.busy = false;
                    self.note = Some(format!("ошибка распознавания: {e}"));
                }
            }
        }
    }

    fn drain_keys(&mut self, ctx: &egui::Context) {
        while let Ok(key) = self.keys.try_recv() {
            match key {
                Key::Captured { vk, ctrl, alt, shift } => {
                    self.settings.on_captured(&self.cfg, vk, ctrl, alt, shift);
                }
                Key::Settings => {
                    // Комбинация начиналась с клавиши разговора — снимаем
                    // возможное недоведённое удержание.
                    self.hold = None;
                    self.settings.toggle(&self.cfg);
                }
                Key::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Key::Session => self.toggle_session(),
                Key::Rate => self.rate_miss(),
                Key::MoveWindow => {
                    self.move_mode = !self.move_mode;
                    eprintln!("[ghost] режим перемещения: {}", self.move_mode);
                    self.note = Some(if self.move_mode {
                        "тяните за полосу или края; «закрепить» или Esc — выход".into()
                    } else {
                        "окно закреплено".into()
                    });
                }
                Key::Screenshot => {
                    if self.cfg.read().unwrap().dev.enabled {
                        // Кадр берётся у самого интерфейса, а не с экрана,
                        // поэтому исключение из захвата трогать не нужно.
                        self.want_disk_shot = true;
                    } else {
                        self.note = Some("режим разработчика выключен".into());
                    }
                }
                // Дальше идёт всё, что относится к разговору. Пока сессия не
                // начата, разговора нет: записывать его некуда, и клавиши
                // молчат, а не делают вид, что работают.
                _ if !self.session.active() => {
                    self.hold = None;
                    self.note = Some("сначала начните сессию".into());
                }
                Key::Listen => self.toggle_auto(),
                Key::Remote => {
                    let on = self.remote.toggle();
                    self.note = Some(if on {
                        format!("канал помощника открыт: {}", self.remote.url())
                    } else {
                        "канал помощника закрыт".into()
                    });
                    eprintln!("[ghost] канал помощника: {on}");
                }
                Key::Input => {
                    self.focus_input = true;
                    win::focus();
                    self.note = Some("Ctrl+Enter — отправить".into());
                }
                Key::Mute => {
                    let mut guard = self.cfg.write().unwrap();
                    guard.remote.audio = !guard.remote.audio;
                    let on = guard.remote.audio;
                    drop(guard);
                    self.note = Some(if on {
                        "помощник снова слышит".into()
                    } else {
                        "помощник не слышит".into()
                    });
                    eprintln!("[ghost] звук помощнику: {on}");
                }
                Key::Reset => {
                    self.track.reset();
                    self.merge = None;
                    self.note = Some("контекст сброшен на обеих дорожках".into());
                }
                Key::HoldCancel => self.hold = None,
                // Отпускание и отмену обрабатываем всегда, даже при открытых
                // настройках: иначе удержание залипает и запись идёт вечно.
                Key::TheirsUp => self.on_up(Speaker::Them),
                Key::MineUp => self.on_up(Speaker::Me),
                // А вот начинать запись, пока настраивают, незачем: человек
                // печатает, а не отвечает собеседнику.
                _ if self.settings.open => {}
                Key::TheirsDown => self.on_down(Speaker::Them),
                Key::MineDown => self.on_down(Speaker::Me),
                Key::Screen => self.on_screen(),
            }
        }
    }

    /// Кладёт ссылку в буфер обмена: сначала локальную, а как только поднимется
    /// туннель — публичную вместо неё. Иначе адрес пришлось бы перепечатывать
    /// с экрана руками, а он длинный и со случайным токеном.
    fn sync_clipboard(&mut self, ctx: &egui::Context) {
        if !self.remote.active() {
            self.copied = None;
            return;
        }

        let url = self.remote.url();
        if self.copied.as_deref() == Some(url.as_str()) {
            return;
        }
        ctx.copy_text(url.clone());
        self.note = Some(match self.remote.tunnel() {
            Tunnel::Ready(_) => "публичная ссылка скопирована".into(),
            _ => "ссылка скопирована (локальная сеть)".into(),
        });
        self.copied = Some(url);
    }

    /// Окно — источник правды по геометрии: его двигают и растягивают мышью,
    /// а конфиг лишь запоминает результат. Обратной подачи нет — иначе вышла бы
    /// петля «конфиг двигает окно, окно правит конфиг».
    fn sync_geometry(&mut self, ctx: &egui::Context) {
        // Пока открыты настройки, окно стоит на месте табло и его размеры —
        // не размеры оверлея. Записывать их нельзя: площадь схлопывалась бы на
        // высоту полосы при каждом заходе в настройки.
        if self.settings.open {
            return;
        }
        if !self.move_mode {
            // В обычном режиме окно — лишь нижняя полоса, и пересчитывать её
            // размеры в размеры оверлея каждый кадр нельзя: система отвечает
            // не тем прямоугольником, который мы просили, и окно уползает.
            // Поэтому положение запоминаем один раз — когда полосу отпустили
            // после перетаскивания.
            // Сравнивать надо внешний прямоугольник с внешним: winit оставляет
            // окну рамку, поэтому внутренний всегда смещён на неё, и по нему
            // окно «переезжало» само при каждом запуске.
            let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else {
                return;
            };
            let dragging = ctx.input(|i| i.pointer.any_down());
            let known = self.cfg.read().unwrap().overlay.height;
            let moved = self
                .placed
                .is_some_and(|p| (p.0 - outer.min.x).abs() > 4.0 || (p.1 - outer.min.y).abs() > 4.0);
            if dragging || !moved {
                return;
            }
            let mut guard = self.cfg.write().unwrap();
            guard.overlay.x = outer.min.x;
            guard.overlay.y = outer.max.y - known;
            self.placed = Some((outer.min.x, outer.min.y, outer.width(), outer.height()));
            return;
        }
        // Внешний прямоугольник, а не внутренний: иначе окно съезжает вниз на
        // толщину рамки при каждой перерисовке.
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else {
            return;
        };
        let (x, y, w, h) = (outer.min.x, outer.min.y, outer.width(), outer.height());
        let mut guard = self.cfg.write().unwrap();
        let o = &mut guard.overlay;
        if (o.x - x).abs() > 0.5
            || (o.y - y).abs() > 0.5
            || (o.width - w).abs() > 0.5
            || (o.height - h).abs() > 0.5
        {
            (o.x, o.y, o.width, o.height) = (x, y, w, h);
        }
    }

    /// Полоса перетаскивания и рамки изменения размера. Только в режиме
    /// настройки: в обычном окно click-through и мышь до него не доходит.
    fn resize_handles(&self, ctx: &egui::Context) {
        use egui::{Rect, ResizeDirection as D};

        const EDGE: f32 = 8.0;
        let r = ctx.viewport_rect();
        let (l, t, rt, b) = (r.min.x, r.min.y, r.max.x, r.max.y);

        // Углы идут последними: при наложении выигрывает тот, кого добавили
        // позже, а тянуть за угол нужнее, чем за край.
        // Курсор намеренно не меняем. Окно исключено из захвата экрана, а
        // курсор рисует система, и он в запись попадает: стрелки изменения
        // размера, ползающие по пустому месту, выдают наличие окна.
        let zones: [(Rect, D); 8] = [
            (Rect::from_min_max(egui::pos2(l, t), egui::pos2(l + EDGE, b)), D::West),
            (Rect::from_min_max(egui::pos2(rt - EDGE, t), egui::pos2(rt, b)), D::East),
            (Rect::from_min_max(egui::pos2(l, t), egui::pos2(rt, t + EDGE)), D::North),
            (Rect::from_min_max(egui::pos2(l, b - EDGE), egui::pos2(rt, b)), D::South),
            (Rect::from_min_max(egui::pos2(l, t), egui::pos2(l + EDGE, t + EDGE)), D::NorthWest),
            (Rect::from_min_max(egui::pos2(rt - EDGE, t), egui::pos2(rt, t + EDGE)), D::NorthEast),
            (Rect::from_min_max(egui::pos2(l, b - EDGE), egui::pos2(l + EDGE, b)), D::SouthWest),
            (Rect::from_min_max(egui::pos2(rt - EDGE, b - EDGE), egui::pos2(rt, b)), D::SouthEast),
        ];

        egui::Area::new(egui::Id::new("ghost_resize"))
            .order(egui::Order::Foreground)
            .fixed_pos(r.min)
            .show(ctx, |ui| {
                for (i, (rect, dir)) in zones.into_iter().enumerate() {
                    let resp = ui.interact(rect, ui.id().with(i), egui::Sense::drag());
                    if resp.drag_started() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
                    }
                }
            });
    }
}

impl eframe::App for Ghost {
    /// Окно управления обычное и непрозрачное: прозрачность живёт на табло,
    /// куда рисуем мы сами.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [13.0 / 255.0, 14.0 / 255.0, 18.0 / 255.0, 1.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_keys(ctx);
        self.sync_interactive(ctx);
        self.drain_utterances();
        self.drain_remote();
        self.drain_stt();
        if let Some(a) = self.track.drain() {
            self.session.count_answer(a.usage.input, a.usage.output, a.usage.cost);
            self.session.bank(&self.last_asker, &self.last_question, &a.text);
            self.session.save_stats();
            self.publish_answer(&a);
            self.start_tracking();
        }
        self.note_troubles();
        self.check_coverage_if_ready();
        self.present_frame(ctx);
        self.flush_merge_if_ready();

        // Ctrl+Enter ловим здесь, до отрисовки: поле ввода многострочное и
        // само съело бы Enter, вставив перевод строки.
        if self.input_focused {
            let send = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter));
            if send {
                self.submit_input();
            }
        }

        // В режиме перемещения окно в фокусе, поэтому Escape до нас доходит.
        if self.move_mode && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.move_mode = false;
        }

        if self.note != self.note_shown {
            self.note_shown = self.note.clone();
            self.note_at = Instant::now();
        }
        self.sync_geometry(ctx);
        self.sync_clipboard(ctx);

        // Сторож: и исключение из захвата, и стиль служебного окна winit
        // сбрасывает при пересоздании окна и при смене любых его свойств.
        if self.last_check.elapsed() >= Duration::from_secs(3) {
            self.last_check = Instant::now();
            if win::affinity_lost(self.affinity) {
                self.restores += 1;
                self.affinity = win::reapply();
                log::warn!("исключение из захвата потеряно, восстановлено: {:?}", self.affinity);
            }
            if win::keep_tool_window() {
                log::warn!("стиль служебного окна был затёрт, возвращён");
            }
            let state = win::describe();
            if state != self.window_state {
                log::info!("окно: {state}");
                self.window_state = state;
            }
        }

        // Кадр запрашиваем каждый раз: именно он и есть то, что увидит
        // пользователь — окно рисуется не показом swapchain, а передачей
        // готовой картинки системе.
        // Значок заводим при первой отрисовке: раньше окна табло ещё нет, а
        // сообщения значка приходят именно в него.
        if self.tray.is_none() {
            if let Some(b) = &self.board {
                self.tray = tray::Tray::new(b.hwnd());
                if self.tray.is_none() {
                    log::warn!("значок в трее не завёлся");
                }
            }
        }
        if tray::Tray::take_toggle() {
            self.hidden = !self.hidden;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!self.hidden));
        }
        // Вторая копия попросила показаться и вышла: значит, ярлык нажали,
        // не найдя окна глазами.
        if tray::Tray::take_show() && self.hidden {
            self.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            log::info!("вторая копия попросила показать окно");
        }
        if tray::Tray::take_quit() {
            self.quit = true;
        }
        // Закрыться могут и мимо наших кнопок — из трея, по клавише, командой
        // системы. Сессию надо закончить в любом из этих случаев: незакрытый
        // WAV остаётся с длиной от последнего сброса, а итоги — ненаписанными.
        if (self.quit || ctx.input(|i| i.viewport().close_requested()))
            && self.session.active()
        {
            self.stop_session();
        }
        if self.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.paint_board(ctx);
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (opacity, font) = {
            let guard = self.cfg.read().unwrap();
            (guard.overlay.opacity, guard.overlay.font_size)
        };
        let _ = opacity;

        egui::Frame::default()
            .fill(egui::Color32::from_rgb(13, 14, 18))
            .corner_radius(12.0)
            .inner_margin(14.0)
            .show(ui, |ui| {
                if self.move_mode || self.settings.open {
                    self.resize_handles(ui.ctx());
                }
                if self.settings.open {
                    let cfg = self.cfg.clone();
                    // Разбор ходит через модель: настройки только просят и
                    // показывают, а очередь запросов живёт здесь.
                    if let Some(text) = self.track.review.take() {
                        self.settings.take_review(text);
                    }
                    if let Some(transcript) = self.settings.review_request.take() {
                        self.track.llm.review(transcript);
                    }
                    let Self { settings, theirs, mine, session, stt, .. } = self;
                    settings.ui(ui, &cfg, &settings::Ctx { theirs, mine, session, stt });
                    return;
                }

                // Окно управления — только то, с чем работают руками. Шапка и
                // лента живут на табло: там нет мыши, зато есть настоящая
                // прозрачность.
                if self.move_mode && move_bar(ui, font) {
                    self.move_mode = false;
                }
                apply_font(ui, font);
                match self.choosing {
                    // Выбор занимает место строки ввода, но не меняет высоты
                    // полосы: окна остаются там же, где стояли.
                    Some(kind) => {
                        let room = (ui.available_height() - self.footer_h - 10.0).max(40.0);
                        ui.allocate_ui(egui::vec2(ui.available_width(), room), |ui| {
                            ui.set_min_height(room);
                            self.chooser(ui, font, kind);
                        });
                    }
                    None if self.session.active() => {
                        self.input_h =
                            ui.scope(|ui| self.input_row(ui, font)).response.rect.height();
                    }
                    // Сессия не начата: вместо строки ввода — кнопка начала.
                    // Спрашивать до неё нечего, а ответ было бы некуда записать.
                    None => {
                        self.input_h =
                            ui.scope(|ui| self.start_row(ui, font)).response.rect.height();
                    }
                }
                self.footer_h = ui.scope(|ui| self.footer(ui, font)).response.rect.height();
                // Высота полосы нужна снаружи: по ней делится площадь между
                // табло и этим окном. Пока идёт выбор, она не пересчитывается.
                if self.choosing.is_none() {
                    self.strip_h = self.input_h + self.footer_h + 34.0;
                }
            });

        // Курсор не меняем никогда: он рисуется системой и попадает в запись
        // экрана, хотя окно из неё исключено. Любая реакция на наведение выдала
        // бы, что под курсором что-то есть.
        ui.ctx().set_cursor_icon(egui::CursorIcon::Default);

        // Маску собрали виджеты, а не мы: сюда она приходит готовой.
        let scale = ui.ctx().pixels_per_point();
        let mask = take_mask(ui.ctx());
        if mask != self.mask {
            log::info!(
                "кликабельных областей {}, режим {}",
                mask.len(),
                if self.interactive { "окно целиком" } else { "по областям" }
            );
            self.mask = mask.clone();
        }
        // Громкий отказ вместо тихого. egui знает, что мышь над чем-то
        // отзывчивым; если этой точки нет в маске, значит виджет добавлен мимо
        // `claim` — он нарисуется, но кликов не получит. Раньше это выяснялось
        // только со слов пользователя.
        // Точка вне окна — это устаревшее положение курсора с прошлого кадра,
        // а не забытая область: сразу после смены размеров окна egui ещё помнит
        // старое.
        let window = ui.ctx().viewport_rect();
        if !self.interactive && ui.ctx().egui_wants_pointer_input() {
            if let Some(p) = ui.ctx().pointer_latest_pos().filter(|p| window.contains(*p)) {
                if !mask.contains(p) {
                    log::warn!(
                        "в точке {:.0},{:.0} виджет реагирует на мышь, но область не объявлена",
                        p.x,
                        p.y
                    );
                }
            }
        }

        // Снимок собственного кадра по требованию: единственный способ увидеть
        // подложку, когда окно исключено из захвата и обычным скриншотом не
        // снимается. Полторы секунды — чтобы окно успело встать на место.
        if self.selfshot_at.is_some_and(|at| Instant::now() >= at) {
            self.selfshot_at = None;
            self.want_disk_shot = true;
        }

        win::set_hot_rects(&mask.physical(scale));

        // Сквозной режим держится на бите стиля, а не на ответе хит-теста:
        // курсор опрашивается сам, потому что в сквозном состоянии окно
        // сообщений о мыши не получает и знать её положение иначе не может.
        let physical = mask.physical(scale);
        let over = win::cursor_in_client().is_some_and(|(x, y)| {
            win::grip_contains(x, y)
                || physical.iter().any(|&(l, t, r, b)| x >= l && x < r && y >= t && y < b)
        });
        // Состояние не кэшируется, а сверяется с настоящим каждый кадр: winit
        // пересчитывает ex-стиль при любом изменении свойств окна и затирает
        // наш бит. Запомненное «уже поставили» после этого означало бы, что мы
        // больше никогда его не вернём.
        let catch = over || self.interactive;
        if win::set_click_through(!catch) {
            log::debug!("мышь: {}", if catch { "ловим" } else { "пропускаем сквозь" });
        }
        self.catching = catch;
    }
}

/// Сколько ждёт подтверждения кнопка завершения, прежде чем передумать.
const STOP_CONFIRM: Duration = Duration::from_secs(4);

/// Закрыть и настройки — единственные две кнопки, доступные всегда, в том
/// числе до начала сессии. Рисуются в раскладке справа налево.
fn strip_buttons(
    ui: &mut egui::Ui,
    font: f32,
    close_app: &mut bool,
    open_settings: &mut bool,
) {
    let close = chip(
        ui,
        "\u{00d7}",
        egui::vec2(30.0, 26.0),
        egui::Color32::from_rgb(48, 30, 32),
        egui::Color32::from_rgb(226, 140, 140),
        font * 0.9,
    );
    if close.clicked() {
        *close_app = true;
    }

    let menu = chip(
        ui,
        "\u{2026}",
        egui::vec2(30.0, 26.0),
        egui::Color32::from_rgb(30, 30, 38),
        egui::Color32::from_gray(180),
        font * 0.8,
    );
    if menu.clicked() {
        *open_settings = true;
    }
}

/// Беда с устройством записи, если она есть.
fn audio_failure(audio: &Audio) -> Option<String> {
    match &*audio.status.lock().unwrap() {
        AudioStatus::Failed(e) => Some(e.clone()),
        _ => None,
    }
}

/// Скругление и заливка карточки: один вид на все поверхности.
fn card(fill: egui::Color32) -> egui::Frame {
    egui::Frame::default().fill(fill).corner_radius(10.0).inner_margin(12.0)
}

/// Круглая или скруглённая кнопка. Возвращает отклик, чтобы вызывающий мог
/// пометить её область кликабельной.
/// Области, по которым окно принимает клики.
///
/// Живёт в памяти egui, а не в полях приложения. Это важно: разметка строится
/// из вложенных замыканий, и поле потребовало бы протаскивать `&mut self`
/// сквозь каждое из них — а значит, появились бы обходные пути и второй способ
/// пометить область. Здесь способ ровно один: [`claim`]. Что не прошло через
/// него — рисуется, но кликов не получает; что прошло — кликается ровно там,
/// где нарисовано, потому что прямоугольник берётся у самого виджета.
#[derive(Clone, Default, PartialEq)]
pub struct Mask {
    rects: Vec<egui::Rect>,
}

impl Mask {
    /// Точка внутри интерактивной области.
    pub fn contains(&self, point: egui::Pos2) -> bool {
        self.rects.iter().any(|r| r.contains(point))
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    /// В физические пиксели клиента — в них Windows задаёт вопрос о попадании.
    /// Границы расширяются наружу: полпикселя недобора по краю кнопки
    /// пользователь воспринимает как «не нажалось».
    pub fn physical(&self, scale: f32) -> Vec<(i32, i32, i32, i32)> {
        self.rects
            .iter()
            .map(|r| {
                (
                    (r.left() * scale).floor() as i32,
                    (r.top() * scale).floor() as i32,
                    (r.right() * scale).ceil() as i32,
                    (r.bottom() * scale).ceil() as i32,
                )
            })
            .collect()
    }
}

fn mask_id() -> egui::Id {
    egui::Id::new("ghost-mask")
}

/// Объявляет область кликабельной. Единственный способ это сделать.
pub fn claim(ctx: &egui::Context, rect: egui::Rect) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Mask>(mask_id()).rects.push(rect));
}

/// Забирает собранную за кадр маску и очищает накопитель.
fn take_mask(ctx: &egui::Context) -> Mask {
    ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Mask>(mask_id())))
}

/// Просвет между табло и полосой управления: два окна вплотную читаются как
/// одно неаккуратное.
const GAP: f32 = 8.0;



/// Что сейчас выбирают в полосе.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chooser {
    /// Модель — сразу для обеих дорожек, ручной и постоянной.
    Model,
    /// Источник звука собеседника.
    Device,
}

/// Заголовок внутри выпадающего списка.
fn menu_title(ui: &mut egui::Ui, text: &str, size: f32) {
    ui.label(egui::RichText::new(text).size(size).color(egui::Color32::from_gray(130)));
}

/// Кнопка-кружок. Сама объявляет себя кликабельной: забыть об этом на месте
/// вызова нельзя, потому что вызывать нечего.
fn chip(
    ui: &mut egui::Ui,
    text: &str,
    size: egui::Vec2,
    fill: egui::Color32,
    fg: egui::Color32,
    font: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    claim(ui.ctx(), rect);
    let painter = ui.painter();
    let radius = size.y.min(size.x) * 0.32;
    painter.rect_filled(rect, radius, fill);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(font),
        fg,
    );
    response
}

/// Лента разговора.
fn pane(
    ui: &mut egui::Ui,
    track: &Track,
    font: f32,
    md: &mut CommonMarkCache,
) {
    let scale = ui.ctx().pixels_per_point();
    egui::ScrollArea::vertical()
        .id_salt("chat")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for msg in &track.chat {
                bubble(ui, msg, font, md, scale);
                ui.add_space(10.0);
            }
            if track.chat.is_empty() {
                ui.add_space(20.0);
                ui.label(
                    egui::RichText::new("здесь появятся вопросы и подсказки")
                        .color(egui::Color32::from_gray(66))
                        .size(font * 0.8),
                );
            }
        });
}

/// Разбирает строку ответа на знак-маркер и текст.
fn split_marker(line: &str) -> (Option<char>, &str) {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        // Знака ⌘ здесь больше нет: промпт его не просит, и иконки ему не
        // назначено — это остаток прежнего формата ответа.
        Some(c) if matches!(c, '\u{25b8}' | '\u{00b7}' | '\u{25be}' | '?') => {
            (Some(c), chars.as_str().trim_start())
        }
        _ => (None, trimmed),
    }
}

/// Кружок-иконка слева от строки ответа. У каждого вида строки свой знак,
/// чтобы вид сообщения читался раньше, чем текст.
fn line_icon(marker: Option<char>, covered: bool) -> Option<(&'static str, egui::Color32)> {
    const AMBER: egui::Color32 = egui::Color32::from_rgb(212, 168, 90);
    const DIM: egui::Color32 = egui::Color32::from_rgb(96, 104, 98);

    if covered {
        return Some(("\u{2713}", DIM));
    }
    match marker {
        Some('\u{00b7}') => Some(("\u{2022}", egui::Color32::from_gray(130))),
        Some('\u{25be}') => Some(("\u{2318}", AMBER)),
        Some('?') => Some(("?", AMBER)),
        _ => None,
    }
}

/// Одно сообщение ленты.
fn bubble(
    ui: &mut egui::Ui,
    msg: &Msg,
    font: f32,
    md: &mut CommonMarkCache,
    _scale: f32,
) {
    match msg.author {
        Author::Model | Author::Helper => model_card(ui, msg, font, md),
        _ => human_line(ui, msg, font),
    }
}

/// Реплика человека: аватар, время, обычный текст. Без карточки — так видно,
/// что это сказанное, а не подсказка.
fn human_line(ui: &mut egui::Ui, msg: &Msg, font: f32) {
    let accent = voice_color(msg.author, &msg.title);
    let initial: String = msg.title.chars().next().unwrap_or('?').to_uppercase().collect();

    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.circle_filled(rect.center(), 15.0, accent.linear_multiply(0.22));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(font * 0.8),
            accent,
        );

        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&msg.title).color(accent).size(font * 0.72),
                );
                ui.label(
                    egui::RichText::new(&msg.time)
                        .color(egui::Color32::from_gray(90))
                        .size(font * 0.7),
                );
            });
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&msg.text)
                        .color(egui::Color32::from_gray(206))
                        .size(font * 0.9),
                )
                .wrap(),
            );
        });
    });
}

/// Подсказка модели или живого помощника — карточкой.
fn model_card(ui: &mut egui::Ui, msg: &Msg, font: f32, md: &mut CommonMarkCache) {
    let helper = msg.author == Author::Helper;
    let accent = if helper {
        egui::Color32::from_rgb(110, 210, 196)
    } else {
        egui::Color32::from_rgb(226, 178, 96)
    };
    let fill = if helper {
        egui::Color32::from_rgba_unmultiplied(20, 40, 40, 210)
    } else {
        egui::Color32::from_rgba_unmultiplied(27, 27, 34, 215)
    };

    card(fill).show(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.circle_filled(rect.center(), 12.0, accent.linear_multiply(0.22));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if helper { "\u{263a}" } else { "\u{2726}" },
                egui::FontId::proportional(font * 0.8),
                accent,
            );

            ui.label(egui::RichText::new(&msg.title).color(accent).size(font * 0.74));
            if msg.draft {
                ui.label(
                    egui::RichText::new("· черновик")
                        .color(egui::Color32::from_gray(110))
                        .size(font * 0.7),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(&msg.time)
                        .color(egui::Color32::from_gray(90))
                        .size(font * 0.7),
                );
            });
        });
        ui.add_space(6.0);

        if msg.text.trim().is_empty() {
            ui.label(
                egui::RichText::new(if msg.streaming { "думает…" } else { "(пустой ответ)" })
                    .color(egui::Color32::from_gray(120))
                    .size(font * 0.85),
            );
            return;
        }
        if helper {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&msg.text)
                        .color(egui::Color32::from_rgb(198, 240, 232))
                        .size(font * 0.95),
                )
                .wrap(),
            );
            return;
        }

        apply_font(ui, font);
        let mut n = 0usize;
        for line in msg.text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let covered = !msg.covered.is_empty() && msg.covered.get(n).copied().unwrap_or(false);
            n += 1;
            answer_line(ui, line, covered, font, md);
            ui.add_space(4.0);
        }

        if let Some(add) = &msg.addendum {
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("+ {add}"))
                        .color(egui::Color32::from_rgb(150, 220, 180))
                        .size(font * 0.88),
                )
                .wrap(),
            );
        }
    });
}

/// Строка ответа с иконкой слева.
///
/// Произнесённое рисуется простым текстом без разметки: жирный внутри markdown
/// сохранял свой цвет и не тускнел, из-за чего закрытые пункты выглядели
/// такими же яркими, как оставшиеся.
fn answer_line(ui: &mut egui::Ui, line: &str, covered: bool, font: f32, md: &mut CommonMarkCache) {
    const DIM: egui::Color32 = egui::Color32::from_rgb(92, 99, 94);

    let (marker, body) = split_marker(line);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
        if let Some((glyph, color)) = line_icon(marker, covered) {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(font * 0.8),
                color,
            );
        }

        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width());
            if covered {
                let plain = body.replace("**", "");
                ui.add(
                    egui::Label::new(egui::RichText::new(plain).color(DIM).size(font * 0.92))
                        .wrap(),
                );
            } else {
                egui_commonmark::CommonMarkViewer::new().show(ui, md, body);
            }
        });
    });
}


/// Подключает системный шрифт символов запасным.
///
/// Во встроенном шрифте egui нет ни стрелок, ни галочек: маркеры формата
/// выводились пустыми квадратами. Обнаружилось это только снимком собственного
/// окна — глазами оно не проверялось, потому что окно исключено из захвата.
fn install_symbol_font(ctx: &egui::Context) {
    const PATH: &str = r"C:\Windows\Fonts\seguisym.ttf";

    let Ok(data) = std::fs::read(PATH) else {
        eprintln!("[ghost] шрифт символов не найден, маркеры могут не отрисоваться");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("symbols".into(), std::sync::Arc::new(egui::FontData::from_owned(data)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("symbols".into());
    }
    ctx.set_fonts(fonts);
}

/// Разным голосам — разный оттенок. Иначе «собеседник 1» и «собеседник 2»
/// отличаются только цифрой, а её надо вычитывать.
fn voice_color(author: Author, title: &str) -> egui::Color32 {
    if author != Author::Them {
        return author.color();
    }
    const HUES: [egui::Color32; 4] = [
        egui::Color32::from_rgb(150, 190, 230),
        egui::Color32::from_rgb(210, 170, 235),
        egui::Color32::from_rgb(150, 225, 215),
        egui::Color32::from_rgb(235, 180, 165),
    ];
    match title.rsplit(' ').next().and_then(|n| n.parse::<usize>().ok()) {
        Some(n) if n >= 1 => HUES[(n - 1) % HUES.len()],
        _ => author.color(),
    }
}

/// Полоса режима перемещения. Возвращает true, если попросили закрепить окно:
/// клавишу знать необязательно, а выйти надо уметь всегда.
fn move_bar(ui: &mut egui::Ui, font: f32) -> bool {
    let mut done = false;
    ui.horizontal(|ui| {
        let bar = ui.allocate_response(
            egui::vec2(ui.available_width() - 92.0, 22.0),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter();
        painter.rect_filled(bar.rect, 4.0, egui::Color32::from_gray(46));
        painter.text(
            egui::pos2(bar.rect.left() + 10.0, bar.rect.center().y),
            egui::Align2::LEFT_CENTER,
            "тяните за эту полосу или за края окна",
            egui::FontId::proportional(font * 0.72),
            egui::Color32::from_gray(150),
        );
        if bar.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if ui.button("закрепить").clicked() {
            done = true;
        }
    });
    ui.add_space(6.0);
    done
}

/// QR со ссылкой: помощнику проще навести камеру, чем принимать длинный адрес
/// со случайным токеном.
fn qr_code(ui: &mut egui::Ui, text: &str, px: f32) {
    let Ok(code) = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium) else {
        return;
    };

    // Тихая зона обязательна: без белых полей сканеры код не находят.
    const QUIET: i32 = 2;
    let modules = code.size();
    let total = (modules + QUIET * 2) as f32;
    let step = px / total;

    let (rect, _) = ui.allocate_exact_size(egui::vec2(px, px), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, egui::Color32::WHITE);

    for y in 0..modules {
        for x in 0..modules {
            if !code.get_module(x, y) {
                continue;
            }
            let min = egui::pos2(
                rect.min.x + (x + QUIET) as f32 * step,
                rect.min.y + (y + QUIET) as f32 * step,
            );
            // Округляем вверх, иначе между модулями остаются светлые щели.
            painter.rect_filled(
                egui::Rect::from_min_size(min, egui::vec2(step.ceil(), step.ceil())),
                0.0,
                egui::Color32::BLACK,
            );
        }
    }
}

/// egui_commonmark берёт размеры из стиля, а не из RichText, поэтому кегль
/// приходится задавать через текстовые стили.
fn apply_font(ui: &mut egui::Ui, font: f32) {
    use egui::{FontId, TextStyle};

    let style = ui.style_mut();
    style.text_styles.insert(TextStyle::Body, FontId::proportional(font));
    style.text_styles.insert(TextStyle::Monospace, FontId::monospace(font * 0.88));
    style.text_styles.insert(TextStyle::Heading, FontId::proportional(font * 1.25));
    style.text_styles.insert(TextStyle::Small, FontId::proportional(font * 0.75));
    style.text_styles.insert(TextStyle::Button, FontId::proportional(font * 0.9));
}

impl Ghost {
    /// Уголок перетаскивания. Рисуется поверх всего и работает в любом режиме:
    /// сама область объявлена системе заголовком окна, поэтому тащит его
    /// Windows, а нам остаётся только сообщить координаты.
    /// Площадка, за которую окно таскают. Стоит в строке ввода, правее кнопки
    /// отправки: в углу поверх содержимого она перекрывала то, что под ней.
    ///
    /// Клики она не принимает — область объявлена системе заголовком окна, и
    /// тащит его сама Windows.
    fn drag_grip(&self, ui: &mut egui::Ui, size: f32) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 6.0, egui::Color32::from_rgb(34, 34, 42));
        let c = rect.center();
        // Четыре точки — привычный признак «за это тянут».
        for (dx, dy) in [(-3.5, -3.5), (3.5, -3.5), (-3.5, 3.5), (3.5, 3.5)] {
            painter.circle_filled(
                egui::pos2(c.x + dx, c.y + dy),
                1.5,
                egui::Color32::from_gray(150),
            );
        }

        // Системе координаты нужны в физических пикселях клиента.
        let scale = ui.ctx().pixels_per_point();
        win::set_grip(
            (rect.left() * scale) as i32,
            (rect.top() * scale) as i32,
            (rect.right() * scale).ceil() as i32,
            (rect.bottom() * scale).ceil() as i32,
        );
    }


    /// Раскладывает окна и рисует табло.
    ///
    /// Площадь из настроек делится между двумя окнами: снизу полоса
    /// управления — её показывает eframe, всё выше — табло. Пока открыты
    /// настройки, окно управления занимает всё, а табло убирается.
    fn paint_board(&mut self, ctx: &egui::Context) {
        let (x, y, w, h, opacity, font) = {
            let g = self.cfg.read().unwrap();
            let o = &g.overlay;
            (o.x, o.y, o.width, o.height, o.opacity, o.font_size)
        };
        let scale = ctx.pixels_per_point();
        let full = self.settings.open || self.move_mode;
        let strip = self.strip_h.clamp(60.0, h);

        // Полосу двигаем только когда меняется задуманное положение. Двигать её
        // каждый кадр нельзя: система отвечает не тем же прямоугольником, что
        // мы просили, и окно уползает вниз кадр за кадром.
        let board_h = (h - strip - GAP).max(1.0);
        let want = if self.settings.open {
            // Настройки встают ровно на место табло и тех же размеров: они
            // читаются вместо ответов, а не вместо всего оверлея.
            (x, y, w, board_h)
        } else if self.move_mode {
            // В режиме перемещения окно занимает всю площадь — её и таскают.
            (x, y, w, h)
        } else {
            (x, y + h - strip, w, strip)
        };
        if self.placed.is_none_or(|p| p != want) {
            self.placed = Some(want);
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(want.0, want.1)));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(want.2, want.3)));
        }

        let Some(rs) = self.wgpu.clone() else { return };
        let mut board = self.board.take();
        if let Some(b) = &mut board {
            if self.hidden {
                b.hide();
                self.board = board;
                return;
            }
            // Табло привязано к настоящему положению полосы, а не к желаемому:
            // пользователь тащит полосу мышью, и табло должно ехать за ней.
            let strip_rect = ctx.input(|i| i.viewport().outer_rect);
            let (bx, by) = match strip_rect {
                Some(r) => (r.min.x, r.min.y - GAP - board_h),
                None => (x, y),
            };
            b.place(
                (bx * scale) as i32,
                (by * scale) as i32,
                (w * scale) as i32,
                (board_h * scale) as i32,
            );
            if full {
                b.hide();
            } else {
                b.show();
                let alpha = (opacity.clamp(0.0, 1.0) * 255.0) as u8;
                b.draw(
                    &rs.device,
                    &rs.queue,
                    (w * scale) as u32,
                    (board_h * scale) as u32,
                    scale,
                    |ui| self.board_ui(ui, font, alpha),
                );
            }
        }
        self.board = board;
    }

    /// Смена модели на ходу. Пишем и в файл: иначе выбор, сделанный посреди
    /// разговора, пропадёт при следующем запуске — а сделан он не случайно.
    fn choose_model(&mut self, manual: bool, model: String) {
        {
            let mut g = self.cfg.write().unwrap();
            if manual {
                g.llm.manual.model = model.clone();
            } else {
                g.llm.auto.model = model.clone();
            }
            let _ = g.save(std::path::Path::new(crate::config::PATH));
        }
        self.note = Some(format!(
            "{}: {model}",
            if manual { "ручная модель" } else { "модель прослушивания" }
        ));
    }

    /// Смена источника звука собеседника: поток переоткрывается сам.
    fn choose_device(&mut self, device: String) {
        self.theirs.select(&device);
        {
            let mut g = self.cfg.write().unwrap();
            g.audio.loopback_device = device.clone();
            let _ = g.save(std::path::Path::new(crate::config::PATH));
        }
        self.note = Some(if device.is_empty() {
            "собеседник: устройство по умолчанию".into()
        } else {
            format!("собеседник: {device}")
        });
    }

    /// Шапка: значок, состояние крупно, пояснение мелко, кнопка настроек.
    fn header(&mut self, ui: &mut egui::Ui, font: f32) {
        let min_hold = Duration::from_millis(self.cfg.read().unwrap().input.min_hold_ms);
        let armed = self.hold.as_ref().filter(|h| h.pressed.elapsed() >= min_hold);

        // Пока сессия не начата, всё остальное состояние неважно: ни запись,
        // ни прослушивание идти не могут, и показывать их значило бы обещать
        // работу, которой нет.
        let live = self.session.active();
        let (dot, title) = match (armed, self.busy) {
            _ if !live => (egui::Color32::from_gray(90), "сессия не начата".to_string()),
            (Some(h), _) => (
                egui::Color32::from_rgb(235, 70, 70),
                format!("запись · {:.1} с", h.pressed.elapsed().as_secs_f32()),
            ),
            (_, true) => (egui::Color32::from_rgb(235, 190, 80), "распознаю…".to_string()),
            _ if self.gates.listening() => {
                (egui::Color32::from_rgb(120, 200, 140), "слушаю".to_string())
            }
            // Пауза — это состояние идущей сессии, а не её отсутствие, и
            // спутать их нельзя: в одном случае запись идёт, в другом нет.
            _ => (egui::Color32::from_rgb(235, 190, 80), "пауза".to_string()),
        };
        let subtitle = self.visible_note().map(str::to_string).unwrap_or_else(|| {
            if !live {
                "нажмите «Начать сессию» — до неё доступны только настройки"
            } else if self.gates.listening() {
                "слушаю разговор и отвечаю сам"
            } else {
                "авто-режим на паузе · запись идёт · клавиша или текст работают"
            }
            .into()
        });

        ui.horizontal_top(|ui| {
            let (badge, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(badge, 9.0, egui::Color32::from_rgb(26, 26, 33));
            painter.circle_stroke(
                badge.center(),
                8.0,
                egui::Stroke::new(1.6, egui::Color32::from_rgb(198, 158, 88)),
            );
            painter.circle_filled(badge.center(), 3.0, egui::Color32::from_rgb(198, 158, 88));

            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .color(egui::Color32::from_gray(232))
                        .size(font * 1.05)
                        .strong(),
                );
                ui.horizontal(|ui| {
                    let (d, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(d.center(), 3.5, dot);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(subtitle)
                                .color(egui::Color32::from_gray(132))
                                .size(font * 0.74),
                        )
                        .wrap(),
                    );
                });
            });

        });

        ui.add_space(6.0);
        let watching = self.hold.as_ref().map_or(Speaker::Them, |h| h.who);
        self.level_meter(ui, watching);
    }

    /// Всё, что показывает табло: шапка и лента. Мышь сюда не доходит, поэтому
    /// ни одной кнопки здесь быть не должно — они живут в окне управления.
    fn board_ui(&mut self, ui: &mut egui::Ui, font: f32, alpha: u8) {
        apply_font(ui, font);
        egui::Frame::default()
            .fill(egui::Color32::from_rgba_unmultiplied(13, 14, 18, alpha))
            .corner_radius(12.0)
            .inner_margin(14.0)
            .show(ui, |ui| {
                self.header(ui, font);
                ui.add_space(10.0);
                let Self { track, md, .. } = self;
                pane(ui, track, font, md);
            });
    }

    /// Выбор модели или устройства на месте строки ввода.
    ///
    /// Полоса при этом не меняет размеров: всплывающий список пришлось бы
    /// обрезать краем окна или раздвигать под него окна, а от того и другого
    /// картинка дёргается.
    fn chooser(&mut self, ui: &mut egui::Ui, font: f32, kind: Chooser) {
        let size = font * 0.78;
        let mut pick_model: Option<(bool, String)> = None;
        let mut pick_device: Option<String> = None;
        let mut close = false;

        let row = card(egui::Color32::from_rgb(23, 23, 29))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(match kind {
                            Chooser::Model => "модель",
                            Chooser::Device => "откуда слушать собеседника",
                        })
                        .size(size * 0.85)
                        .color(egui::Color32::from_gray(130)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if chip(
                            ui,
                            "\u{00d7}",
                            egui::vec2(24.0, 22.0),
                            egui::Color32::from_rgb(30, 30, 38),
                            egui::Color32::from_gray(170),
                            size,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });

                egui::ScrollArea::vertical().max_height(ui.available_height()).show(ui, |ui| {
                    match kind {
                        Chooser::Model => {
                            let (manual, auto, models) = {
                                let g = self.cfg.read().unwrap();
                                (
                                    g.llm.manual.model.clone(),
                                    g.llm.auto.model.clone(),
                                    g.llm.models.clone(),
                                )
                            };
                            menu_title(ui, "ручная", size * 0.85);
                            for name in &models {
                                if ui.selectable_label(*name == manual, name).clicked() {
                                    pick_model = Some((true, name.clone()));
                                }
                            }
                            ui.add_space(4.0);
                            menu_title(ui, "автоматический режим", size * 0.85);
                            for name in &models {
                                if ui.selectable_label(*name == auto, name).clicked() {
                                    pick_model = Some((false, name.clone()));
                                }
                            }
                        }
                        Chooser::Device => {
                            let devices = self.theirs.devices.lock().unwrap().clone();
                            let chosen = self.theirs.selected();
                            if ui.selectable_label(chosen.is_empty(), "по умолчанию").clicked() {
                                pick_device = Some(String::new());
                            }
                            for name in &devices {
                                if ui.selectable_label(*name == chosen, name).clicked() {
                                    pick_device = Some(name.clone());
                                }
                            }
                        }
                    }
                });
            })
            .response
            .rect;
        claim(ui.ctx(), row);

        if let Some((manual, model)) = pick_model {
            self.choose_model(manual, model);
            close = true;
        }
        if let Some(device) = pick_device {
            self.choose_device(device);
            close = true;
        }
        if close {
            self.choosing = None;
        }
    }

    /// Строка ввода: единственное место окна, кроме кнопок, которое ловит мышь.
    /// Начало сессии. Пока её не нажали, окно только висит на экране: звук
    /// никуда не пишется, модель ничего не слышит, и доступны одни настройки.
    ///
    /// Кнопка стоит на месте строки ввода, а не рядом с ней: до начала сессии
    /// строка всё равно ничего не примет, а высота полосы так не меняется —
    /// окна остаются там же, где стояли.
    fn start_row(&mut self, ui: &mut egui::Ui, font: f32) {
        ui.add_space(8.0);
        let mut start = false;

        card(egui::Color32::from_rgb(23, 23, 29)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let grip = 34.0;
                let width = (ui.available_width() - grip - 6.0).max(120.0);
                let go = chip(
                    ui,
                    "\u{25b6}  Начать сессию",
                    egui::vec2(width, 40.0),
                    egui::Color32::from_rgb(38, 68, 48),
                    egui::Color32::from_rgb(150, 225, 170),
                    font * 0.95,
                );
                if go.clicked() {
                    start = true;
                }
                ui.add_space(6.0);
                self.drag_grip(ui, grip);
            });
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "с этого мгновения пишется звук и весь разговор целиком — \
разбор потом берётся отсюда. До начала доступны только настройки.",
                    )
                    .color(egui::Color32::from_gray(112))
                    .size(font * 0.7),
                )
                .wrap(),
            );
        });

        if start {
            self.start_session();
        }
    }

    fn input_row(&mut self, ui: &mut egui::Ui, font: f32) {
        ui.add_space(8.0);
        let mut send = false;

        const FIELD: egui::Color32 = egui::Color32::from_rgb(23, 23, 29);
        let row = card(FIELD)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let button = 34.0;
                    let width = (ui.available_width() - button - 96.0).max(80.0);
                    // Поле не выделяется на карточке: рамка снята, а фон тот
                    // же. Видимая граница здесь только мешала — нажимают всё
                    // равно по всей серой области.
                    let response = ui.add(
                        egui::TextEdit::multiline(&mut self.input_text)
                            .desired_rows(1)
                            .desired_width(width)
                            .frame(egui::Frame::NONE)
                            .background_color(FIELD)
                            .hint_text("Спросите текстом…")
                            .font(egui::FontId::proportional(font * 0.92)),
                    );
                    if self.focus_input {
                        response.request_focus();
                        self.focus_input = false;
                    }
                    self.input_focused = response.has_focus();

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.drag_grip(ui, button);
                        ui.add_space(6.0);
                        let go = chip(
                            ui,
                            "\u{2192}",
                            egui::vec2(button, button),
                            egui::Color32::from_rgb(150, 118, 58),
                            egui::Color32::from_rgb(28, 24, 16),
                            font,
                        );
                        if go.clicked() {
                            send = true;
                        }

                        ui.label(
                            egui::RichText::new(if self.input_focused {
                                "Ctrl+Enter"
                            } else {
                                "Ctrl+Alt+Enter"
                            })
                            .color(egui::Color32::from_gray(96))
                            .size(font * 0.7),
                        );
                    });
                });
            })
            .response
            .rect;

        // Вся карточка ввода кликабельна: иначе в поле не попасть. И клик по
        // любому её месту ставит курсор в поле — целиться в узкую строку
        // мышью неудобно, а промах выглядит как «не реагирует».
        claim(ui.ctx(), row);
        let card_hit = ui.interact(row, ui.id().with("input-card"), egui::Sense::click());
        if card_hit.clicked() && !self.input_focused {
            self.focus_input = true;
            win::focus();
        }
        if send {
            self.submit_input();
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui, font: f32) {
        let size = font * 0.7;
        ui.add_space(8.0);

        // Что показывать в подвале, решает одно: идёт сессия или нет. Пока
        // она не начата, менять модель и устройство незачем — всё это живёт
        // и в настройках, а здесь только сбивало бы с толку.
        let live = self.session.active();
        let confirming =
            self.confirm_stop.is_some_and(|at| at.elapsed() < STOP_CONFIRM);

        let mut toggle_listen = false;
        let mut open_settings = false;
        let mut open_chooser: Option<Chooser> = None;
        let mut close_app = false;
        let mut stop_hit = false;
        let mut rate_hit = false;
        ui.horizontal(|ui| {
            let (d, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            let ok = matches!(&*self.stt.status.lock().unwrap(), SttStatus::Ready { .. });
            ui.painter().circle_filled(
                d.center(),
                4.0,
                if ok {
                    egui::Color32::from_rgb(120, 200, 140)
                } else {
                    egui::Color32::from_rgb(235, 190, 80)
                },
            );

            if !live {
                ui.label(
                    egui::RichText::new("сессия не начата")
                        .color(egui::Color32::from_gray(150))
                        .size(size),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    strip_buttons(ui, font, &mut close_app, &mut open_settings);
                });
                return;
            }

            // Завершение стоит первым слева и не смешивается с прочими
            // кнопками: это единственное действие, которое нельзя отменить.
            let stop = chip(
                ui,
                &if confirming {
                    "завершить?".to_string()
                } else {
                    let secs = (self.session.stats().minutes() * 60.0) as u64;
                    format!("\u{25a0} {}:{:02}", secs / 60, secs % 60)
                },
                egui::vec2(96.0, 24.0),
                if confirming {
                    egui::Color32::from_rgb(118, 40, 44)
                } else {
                    egui::Color32::from_rgb(46, 26, 28)
                },
                if confirming {
                    egui::Color32::from_rgb(255, 226, 226)
                } else {
                    egui::Color32::from_rgb(232, 122, 122)
                },
                size,
            );
            if stop.clicked() {
                stop_hit = true;
            }
            ui.add_space(6.0);

            // «Мимо» — рядом с завершением: обе кнопки про запись, а не про
            // разговор. Отмечать есть что только когда подсказка уже была.
            if self.track.tracked.is_some() {
                let miss = chip(
                    ui,
                    "\u{2715} мимо",
                    egui::vec2(64.0, 24.0),
                    egui::Color32::from_rgb(40, 34, 26),
                    egui::Color32::from_rgb(210, 170, 110),
                    size,
                );
                if miss.clicked() {
                    rate_hit = true;
                }
                ui.add_space(6.0);
            }

            let (manual, auto, dev) = {
                let g = self.cfg.read().unwrap();
                (g.llm.manual.model.clone(), g.llm.auto.model.clone(), g.dev.enabled)
            };

            // Названия моделей длинные, а места в подвале нет: показываем
            // хвост после косой черты, полное имя видно в списке.
            let short = |name: &str| name.rsplit('/').next().unwrap_or(name).to_string();
            let label = if manual == auto {
                short(&manual)
            } else {
                format!("{} / {}", short(&manual), short(&auto))
            };
            let model_hit = ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{label} \u{2304}"))
                        .color(egui::Color32::from_gray(150))
                        .size(size),
                )
                .sense(egui::Sense::click()),
            );
            claim(ui.ctx(), model_hit.rect);
            if model_hit.clicked() {
                open_chooser = Some(Chooser::Model);
            }

            let mut marks = format!(" \u{b7} контекст {}", self.track.llm.context_len());
            if dev {
                marks.push_str(" \u{b7} разработчик");
            }
            ui.label(
                egui::RichText::new(marks).color(egui::Color32::from_gray(120)).size(size),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                strip_buttons(ui, font, &mut close_app, &mut open_settings);

                // Кнопка называет действие, а не состояние: состояние и так
                // написано в шапке крупно, а по кнопке нужно понимать, что
                // случится от нажатия.
                let listening = self.gates.listening();
                let auto = chip(
                    ui,
                    if listening { "\u{2016} пауза" } else { "\u{25b6} продолжить" },
                    egui::vec2(104.0, 26.0),
                    if listening {
                        egui::Color32::from_rgb(30, 30, 38)
                    } else {
                        egui::Color32::from_rgb(38, 62, 46)
                    },
                    if listening {
                        egui::Color32::from_gray(170)
                    } else {
                        egui::Color32::from_rgb(150, 225, 170)
                    },
                    size,
                );
                if auto.clicked() {
                    toggle_listen = true;
                }

                let device = match &*self.theirs.status.lock().unwrap() {
                    AudioStatus::Running { device } => device.clone(),
                    AudioStatus::Starting => "запускается…".into(),
                    AudioStatus::Failed(_) => "нет звука".into(),
                };
                let device_hit = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("собеседник: {device} \u{2304}"))
                            .color(egui::Color32::from_gray(140))
                            .size(size),
                    )
                    .sense(egui::Sense::click()),
                );
                claim(ui.ctx(), device_hit.rect);
                if device_hit.clicked() {
                    open_chooser = Some(Chooser::Device);
                }
            });
        });

        // Первое нажатие только взводит вопрос, второе завершает: промах по
        // этой кнопке посреди разговора стоил бы всей записи.
        if stop_hit {
            self.toggle_session();
        } else if !confirming {
            self.confirm_stop = None;
        }
        if rate_hit {
            self.rate_miss();
        }

        if let Some(kind) = open_chooser {
            self.choosing = Some(kind);
        }
        if close_app {
            self.quit = true;
        }
        if toggle_listen {
            self.toggle_auto();
        }
        if open_settings {
            self.settings.toggle(&self.cfg);
        }

        self.status_lines(ui, size);
    }

    /// Только то, что требует внимания. Обычные зелёные состояния не пишем:
    /// исправную работу видно по точке в подвале, а список из семи строк
    /// съедал место и перестал читаться.
    fn status_lines(&mut self, ui: &mut egui::Ui, size: f32) {
        let bad = egui::Color32::from_rgb(240, 110, 110);
        let warn = egui::Color32::from_rgb(240, 175, 110);

        let trouble = |ui: &mut egui::Ui, text: String, color: egui::Color32| {
            ui.add(egui::Label::new(egui::RichText::new(text).color(color).size(size)).wrap());
        };

        if let SttStatus::Failed(e) = &*self.stt.status.lock().unwrap() {
            trouble(ui, format!("распознавание не работает: {e}"), bad);
        }
        if let LlmStatus::Failed(e) = &*self.track.llm.status.lock().unwrap() {
            trouble(ui, format!("модель не отвечает: {e}"), bad);
        }
        for audio in [&self.theirs, &self.mine] {
            if let AudioStatus::Failed(e) = &*audio.status.lock().unwrap() {
                trouble(ui, format!("звук: {e}"), bad);
            }
        }
        match self.affinity {
            Affinity::Excluded => {}
            Affinity::MonitorOnly => {
                trouble(ui, "в захвате будет чёрный прямоугольник".into(), warn)
            }
            _ => trouble(ui, "ОКНО ВИДНО В ЗАХВАТЕ ЭКРАНА".into(), bad),
        }

        if self.remote.active() {
            // Канал транслирует экран и разговор наружу — это должно быть
            // видно всегда, пока он открыт.
            let sound = if self.cfg.read().unwrap().remote.audio {
                "звук идёт"
            } else {
                "звук выключен"
            };
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "\u{25cf} канал помощника открыт · зрителей: {} · {sound}",
                    self.remote.viewers()
                ))
                .color(egui::Color32::from_rgb(255, 140, 90))
                .size(size)
                .strong(),
            );

            let (link, color) = match self.remote.tunnel() {
                Tunnel::Ready(_) => (self.remote.url(), egui::Color32::from_rgb(140, 200, 240)),
                Tunnel::Starting => (
                    format!("туннель поднимается… пока по сети: {}", self.remote.lan_url),
                    egui::Color32::from_gray(150),
                ),
                Tunnel::Failed(e) => (
                    format!("туннель не поднялся ({e}); только локальная сеть"),
                    warn,
                ),
                Tunnel::Off => (self.remote.lan_url.clone(), egui::Color32::from_gray(150)),
            };
            ui.horizontal_top(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(link).color(color).size(size)).wrap(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    qr_code(ui, &self.remote.url(), 92.0);
                });
            });
        }
    }

    /// Индикатор уровня. Шкала корневая, а не линейная: тихая речь на линейной
    /// шкале почти неотличима от тишины.
    fn level_meter(&self, ui: &mut egui::Ui, who: Speaker) {
        let level = self.source_of(who).ring.lock().unwrap().peak(120);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 2.0, egui::Color32::from_gray(30));

        let filled = level.sqrt().clamp(0.0, 1.0);
        if filled > 0.005 {
            let mut bar = rect;
            bar.set_width(rect.width() * filled);
            let color = if level > 0.92 {
                egui::Color32::from_rgb(235, 70, 70)
            } else {
                egui::Color32::from_rgb(90, 170, 210)
            };
            p.rect_filled(bar, 2.0, color);
        }
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    fn mask(rects: &[(f32, f32, f32, f32)]) -> Mask {
        Mask {
            rects: rects
                .iter()
                .map(|&(l, t, r, b)| egui::Rect::from_min_max(egui::pos2(l, t), egui::pos2(r, b)))
                .collect(),
        }
    }

    #[test]
    fn клик_попадает_только_внутрь_объявленных_областей() {
        let m = mask(&[(10.0, 10.0, 50.0, 30.0), (100.0, 0.0, 120.0, 20.0)]);
        assert!(m.contains(egui::pos2(20.0, 20.0)), "внутри первой");
        assert!(m.contains(egui::pos2(110.0, 10.0)), "внутри второй");
        assert!(!m.contains(egui::pos2(75.0, 20.0)), "между ними окно сквозное");
        assert!(!m.contains(egui::pos2(20.0, 100.0)), "ниже всех");
    }

    #[test]
    fn пустая_маска_не_ловит_ничего() {
        // Кадр без интерактивных элементов не должен внезапно сделать окно
        // кликабельным: именно так выглядел бы возврат старого дефекта.
        assert!(!Mask::default().contains(egui::pos2(0.0, 0.0)));
        assert!(Mask::default().physical(1.0).is_empty());
    }

    #[test]
    fn границы_расширяются_наружу() {
        // Дробные координаты неизбежны при масштабе экрана. Округление внутрь
        // съедало бы край кнопки, и нажатие по её кромке не срабатывало.
        let m = mask(&[(10.4, 10.6, 50.2, 30.8)]);
        assert_eq!(m.physical(1.0), vec![(10, 10, 51, 31)]);
    }

    #[test]
    fn масштаб_экрана_учитывается() {
        // Windows спрашивает о попадании в физических пикселях, а разметка
        // считается в логических: при 150% без множителя все области уехали бы.
        let m = mask(&[(10.0, 20.0, 30.0, 40.0)]);
        assert_eq!(m.physical(1.5), vec![(15, 30, 45, 60)]);
    }
}

#[cfg(test)]
mod tests_merge {
    use super::*;

    fn merge(label: &str) -> Merge {
        Merge {
            who: Speaker::Them,
            label: label.into(),
            text: "начало".into(),
            auto: true,
            since: Instant::now(),
            finished: false,
            doubt: None,
        }
    }

    #[test]
    fn сбой_различения_голосов_не_рвёт_вопрос() {
        // Метка на одном куске в середине фразы ошиблась и тут же вернулась.
        // Раньше вопрос от этого разрывался надвое, и половина уходила в
        // модель как законченная.
        let mut m = merge("собеседник 1");
        assert!(m.accepts(Speaker::Them, "собеседник 2", true), "одна метка — ещё не смена");
        assert!(m.accepts(Speaker::Them, "собеседник 1", true), "метка вернулась, это был сбой");
    }

    #[test]
    fn настоящая_смена_говорящего_закрывает_вопрос() {
        // Живой собеседник говорит дольше одного куска: вторая подряд новая
        // метка — уже не сбой.
        let mut m = merge("собеседник 1");
        assert!(m.accepts(Speaker::Them, "собеседник 2", true));
        assert!(!m.accepts(Speaker::Them, "собеседник 2", true));
    }

    #[test]
    fn чужой_канал_не_приклеивается() {
        // Микрофон и канал собеседника — разные разговоры, как бы ни совпали
        // метки.
        let mut m = merge("собеседник 1");
        assert!(!m.accepts(Speaker::Me, "собеседник 1", true));
    }

    #[test]
    fn записанное_клавишей_не_склеивается_с_потоком() {
        let mut m = merge("собеседник 1");
        assert!(!m.accepts(Speaker::Them, "собеседник 1", false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn законченность_фразы_определяется_по_знаку() {
        // Отсюда решается, ждать ли продолжения: пауза на размышление длиннее
        // паузы для вдоха, и рвать по ней вопрос нельзя.
        assert!(looks_finished("Что такое идемпотентность?"));
        assert!(looks_finished("Понятно."));
        assert!(looks_finished("Стоп!"));
        assert!(!looks_finished("а если сервер вернёт"));
        assert!(!looks_finished("расскажите про"));
    }

    #[test]
    fn маркер_отделяется_от_текста() {
        let (marker, body) = split_marker("\u{25b8} Гибридно: контентные признаки");
        assert_eq!(marker, Some('\u{25b8}'));
        assert_eq!(body, "Гибридно: контентные признаки");

        let (marker, body) = split_marker("· эмбеддинги item2vec");
        assert_eq!(marker, Some('\u{00b7}'));
        assert_eq!(body, "эмбеддинги item2vec");

        let (marker, body) = split_marker("? речь про новых или про товары");
        assert_eq!(marker, Some('?'));
        assert_eq!(body, "речь про новых или про товары");
    }

    #[test]
    fn строка_без_маркера_остаётся_целой() {
        let (marker, body) = split_marker("просто текст");
        assert!(marker.is_none());
        assert_eq!(body, "просто текст");
    }

    #[test]
    fn произнесённая_строка_получает_галочку() {
        // Иконка меняется раньше цвета: её видно боковым зрением.
        let (glyph, _) = line_icon(Some('\u{00b7}'), true).unwrap();
        assert_eq!(glyph, "\u{2713}");

        let (glyph, _) = line_icon(Some('\u{00b7}'), false).unwrap();
        assert_eq!(glyph, "\u{2022}");
    }

    #[test]
    fn разные_собеседники_получают_разные_цвета() {
        // Иначе «собеседник 1» и «собеседник 2» отличаются только цифрой.
        let first = voice_color(Author::Them, "собеседник 1");
        let second = voice_color(Author::Them, "собеседник 2");
        assert_ne!(first, second);
        assert_eq!(voice_color(Author::Them, "собеседник 1"), first, "цвет устойчив");
    }

    #[test]
    fn свои_реплики_не_красятся_как_собеседники() {
        assert_eq!(voice_color(Author::Me, "я"), Author::Me.color());
        assert_eq!(voice_color(Author::Model, "подсказка"), Author::Model.color());
    }
}
