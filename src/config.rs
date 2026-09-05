//! Настройки. Файл `config.toml` — источник правды; режим настроек его правит
//! и сохраняет. Всё, что можно применить на лету, применяется на лету.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

pub const PATH: &str = "config.toml";

/// Потолок затравки для распознавания, в символах. У whisper на неё около
/// 224 токенов; на русском это примерно пятьсот символов, и упираться в самый
/// край незачем.
const GLOSSARY_MAX: usize = 400;

pub type Shared = Arc<RwLock<Config>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub overlay: Overlay,
    pub input: Input,
    pub vad: Vad,
    pub speculation: Speculation,
    pub coverage: Coverage,
    pub llm: Llm,
    pub stt: Stt,
    pub audio: Audio,
    pub session: Session,
    pub dev: Dev,
    pub voices: Voices,
    pub remote: Remote,
    pub hotkeys: Hotkeys,
}

/// Раздача экрана и звука помощнику на другом устройстве.
///
/// Выключено по умолчанию: это канал, который транслирует наружу ваш экран и
/// весь разговор.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    /// Поднимать ли сервер при запуске.
    pub enabled: bool,
    pub port: u16,
    /// Ширина кадра. Чем меньше, тем легче поток и тем хуже читается мелкий код.
    pub width: u32,
    pub fps: u32,
    /// Качество JPEG. Ниже 50 текст начинает мылиться.
    pub quality: u8,
    /// Передавать ли звук разговора.
    pub audio: bool,
    /// Пустой — будет сгенерирован при первом запуске.
    pub token: String,
    /// Публичный туннель: нужен, когда белого IP нет, иначе помощник просто
    /// не достучится до машины. `ssh` (localhost.run), `cloudflared` или `off`.
    pub tunnel: String,
    /// Путь к программе туннеля. Пустой — искать в обычном месте и в PATH.
    pub tunnel_exe: String,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8788,
            width: 1600,
            fps: 3,
            quality: 62,
            audio: true,
            token: String::new(),
            tunnel: "ssh".into(),
            tunnel_exe: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// 0.0 — полностью прозрачная панель, 1.0 — сплошная.
    pub opacity: f32,
    /// Базовый кегль. Остальные размеры выводятся из него пропорционально.
    pub font_size: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Input {
    /// Сколько звука доклеивается перед нажатием клавиши: начало первого слова
    /// произносят одновременно с нажатием.
    pub pre_roll_ms: u64,
    /// Короче этого удержание считается шорткатом активного приложения
    /// (Ctrl+C и подобные), а не намерением записать реплику.
    pub min_hold_ms: u64,
    /// Начинать сессию сразу в автоматическом режиме: слушать и отвечать без
    /// удержания клавиши. Иначе разговор пришлось бы вести, помня про кнопку,
    /// а про неё забывают на первой же фразе. Ставится на паузу на ходу — и
    /// кнопкой в подвале, и клавишей.
    ///
    /// Поле переименовано из `always_on` намеренно: старое значение при чтении
    /// конфига отбрасывается, и режим включается у всех, кто его когда-то
    /// выключил ещё до появления сессий.
    pub auto: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Vad {
    /// Порог вероятности речи. Ниже — чувствительнее и больше ложных
    /// срабатываний на шум.
    pub threshold: f32,
    /// Тишина такой длины означает «договорил». Главный параметр режима:
    /// меньше — быстрее ответ, но рвутся фразы на паузах для вдоха.
    pub endpoint_ms: u64,
    /// Реплики короче отбрасываются: кашель, щелчок, короткое «ага».
    pub min_speech_ms: u64,
    pub pre_roll_ms: u64,
    /// Предохранитель от бесконечной реплики, если тишины так и нет.
    pub max_utterance_s: u64,
    /// Гасить микрофон, пока говорит собеседник. Без наушников микрофон
    /// слышит колонки и записал бы собеседника как вашу реплику.
    pub suppress_mic_while_loopback: bool,
    /// Отвечать на реплики собеседника автоматически.
    pub answer_them: bool,
    /// Отвечать на ваши собственные реплики. По умолчанию нет: вы говорите
    /// собеседнику, а не спрашиваете модель — ваша речь только пополняет контекст.
    pub answer_me: bool,
    /// Сколько ждать продолжения, прежде чем отвечать. Пауза для вдоха рвёт
    /// один вопрос на куски, и модель отвечает на обрывок; за это время куски
    /// одного говорящего собираются в общий смысл.
    pub merge_ms: u64,
    /// Столько ждём, если фраза явно не закончена: нет точки или знака вопроса
    /// на конце. Пауза на размышление длиннее паузы для вдоха.
    pub merge_open_ms: u64,
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            endpoint_ms: 550,
            min_speech_ms: 400,
            pre_roll_ms: 300,
            max_utterance_s: 30,
            suppress_mic_while_loopback: true,
            answer_them: true,
            answer_me: false,
            merge_ms: 1200,
            merge_open_ms: 2600,
        }
    }
}

