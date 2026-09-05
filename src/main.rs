// В release прячем консоль; в debug она нужна, чтобы видеть логи.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod hotkeys;
mod llm;
mod overlay;
mod remote;
mod screen;
mod session;
mod speaker;
mod stt;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use audio::capture::Source;
use audio::vad;

fn main() -> eframe::Result<()> {
    start_logging();
    if already_running() {
        // Второй ghost на машине ведёт себя плохо и молча: хуки клавиатуры
        // стоят оба, и комбинацию глотает тот, кто первый в цепочке, — до
        // второго она может не дойти вовсе. Порт помощника остаётся за первым,
        // но второй всё равно печатает и кладёт в буфер ссылку, которая не
        // работает. И запись разговора каждый ведёт свою.
        show_running();
        log::warn!("ghost уже запущен, эта копия выходит");
        eprintln!("[ghost] уже запущен");
        return Ok(());
    }
    load_env(Path::new(".env"));

    // Режим для отладки окна: без распознавания. Загрузка whisper занимает
    // двадцать секунд, и каждая проверка поведения окна стоила этих секунд —
    // а окну модель не нужна.
    let ui_only = std::env::args().any(|a| a == "--ui-only");

    let mut cfg = config::Config::load(Path::new(config::PATH));
    if ui_only {
        cfg.stt.model_path.clear();
        log::info!("режим --ui-only: распознавание не загружается");
    }
    let cfg = cfg;
    // Первый запуск: кладём файл на диск, чтобы было что править руками.
    if !Path::new(config::PATH).exists() {
        if let Err(e) = cfg.save(Path::new(config::PATH)) {
            eprintln!("[ghost] config.toml не записан: {e:#}");
        }
    }

    let glossary = cfg.glossary();
    eprintln!(
        "[ghost] обстановка: {} символов, терминов в затравке: {}",
        cfg.situation().trim().len(),
        if glossary.is_empty() { 0 } else { glossary.matches(',').count() + 1 }
    );

    let stt_model = PathBuf::from(&cfg.stt.model_path);
    let language = cfg.stt.language.clone();
    let overlay_cfg = cfg.overlay.clone();
    let hotkeys_cfg = cfg.hotkeys.clone();
    eprintln!(
        "[ghost] llm: ручная {} · авто {} @ {}",
        cfg.llm.manual.model, cfg.llm.auto.model, cfg.llm.base_url
    );

    let cfg: config::Shared = Arc::new(RwLock::new(cfg));

    let keys = hotkeys::spawn(&hotkeys_cfg);
    let (loopback_device, input_device) = {
        let g = cfg.read().unwrap();
        (g.audio.loopback_device.clone(), g.audio.input_device.clone())
    };
    let theirs = audio::capture::spawn(Source::Loopback, &loopback_device);
    let mine = audio::capture::spawn(Source::Mic, &input_device);
    let stt = stt::spawn(cfg.clone(), stt_model, language);

    // Нарезка на реплики идёт всегда, но выдаёт их только в автоматическом
    // режиме. При запуске он выключен: слушать до начала сессии нечего и
    // незачем — режим включится сам, когда сессию начнут.
    let gates = vad::Gates::new();
    let (utt_tx, utt_rx) = crossbeam_channel::bounded(16);
    vad::spawn(stt::Speaker::Them, theirs.subscribe(), cfg.clone(), gates.clone(), utt_tx.clone());
    vad::spawn(stt::Speaker::Me, mine.subscribe(), cfg.clone(), gates.clone(), utt_tx);
    // Две независимые дорожки: у каждой свой контекст, иначе ручные и
    // автоматические реплики перемешались бы в одной истории.
    let key = std::env::var("ROUTERAI_KEY").unwrap_or_default();
    // Сессия пока только заведена: каталог и запись появятся, когда её начнут
    // кнопкой.
    let session = session::Session::new(&cfg);
    let remote = remote::spawn(cfg.clone(), &theirs, &mine);
    eprintln!("[ghost] канал помощника (локально): {}", remote.lan_url);

    // Лента одна, поэтому и история одна. Модель при этом по-прежнему
    // выбирается по источнику реплики: ручной вопрос и поток прослушивания
    // ходят к разным.
    let assistant = llm::spawn(cfg.clone(), key);

    let viewport = egui::ViewportBuilder::default()
        .with_title("ghost")
        .with_inner_size([overlay_cfg.width, overlay_cfg.height])
        .with_position([overlay_cfg.x, overlay_cfg.y])
        .with_decorations(false)
        // Прозрачность не просим у winit: кадр с альфой мы отдаём системе
        // сами (`overlay::layered`), а его попиксельная композиция через DWM
        // этому только мешает.
        .with_transparent(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_active(false)
        // Обязательно true: при false winit фиксирует максимальный размер
        // равным начальному, и окно потом не растянуть.
        // Сквозной стиль не ставим: клики разбираются по попаданию, иначе
        // уголок перетаскивания тоже стал бы прозрачным (см. overlay::win).
        .with_resizable(true);

    // Бэкенд задаётся явно. По умолчанию wgpu выбирает Vulkan, а прозрачную
    // композицию на Windows умеет DXGI: у Vulkan-поверхности режим смешивания
    // с рабочим столом обычно только непрозрачный, и окно молча становится
    // сплошным. Это выяснилось из журнала: «wgpu adapter: backend: Vulkan».
    // Переопределяется переменной GHOST_BACKEND — чтобы сравнить бэкенды на
    // живой машине, не пересобирая, и чтобы было куда отступить, если DXGI
    // где-то поведёт себя хуже.
    let backends = match std::env::var("GHOST_BACKEND").unwrap_or_default().as_str() {
        "vulkan" => eframe::wgpu::Backends::VULKAN,
        "gl" => eframe::wgpu::Backends::GL,
        _ => eframe::wgpu::Backends::DX12,
    };
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = backends;
    let wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        wgpu_setup: setup.into(),
        ..Default::default()
    };

    eframe::run_native(
        "ghost",
        eframe::NativeOptions { viewport, wgpu_options, ..Default::default() },
        Box::new(move |cc| {
            Ok(Box::new(overlay::Ghost::new(
                cc, cfg, keys, theirs, mine, stt, assistant, gates, utt_rx, remote, session,
            )))
        }),
    )
}

