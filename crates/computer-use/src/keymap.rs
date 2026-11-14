//! Cross-platform key-chord parsing and key-name tables.
//!
//! The accepted syntax mirrors ZCode/xdotool: `ctrl+shift+t`, `super+c`,
//! `Control_L+a`, `Return`, `Page_Down`, `F5`. Backends translate the parsed
//! chord into native keycodes/keysyms.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    pub modifiers: Vec<Modifier>,
    pub key: String,
}

impl KeyChord {
    pub fn has(&self, modifier: Modifier) -> bool {
        self.modifiers.contains(&modifier)
    }
}

/// Parse a chord. The final `+`-separated token is the key; all previous
/// tokens must be modifiers.
pub fn parse_chord(input: &str) -> Result<KeyChord, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("key chord is empty".to_string());
    }

    // `plus` is the escape hatch for the literal '+' key.
    let parts: Vec<&str> = if trimmed.eq_ignore_ascii_case("plus") {
        vec!["plus"]
    } else {
        trimmed.split('+').map(str::trim).collect()
    };
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!(
            "invalid key chord `{input}`; use `plus` for the + key"
        ));
    }

    let (key_parts, modifier_parts) = parts.split_last().expect("non-empty");
    let mut modifiers = Vec::new();
    for part in modifier_parts {
        let modifier = parse_modifier(part).ok_or_else(|| {
            format!("`{part}` is not a modifier; expected ctrl, shift, alt or super")
        })?;
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }

    let mut key = normalize_key(key_parts);
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_uppercase() {
            if !modifiers.contains(&Modifier::Shift) {
                modifiers.push(Modifier::Shift);
            }
            key = ch.to_ascii_lowercase().to_string();
        }
    }

    Ok(KeyChord { modifiers, key })
}

/// Parse a `+`-separated modifier list (`"ctrl+shift"`, `""`, `"super"`).
pub fn parse_modifiers(input: &str) -> Result<Vec<Modifier>, String> {
    let mut modifiers = Vec::new();
    for part in input
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let modifier = parse_modifier(part).ok_or_else(|| format!("`{part}` is not a modifier"))?;
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    Ok(modifiers)
}

pub fn parse_modifier(input: &str) -> Option<Modifier> {
    match input.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "control_l" | "control_r" => Some(Modifier::Ctrl),
        "shift" | "shift_l" | "shift_r" => Some(Modifier::Shift),
        "alt" | "option" | "alt_l" | "alt_r" => Some(Modifier::Alt),
        "super" | "meta" | "win" | "cmd" | "command" | "super_l" | "super_r" => {
            Some(Modifier::Super)
        }
        _ => None,
    }
}

pub fn normalize_key(input: &str) -> String {
    let trimmed = input.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "enter" | "return" | "kp_enter" => "Return".to_string(),
        "esc" | "escape" => "Escape".to_string(),
        "backspace" | "back_space" => "BackSpace".to_string(),
        "del" | "delete" => "Delete".to_string(),
        "tab" => "Tab".to_string(),
        "space" | "spacebar" => "Space".to_string(),
        "up" | "arrowup" | "arrow_up" => "Up".to_string(),
        "down" | "arrowdown" | "arrow_down" => "Down".to_string(),
        "left" | "arrowleft" | "arrow_left" => "Left".to_string(),
        "right" | "arrowright" | "arrow_right" => "Right".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "insert" | "ins" => "Insert".to_string(),
        "pageup" | "page_up" | "prior" => "Page_Up".to_string(),
        "pagedown" | "page_down" | "next" => "Page_Down".to_string(),
        "plus" => "+".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "comma" => ",".to_string(),
        "period" | "dot" => ".".to_string(),
        "slash" => "/".to_string(),
        "backslash" => "\\".to_string(),
        "semicolon" => ";".to_string(),
        "apostrophe" | "quote" => "'".to_string(),
        "grave" | "backtick" => "`".to_string(),
        "bracketleft" | "leftbracket" => "[".to_string(),
        "bracketright" | "rightbracket" => "]".to_string(),
        "ctrl" | "control" => "Control_L".to_string(),
        "shift" => "Shift_L".to_string(),
        "alt" | "option" => "Alt_L".to_string(),
        "super" | "meta" | "win" | "cmd" => "Super_L".to_string(),
        lower => {
            if lower.len() >= 2
                && lower.starts_with('f')
                && lower[1..].chars().all(|ch| ch.is_ascii_digit())
            {
                format!("F{}", &lower[1..])
            } else {
                trimmed.to_string()
            }
        }
    }
}