/// Упреждающий ответ: запрос уходит, пока собеседник ещё говорит, чтобы
/// черновик был на экране раньше, чем он договорит.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Speculation {
    pub enabled: bool,
    /// Не раньше этого от начала речи: на «ну, э-э, слушайте» отвечать нечего.
    pub min_speech_ms: u64,
    /// Не чаще: иначе запросы польются на каждое слово.
    pub interval_ms: u64,
    /// Потолок догадок на реплику — прямой потолок расхода.
    pub max_per_utterance: u32,
}

impl Default for Speculation {
    fn default() -> Self {
        Self { enabled: true, min_speech_ms: 1500, interval_ms: 1400, max_per_utterance: 2 }
    }
}

/// Настройки одной дорожки. Ручная и автоматическая различаются намеренно:
/// первую вы запускаете сами и готовы ждать, вторая молотит весь разговор.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub model: String,
    /// Куда переключиться, если основная модель отказала: кончились токены,
    /// провайдер лёг, лимит запросов. Пустая строка — не переключаться.
    pub fallback_model: String,
    pub max_tokens: u32,
    /// Рассуждение модели — чистая задержка до первого видимого слова.
    pub reasoning: bool,
}

/// Отслеживание, что из подсказки уже произнесено вслух.
///
/// Подсказка перестаёт быть одноразовой: пока вы говорите, видно, какие пункты
/// закрыты, а какие ещё нет — и не нужно держать это в голове.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Coverage {
    pub enabled: bool,
    /// Сколько ждать после вашей реплики, прежде чем сверять. Проверять на
    /// каждое слово и дорого, и бессмысленно: мысль ещё не закончена.
    pub debounce_ms: u64,
}

