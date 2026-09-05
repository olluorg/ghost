//! Всё, что делает окно невидимым для захвата экрана и безобидным для фокуса.
//!
//! Три вещи, каждая отвечает за своё:
//!   * `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` — окна нет в кадре,
//!     который DWM отдаёт потребителям захвата (Win10 2004 / build 19041+);
//!   * `WS_EX_NOACTIVATE` — окно никогда не забирает фокус, поэтому приложение
//!     конференции не видит переключения окна;
//!   * `WS_EX_TOOLWINDOW` — нет в Alt-Tab, в панели задач и в списке
//!     «поделиться окном».

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::OnceLock;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowLongPtrW,
    GetWindowThreadProcessId, SetForegroundWindow, SetWindowDisplayAffinity, SetWindowLongPtrW,
    GetCursorPos, SetWindowPos, GWLP_WNDPROC, GWL_EXSTYLE, GWL_STYLE, HTCAPTION, HTCLIENT, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WM_NCHITTEST,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    /// Окна нет в захвате вообще.
    Excluded,
    /// ОС старше Win10 2004: в захвате чёрный прямоугольник вместо окна.
    MonitorOnly,
    /// Ни один режим не применился.
    Failed,
    /// HWND не удалось получить.
    NoWindow,
}

/// HWND нужен и после создания окна (watchdog), поэтому запоминаем его.
/// Храним как isize: HWND — сырой указатель и не Send/Sync.
static HWND_RAW: OnceLock<isize> = OnceLock::new();

fn hwnd() -> Option<HWND> {
    HWND_RAW.get().map(|raw| HWND(*raw as *mut c_void))
}

/// Вызывается один раз при создании окна.
pub fn harden(cc: &eframe::CreationContext<'_>) -> Affinity {
    let Ok(handle) = cc.window_handle() else {
        return Affinity::NoWindow;
    };
    let RawWindowHandle::Win32(w) = handle.as_raw() else {
        return Affinity::NoWindow;
    };

    let raw = w.hwnd.get();
    let _ = HWND_RAW.set(raw);
    let h = HWND(raw as *mut c_void);

    unsafe {
        // WS_EX_NOACTIVATE не ставим: с ним окно не может получить клавиатуру,
        // а поле ввода без неё бесполезно. Случайной активации не будет — всё,
        // кроме строки ввода и уголка, для мыши сквозное.
        // WS_EX_LAYERED обязателен, и не ради вида. Сквозной хит-тест работает
        // только у слоёного окна: проверка на стенде (dev/probe) показала, что
        // без него бит WS_EX_TRANSPARENT при попадании просто игнорируется, а
        // ответ HTTRANSPARENT на WM_NCHITTEST гасит клик — он не достаётся ни
        // нам, ни окну под нами. С WS_EX_LAYERED клик доходит до чужого
        // процесса, как и требуется.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE)
            | (WS_EX_TOOLWINDOW.0 as isize)
            | (WS_EX_LAYERED.0 as isize);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex);

        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    install_hit_test(h);
    strip_frame(h);
    apply(h)
}

/// Убирает всё, что рисует вокруг окна система.
///
/// winit держит безрамочное окно как обычное — со стилями `WS_CAPTION` и
/// `WS_SYSMENU` (иначе не работает Aero Snap), а неклиентскую область срезает
/// в `WM_NCCALCSIZE`. Побочно окно получает от DWM тень и скруглённые углы
/// Windows 11. На прозрачном окне поверх светлой страницы это и видно как
/// белёсую подложку по краям и в углах: под нашим скруглением остаётся чужое.
fn strip_frame(h: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    unsafe {
        let policy = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            h,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const _ as *const _,
            std::mem::size_of_val(&policy) as u32,
        );
        // Углы скругляет система: окно управления непрозрачное, и нарисовать
        // ему скругление своими силами нельзя — по углам осталась бы заливка.
        // На Windows 10 вызов просто вернёт ошибку, и углы будут прямыми.
        let corners = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            h,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corners as *const _ as *const _,
            std::mem::size_of_val(&corners) as u32,
        );
    }
}

fn apply(h: HWND) -> Affinity {
    // Отладочная лазейка. Окно исключено из захвата, поэтому увидеть, как оно
    // выглядит поверх экрана, нельзя ничем: ни скриншотом, ни записью. Свой
    // кадр программа снять умеет, но в нём нет композиции — а прозрачность
    // живёт именно там. С этим флагом окно попадает в захват, и композицию
    // видно обычным скриншотом.
    if std::env::var_os("GHOST_SHOW_IN_CAPTURE").is_some() {
        log::warn!("GHOST_SHOW_IN_CAPTURE: окно НЕ исключено из захвата");
        return Affinity::Failed;
    }
    unsafe {
        if SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE).is_ok() {
            return Affinity::Excluded;
        }
        // Не Win10 2004+: доступен только чёрный прямоугольник.
        if SetWindowDisplayAffinity(h, WDA_MONITOR).is_ok() {
            return Affinity::MonitorOnly;
        }
        Affinity::Failed
    }
}

