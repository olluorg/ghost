//! Захват звука через WASAPI. Два источника одним кодом.
//!
//! Loopback (собеседник): устройство берётся с направлением `Render` (то, что
//! играет), а клиент инициализируется как `Capture` — именно эта комбинация
//! включает `AUDCLNT_STREAMFLAGS_LOOPBACK`.
//!
//! Микрофон (я): обычное устройство `Capture`.
//!
//! `autoconvert: true` добавляет `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, поэтому
//! оба источника отдают сразу 16 кГц моно f32 и ресемплить самим не нужно.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use wasapi::{
    initialize_mta, Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

use super::{Ring, SAMPLE_RATE};

/// Сколько ждём пакет, прежде чем счесть, что наступила тишина.
///
/// Это напрямую задержка: пока звук идёт, событие приходит каждые ~10 мс и
/// ждать нечего, но в тишине мы узнаём о ней только по этому таймауту. А
/// тишина — ровно то, по чему детектор речи понимает «собеседник договорил».
const WAIT_MS: u32 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Системный вывод — речь собеседника.
    Loopback,
    /// Микрофон — собственная речь.
    Mic,
}

impl Source {
    /// Направление УСТРОЙСТВА. Клиент в обоих случаях инициализируется как
    /// Capture; разница в устройстве и определяет, включится ли loopback.
    fn device_direction(self) -> Direction {
        match self {
            Source::Loopback => Direction::Render,
            Source::Mic => Direction::Capture,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Loopback => "собеседник",
            Source::Mic => "микрофон",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    Starting,
    Running { device: String },
    Failed(String),
}

/// Реестр подписчиков на живой поток. Потребителей больше одного (детектор
/// речи, раздача помощнику), поэтому одним каналом не обойтись.
type Taps = Arc<Mutex<Vec<Sender<Vec<f32>>>>>;

pub struct Audio {
    pub ring: Arc<Mutex<Ring>>,
    pub status: Arc<Mutex<Status>>,
    /// Что предлагать в настройках. Обновляется при каждом открытии потока.
    pub devices: Arc<Mutex<Vec<String>>>,
    taps: Taps,
    /// Выбранное устройство. Пусто — системное по умолчанию.
    wanted: Arc<Mutex<String>>,
    /// Счётчик пересоздания потока: увеличение просит переоткрыть устройство.
    generation: Arc<AtomicU64>,
}

impl Audio {
    /// Новое ответвление живого потока. Медленный подписчик теряет куски, а не
    /// тормозит захват: очередь ограничена и переполнение просто отбрасывается.
    /// Меняет устройство на ходу. Поток переоткрывается сам, перезапуск
    /// приложения посреди разговора недопустим.
    pub fn select(&self, name: &str) {
        let mut wanted = self.wanted.lock().unwrap();
        if *wanted == name {
            return;
        }
        *wanted = name.to_string();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn selected(&self) -> String {
        self.wanted.lock().unwrap().clone()
    }

    pub fn subscribe(&self) -> Receiver<Vec<f32>> {
        let (tx, rx) = bounded(128);
        self.taps.lock().unwrap().push(tx);
        rx
    }
}

/// Кольцевой буфер даёт ретроспективный доступ, ответвления — живой поток.
/// Одно другим не заменить: по буферу нельзя узнать, что звук пришёл ИМЕННО
/// СЕЙЧАС, а по потоку — заглянуть на полминуты назад.
pub fn spawn(source: Source, device: &str) -> Audio {
    let ring = Arc::new(Mutex::new(Ring::new()));
    let status = Arc::new(Mutex::new(Status::Starting));
    let taps: Taps = Arc::new(Mutex::new(Vec::new()));
    let devices = Arc::new(Mutex::new(Vec::new()));
    let wanted = Arc::new(Mutex::new(device.to_string()));
    let generation = Arc::new(AtomicU64::new(0));

    thread::spawn({
        let ring = Arc::clone(&ring);
        let status = Arc::clone(&status);
        let taps = Arc::clone(&taps);
        let devices = Arc::clone(&devices);
        let wanted = Arc::clone(&wanted);
        let generation = Arc::clone(&generation);
        move || {
            if initialize_mta().ok().is_err() {
                *status.lock().unwrap() = Status::Failed("COM не поднялся".into());
                return;
            }
            // Внешний цикл переживает и смену устройства, и его пропажу:
            // выдернутые наушники не должны глушить приложение навсегда.
            loop {
                let target = wanted.lock().unwrap().clone();
                let at_start = generation.load(Ordering::SeqCst);
                *devices.lock().unwrap() = list_devices(source);

                match capture(source, &target, &ring, &status, &taps, &generation, at_start) {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("[ghost] {}: {e:#}", source.label());
                        *status.lock().unwrap() = Status::Failed(format!("{e:#}"));
                        thread::sleep(Duration::from_millis(1500));
                    }
                }
            }
        }
    });

    Audio { ring, status, devices, taps, wanted, generation }
}

/// Список устройств, из которых можно выбирать.
fn list_devices(source: Source) -> Vec<String> {
    let Ok(enumerator) = DeviceEnumerator::new() else {
        return Vec::new();
    };
    let Ok(collection) = enumerator.get_device_collection(&source.device_direction()) else {
        return Vec::new();
    };
    let Ok(count) = collection.get_nbr_devices() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|i| collection.get_device_at_index(i).ok())
        .filter_map(|d| d.get_friendlyname().ok())
        .collect()
}

/// Устройство по имени, а при пустом имени или промахе — системное.
fn pick_device(source: Source, wanted: &str) -> Result<Device> {
    let direction = source.device_direction();
    let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
    if !wanted.is_empty() {
        if let Ok(collection) = enumerator.get_device_collection(&direction) {
            if let Ok(device) = collection.get_device_with_name(wanted) {
                return Ok(device);
            }
        }
        eprintln!("[ghost] устройство «{wanted}» не найдено, беру системное");
    }
    enumerator
        .get_default_device(&direction)
        .with_context(|| format!("устройство по умолчанию для «{}»", source.label()))
}

/// Рассылает кусок всем подписчикам и выкидывает отвалившихся.
fn fan_out(taps: &Taps, chunk: &[f32]) {
    let mut list = taps.lock().unwrap();
    list.retain(|tap| !matches!(tap.try_send(chunk.to_vec()), Err(TrySendError::Disconnected(_))));
}

#[allow(clippy::too_many_arguments)]
fn capture(
    source: Source,
    wanted: &str,
    ring: &Arc<Mutex<Ring>>,
    status: &Arc<Mutex<Status>>,
    taps: &Taps,
    generation: &AtomicU64,
    at_start: u64,
) -> Result<()> {
    let device = pick_device(source, wanted)?;
    let name = device.get_friendlyname().unwrap_or_else(|_| "?".into());

    let mut client = device.get_iaudioclient().context("IAudioClient")?;

    // Просим ровно то, что нужно whisper; конвертацию делает движок WASAPI.
    let want = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 1, None);
    let (_default_period, min_period) = client.get_device_period()?;

