//! Режим настройки.
//!
//! Правки идут прямо в разделяемый конфиг, поэтому применяются на лету — так
//! видно результат, а не приходится гадать. На диск попадают только по кнопке
//! «Сохранить»; «Отменить» возвращает снимок, сделанный при открытии.

use crate::audio::capture::Audio;
use crate::config::{self, Config};
use crate::hotkeys::keys;
use crate::session::{self, Session};
use std::path::PathBuf;

/// Что настройкам нужно снаружи: живые устройства и запись сессии.
pub struct Ctx<'a> {
    pub theirs: &'a Audio,
    pub mine: &'a Audio,
    pub session: &'a Session,
    pub stt: &'a crate::stt::Stt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Theirs,
    Mine,
    Screen,
    Reset,
    Settings,
    Listen,
    Remote,
    Mute,
    MoveWindow,
    Input,
    Screenshot,
    Session,
    Rate,
    Quit,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Theirs => "слушать собеседника",
            Action::Mine => "слушать себя",
            Action::Screen => "снять экран",
            Action::Reset => "сбросить контекст",
            Action::Settings => "настройки",
            Action::Listen => "пауза авто-режима",
            Action::Remote => "канал помощника",
            Action::Mute => "звук помощнику",
            Action::MoveWindow => "двигать окно",
            Action::Input => "строка ввода",
            Action::Screenshot => "снимок окна",
            Action::Session => "начать или закончить сессию",
            Action::Rate => "подсказка мимо",
            Action::Quit => "выход",
        }
    }

    /// Удерживаемые клавиши назначаются голыми: модификатор здесь сам по себе
    /// рабочая клавиша, а не приставка к другой.
    fn is_hold(self) -> bool {
        matches!(self, Action::Theirs | Action::Mine | Action::Screen)
    }

    fn slot(self, keys: &mut config::Hotkeys) -> &mut String {
        match self {
            Action::Theirs => &mut keys.theirs,
            Action::Mine => &mut keys.mine,
            Action::Screen => &mut keys.screen,
            Action::Reset => &mut keys.reset,
            Action::Settings => &mut keys.settings,
            Action::Listen => &mut keys.listen,
            Action::Remote => &mut keys.remote,
            Action::Mute => &mut keys.mute,
            Action::MoveWindow => &mut keys.move_window,
            Action::Input => &mut keys.input,
            Action::Screenshot => &mut keys.screenshot,
            Action::Session => &mut keys.session,
            Action::Rate => &mut keys.rate,
            Action::Quit => &mut keys.quit,
        }
    }
}

const ALL: [Action; 14] = [
    Action::Theirs,
    Action::Mine,
    Action::Screen,
    Action::Reset,
    Action::Settings,
    Action::Listen,
    Action::Remote,
    Action::Mute,
    Action::MoveWindow,
    Action::Input,
    Action::Screenshot,
    Action::Session,
    Action::Rate,
    Action::Quit,
];

pub struct Settings {
    pub open: bool,
    /// Черновик промпта: файл переписывается только при сохранении.
    pub prompt: String,
    /// Черновик обстановки сессии — правится там же, где промпт.
    pub brief: String,
    /// Что попросить разобрать после сессии: расшифровка ждёт, пока её заберёт
    /// поток интерфейса — сами настройки до модели не дотягиваются.
    pub review_request: Option<String>,
    /// Куда положить пришедший разбор.
    review_target: Option<PathBuf>,
    /// Готовый разбор открытой сессии.
    review: Option<String>,
    waiting: bool,
    /// Записи на диске, перечитываются при входе в настройки.
    sessions: Vec<session::Entry>,
    /// Уборка пустых записей нажата один раз и ждёт подтверждения: она удаляет
    /// файлы, и вернуть их будет неоткуда.
    confirm_clean: bool,
    /// Раскрытая сессия и её журнал.
    opened: Option<usize>,
    log: Vec<(String, String, String)>,
    bank: Vec<session::Pair>,
    bank_open: bool,
    /// Отбор по подстроке — банк за десяток сессий глазами уже не просмотреть.
    filter: String,
    /// Какое действие сейчас ждёт нажатия клавиши.
    pub capturing: Option<Action>,
    /// Снимок на момент открытия — для отмены.
    snapshot: Option<Config>,
    pub note: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            open: false,
            prompt: String::new(),
            brief: String::new(),
            review_request: None,
            review_target: None,
            review: None,
            waiting: false,
            sessions: Vec::new(),
            confirm_clean: false,
            opened: None,
            log: Vec::new(),
            bank: Vec::new(),
            bank_open: false,
            filter: String::new(),
            capturing: None,
            snapshot: None,
            note: None,
        }
    }
}