/// Повторное применение после того, как watchdog заметил потерю.
pub fn reapply() -> Affinity {
    match hwnd() {
        Some(h) => apply(h),
        None => Affinity::NoWindow,
    }
}

/// Что окно представляет собой прямо сейчас, глазами Windows.
///
/// Диагностика на случай «у меня всё не так»: winit пересчитывает стили при
/// любом изменении свойств окна и затирает наши правки, а по внешнему виду это
/// не отличить. Строка уходит в журнал при запуске и при каждом расхождении.
pub fn describe() -> String {
    let Some(h) = hwnd() else { return "окна нет".into() };
    unsafe {
        let style = GetWindowLongPtrW(h, GWL_STYLE);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let mut aff: u32 = 0;
        let _ = GetWindowDisplayAffinity(h, &mut aff);
        format!(
            "style=0x{style:08X} ex=0x{ex:08X} affinity={aff} \
             tool={} сквозное={}",
            ex & WS_EX_TOOLWINDOW.0 as isize != 0,
            ex & 0x20 != 0,
        )
    }
}

/// Возвращает `WS_EX_TOOLWINDOW`, если winit его затёр.
///
/// Без него окно появляется в Alt+Tab и на панели задач — то есть перестаёт
/// быть незаметным. winit сбрасывает ex-стиль целиком всякий раз, когда меняет
/// любое свойство окна, поэтому одной установки при запуске не хватает.
pub fn keep_tool_window() -> bool {
    let Some(h) = hwnd() else { return false };
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        if ex & WS_EX_TOOLWINDOW.0 as isize != 0 {
            return false;
        }
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);
        true
    }
}

/// True, если фактический режим окна разошёлся с ожидаемым.
pub fn affinity_lost(expected: Affinity) -> bool {
    let want = match expected {
        Affinity::Excluded => WDA_EXCLUDEFROMCAPTURE,
        Affinity::MonitorOnly => WDA_MONITOR,
        // Нечего терять — режим и так не применён.
        Affinity::Failed | Affinity::NoWindow => return false,
    };
    let Some(h) = hwnd() else { return false };

    unsafe {
        let mut cur: u32 = 0;
        match GetWindowDisplayAffinity(h, &mut cur) {
            Ok(()) => cur != want.0,
            // Не смогли прочитать — считаем, что всё на месте, чтобы не
            // перезаписывать affinity каждые три секунды впустую.
            Err(_) => false,
        }
    }
}

/// Пускает мышь и фокус в окно на время настройки — и забирает обратно.
///
/// Обычный режим: окно click-through и с `WS_EX_NOACTIVATE`, поэтому до него
/// не доходят ни клики, ни клавиатура. Чтобы что-то настроить, это надо снять.
/// Исключение из захвата при этом не трогается: невидимость не должна зависеть
/// от режима.
pub fn set_interactive(on: bool) {
    INTERACTIVE.store(on, Ordering::Relaxed);
    if on {
        focus();
    }
}

/// Забирает фокус клавиатуры.
///
/// Просто `SetForegroundWindow` из фонового процесса Windows игнорирует —
/// окно получало мышь, но не клавиатуру, и поле ввода не набиралось. Обходится
/// временной привязкой к потоку ввода активного окна.
pub fn focus() {
    let Some(h) = hwnd() else { return };

    unsafe {
        let foreground = GetForegroundWindow();
        let other = GetWindowThreadProcessId(foreground, None);
        let ours = GetCurrentThreadId();

        let attached = other != 0 && other != ours && AttachThreadInput(other, ours, true).as_bool();
        let _ = SetForegroundWindow(h);
        let _ = SetFocus(Some(h));
        if attached {
            let _ = AttachThreadInput(other, ours, false);
        }
    }
}


/// Области, принимающие клики: кнопки и строка ввода. Всё остальное окно
/// остаётся сквозным, иначе оверлей перехватывал бы работу под собой.
const HOT_MAX: usize = 12;
static HOT: [[AtomicI32; 4]; HOT_MAX] = [const {
    [
        AtomicI32::new(0),
        AtomicI32::new(0),
        AtomicI32::new(0),
        AtomicI32::new(0),
    ]
}; HOT_MAX];
static HOT_COUNT: AtomicI32 = AtomicI32::new(0);

/// Прямоугольник уголка для перетаскивания, в физических пикселях клиента.
static GRIP: [AtomicI32; 4] = [
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
    AtomicI32::new(0),
];
/// Окно сейчас полностью принимает мышь (настройки или режим перемещения).
static INTERACTIVE: AtomicBool = AtomicBool::new(false);
/// Оконная процедура winit: мы встраиваемся перед ней, а не подменяем её.
static PREV_PROC: AtomicIsize = AtomicIsize::new(0);

/// Область, за которую окно таскается в любом режиме.
pub fn set_grip(left: i32, top: i32, right: i32, bottom: i32) {
    store(&GRIP, left, top, right, bottom);
}