impl Default for Coverage {
    fn default() -> Self {
        Self { enabled: true, debounce_ms: 1500 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Llm {
    pub base_url: String,
    pub prompt_path: String,
    /// Сколько ходов истории держим: про задержку и цену, а не про переполнение.
    ///
    /// История одна на весь разговор, поэтому и лимит один. Пока он лежал в
    /// профиле, любой автоматический ответ обрезал общую историю по своему
    /// лимиту, и ручная модель никогда не видела отведённых ей ходов.
    pub history_turns: usize,
    /// Сколько последних снимков остаётся в контексте. Каждый — примерно
    /// полторы тысячи токенов на каждом последующем запросе.
    pub history_images: usize,
    /// Обстановка сессии: кто вы, о чём встреча, имена и термины. Оттуда же
    /// берётся глоссарий для затравки распознавания.
    pub brief_path: String,
    /// Дорожка постоянного прослушивания: дешёвая и быстрая, потому что через
    /// неё проходит весь разговор целиком.
    pub auto: Profile,
    /// Ручная дорожка: сюда вы обращаетесь осознанно и готовы ждать дольше,
    /// поэтому и модель сильнее, и памяти ей отведено больше.
    pub manual: Profile,
    /// Отдельная модель для снимков экрана: текстовая их не понимает.
    pub vision_model: String,
    /// У моделей со зрением рассуждение обычно принудительное и съедает
    /// тысячи токенов, поэтому бюджет здесь отдельный и больше.
    pub vision_max_tokens: u32,
    /// Что снимать для модели: `window` — окно в фокусе, `screen` — весь экран.
    pub screen_target: String,
    /// Что спрашивать про экран.
    pub screen_prompt: String,
    /// Модели для быстрой смены прямо из подвала. Список свой, а не выданный
    /// провайдером: у прокси их сотни, а в разговоре перебирать некогда.
    pub models: Vec<String>,
}

impl Llm {
    /// Профиль по источнику реплики, а не по панели: иначе при выключенном
    /// режиме сравнения дорогая модель молча обрабатывала бы весь поток.
    pub fn profile(&self, manual: bool) -> &Profile {
        if manual {
            &self.manual
        } else {
            &self.auto
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Stt {
    /// Меняется только с перезапуском: модель загружается один раз.
    pub model_path: String,
    pub language: String,
    /// Ниже этой уверенности слово помечается сомнительным. Модель тогда знает,
    /// что его надо восстановить по смыслу, а не принимать на веру.
    pub confidence: f32,
    /// Сколько разборов пробует whisper, когда первый признан негодным.
    ///
    /// Откат по температуре включён у whisper по умолчанию, но при единице он
    /// бессмыслен: повтор даёт тот же результат. Пять — стандарт whisper.cpp,
    /// и платим мы за них только на неудачных кусках.
    pub best_of: usize,
    /// Порог «здесь не речь». Выше — строже, меньше выдуманных фраз на тишине,
    /// но легче потерять тихое начало реплики.
    pub no_speech_thold: f32,
    /// Порог бессвязности разбора: выше — чаще пересобирает.
    pub entropy_thold: f32,
    /// Порог средней уверенности разбора.
    pub logprob_thold: f32,
}

/// Выбор устройств. Пустая строка — системное по умолчанию.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// Откуда слушаем собеседника: устройство ВЫВОДА, с которого снимается
    /// то, что играет.
    pub loopback_device: String,
    pub input_device: String,
}

/// Запись сессии на диск для последующего разбора.
///
/// Пишется только локально и только между «начать» и «завершить»: выключателя
/// здесь нет намеренно, выключателем служит сама кнопка. Это разговор целиком,
/// поэтому состояние видно в строке статуса.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub dir: String,
    /// Писать ли сведённое аудио. Текст весит копейки, звук — мегабайты.
    pub audio: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self { dir: "sessions".into(), audio: true }
    }
}

/// Режим разработчика.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Dev {
    pub enabled: bool,
    pub dir: String,
}

impl Default for Dev {
    fn default() -> Self {
        Self { enabled: false, dir: "dev".into() }
    }
}

/// Различение голосов внутри канала собеседника.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Voices {
    pub enabled: bool,
    /// Меняется только с перезапуском: модель загружается один раз.
    pub model_path: String,
    /// Выше порога — тот же человек. На замере один голос давал 0.92, разные —
    /// 0.50, так что запас с обеих сторон большой. Реальные голоса в сжатом
    /// звонке ближе друг к другу, поэтому порог взят с запасом вниз.
    pub threshold: f32,
    /// Потолок числа голосов: без него шум и обрывки наплодят «собеседников».
    pub max_voices: usize,
}

impl Default for Voices {
    fn default() -> Self {
        Self {
            enabled: true,
            model_path: "models/spk-resnet34.onnx".into(),
            threshold: 0.62,
            max_voices: 6,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub theirs: String,
    pub mine: String,
    pub screen: String,
    pub reset: String,
    pub settings: String,
    pub listen: String,
    pub remote: String,
    pub mute: String,
    pub move_window: String,
    /// Поле ввода: окно принимает клавиатуру, курсор встаёт в строку.
    pub input: String,
    /// Снимок собственного окна. Работает только в режиме разработчика.
    pub screenshot: String,
    /// Начать сессию или закончить её. Единственное действие, без которого не
    /// работает ничего остального, — а мышью в начале разговора не до кнопок.
    pub session: String,
    /// Отметить последнюю подсказку негодной. Единственная оценка в записи:
    /// без неё удачные и провальные ответы в журнале неотличимы, и учиться
    /// на записях не на чем.
    pub rate: String,
    pub quit: String,
}

impl Default for Overlay {
    fn default() -> Self {
        Self { x: 1_370.0, y: 40.0, width: 470.0, height: 560.0, opacity: 0.93, font_size: 15.0 }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self { pre_roll_ms: 300, min_hold_ms: 250, auto: true }
    }
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            base_url: "https://routerai.ru/api/v1".into(),
            prompt_path: "prompts/default.md".into(),
            history_turns: 60,
            history_images: 3,
            brief_path: "prompts/brief.md".into(),
            auto: Profile {
                model: "inception/mercury-2.5-preview".into(),
                fallback_model: "anthropic/claude-haiku-4.5".into(),
                max_tokens: 4000,
                reasoning: false,
            },
            manual: Profile {
                // Opus оказался прожорлив не по задаче: цена за ответ на два
                // порядка выше, а суфлёру нужна короткая реплика.
                model: "inception/mercury-2.5-preview".into(),
                fallback_model: "anthropic/claude-haiku-4.5".into(),
                max_tokens: 8000,
                reasoning: false,
            },
            vision_model: "meta/muse-spark-1.3-contributor".into(),
            vision_max_tokens: 8000,
            models: [
                "inception/mercury-2.5-preview",
                "meta/muse-spark-1.3-contributor",
                "anthropic/claude-haiku-4.5",
                "anthropic/claude-sonnet-4.5",
                "~anthropic/claude-opus-latest",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            screen_target: "window".into(),
            screen_prompt: "На экране то, что сейчас видит пользователь. Не пересказывай \
экран — он его видит. Если это код или текст для правки, назови, что исправить, со ссылкой \
на строку. Если это задача или вопрос, дай ответ. Держись формата суфлёра."
                .into(),
        }
    }
}

impl Default for Profile {
    fn default() -> Self {
        Llm::default().auto
    }
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            model_path: "models/ggml-large-v3-turbo-q5_0.bin".into(),
            language: "ru".into(),
            confidence: 0.55,
            // Пороги — те же, что у whisper.cpp: их правка должна быть
            // осознанной, а не побочной от обновления ghost.
            best_of: 5,
            no_speech_thold: 0.6,
            entropy_thold: 2.4,
            logprob_thold: -1.0,
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            theirs: "LControl".into(),
            mine: "LShift".into(),
            screen: "RControl".into(),
            reset: "Ctrl+Alt+R".into(),
            settings: "Ctrl+Alt+S".into(),
            listen: "Ctrl+Alt+L".into(),
            remote: "Ctrl+Alt+G".into(),
            mute: "Ctrl+Alt+M".into(),
            move_window: "Ctrl+Alt+D".into(),
            input: "Ctrl+Alt+Enter".into(),
            screenshot: "Ctrl+Alt+P".into(),
            session: "Ctrl+Alt+Space".into(),
            rate: "Ctrl+Alt+X".into(),
            quit: "Ctrl+Alt+Q".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            overlay: Overlay::default(),
            input: Input::default(),
            vad: Vad::default(),
            speculation: Speculation::default(),
            coverage: Coverage::default(),
            llm: Llm::default(),
            stt: Stt::default(),
            audio: Audio::default(),
            session: Session::default(),
            dev: Dev::default(),
            voices: Voices::default(),
            remote: Remote::default(),
            hotkeys: Hotkeys::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(body) => match toml::from_str(&body) {
                Ok(cfg) => cfg,
                Err(e) => {
                    // Битый конфиг не повод не запуститься: работаем на
                    // значениях по умолчанию и говорим об этом.
                    eprintln!("[ghost] config.toml не разобран ({e}), беру значения по умолчанию");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = toml::to_string_pretty(self)?;
        std::fs::write(path, body)?;
        Ok(())
    }

    /// Файл обстановки как есть — для правки в настройках.
    ///
    /// Возвращается вместе с пояснениями: их правят и сохраняют обратно, и
    /// вычищать их здесь значило бы стереть их при первом же сохранении.
    pub fn brief(&self) -> String {
        std::fs::read_to_string(&self.llm.brief_path).unwrap_or_default()
    }

    /// Обстановка в том виде, в каком её видит модель.
    ///
    /// Пояснения в файле — для человека, а не для модели: пока их не убирали,
    /// в системный промпт на каждом запросе уходило полторы тысячи символов
    /// инструкции «заполните перед сессией», и модель читала указание
    /// заполнить форму вместо описания встречи.
    ///
    /// Незаполненный шаблон — это отсутствие обстановки, а не пустая
    /// обстановка: одни заголовки в промпт не идут.
    pub fn situation(&self) -> String {
        let text = strip_comments(&self.brief());
        let has_body = text
            .lines()
            .map(str::trim)
            .any(|l| !l.is_empty() && !l.starts_with('#'));
        if has_body {
            text
        } else {
            String::new()
        }
    }

    /// Термины и имена из обстановки — затравка для распознавания.
    ///
    /// Без неё whisper пишет незнакомые слова фонетически, и модель получает
    /// вопрос с искажённым ключевым термином.
    pub fn glossary(&self) -> String {
        let mut terms: Vec<String> = Vec::new();
        let mut inside = false;
        for line in self.situation().lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                inside = trimmed.to_lowercase().contains("термин");
                continue;
            }
            if inside && !trimmed.is_empty() {
                terms.extend(
                    trimmed
                        .split([',', ';'])
                        .map(str::trim)
                        .filter(|t| looks_like_term(t))
                        .map(str::to_string),
                );
            }
        }
        if terms.is_empty() {
            return String::new();
        }

