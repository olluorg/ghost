//! Глобальные горячие клавиши через `WH_KEYBOARD_LL`.
//!
//! Почему низкоуровневый хук, а не `RegisterHotKey`: нужен не только факт
//! нажатия, но и ОТПУСКАНИЕ — без него не сделать push-to-talk.
//!
//! Колбэк хука обязан вернуться быстрее `LowLevelHooksTimeout` (по умолчанию
//! 300 мс), иначе Windows молча снимет хук. Поэтому внутри — только чтение
//! разделяемого состояния и неблокирующая отправка в канал.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetTimer, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
};

use crate::config;
use keys::Binding;

use super::keys;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    TheirsDown,
    TheirsUp,
    MineDown,
    MineUp,
    /// Удержание оказалось частью комбинации, а не намерением говорить.
    HoldCancel,
    Screen,
    Reset,
    Settings,
    /// Включить или выключить постоянное прослушивание.
    Listen,
    /// Включить или выключить канал помощника.
    Remote,
    /// Отключить помощнику звук, оставив картинку.
    Mute,
    /// Разрешить двигать и растягивать окно, не заходя в настройки.
    MoveWindow,
    /// Встать в поле ввода.
    Input,
    /// Снимок собственного окна.
    Screenshot,
    /// Начать сессию или закончить её.
    Session,
    /// Отметить последнюю подсказку негодной.
    Rate,
    Quit,
    /// Клавиша, пойманная в режиме назначения. Приходит только когда включён
    /// `capture_mode`. Модификаторы нужны, чтобы отличить «R» от «Ctrl+Alt+R».
    Captured { vk: u16, ctrl: bool, alt: bool, shift: bool },
}

#[derive(Clone, Copy, Default)]
struct Bindings {
    theirs: Option<Binding>,
    mine: Option<Binding>,
    screen: Option<Binding>,
    reset: Option<Binding>,
    settings: Option<Binding>,
    listen: Option<Binding>,
    remote: Option<Binding>,
    mute: Option<Binding>,
    move_window: Option<Binding>,
    input: Option<Binding>,
    screenshot: Option<Binding>,
    session: Option<Binding>,
    rate: Option<Binding>,
    quit: Option<Binding>,
}

static TX: OnceLock<Sender<Event>> = OnceLock::new();
static BINDINGS: RwLock<Bindings> = RwLock::new(Bindings {
    theirs: None,
    mine: None,
    screen: None,
    reset: None,
    settings: None,
    listen: None,
    remote: None,
    mute: None,
    move_window: None,
    input: None,
    screenshot: None,
    session: None,
    rate: None,
    quit: None,
});
/// Клавиши уже удерживаются: гасим автоповтор WM_KEYDOWN.
static THEIRS_HELD: AtomicBool = AtomicBool::new(false);
static MINE_HELD: AtomicBool = AtomicBool::new(false);
/// В режиме назначения любая клавиша уходит в UI и не делает ничего другого.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Включает режим назначения клавиши.
pub fn capture_mode(on: bool) {
    CAPTURING.store(on, Ordering::SeqCst);
}

/// Применяет привязки из конфига. Можно звать на лету.
pub fn rebind(cfg: &config::Hotkeys) {
    let parsed = Bindings {
        theirs: keys::parse(&cfg.theirs),
        mine: keys::parse(&cfg.mine),
        screen: keys::parse(&cfg.screen),
        reset: keys::parse(&cfg.reset),
        settings: keys::parse(&cfg.settings),
        listen: keys::parse(&cfg.listen),
        remote: keys::parse(&cfg.remote),
        mute: keys::parse(&cfg.mute),
        move_window: keys::parse(&cfg.move_window),
        input: keys::parse(&cfg.input),
        screenshot: keys::parse(&cfg.screenshot),
        session: keys::parse(&cfg.session),
        rate: keys::parse(&cfg.rate),
        quit: keys::parse(&cfg.quit),
    };
    *BINDINGS.write().unwrap() = parsed;
}

pub fn spawn(cfg: &config::Hotkeys) -> Receiver<Event> {
    rebind(cfg);
    let (tx, rx) = bounded(64);
    let _ = TX.set(tx);
    thread::spawn(|| unsafe { pump() });
    rx
}