/// Заменяет список кликабельных областей целиком. Вызывается каждый кадр:
/// кнопки переезжают вместе с раскладкой.
pub fn set_hot_rects(rects: &[(i32, i32, i32, i32)]) {
    if rects.len() > HOT_MAX {
        // Лишние области просто не попадут в список, и соответствующие кнопки
        // молча перестанут нажиматься. Молчать об этом нельзя.
        log::warn!("областей {} — больше предела {HOT_MAX}, лишние не работают", rects.len());
    }
    let n = rects.len().min(HOT_MAX);
    for (slot, r) in HOT.iter().zip(rects.iter()) {
        store(slot, r.0, r.1, r.2, r.3);
    }
    HOT_COUNT.store(n as i32, Ordering::Relaxed);
}

/// Пропускать ли мышь сквозь окно.
///
/// Единственный механизм, который действительно отдаёт клик чужому процессу.
/// Ответ `HTTRANSPARENT` на `WM_NCHITTEST`, которым это делалось раньше, лишь
/// заставляет систему не отдавать клик нам — до окна под нами он не доходит
/// вовсе: проверка живым кликом показывает, что после него активного окна не
/// остаётся ни одного. Здесь же меняется один бит стиля, без пересчёта рамки и
/// без `WS_EX_LAYERED`, от которого зависит альфа окна.
/// Исключает из записи экрана произвольное окно — например табло.
pub fn exclude_from_capture(h: HWND) {
    unsafe {
        if SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE).is_err() {
            log::warn!("окно не удалось исключить из захвата");
        }
    }
}

/// Возвращает true, если состояние пришлось менять.
pub fn set_click_through(on: bool) -> bool {
    let Some(h) = hwnd() else { return false };
    unsafe {
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        // Слоёный стиль тоже возвращаем: без него бит сквозного режима
        // системой игнорируется, а winit его периодически сбрасывает.
        let mut want = ex | WS_EX_LAYERED.0 as isize;
        want = if on {
            want | WS_EX_TRANSPARENT.0 as isize
        } else {
            want & !(WS_EX_TRANSPARENT.0 as isize)
        };
        if want == ex {
            return false;
        }
        SetWindowLongPtrW(h, GWL_EXSTYLE, want);

        true
    }
}

/// Курсор в координатах клиента, в физических пикселях.
pub fn cursor_in_client() -> Option<(i32, i32)> {
    let h = hwnd()?;
    unsafe {
        let mut p = POINT::default();
        GetCursorPos(&mut p).ok()?;
        // ScreenToClient возвращает BOOL, а не Result с полезной ошибкой.
        if !ScreenToClient(h, &mut p).as_bool() {
            return None;
        }
        Some((p.x, p.y))
    }
}

/// Прямоугольник уголка перетаскивания — он тоже должен ловить мышь.
pub fn grip_contains(x: i32, y: i32) -> bool {
    inside(&GRIP, x, y)
}


fn store(slots: &[AtomicI32; 4], left: i32, top: i32, right: i32, bottom: i32) {
    for (slot, value) in slots.iter().zip([left, top, right, bottom]) {
        slot.store(value, Ordering::Relaxed);
    }
}

fn inside(slots: &[AtomicI32; 4], x: i32, y: i32) -> bool {
    let (l, t, r, b) = (
        slots[0].load(Ordering::Relaxed),
        slots[1].load(Ordering::Relaxed),
        slots[2].load(Ordering::Relaxed),
        slots[3].load(Ordering::Relaxed),
    );
    x >= l && x < r && y >= t && y < b
}

/// Заменяет сквозной стиль на разбор попаданий.
///
/// `WS_EX_TRANSPARENT` делает сквозным окно целиком, и никакая его часть не
/// может ловить мышь. Поэтому стиль снимается, а решение принимается на каждый
/// клик: внутри уголка отвечаем «это заголовок» — и Windows сама тащит окно,
/// снаружи «здесь пусто» — и клик уходит в приложение под нами.
fn install_hit_test(h: HWND) {
    unsafe {
        let prev = SetWindowLongPtrW(h, GWLP_WNDPROC, hit_test_proc as *const () as isize);
        PREV_PROC.store(prev, Ordering::SeqCst);
    }
}

unsafe extern "system" fn hit_test_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let prev = PREV_PROC.load(Ordering::SeqCst);
    let chain = std::mem::transmute::<
        isize,
        Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
    >(prev);

    if msg == WM_NCHITTEST && !INTERACTIVE.load(Ordering::Relaxed) {
        // Координаты приходят экранные, а области мы знаем в клиентских.
        let mut point = POINT {
            x: (lparam.0 & 0xFFFF) as i16 as i32,
            y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
        };
        let _ = ScreenToClient(hwnd, &mut point);

        // Сквозной режим здесь больше не решается: им заведует бит
        // WS_EX_TRANSPARENT, и до этой процедуры в сквозном состоянии дело
        // просто не доходит. Осталась одна задача — отдать уголок системе как
        // заголовок, чтобы окно таскала она сама.
        let code: isize = if inside(&GRIP, point.x, point.y) {
            HTCAPTION as isize
        } else {
            HTCLIENT as isize
        };
        return LRESULT(code);
    }

    CallWindowProcW(chain, hwnd, msg, wparam, lparam)
}


/// Местное время «часы:минуты» для подписи сообщения.
pub fn now_hhmm() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}
