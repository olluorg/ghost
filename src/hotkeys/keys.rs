//! Разбор человекочитаемых имён клавиш в виртуальные коды Windows и обратно.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub vk: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Binding {
    /// Клавиша-модификатор сама себе модификатор: требовать от Ctrl, чтобы был
    /// нажат Ctrl, бессмысленно. Для таких привязок сверяем только код.
    pub fn is_bare_modifier(&self) -> bool {
        is_modifier(self.vk)
    }
}

pub fn is_modifier(vk: u16) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5)
}

/// Таблица имён. Порядок важен только для обратного поиска: первое совпадение
/// по коду и станет отображаемым именем.
const NAMED: &[(&str, u16)] = &[
    ("LControl", 0xA2),
    ("RControl", 0xA3),
    ("LShift", 0xA0),
    ("RShift", 0xA1),
    ("LAlt", 0xA4),
    ("RAlt", 0xA5),
    ("Space", 0x20),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Escape", 0x1B),
    ("Backspace", 0x08),
    ("CapsLock", 0x14),
    ("ScrollLock", 0x91),
    ("Pause", 0x13),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
];

pub fn parse(spec: &str) -> Option<Binding> {
    let mut binding = Binding { vk: 0, ctrl: false, alt: false, shift: false };

    for part in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => binding.ctrl = true,
            "alt" => binding.alt = true,
            "shift" => binding.shift = true,
            _ => binding.vk = vk_of(part)?,
        }
    }

    (binding.vk != 0).then_some(binding)
}

fn vk_of(name: &str) -> Option<u16> {
    if let Some((_, vk)) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(*vk);
    }
    // F1..F24
    if let Some(n) = name.strip_prefix(['F', 'f']).and_then(|d| d.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x6F + n);
        }
    }
    // Одиночные буквы и цифры совпадают с ASCII-кодом заглавной.
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as u16),
        _ => None,
    }
}

/// Имя клавиши для показа в настройках.
pub fn name_of(vk: u16) -> String {
    if let Some((n, _)) = NAMED.iter().find(|(_, code)| *code == vk) {
        return (*n).to_string();
    }
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => ((vk as u8) as char).to_string(),
        other => format!("VK_{other:#04X}"),
    }
}

/// Полное имя привязки, включая модификаторы.
pub fn spec_of(binding: &Binding) -> String {
    let mut out = String::new();
    if binding.ctrl {
        out.push_str("Ctrl+");
    }
    if binding.alt {
        out.push_str("Alt+");
    }
    if binding.shift {
        out.push_str("Shift+");
    }
    out.push_str(&name_of(binding.vk));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn одиночные_клавиши() {
        assert_eq!(parse("LControl").unwrap().vk, 0xA2);
        assert_eq!(parse("RShift").unwrap().vk, 0xA1);
        assert_eq!(parse("F13").unwrap().vk, 0x7C);
        assert_eq!(parse("Q").unwrap().vk, b'Q' as u16);
        assert_eq!(parse("q").unwrap().vk, b'Q' as u16, "регистр не должен влиять");
        assert_eq!(parse("Enter").unwrap().vk, 0x0D);
    }

    #[test]
    fn комбинации_разбираются_с_модификаторами() {
        let b = parse("Ctrl+Alt+R").unwrap();
        assert_eq!(b.vk, b'R' as u16);
        assert!(b.ctrl && b.alt && !b.shift);
    }

    #[test]
    fn мусор_не_разбирается() {
        assert!(parse("").is_none());
        assert!(parse("Ctrl+Alt").is_none(), "одни модификаторы — не привязка");
        assert!(parse("Ctrl+Пробел").is_none());
        assert!(parse("F99").is_none());
    }

    #[test]
    fn модификатор_сам_по_себе_узнаётся() {
        // На таких привязках держится push-to-talk: сверять состояние
        // модификаторов для них бессмысленно.
        assert!(parse("LControl").unwrap().is_bare_modifier());
        assert!(!parse("Ctrl+Alt+R").unwrap().is_bare_modifier());
    }

    #[test]
    fn имя_восстанавливается_обратно() {
        for spec in ["LControl", "RShift", "F13", "Ctrl+Alt+R", "Ctrl+Alt+Enter", "Q"] {
            let binding = parse(spec).unwrap();
            assert_eq!(spec_of(&binding), spec, "разбор и сборка должны сходиться");
        }
    }
}
