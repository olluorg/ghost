//! Третий канал: помощник на другом устройстве видит экран, слышит разговор и
//! пишет подсказки.
//!
//! Транспорт намеренно простой — кадры JPEG по запросу и звук с чатом по
//! WebSocket. Полноценный WebRTC дал бы задержку 100–200 мс вместо 300–600, но
//! потребовал бы видеокодека, ICE, DTLS и TURN; человеку, читающему код на
//! чужом экране, эта разница незаметна.
//!
//! Канал транслирует наружу экран и весь разговор, поэтому выключен по
//! умолчанию, закрыт токеном и показывает несворачиваемый индикатор, пока
//! работает.

mod page;
mod tunnel;

pub use tunnel::Tunnel;

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use crossbeam_channel::{bounded, Receiver, Sender};
use serde::Deserialize;

use crate::audio::capture::Audio;
use crate::config;

/// Сколько кадров держим в памяти: только последний, помощнику нужен «сейчас».
type Frame = Arc<Mutex<Vec<u8>>>;

pub struct Remote {
    pub active: Arc<AtomicBool>,
    pub viewers: Arc<AtomicUsize>,
    /// Сообщения помощника.
    pub messages: Receiver<String>,
    /// Адрес в локальной сети. Годится, только если помощник в той же сети.
    pub lan_url: String,
    token: String,
    cfg: config::Shared,
    tunnel: tunnel::Handle,
    feed: tokio::sync::broadcast::Sender<String>,
}

impl Remote {
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn tunnel(&self) -> Tunnel {
        self.tunnel.state()
    }

    /// Ссылка, которую надо отдать помощнику. Пока туннель поднимается, отдаём
    /// локальную: в одной сети она уже работает.
    pub fn url(&self) -> String {
        match self.tunnel.state() {
            Tunnel::Ready(base) => format!("{base}/?t={}", self.token),
            _ => self.lan_url.clone(),
        }
    }

    pub fn toggle(&self) -> bool {
        let next = !self.active();
        self.active.store(next, Ordering::Relaxed);

        let (kind, exe, port) = {
            let g = self.cfg.read().unwrap();
            (g.remote.tunnel.clone(), g.remote.tunnel_exe.clone(), g.remote.port)
        };

        // Туннель живёт ровно столько же, сколько открыт канал: держать
        // публичный адрес при закрытом канале незачем.
        if next {
            self.tunnel.start(&kind, exe, port);
        } else {
            self.tunnel.stop();
        }

        next
    }

    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Relaxed)
    }

    /// Отправляет помощнику строку ленты. Молча ничего не делает, если канал
    /// закрыт или никто не смотрит.
    pub fn publish(&self, kind: &str, who: &str, text: &str) {
        if !self.active() {
            return;
        }
        let payload = format!(
            "{{\"kind\":\"{}\",\"who\":\"{}\",\"text\":\"{}\"}}",
            escape(kind),
            escape(who),
            escape(text)
        );
        let _ = self.feed.send(payload);
    }
}

struct Srv {
    token: String,
    frame: Frame,
    audio: tokio::sync::broadcast::Sender<Vec<u8>>,
    /// Что показать помощнику текстом: расшифровка и ответы модели. Без этого
    /// он не знает, что уже сказано, и дублирует подсказки.
    feed: tokio::sync::broadcast::Sender<String>,
    active: Arc<AtomicBool>,
    viewers: Arc<AtomicUsize>,
    chat: Sender<String>,
}