/// Занимает признак единственной копии. `true` — ghost уже запущен.
///
/// Мьютекс намеренно не закрывается и не хранится: он живёт, пока жив процесс,
/// и система освобождает его сама — в том числе если процесс упал.
fn already_running() -> bool {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    // `Local\` — на сеанс входа: две учётные записи на одной машине друг другу
    // не мешают, у каждой свой ghost.
    unsafe {
        if CreateMutexW(None, true, windows::core::w!("Local\\ghost-один-экземпляр")).is_err() {
            // Признак не занялся — пускаем: отказать в запуске из-за сбоя
            // системного вызова хуже, чем пропустить вторую копию.
            return false;
        }
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

/// Просит уже запущенную копию показаться.
///
/// Окно спрятано от панели задач и Alt+Tab, поэтому человек, нажавший ярлык
/// второй раз, просто не видит ghost и считает, что тот не запустился.
/// Сообщения показать нельзя: оверлей исключён из записи экрана, а окно с
/// текстом «уже запущен» — нет, и на демонстрации экрана его увидят все.
fn show_running() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

    unsafe {
        let Ok(hwnd) = FindWindowW(windows::core::w!("ghost_board"), None) else {
            return;
        };
        let _ = PostMessageW(Some(hwnd), overlay::tray::SHOW, WPARAM(0), LPARAM(0));
    }
}

/// Диагностика оконного стека — в файл.
///
/// eframe, winit и wgpu сообщают о своих бедах через `log`, и до сих пор эти
/// сообщения уходили в никуда: логгер не был установлен, а окно запускается без
/// консоли. Именно там пишется, например, что прозрачность запрошена, но
/// поверхность её не поддерживает — то есть ровно то, что мы неделю искали
/// по фотографиям экрана.
fn start_logging() {
    let path = std::path::Path::new("ghost.log");
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[ghost] журнал {} не открылся: {e}", path.display());
            return;
        }
    };

    // Уровень можно поднять через RUST_LOG, не пересобирая: подробности про
    // поверхность и композицию нужны редко, но когда нужны — ждать сборку
    // ради одной строки фильтра неправильно.
    let filters = std::env::var("RUST_LOG")
        // wgpu и naga на info захлёбываются подробностями про шейдеры.
        .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into());

    env_logger::Builder::new()
        .parse_filters(&filters)
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
    log::info!("ghost запущен");
}

/// Простой разбор .env: секреты не должны попадать ни в конфиг, ни в
/// репозиторий. Уже заданная переменная окружения имеет приоритет над файлом.
fn load_env(path: &Path) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}