impl Settings {
    pub fn toggle(&mut self, cfg: &config::Shared) {
        if self.open {
            self.close();
        } else {
            self.open = true;
            let guard = cfg.read().unwrap();
            self.snapshot = Some(guard.clone());
            self.prompt = std::fs::read_to_string(&guard.llm.prompt_path).unwrap_or_default();
            self.brief = guard.brief();
            self.note = None;
            let root = PathBuf::from(&guard.session.dir);
            self.sessions = session::list(&root);
            self.confirm_clean = false;
            self.bank = session::read_bank(&root, 500);
            self.opened = None;
            self.log.clear();
            self.review = None;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.capturing = None;
        self.snapshot = None;
    }

    /// Пришла клавиша в режиме назначения.
    pub fn on_captured(
        &mut self,
        cfg: &config::Shared,
        vk: u16,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) {
        let Some(action) = self.capturing else { return };

        if !action.is_hold() && keys::is_modifier(vk) {
            // Ждём саму клавишу: пользователь ещё набирает комбинацию.
            return;
        }

        let binding = keys::Binding {
            vk,
            ctrl: !action.is_hold() && ctrl,
            alt: !action.is_hold() && alt,
            shift: !action.is_hold() && shift,
        };

        {
            let mut guard = cfg.write().unwrap();
            *action.slot(&mut guard.hotkeys) = keys::spec_of(&binding);
        }
        crate::hotkeys::rebind(&cfg.read().unwrap().hotkeys);

        self.capturing = None;
        crate::hotkeys::capture_mode(false);
    }

    /// Разбор пришёл от модели: показываем и кладём рядом с записью, чтобы он
    /// нашёлся и в следующий раз.
    pub fn take_review(&mut self, text: String) {
        self.waiting = false;
        if let Some(dir) = self.review_target.take() {
            let _ = std::fs::write(dir.join("review.md"), &text);
            for entry in &mut self.sessions {
                if entry.dir == dir {
                    entry.reviewed = true;
                }
            }
        }
        self.review = Some(text);
    }

    /// Список записей. Идущая сессия тоже здесь — её каталог создан в момент
    /// начала, и разобрать разговор можно, не дожидаясь конца.
    fn sessions_ui(&mut self, ui: &mut egui::Ui, ctx: &Ctx) {
        let running = ctx.session.dir();
        let stats = ctx.session.stats();
        ui.label(if running.is_some() {
            egui::RichText::new(format!(
                "идёт: {:.0} мин · вопросов {} · ответов {} · токенов {}→{} · ${:.3}",
                stats.minutes(),
                stats.questions,
                stats.answers,
                stats.tokens_in,
                stats.tokens_out,
                stats.cost
            ))
            .size(12.0)
        } else {
            egui::RichText::new("сессия не начата — пока она не начата, записи не ведётся")
                .size(12.0)
                .color(egui::Color32::from_gray(150))
        });
        ui.add_space(4.0);

        if self.sessions.is_empty() {
            ui.label(
                egui::RichText::new("записей нет — они появятся после первой сессии")
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
            );
            return;
        }

        self.clean_ui(ui, running.as_deref());

        let mut open_request = None;
        let mut review_request = None;
        egui::ScrollArea::vertical().max_height(200.0).id_salt("sessions").show(ui, |ui| {
            for (i, entry) in self.sessions.iter().enumerate() {
                ui.horizontal(|ui| {
                    let opened = self.opened == Some(i);
                    if ui.selectable_label(opened, entry.when()).clicked() {
                        open_request = Some(if opened { None } else { Some(i) });
                    }
                    if let Some(s) = &entry.stats {
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} мин · {}/{} · ${:.3}",
                                entry.minutes, s.questions, s.answers, s.cost
                            ))
                            .size(11.0)
                            .color(egui::Color32::from_gray(140)),
                        );
                    }
                    if entry.reviewed {
                        ui.label(egui::RichText::new("разобрано").size(11.0).color(
                            egui::Color32::from_rgb(120, 180, 120),
                        ));
                    }
                    // Идущая сессия дописывается прямо сейчас: разбирать её
                    // можно, но видно должно быть, что это не весь разговор.
                    if running.as_deref() == Some(entry.dir.as_path()) {
                        ui.label(egui::RichText::new("идёт").size(11.0).color(
                            egui::Color32::from_rgb(232, 122, 122),
                        ));
                    } else if entry.empty {
                        ui.label(
                            egui::RichText::new("пусто")
                                .size(11.0)
                                .color(egui::Color32::from_gray(110)),
                        );
                    }
                    if ui.button("разобрать").clicked() {
                        review_request = Some(i);
                    }
                });
            }
        });

        if let Some(target) = open_request {
            self.opened = target;
            self.log = target.map(|i| session::read_log(&self.sessions[i].dir)).unwrap_or_default();
            self.review = target
                .and_then(|i| std::fs::read_to_string(self.sessions[i].dir.join("review.md")).ok());
        }
        if let Some(i) = review_request {
            let dir = self.sessions[i].dir.clone();
            let transcript = session::transcript(&dir);
            if transcript.trim().is_empty() {
                self.note = Some("в этой записи нечего разбирать".into());
            } else {
                self.review_request = Some(transcript);
                self.review_target = Some(dir);
                self.waiting = true;
                self.review = None;
                self.opened = Some(i);
                self.log = session::read_log(&self.sessions[i].dir);
            }
        }

        if self.opened.is_none() {
            return;
        }

        if self.waiting {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("модель разбирает…").size(12.0));
        }
        if let Some(review) = self.review.clone() {
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(220.0).id_salt("review").show(ui, |ui| {
                ui.label(egui::RichText::new(review).size(13.0));
            });
        }

        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("журнал, {} строк:", self.log.len())).size(12.0));
        egui::ScrollArea::vertical().max_height(260.0).id_salt("log").show(ui, |ui| {
            for (kind, who, text) in &self.log {
                let color = match kind.as_str() {
                    "llm" => egui::Color32::from_rgb(200, 180, 120),
                    "helper" => egui::Color32::from_rgb(140, 190, 230),
                    // Отметки начала, пауз и сбоев — не разговор, и читаться
                    // должны отдельно от него.
                    "error" => egui::Color32::from_rgb(232, 122, 122),
                    "session" => egui::Color32::from_gray(120),
                    "meta" => egui::Color32::from_gray(110),
                    _ => egui::Color32::from_gray(200),
                };
                ui.label(
                    egui::RichText::new(format!("{who}: {text}"))
                        .size(12.0)
                        .color(color),
                );
            }
        });
    }

    /// Уборка записей, в которых не осталось ни одной реплики.
    ///
    /// Такие копятся сами: включили и не поговорили. Каждая при этом тащит за
    /// собой звук, и список из ста пустых строк перестаёт читаться.
    fn clean_ui(&mut self, ui: &mut egui::Ui, running: Option<&std::path::Path>) {
        let empty: Vec<&session::Entry> = self
            .sessions
            .iter()
            .filter(|e| e.empty && Some(e.dir.as_path()) != running)
            .collect();
        if empty.is_empty() {
            self.confirm_clean = false;
            return;
        }
        let count = empty.len();

        ui.horizontal(|ui| {
            if self.confirm_clean {
                ui.label(
                    egui::RichText::new(format!("убрать {count} записей без единой реплики?"))
                        .size(11.0)
                        .color(egui::Color32::from_rgb(232, 122, 122)),
                );
                if ui.button("да, убрать").clicked() {
                    let (done, bytes) = session::remove_empty(&self.sessions, running);
                    self.sessions.retain(|e| !e.empty || Some(e.dir.as_path()) == running);
                    self.opened = None;
                    self.log.clear();
                    self.confirm_clean = false;
                    self.note = Some(format!(
                        "убрано записей: {done}, освободилось {:.0} МБ",
                        bytes as f64 / (1024.0 * 1024.0)
                    ));
                }
                if ui.button("отмена").clicked() {
                    self.confirm_clean = false;
                }
            } else if ui.button(format!("убрать пустые ({count})")).clicked() {
                self.confirm_clean = true;
            }
        });
        ui.add_space(4.0);
    }

    /// Накопленные пары «вопрос — ответ» за все сессии.
    fn bank_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{} пар", self.bank.len())).size(12.0));
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("отбор по словам")
                    .desired_width(220.0),
            );
            if ui.button("показать").clicked() {
                self.bank_open = !self.bank_open;
            }
        });
        if !self.bank_open {
            return;
        }

        let needle = self.filter.to_lowercase();
        egui::ScrollArea::vertical().max_height(320.0).id_salt("bank").show(ui, |ui| {
            for pair in &self.bank {
                if !needle.is_empty()
                    && !pair.question.to_lowercase().contains(&needle)
                    && !pair.answer.to_lowercase().contains(&needle)
                {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{}: {}", pair.who, pair.question))
                            .size(12.0)
                            .color(egui::Color32::from_gray(215)),
                    );
                    // Забракованные пары — то, ради чего оценка и вводилась:
                    // их видно с одного взгляда, не вчитываясь в ответы.
                    if pair.missed {
                        ui.label(
                            egui::RichText::new("мимо")
                                .size(11.0)
                                .color(egui::Color32::from_rgb(210, 130, 130)),
                        );
                    }
                });
                ui.label(
                    egui::RichText::new(&pair.answer)
                        .size(12.0)
                        .color(if pair.missed {
                            egui::Color32::from_gray(120)
                        } else {
                            egui::Color32::from_rgb(190, 170, 120)
                        }),
                );
                ui.add_space(6.0);
            }
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cfg: &config::Shared, ctx: &Ctx) {
        let mut guard = cfg.write().unwrap();

        drag_bar(ui);
        ui.add_space(6.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Настройки");
            ui.add_space(6.0);

            section(ui, "Окно", |ui| {
                slider(ui, &mut guard.overlay.font_size, 10.0..=32.0, "кегль");
                slider(ui, &mut guard.overlay.opacity, 0.3..=1.0, "непрозрачность");
                ui.label(
                    egui::RichText::new(format!(
                        "положение {:.0}×{:.0}, размер {:.0}×{:.0} — тяните за полосу сверху \
                         и за края окна, как обычное окно Windows",
                        guard.overlay.x, guard.overlay.y, guard.overlay.width, guard.overlay.height
                    ))
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Ввод", |ui| {
                let mut pre = guard.input.pre_roll_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut pre, 0.0..=3000.0).suffix(" мс").text("захват до нажатия"))
                    .changed()
                {
                    guard.input.pre_roll_ms = pre as u64;
                }
                let mut hold = guard.input.min_hold_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut hold, 0.0..=1000.0).suffix(" мс").text("минимум удержания"))
                    .changed()
                {
                    guard.input.min_hold_ms = hold as u64;
                }
                ui.label(
                    egui::RichText::new(
                        "удержание короче минимума считается обычным шорткатом \
                         активного приложения и игнорируется",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Звук", |ui| {
                device_picker(ui, "собеседник", ctx.theirs, &mut guard.audio.loopback_device);
                device_picker(ui, "микрофон", ctx.mine, &mut guard.audio.input_device);
                ui.label(
                    egui::RichText::new(
                        "устройство меняется на ходу: поток переоткрывается сам, перезапуск \
                         посреди разговора не нужен",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );

                ui.add_space(6.0);
                ui.label(egui::RichText::new("проверка микрофона — скажите что-нибудь:").size(12.0));
                meter(ui, ctx.mine.ring.lock().unwrap().peak(200));
                ui.label(
                    egui::RichText::new("полоса должна отзываться на голос; если стоит на месте — \
                                         выбрано не то устройство")
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new("собеседник:").size(12.0));
                meter(ui, ctx.theirs.ring.lock().unwrap().peak(200));
            });

            section(ui, "Автоматический режим", |ui| {
                ui.checkbox(
                    &mut guard.input.auto,
                    "начинать сессию сразу в автоматическом режиме",
                );
                ui.label(
                    egui::RichText::new(
                        "в этом режиме реплики распознаются и уходят в модель сами, \
                         без удержания клавиши. На паузу ставится кнопкой в подвале \
                         или клавишей — запись сессии при этом продолжается",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                let mut ep = guard.vad.endpoint_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut ep, 200.0..=2000.0).suffix(" мс").text("пауза = конец реплики"))
                    .changed()
                {
                    guard.vad.endpoint_ms = ep as u64;
                }
                ui.label(
                    egui::RichText::new(
                        "меньше — ответ приходит раньше, но фразы рвутся на паузах для вдоха",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                let mut ms = guard.vad.min_speech_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut ms, 100.0..=2000.0).suffix(" мс").text("минимум речи"))
                    .changed()
                {
                    guard.vad.min_speech_ms = ms as u64;
                }
                slider(ui, &mut guard.vad.threshold, 0.2..=0.9, "порог речи");
                ui.checkbox(
                    &mut guard.vad.suppress_mic_while_loopback,
                    "глушить микрофон, пока говорит собеседник",
                );
                ui.label(
                    egui::RichText::new(
                        "без наушников микрофон слышит колонки и записал бы собеседника \
                         как вашу реплику",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                ui.checkbox(&mut guard.vad.answer_them, "отвечать на реплики собеседника");
                ui.checkbox(&mut guard.vad.answer_me, "отвечать на мои реплики");
                ui.label(
                    egui::RichText::new(
                        "ваши реплики по умолчанию только пополняют контекст: вы говорите \
                         собеседнику, а не спрашиваете модель",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Помощник на другом устройстве", |ui| {
                ui.checkbox(&mut guard.remote.enabled, "включать канал при запуске");
                ui.label(
                    egui::RichText::new(
                        "канал транслирует наружу ваш экран и весь разговор. Ссылка открывается \
                         в браузере помощника; она содержит токен, без него доступа нет. \
                         Трафик идёт без шифрования — годится для доверенной локальной сети \
                         или через туннель, но не через открытый интернет",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_rgb(230, 180, 120)),
                );
                let mut port = guard.remote.port as f32;
                if ui.add(egui::Slider::new(&mut port, 1024.0..=65000.0).text("порт")).changed() {
                    guard.remote.port = port as u16;
                }
                let mut w = guard.remote.width as f32;
                if ui.add(egui::Slider::new(&mut w, 640.0..=2560.0).text("ширина кадра")).changed() {
                    guard.remote.width = w as u32;
                }
                let mut fps = guard.remote.fps as f32;
                if ui.add(egui::Slider::new(&mut fps, 1.0..=10.0).text("кадров в секунду")).changed() {
                    guard.remote.fps = fps as u32;
                }
                let mut q = guard.remote.quality as f32;
                if ui.add(egui::Slider::new(&mut q, 30.0..=90.0).text("качество JPEG")).changed() {
                    guard.remote.quality = q as u8;
                }
                ui.label(
                    egui::RichText::new("ниже 50 мелкий код начинает мылиться")
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
                ui.checkbox(&mut guard.remote.audio, "передавать звук разговора");
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("публичная ссылка:").size(12.0));
                    for (value, title) in
                        [("ssh", "localhost.run"), ("cloudflared", "cloudflare"), ("off", "нет")]
                    {
                        let chosen = guard.remote.tunnel == value;
                        if ui.selectable_label(chosen, title).clicked() {
                            guard.remote.tunnel = value.to_string();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "нужна, если белого IP нет. Туннель живёт, пока открыт канал, и даёт \
                         адрес уже по HTTPS. На этой сети cloudflare не смог зарегистрировать \
                         туннель, а localhost.run поднялся сразу — поэтому он и по умолчанию",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                ui.label(
                    egui::RichText::new("порт и ширина применяются после перезапуска")
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Модель · ручная дорожка", |ui| {
                ui.label(
                    egui::RichText::new(
                        "сюда вы обращаетесь сами и готовы подождать — модель сильнее, \
                         памяти больше, цена выше",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                profile_ui(ui, &mut guard.llm.manual);
            });

            section(ui, "Модель · автоматический режим", |ui| {
                ui.label(
                    egui::RichText::new(
                        "через неё проходит весь разговор подряд, поэтому дешёвая и быстрая",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                profile_ui(ui, &mut guard.llm.auto);
            });

            section(ui, "Общее для обеих", |ui| {
                let mut turns = guard.llm.history_turns as f32;
                if ui
                    .add(egui::Slider::new(&mut turns, 2.0..=200.0).text("реплик в истории"))
                    .changed()
                {
                    guard.llm.history_turns = turns as usize;
                }
                let mut images = guard.llm.history_images as f32;
                if ui
                    .add(egui::Slider::new(&mut images, 0.0..=5.0).text("снимков в контексте"))
                    .changed()
                {
                    guard.llm.history_images = images as usize;
                }
                ui.label(
                    egui::RichText::new(
                        "история считается по репликам, вопросы и ответы вместе; старое \
                         отбрасывается парами, чтобы в начале не остался ответ без вопроса",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                ui.add_space(4.0);
                labelled(ui, "адрес", &mut guard.llm.base_url);
            });

            section(ui, "Что уже сказано", |ui| {
                ui.checkbox(&mut guard.coverage.enabled, "отмечать произнесённые пункты");
                let mut d = guard.coverage.debounce_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut d, 500.0..=5000.0).suffix(" мс").text("пауза перед сверкой"))
                    .changed()
                {
                    guard.coverage.debounce_ms = d as u64;
                }
                ui.label(
                    egui::RichText::new(
                        "после вашей паузы подсказка сверяется с тем, что вы произнесли: \
                         закрытые пункты тускнеют и получают галочку, оставшиеся видны ярко. \
                         Отдельной строкой приходит, что стоит добавить. Сверку делает дешёвая \
                         модель автодорожки",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Голоса собеседников", |ui| {
                ui.checkbox(&mut guard.voices.enabled, "различать голоса в канале собеседника");
                slider(ui, &mut guard.voices.threshold, 0.35..=0.85, "порог «тот же голос»");
                ui.label(
                    egui::RichText::new(
                        "выше порога — тот же человек. На замере один голос давал 0.92, \
                         разные — 0.50. Ниже — реже путает двух людей, но чаще дробит одного \
                         на нескольких",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                let mut max = guard.voices.max_voices as f32;
                if ui.add(egui::Slider::new(&mut max, 2.0..=12.0).text("потолок числа голосов")).changed() {
                    guard.voices.max_voices = max as usize;
                }
                ui.label(
                    egui::RichText::new(
                        "ваш голос запоминается с микрофона сам, знакомиться отдельно не нужно. \
                         Включение и модель применяются после перезапуска",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Экран", |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("снимать:").size(12.0));
                    for (value, title) in [("window", "окно в фокусе"), ("screen", "весь экран")] {
                        let chosen = guard.llm.screen_target == value;
                        if ui.selectable_label(chosen, title).clicked() {
                            guard.llm.screen_target = value.to_string();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "окно в фокусе — меньше шума для модели, меньше токенов и меньше \
                         лишнего покидает машину. Помощнику экран всегда раздаётся целиком",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                labelled(ui, "модель со зрением", &mut guard.llm.vision_model);
                let mut vt = guard.llm.vision_max_tokens as f32;
                if ui
                    .add(egui::Slider::new(&mut vt, 1000.0..=32000.0).text("бюджет токенов"))
                    .changed()
                {
                    guard.llm.vision_max_tokens = vt as u32;
                }
                ui.label(egui::RichText::new("вопрос к экрану:").size(12.0));
                ui.add(
                    egui::TextEdit::multiline(&mut guard.llm.screen_prompt)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
            });

            section(ui, "Распознавание", |ui| {
                // Что на самом деле загрузилось. Полезно ровно тогда, когда в
                // пути опечатка: без этой строки видно только тишину и
                // непонятно, дошло ли дело до модели вообще.
                use crate::stt::Status;
                let (text, color) = match &*ctx.stt.status.lock().unwrap() {
                    Status::Loading => {
                        ("модель загружается…".to_string(), egui::Color32::from_rgb(235, 190, 80))
                    }
                    Status::Ready { model, threads } => (
                        format!("загружена: {model}, потоков {threads}"),
                        egui::Color32::from_rgb(120, 200, 140),
                    ),
                    Status::Failed(e) => (
                        format!("не загрузилась: {e}"),
                        egui::Color32::from_rgb(232, 122, 122),
                    ),
                };
                ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(color)).wrap());
                ui.add_space(4.0);

                labelled(ui, "модель whisper", &mut guard.stt.model_path);
                labelled(ui, "язык", &mut guard.stt.language);
                ui.label(
                    egui::RichText::new("модель и язык применяются после перезапуска; пороги — сразу")
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );

                let slider = |ui: &mut egui::Ui, v: &mut f32, lo, hi, text| {
                    ui.add(egui::Slider::new(v, lo..=hi).text(text));
                };
                slider(ui, &mut guard.stt.no_speech_thold, 0.0, 1.0, "порог «здесь не речь»");
                slider(ui, &mut guard.stt.entropy_thold, 1.0, 4.0, "порог бессвязности");
                slider(ui, &mut guard.stt.logprob_thold, -4.0, 0.0, "порог уверенности");
                let mut best = guard.stt.best_of as f32;
                if ui.add(egui::Slider::new(&mut best, 1.0..=5.0).text("попыток разбора")).changed() {
                    guard.stt.best_of = best as usize;
                }
                ui.label(
                    egui::RichText::new(
                        "пороги отбраковки whisper. Выше «здесь не речь» — меньше выдуманных \
                         фраз на тишине, но легче потерять тихое начало реплики. Подбирать \
                         удобнее на записанных сессиях",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
            });

            section(ui, "Режим разработчика", |ui| {
                ui.checkbox(&mut guard.dev.enabled, "снимки собственного окна");
                ui.label(
                    egui::RichText::new(
                        "окно исключено из захвата экрана, поэтому обычным скриншотом его не \
                         снять. Здесь кадр берётся у самого интерфейса — исключение при этом \
                         не отключается ни на мгновение",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                labelled(ui, "каталог", &mut guard.dev.dir);
            });

            section(ui, "Клавиши", |ui| {
                for action in ALL {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{:<22}", action.label()))
                                .monospace()
                                .size(12.0),
                        );
                        let current = action.slot(&mut guard.hotkeys).clone();
                        let waiting = self.capturing == Some(action);
                        let caption = if waiting { "нажмите клавишу…".into() } else { current };
                        if ui.button(caption).clicked() {
                            self.capturing = Some(action);
                            crate::hotkeys::capture_mode(true);
                        }
                    });
                }
            });

            section(ui, "Сессии", |ui| {
                ui.checkbox(&mut guard.session.audio, "писать звук разговора");
                ui.label(
                    egui::RichText::new(
                        "журнал разговора пишется всегда — он весит копейки; звук \
                         разбирать удобнее, но это мегабайты на каждую сессию",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                ui.add_space(4.0);
                self.sessions_ui(ui, ctx);
            });

            section(ui, "Банк вопросов", |ui| {
                self.bank_ui(ui);
            });

            section(ui, "Обстановка сессии", |ui| {
                ui.label(
                    egui::RichText::new(&guard.llm.brief_path)
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
                ui.label(
                    egui::RichText::new(
                        "кто вы, что за встреча, имена и термины. Раздел «Термины» идёт \
                         не только в модель: он же подсказывает распознаванию, каких слов ждать",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_gray(130)),
                );
                ui.add(
                    egui::TextEdit::multiline(&mut self.brief)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );

                // Что из написанного действительно уходит в распознавание.
                // Разбор берёт только термины и только до потолка затравки, и
                // видеть результат надо здесь, а не выяснять его по искажённым
                // словам в расшифровке.
                ui.add_space(4.0);
                let glossary = guard.glossary();
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(if glossary.is_empty() {
                            "в распознавание не уходит ничего: раздел «Термины» пуст".to_string()
                        } else {
                            format!("в распознавание уходит: {glossary}")
                        })
                        .size(11.0)
                        .color(egui::Color32::from_gray(120)),
                    )
                    .wrap(),
                );
            });

            section(ui, "Промпт", |ui| {
                ui.label(
                    egui::RichText::new(&guard.llm.prompt_path)
                        .size(11.0)
                        .color(egui::Color32::from_gray(130)),
                );
                ui.add(
                    egui::TextEdit::multiline(&mut self.prompt)
                        .desired_rows(12)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Сохранить").clicked() {
                    self.note = Some(match save(&guard, &self.prompt, &self.brief) {
                        Ok(()) => "сохранено".into(),
                        Err(e) => format!("не сохранилось: {e:#}"),
                    });
                }
                if ui.button("Отменить").clicked() {
                    if let Some(snapshot) = self.snapshot.clone() {
                        *guard = snapshot;
                        crate::hotkeys::rebind(&guard.hotkeys);
                    }
                    self.note = Some("правки отменены".into());
                }
                if ui.button("Закрыть").clicked() {
                    self.open = false;
                }
            });

            if let Some(note) = &self.note {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(note)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(150, 200, 160)),
                );
            }
        });

        if !self.open {
            drop(guard);
            self.close();
        }
    }
}

fn save(cfg: &Config, prompt: &str, brief: &str) -> anyhow::Result<()> {
    std::fs::write(cfg.prompt_path(), prompt)?;
    std::fs::write(&cfg.llm.brief_path, brief)?;
    cfg.save(std::path::Path::new(config::PATH))?;
    Ok(())
}

/// Выбор устройства из тех, что видит система.
fn device_picker(ui: &mut egui::Ui, label: &str, audio: &Audio, stored: &mut String) {
    let devices = audio.devices.lock().unwrap().clone();
    let current = audio.selected();
    let shown = if current.is_empty() { "по умолчанию".to_string() } else { current.clone() };

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(12.0));
        egui::ComboBox::from_id_salt(label)
            .selected_text(shown)
            .width(320.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_empty(), "по умолчанию").clicked() {
                    stored.clear();
                    audio.select("");
                }
                for name in devices {
                    if ui.selectable_label(current == name, &name).clicked() {
                        *stored = name.clone();
                        audio.select(&name);
                    }
                }
            });
    });
}

/// Полоса уровня для проверки: шкала корневая, иначе тихая речь неотличима
/// от тишины.
fn meter(ui: &mut egui::Ui, level: f32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width().min(320.0), 10.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(34));
    let filled = level.sqrt().clamp(0.0, 1.0);
    if filled > 0.005 {
        let mut bar = rect;
        bar.set_width(rect.width() * filled);
        painter.rect_filled(bar, 3.0, egui::Color32::from_rgb(110, 190, 230));
    }
}

/// Настройки одной дорожки. Профили независимы: ручная и автоматическая
/// решают разные задачи и настраиваются порознь.
fn profile_ui(ui: &mut egui::Ui, p: &mut config::Profile) {
    labelled(ui, "модель", &mut p.model);

    let mut tokens = p.max_tokens as f32;
    if ui.add(egui::Slider::new(&mut tokens, 200.0..=32000.0).text("максимум токенов")).changed() {
        p.max_tokens = tokens as u32;
    }

    ui.checkbox(&mut p.reasoning, "разрешить рассуждение");
    ui.label(
        egui::RichText::new(
            "история общая для обеих моделей и настраивается отдельно, в разделе ниже",
        )
        .size(11.0)
        .color(egui::Color32::from_gray(130)),
    );
}

/// Полоса для перетаскивания окна — заменяет заголовок, которого у окна нет.
pub fn drag_bar(ui: &mut egui::Ui) {
    let bar = ui.allocate_response(
        egui::vec2(ui.available_width(), 20.0),
        egui::Sense::click_and_drag(),
    );

    let painter = ui.painter();
    painter.rect_filled(bar.rect, 4.0, egui::Color32::from_gray(38));
    // Три точки посередине — привычный признак «за это тянут».
    let c = bar.rect.center();
    for dx in [-8.0, 0.0, 8.0] {
        painter.circle_filled(egui::pos2(c.x + dx, c.y), 1.6, egui::Color32::from_gray(120));
    }

    if bar.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(title)
            .size(13.0)
            .color(egui::Color32::from_rgb(150, 190, 230)),
    );
    ui.separator();
    body(ui);
}

fn slider(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, text: &str) {
    ui.add(egui::Slider::new(value, range).text(text));
}

fn labelled(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).size(12.0));
        ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
    });
}