pub fn spawn(cfg: config::Shared, theirs: &Audio, mine: &Audio) -> Remote {
    let (token, port, enabled, fresh) = {
        let mut guard = cfg.write().unwrap();
        let fresh = guard.remote.token.is_empty();
        if fresh {
            guard.remote.token = make_token();
        }
        (guard.remote.token.clone(), guard.remote.port, guard.remote.enabled, fresh)
    };

    // Токен сохраняем сразу: иначе ссылка менялась бы при каждом запуске, и
    // помощнику пришлось бы присылать её заново.
    if fresh {
        if let Err(e) = cfg.read().unwrap().save(std::path::Path::new(config::PATH)) {
            eprintln!("[ghost] токен помощника не сохранён: {e:#}");
        }
    }

    let active = Arc::new(AtomicBool::new(enabled));
    let viewers = Arc::new(AtomicUsize::new(0));
    let frame: Frame = Arc::new(Mutex::new(Vec::new()));
    let (chat_tx, chat_rx) = bounded::<String>(32);
    let (audio_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(64);
    let (feed_tx, _) = tokio::sync::broadcast::channel::<String>(64);

    spawn_video(cfg.clone(), Arc::clone(&active), Arc::clone(&frame));
    spawn_audio(cfg.clone(), Arc::clone(&active), theirs, mine, audio_tx.clone());

    let srv = Arc::new(Srv {
        token: token.clone(),
        frame,
        audio: audio_tx,
        feed: feed_tx.clone(),
        active: Arc::clone(&active),
        viewers: Arc::clone(&viewers),
        chat: chat_tx,
    });
    spawn_server(srv, port);

    let host = local_ip().map_or("localhost".to_string(), |ip| ip.to_string());
    let remote = Remote {
        active,
        viewers,
        messages: chat_rx,
        lan_url: format!("http://{host}:{port}/?t={token}"),
        token,
        cfg: cfg.clone(),
        tunnel: tunnel::Handle::new(),
        feed: feed_tx,
    };

    // Канал мог быть включён в конфиге — тогда туннель нужен сразу.
    if enabled {
        let (kind, exe, port) = {
            let g = cfg.read().unwrap();
            (g.remote.tunnel.clone(), g.remote.tunnel_exe.clone(), g.remote.port)
        };
        remote.tunnel.start(&kind, exe, port);
    }

    remote
}

/// Кадры снимаются только пока канал включён: захват экрана стоит около
/// сотни миллисекунд, впустую его крутить незачем.
fn spawn_video(cfg: config::Shared, active: Arc<AtomicBool>, frame: Frame) {
    thread::spawn(move || loop {
        let (fps, width, quality) = {
            let g = cfg.read().unwrap();
            (g.remote.fps.max(1), g.remote.width, g.remote.quality)
        };

        if !active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(300));
            continue;
        }

        match crate::screen::capture_jpeg(width, quality) {
            Ok(jpeg) => *frame.lock().unwrap() = jpeg,
            Err(e) => eprintln!("[ghost] кадр для помощника: {e:#}"),
        }
        thread::sleep(Duration::from_millis(1000 / fps as u64));
    });
}

/// Сводит оба канала в один поток: помощник должен слышать весь разговор, а не
/// одну его сторону.
fn spawn_audio(
    cfg: config::Shared,
    active: Arc<AtomicBool>,
    theirs: &Audio,
    mine: &Audio,
    out: tokio::sync::broadcast::Sender<Vec<u8>>,
) {
    let (a, b) = (theirs.subscribe(), mine.subscribe());

    thread::spawn(move || {
        let mut buf_a: Vec<f32> = Vec::new();
        let mut buf_b: Vec<f32> = Vec::new();
        // Приглушение микрофона держим между блоками и меняем плавно: резкие
        // переключения слышны как щелчки на каждой фразе.
        let mut duck = 1.0f32;

        loop {
            crossbeam_channel::select! {
                recv(a) -> chunk => match chunk { Ok(c) => buf_a.extend(c), Err(_) => break },
                recv(b) -> chunk => match chunk { Ok(c) => buf_b.extend(c), Err(_) => break },
            }

            let (on, suppress) = {
                let g = cfg.read().unwrap();
                (
                    active.load(Ordering::Relaxed) && g.remote.audio,
                    g.vad.suppress_mic_while_loopback,
                )
            };

            // Сведение идёт по меньшему из буферов, поэтому замолчавшее совсем
            // устройство остановило бы и второй канал. Добираем тишиной.
            const STALL: usize = crate::audio::SAMPLE_RATE as usize / 2;
            if buf_a.len() > STALL && buf_b.is_empty() {
                buf_b.resize(buf_a.len(), 0.0);
            }
            if buf_b.len() > STALL && buf_a.is_empty() {
                buf_a.resize(buf_b.len(), 0.0);
            }

            // Каналы добивают тишину независимо, каждый по своему таймеру, и со
            // временем расходятся. Отставший канал даёт вторую копию голоса всё
            // позже — это слышно как нарастающее эхо. Выравниваем.
            const MAX_SKEW: usize = crate::audio::SAMPLE_RATE as usize / 5;
            if buf_a.len() > buf_b.len() + MAX_SKEW {
                let extra = buf_a.len() - buf_b.len() - MAX_SKEW;
                buf_a.drain(..extra);
            }
            if buf_b.len() > buf_a.len() + MAX_SKEW {
                let extra = buf_b.len() - buf_a.len() - MAX_SKEW;
                buf_b.drain(..extra);
            }

            let n = buf_a.len().min(buf_b.len());
            if n == 0 {
                continue;
            }
            if !on {
                buf_a.drain(..n);
                buf_b.drain(..n);
                continue;
            }

            // Главный источник эха: без наушников микрофон слышит собеседника
            // из колонок, и помощник получает его дважды — вторую копию с
            // задержкой акустического пути. Пока говорит дальняя сторона,
            // микрофон приглушается.
            const ECHO_FLOOR: f32 = 0.004;
            let far = rms(&buf_a[..n]);
            let near = rms(&buf_b[..n]);
            let target = if suppress && far > ECHO_FLOOR && far > near * 1.2 { 0.06 } else { 1.0 };
            duck += (target - duck) * 0.25;

            // Сумма с ограничением: две дорожки редко громкие одновременно,
            // а клиппинг слышнее, чем потеря половины громкости.
            let mut pcm = Vec::with_capacity(n * 2);
            for i in 0..n {
                let mixed = (buf_a[i] + buf_b[i] * duck).clamp(-1.0, 1.0);
                pcm.extend_from_slice(&((mixed * i16::MAX as f32) as i16).to_le_bytes());
            }
            buf_a.drain(..n);
            buf_b.drain(..n);
            let _ = out.send(pcm);
        }
    });
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

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

fn spawn_server(srv: Arc<Srv>, port: u16) {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[ghost] сервер помощника: {e}");
                return;
            }
        };

        runtime.block_on(async move {
            let app = Router::new()
                .route("/", get(index))
                .route("/frame.jpg", get(frame_jpg))
                .route("/ws", get(ws_upgrade))
                .with_state(srv);

            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    eprintln!("[ghost] сервер помощника слушает {addr}");
                    if let Err(e) = axum::serve(listener, app).await {
                        eprintln!("[ghost] сервер помощника: {e}");
                    }
                }
                Err(e) => eprintln!("[ghost] порт {port} занят: {e}"),
            }
        });
    });
}