        // Бюджет затравки у whisper небольшой, и лишнее в ней не нейтрально:
        // затравка смещает декодирование, поэтому длинный хвост разбавляет
        // нужные слова, вместо того чтобы помогать.
        let mut joined = String::new();
        for term in terms {
            if joined.chars().count() + term.chars().count() + 2 > GLOSSARY_MAX {
                break;
            }
            if !joined.is_empty() {
                joined.push_str(", ");
            }
            joined.push_str(&term);
        }
        // Затравка работает как обычный текст, поэтому её оформляем фразой.
        format!("Термины и имена: {joined}.")
    }

    pub fn prompt(&self) -> String {
        let base = std::fs::read_to_string(&self.llm.prompt_path).unwrap_or_else(|e| {
            eprintln!("[ghost] промпт не прочитан ({e})");
            "Ты — суфлёр. Отвечай коротко.".into()
        });
        let brief = self.situation();
        if brief.trim().is_empty() {
            return base;
        }
        format!("{base}\n\n# Обстановка этой сессии\n\n{}", brief.trim())
    }

    pub fn prompt_path(&self) -> PathBuf {
        PathBuf::from(&self.llm.prompt_path)
    }
}

/// Убирает пояснения в скобках `<!-- -->`.
///
/// Они живут в том же файле, что и данные: человек правит обстановку прямо
/// поверх подсказок и не должен переписывать их в отдельный файл, чтобы
/// вспомнить, что куда писать.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            // Незакрытая скобка: считаем, что пояснение идёт до конца файла —
            // иначе недописанный комментарий уехал бы в модель целиком.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Похоже ли на термин, а не на фразу.
///
/// Разбор берёт строки под заголовком «Термины» и режет их по запятым, но в
/// том же файле лежат пояснения человеку. Пока их не отсеивали, в затравку
/// уходили куски инструкции — и «идемпотентность» распознавалась как
/// «темпотерпность», потому что бюджет затравки был занят прозой.
fn looks_like_term(text: &str) -> bool {
    !text.is_empty()
        && text.chars().count() <= 40
        && text.split_whitespace().count() <= 4
        && !text.contains(['.', '!', '?', ':', '«', '»', '*'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn неполный_файл_дополняется_умолчаниями() {
        // Файл на диске отстаёт от кода: новые разделы должны появляться сами,
        // иначе после обновления половина настроек оказывается нулевой.
        let toml = "[overlay]\nfont_size = 20.0\n";
        let cfg: Config = toml::from_str(toml).unwrap();

        assert_eq!(cfg.overlay.font_size, 20.0, "заданное читается");
        assert_eq!(cfg.overlay.opacity, Overlay::default().opacity, "остальное берётся из умолчаний");
        assert_eq!(cfg.hotkeys.quit, Hotkeys::default().quit, "и целые пропущенные разделы тоже");
        assert!(!cfg.llm.auto.model.is_empty());
    }

    #[test]
    fn настройки_переживают_запись_и_чтение() {
        let mut cfg = Config::default();
        cfg.llm.manual.model = "проверка".into();
        cfg.vad.endpoint_ms = 777;
        cfg.hotkeys.listen = "F13".into();

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();

        assert_eq!(back.llm.manual.model, "проверка");
        assert_eq!(back.vad.endpoint_ms, 777);
        assert_eq!(back.hotkeys.listen, "F13");
    }

    #[test]
    fn битый_файл_не_роняет_запуск() {
        let path = std::env::temp_dir().join("ghost-битый.toml");
        std::fs::write(&path, "это не toml [[[").unwrap();

        let cfg = Config::load(&path);

        assert_eq!(cfg.overlay.font_size, Overlay::default().font_size);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn все_клавиши_по_умолчанию_разбираются() {
        // Опечатка в умолчании оставила бы действие без привязки молча.
        let k = Hotkeys::default();
        for spec in [
            &k.theirs, &k.mine, &k.screen, &k.reset, &k.settings, &k.listen, &k.remote, &k.mute,
            &k.move_window, &k.input, &k.screenshot, &k.session, &k.rate, &k.quit,
        ] {
            assert!(
                crate::hotkeys::keys::parse(spec).is_some(),
                "привязка «{spec}» не разбирается"
            );
        }
    }

    #[test]
    fn глоссарий_собирается_из_раздела_терминов() {
        let dir = std::env::temp_dir();
        let path = dir.join("ghost-бриф.md");
        std::fs::write(
            &path,
            "# Обстановка\nсобеседование\n\n# Термины\nitem2vec, ClickHouse\nKafka\n",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.llm.brief_path = path.to_string_lossy().into_owned();

        let glossary = cfg.glossary();
        assert!(glossary.contains("item2vec"));
        assert!(glossary.contains("ClickHouse"));
        assert!(glossary.contains("Kafka"), "термины со следующей строки тоже берутся");
        assert!(!glossary.contains("собеседование"), "обстановка в затравку не идёт");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn пояснения_из_шаблона_не_уходят_ни_в_модель_ни_в_затравку() {
        // Так выглядел незаполненный `prompts/brief.md`: пояснения лежали в
        // тексте, и в затравку whisper уходило пятьсот символов инструкции про
        // «фонетически» и «Пример строки для замены», а в системный промпт —
        // указание заполнить форму. Термины при этом были только в хвосте.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ghost-шаблон-{}.md", std::process::id()));
        std::fs::write(
            &path,
            "<!--\nЗаполните перед сессией. Например:\n  item2vec, Kafka\n-->\n\n# Обстановка\n\n# Термины\n",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.llm.brief_path = path.to_string_lossy().into_owned();

        assert!(cfg.situation().is_empty(), "незаполненный шаблон — это отсутствие обстановки");
        assert!(cfg.glossary().is_empty(), "затравка из пояснений хуже, чем никакой");
        assert!(!cfg.prompt().contains("Обстановка этой сессии"));
        // Для правки файл всё равно отдаётся целиком: иначе первое сохранение
        // стёрло бы подсказки, ради которых они там и лежат.
        assert!(cfg.brief().contains("Заполните перед сессией"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn в_затравку_идут_термины_а_не_фразы() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ghost-термины-{}.md", std::process::id()));
        std::fs::write(
            &path,
            "# Термины\nItem2vec, ClickHouse\nБез него незнакомое слово записывается фонетически.\nA/B-тест\n",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.llm.brief_path = path.to_string_lossy().into_owned();

        let glossary = cfg.glossary();
        assert!(glossary.contains("Item2vec") && glossary.contains("A/B-тест"));
        assert!(!glossary.contains("фонетически"), "проза вытесняет из затравки нужное");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn затравка_не_длиннее_бюджета() {
        // Длинный список не нейтрален: затравка смещает декодирование, и хвост
        // разбавляет те слова, ради которых её и пишут.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ghost-длинно-{}.md", std::process::id()));
        let many: Vec<String> = (0..300).map(|i| format!("термин{i}")).collect();
        std::fs::write(&path, format!("# Термины\n{}\n", many.join(", "))).unwrap();

        let mut cfg = Config::default();
        cfg.llm.brief_path = path.to_string_lossy().into_owned();

        let glossary = cfg.glossary();
        assert!(glossary.chars().count() < GLOSSARY_MAX + 40, "затравка {}", glossary.len());
        assert!(glossary.contains("термин0"), "первые термины важнее последних");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn без_брифа_глоссарий_пуст() {
        let mut cfg = Config::default();
        cfg.llm.brief_path = "нет-такого-файла.md".into();
        assert!(cfg.glossary().is_empty());
    }
}