pub fn modifier_keysym(modifier: Modifier) -> u32 {
    match modifier {
        Modifier::Ctrl => 0xffe3,  // Control_L
        Modifier::Shift => 0xffe1, // Shift_L
        Modifier::Alt => 0xffe9,   // Alt_L
        Modifier::Super => 0xffeb, // Super_L
    }
}

/// X11 keysym for a normalized key name. Printable ASCII uses its code point;
/// non-ASCII text is represented as a Unicode keysym.
pub fn keysym_for_key(key: &str) -> Option<u32> {
    match key {
        "Return" => Some(0xff0d),
        "Tab" => Some(0xff09),
        "Escape" => Some(0xff1b),
        "BackSpace" => Some(0xff08),
        "Delete" => Some(0xffff),
        "Insert" => Some(0xff63),
        "Home" => Some(0xff50),
        "End" => Some(0xff57),
        "Page_Up" => Some(0xff55),
        "Page_Down" => Some(0xff56),
        "Up" => Some(0xff52),
        "Down" => Some(0xff54),
        "Left" => Some(0xff51),
        "Right" => Some(0xff53),
        "Space" => Some(0x20),
        "Control_L" => Some(0xffe3),
        "Shift_L" => Some(0xffe1),
        "Alt_L" => Some(0xffe9),
        "Super_L" => Some(0xffeb),
        _ => {
            if let Some(rest) = key.strip_prefix('F')
                && let Ok(number) = rest.parse::<u32>()
                && (1..=24).contains(&number)
            {
                return Some(0xffbe + number - 1);
            }
            let mut chars = key.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(char_keysym(first))
        }
    }
}

pub fn char_keysym(ch: char) -> u32 {
    if ch == '\n' || ch == '\r' {
        0xff0d
    } else if ch == '\t' {
        0xff09
    } else if ch == ' ' || ('!'..='~').contains(&ch) {
        ch as u32
    } else {
        0x0100_0000 | (ch as u32)
    }
}

/// Windows virtual-key code for a normalized key name. Printable punctuation
/// that needs a keyboard-layout lookup is left to `VkKeyScanW` in the backend.
pub fn windows_vk_for_key(key: &str) -> Option<u32> {
    let vk = match key {
        "Return" => 0x0d,
        "Tab" => 0x09,
        "Escape" => 0x1b,
        "BackSpace" => 0x08,
        "Delete" => 0x2e,
        "Insert" => 0x2d,
        "Home" => 0x24,
        "End" => 0x23,
        "Page_Up" => 0x21,
        "Page_Down" => 0x22,
        "Up" => 0x26,
        "Down" => 0x28,
        "Left" => 0x25,
        "Right" => 0x27,
        "Space" => 0x20,
        "Control_L" => 0x11,
        "Shift_L" => 0x10,
        "Alt_L" => 0x12,
        "Super_L" => 0x5b,
        _ => {
            if let Some(rest) = key.strip_prefix('F')
                && let Ok(number) = rest.parse::<u32>()
                && (1..=24).contains(&number)
            {
                return Some(0x70 + number - 1);
            }
            let mut chars = key.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if first.is_ascii_alphanumeric() {
                return Some(first.to_ascii_uppercase() as u32);
            }
            return None;
        }
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_chords() {
        let chord = parse_chord("ctrl+shift+t").unwrap();
        assert_eq!(chord.modifiers, vec![Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(chord.key, "t");

        let chord = parse_chord("Control_L+a").unwrap();
        assert_eq!(chord.modifiers, vec![Modifier::Ctrl]);
        assert_eq!(chord.key, "a");

        let chord = parse_chord("super+c").unwrap();
        assert_eq!(chord.modifiers, vec![Modifier::Super]);

        let chord = parse_chord("Return").unwrap();
        assert!(chord.modifiers.is_empty());
        assert_eq!(chord.key, "Return");
    }

    #[test]
    fn uppercase_letter_implies_shift() {
        let chord = parse_chord("A").unwrap();
        assert_eq!(chord.modifiers, vec![Modifier::Shift]);
        assert_eq!(chord.key, "a");
    }

    #[test]
    fn rejects_unknown_modifiers_and_empty_chords() {
        assert!(parse_chord("hyper+x").is_err());
        assert!(parse_chord("ctrl++").is_err());
        assert!(parse_chord("   ").is_err());
    }

    #[test]
    fn maps_named_and_function_keys() {
        assert_eq!(keysym_for_key("Return"), Some(0xff0d));
        assert_eq!(keysym_for_key("F5"), Some(0xffc2));
        assert_eq!(windows_vk_for_key("F5"), Some(0x74));
        assert_eq!(windows_vk_for_key("Page_Down"), Some(0x22));
        assert_eq!(keysym_for_key("中"), Some(0x0100_4e2d));
    }
}