#[derive(Deserialize)]
struct Auth {
    t: Option<String>,
}

/// Токен обязателен на каждом маршруте, включая кадры: иначе картинку можно
/// было бы забрать, зная один только адрес.
fn allowed(srv: &Srv, auth: &Auth) -> bool {
    srv.active.load(Ordering::Relaxed) && auth.t.as_deref() == Some(srv.token.as_str())
}

async fn index(State(srv): State<Arc<Srv>>, Query(auth): Query<Auth>) -> Response {
    if !allowed(&srv, &auth) {
        return (StatusCode::FORBIDDEN, "нет доступа").into_response();
    }
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page::HTML).into_response()
}

async fn frame_jpg(State(srv): State<Arc<Srv>>, Query(auth): Query<Auth>) -> Response {
    if !allowed(&srv, &auth) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let jpeg = srv.frame.lock().unwrap().clone();
    if jpeg.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        jpeg,
    )
        .into_response()
}

async fn ws_upgrade(
    State(srv): State<Arc<Srv>>,
    Query(auth): Query<Auth>,
    ws: WebSocketUpgrade,
) -> Response {
    if !allowed(&srv, &auth) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| serve_ws(socket, srv))
}

async fn serve_ws(mut socket: WebSocket, srv: Arc<Srv>) {
    srv.viewers.fetch_add(1, Ordering::Relaxed);
    let mut audio = srv.audio.subscribe();
    let mut feed = srv.feed.subscribe();

    loop {
        tokio::select! {
            frame = audio.recv() => match frame {
                Ok(pcm) => {
                    if socket.send(Message::Binary(pcm.into())).await.is_err() {
                        break;
                    }
                }
                // Отставших не догоняем: помощнику нужен звук «сейчас».
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            line = feed.recv() => match line {
                Ok(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        let _ = srv.chat.try_send(text);
                    }
                }
                Some(Ok(_)) => {}
                _ => break,
            },
        }

        if !srv.active.load(Ordering::Relaxed) {
            break;
        }
    }

    srv.viewers.fetch_sub(1, Ordering::Relaxed);
}

fn make_token() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..20).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

/// Адрес в локальной сети. Пакеты никуда не уходят — сокет только выбирает
/// маршрут, чтобы узнать, с какого интерфейса нас увидят.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}