    client
        .initialize_client(
            &want,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: min_period,
            },
        )
        .context("initialize_client (16 кГц моно f32, autoconvert)")?;

    let h_event = client.set_get_eventhandle()?;
    let capture_client = client.get_audiocaptureclient()?;
    let block = want.get_blockalign() as usize;

    let mut bytes: VecDeque<u8> = VecDeque::with_capacity(block * SAMPLE_RATE as usize);
    // Момент, до которого шкала буфера уже заполнена: по нему вычисляется,
    // сколько тишины дописать, чтобы она соответствовала реальному времени.
    let mut filled = Instant::now();
    client.start_stream()?;

    eprintln!("[ghost] {} запущен: {name}", source.label());
    *status.lock().unwrap() = Status::Running { device: name };

    loop {
        // Выбрали другое устройство — выходим и открываем его заново.
        if generation.load(Ordering::SeqCst) != at_start {
            let _ = client.stop_stream();
            return Ok(());
        }

        capture_client.read_from_device_to_deque(&mut bytes)?;

        let frames = bytes.len() / block;
        if frames > 0 {
            let mut out = Vec::with_capacity(frames);
            for _ in 0..frames {
                let mut b = [0u8; 4];
                for slot in b.iter_mut() {
                    *slot = bytes.pop_front().unwrap();
                }
                out.push(f32::from_le_bytes(b));
            }
            filled = Instant::now();
            ring.lock().unwrap().push(&out);
            fan_out(taps, &out);
        }

        if h_event.wait_for_event(WAIT_MS).is_err() {
            // При полной тишине пакеты могут не приходить вовсе. Дописываем
            // ровно столько нулей, сколько прошло времени: фиксированный блок
            // и растягивал бы шкалу, и грубил детектору паузу до размера блока.
            let n = (SAMPLE_RATE as f64 * filled.elapsed().as_secs_f64()) as usize;
            if n > 0 {
                filled = Instant::now();
                let silence = vec![0.0; n];
                ring.lock().unwrap().push(&silence);
                fan_out(taps, &silence);
            }
        }
    }
}