unsafe fn pump() {
    let hmod = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => {
            eprintln!("[ghost] GetModuleHandleW: {e}");
            return;
        }
    };

    let mut hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hmod), 0) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[ghost] SetWindowsHookExW: {e}");
            return;
        }
    };
    eprintln!("[ghost] клавиатурный хук установлен");

    // Windows молча снимает низкоуровневый хук, если система решила, что он
    // отвечает слишком долго. Восстановить его нечем — узнать об этом изнутри
    // нельзя, поэтому просто переставляем заново по таймеру.
    const REINSTALL_MS: u32 = 20_000;
    SetTimer(None, 1, REINSTALL_MS, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message == WM_TIMER {
            hook = reinstall(hook, hmod);
            continue;
        }
        // Хук доставляется через очередь сообщений этого потока, поэтому насос
        // обязателен, даже если своих окон у потока нет.
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn reinstall(old: HHOOK, hmod: HINSTANCE) -> HHOOK {
    let _ = UnhookWindowsHookEx(old);
    match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hmod), 0) {
        Ok(new) => new,
        Err(e) => {
            eprintln!("[ghost] хук не переустановился: {e}");
            old
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = info.vkCode as u16;
    let msg = wparam.0 as u32;
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

    if CAPTURING.load(Ordering::SeqCst) {
        if down {
            send(Event::Captured {
                vk,
                ctrl: key_down(VK_CONTROL),
                alt: key_down(VK_MENU),
                shift: key_down(VK_SHIFT),
            });
        }
        // Глотаем всё: назначаемая клавиша не должна сработать по назначению.
        return LRESULT(1);
    }

    let b = *BINDINGS.read().unwrap();
    let is_theirs = hold_matches(b.theirs, vk);
    let is_mine = hold_matches(b.mine, vk);

    // Ctrl+Alt+S начинается с одинокого Ctrl, и в этот момент отличить начало
    // комбинации от намерения говорить нельзя — Alt ещё не нажат. Поэтому
    // решение откладывается: любая ДРУГАЯ клавиша, нажатая во время удержания,
    // задним числом объявляет его частью комбинации и отменяет запись.
    if down && !is_theirs && !is_mine {
        let cancelled = THEIRS_HELD.swap(false, Ordering::SeqCst)
            | MINE_HELD.swap(false, Ordering::SeqCst);
        if cancelled {
            send(Event::HoldCancel);
        }
    }

    // Удерживаемые клавиши пропускаем дальше в активное приложение: левый Ctrl
    // и Shift слишком нужны там, чтобы их проглатывать.
    if is_theirs {
        if down && !alt_held() && !THEIRS_HELD.swap(true, Ordering::SeqCst) {
            send(Event::TheirsDown);
        } else if up && THEIRS_HELD.swap(false, Ordering::SeqCst) {
            send(Event::TheirsUp);
        }
    } else if is_mine {
        if down && !alt_held() && !MINE_HELD.swap(true, Ordering::SeqCst) {
            send(Event::MineDown);
        } else if up && MINE_HELD.swap(false, Ordering::SeqCst) {
            send(Event::MineUp);
        }
    }

    if down {
        for (binding, event, swallow) in [
            (b.screen, Event::Screen, false),
            (b.reset, Event::Reset, true),
            (b.settings, Event::Settings, true),
            (b.listen, Event::Listen, true),
            (b.remote, Event::Remote, true),
            (b.mute, Event::Mute, true),
            (b.move_window, Event::MoveWindow, true),
            (b.input, Event::Input, true),
            (b.screenshot, Event::Screenshot, true),
            (b.session, Event::Session, true),
            (b.rate, Event::Rate, true),
            (b.quit, Event::Quit, true),
        ] {
            if command_matches(binding, vk) {
                send(event);
                // Одна клавиша — одно действие: дальше по списку не идём.
                return if swallow {
                    LRESULT(1)
                } else {
                    CallNextHookEx(None, code, wparam, lparam)
                };
            }
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

fn hold_matches(binding: Option<Binding>, vk: u16) -> bool {
    binding.is_some_and(|b| b.vk == vk)
}

fn command_matches(binding: Option<Binding>, vk: u16) -> bool {
    let Some(b) = binding else { return false };
    if b.vk != vk {
        return false;
    }
    // Привязку на голый модификатор сверяем только по коду: требовать от Ctrl,
    // чтобы был нажат Ctrl, бессмысленно.
    if b.is_bare_modifier() {
        return true;
    }
    b.ctrl == key_down(VK_CONTROL) && b.alt == key_down(VK_MENU) && b.shift == key_down(VK_SHIFT)
}

fn send(e: Event) {
    if let Some(tx) = TX.get() {
        // Только try_send: блокировка здесь стоила бы нам хука.
        let _ = tx.try_send(e);
    }
}

fn key_down(k: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(k.0 as i32) as u32 & 0x8000) != 0 }
}

fn alt_held() -> bool {
    key_down(VK_MENU)
}
