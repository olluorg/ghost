//! Значок в области уведомлений.
//!
//! Оверлея нет ни в панели задач, ни в Alt+Tab — так задумано, он не должен
//! попадаться на глаза. Но тогда у него нет и обычного способа сказать «спрячь»
//! или «закройся», кроме горячих клавиш. Значок в трее это и закрывает.
//!
//! Сообщения от значка приходят в оконную процедуру табло; распоряжения оттуда
//! забирает поток интерфейса — через простые флаги, потому что процедура окна
//! живёт своей жизнью и до состояния приложения не дотягивается.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, IDI_APPLICATION, MF_STRING, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL, WM_RBUTTONUP,
};

/// Своё сообщение значка: `WM_APP` плюс единица.
pub const CALLBACK: u32 = 0x8000 + 1;
/// «Покажись»: его шлёт вторая копия, запущенная по ярлыку, и сама выходит.
/// Именно показать, а не переключить: человек нажал ярлык, потому что окна не
/// видит, и спрятать его в этот момент — ровно наоборот.
pub const SHOW: u32 = 0x8000 + 2;

const CMD_TOGGLE: u32 = 1;
const CMD_QUIT: u32 = 2;

static TOGGLE: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static SHOW_ASKED: AtomicBool = AtomicBool::new(false);

pub struct Tray {
    hwnd: HWND,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Option<Self> {
        unsafe {
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: CALLBACK,
                hIcon: LoadIconW(None, IDI_APPLICATION).ok()?,
                ..Default::default()
            };
            for (i, c) in "ghost".encode_utf16().enumerate() {
                data.szTip[i] = c;
            }
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                return None;
            }
        }
        Some(Self { hwnd })
    }

    /// Забрать распоряжение, если оно есть.
    pub fn take_toggle() -> bool {
        TOGGLE.swap(false, Ordering::Relaxed)
    }

    pub fn take_quit() -> bool {
        QUIT.swap(false, Ordering::Relaxed)
    }

    pub fn take_show() -> bool {
        SHOW_ASKED.swap(false, Ordering::Relaxed)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

/// Разбор сообщения от значка. Возвращает true, если сообщение было наше.
pub unsafe fn handle(hwnd: HWND, msg: u32, _w: WPARAM, l: LPARAM) -> bool {
    if msg == SHOW {
        SHOW_ASKED.store(true, Ordering::Relaxed);
        return true;
    }
    if msg != CALLBACK {
        return false;
    }
    match l.0 as u32 {
        WM_RBUTTONUP => menu(hwnd),
        // Левая кнопка — самое частое действие, поэтому без меню.
        0x0202 => TOGGLE.store(true, Ordering::Relaxed),
        _ => {}
    }
    true
}

unsafe fn menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let show = w("Показать или скрыть");
    let quit = w("Выход");
    let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE as usize, PCWSTR(show.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, PCWSTR(quit.as_ptr()));

    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    // Меню трея закрывается только если окно-хозяин впереди, иначе оно
    // остаётся висеть на экране до следующего клика.
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        p.x,
        p.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match chosen.0 as u32 {
        CMD_TOGGLE => TOGGLE.store(true, Ordering::Relaxed),
        CMD_QUIT => QUIT.store(true, Ordering::Relaxed),
        _ => {}
    }
}

/// Строка в том виде, в каком её ждёт Win32.
fn w(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

