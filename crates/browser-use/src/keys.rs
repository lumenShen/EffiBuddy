//! Key-chord parsing and CDP key-event translation.
//!
//! Syntax matches the desktop `computer` tool so the model does not have to
//! learn a second dialect: `ctrl+shift+t`, `Return`, `Escape`, `Page_Down`,
//! `F5`, `arrowdown`, and `plus` for the literal `+` key.
//!
//! The backend feeds these specs to `Input.dispatchKeyEvent`, which needs four
//! pieces per key: `key`, `code`, `windowsVirtualKeyCode` and (for text keys)
//! `text`.

use crate::backend::{KeyChord, KeyModifier};

/// One CDP key event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// DOM `KeyboardEvent.key` value (`t`, `Enter`, `Escape`, `ArrowDown`).
    pub key: String,
    /// DOM `KeyboardEvent.code` value (`KeyT`, `Enter`, `Escape`, `ArrowDown`).
    pub code: String,
    /// Virtual key code, used by Chromium's input router.
    pub vk: i32,
    /// Characters produced by the key press, if it is a printable key.
    pub text: Option<String>,
    /// `0` = standard, `3` = numpad. Chromium otherwise mis-routes Numpad
    /// keys through the standard layout.
    pub location: i32,
}

impl KeySpec {
    fn new(key: &str, code: &str, vk: i32) -> Self {
        Self {
            key: key.to_string(),
            code: code.to_string(),
            vk,
            text: None,
            location: 0,
        }
    }

    fn with_text(mut self, text: &str) -> Self {
        self.text = Some(text.to_string());
        self
    }

    fn numpad(mut self) -> Self {
        self.location = 3;
        self
    }
}

/// Parse a chord into modifiers + a normalized key name.
///
/// Returns the same errors as the desktop parser so the model sees one
/// consistent message style.
pub fn parse_chord(input: &str) -> Result<KeyChord, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("key chord is empty".to_string());
    }

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
            if !modifiers.contains(&KeyModifier::Shift) {
                modifiers.push(KeyModifier::Shift);
            }
            key = ch.to_ascii_lowercase().to_string();
        }
    }

    Ok(KeyChord { modifiers, key })
}

pub fn parse_modifier(token: &str) -> Option<KeyModifier> {
    match token.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctl" => Some(KeyModifier::Ctrl),
        "shift" => Some(KeyModifier::Shift),
        "alt" | "option" | "opt" | "meta" => Some(KeyModifier::Alt),
        "super" | "cmd" | "command" | "win" => Some(KeyModifier::Super),
        _ => None,
    }
}

/// Lowercase and strip the separators models tend to invent.
pub fn normalize_key(input: &str) -> String {
    let lowered = input.trim().to_ascii_lowercase();
    let condensed: String = lowered
        .chars()
        .filter(|ch| !matches!(ch, '_' | '-' | ' '))
        .collect();
    match condensed.as_str() {
        "ret" | "return" | "enter" => "enter".to_string(),
        "esc" => "escape".to_string(),
        "del" => "delete".to_string(),
        "bs" | "backspace" => "backspace".to_string(),
        "space" | "spacebar" => " ".to_string(),
        "plus" => "+".to_string(),
        "pgup" | "pageup" => "pageup".to_string(),
        "pgdn" | "pagedown" => "pagedown".to_string(),
        "up" => "arrowup".to_string(),
        "down" => "arrowdown".to_string(),
        "left" => "arrowleft".to_string(),
        "right" => "arrowright".to_string(),
        other => other.to_string(),
    }
}

/// Modifier key specs, in the order they must be pressed.
pub fn modifier_specs(modifiers: &[KeyModifier]) -> Vec<KeySpec> {
    let mut specs = Vec::new();
    for modifier in modifiers {
        let spec = match modifier {
            KeyModifier::Ctrl => KeySpec::new("Control", "ControlLeft", 17),
            KeyModifier::Shift => KeySpec::new("Shift", "ShiftLeft", 16),
            KeyModifier::Alt => KeySpec::new("Alt", "AltLeft", 18),
            KeyModifier::Super => KeySpec::new("Meta", "MetaLeft", 91),
        };
        specs.push(spec);
    }
    specs
}

/// Build the event specs for the non-modifier key of a chord.
pub fn key_spec(chord: &KeyChord) -> Result<KeySpec, String> {
    let key = chord.key.as_str();

    // Single characters: printable, produced by `text`.
    if key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphanumeric() {
            let upper = ch.to_ascii_uppercase();
            let mut spec = KeySpec::new(
                &if chord.has(KeyModifier::Shift) {
                    upper.to_string()
                } else {
                    ch.to_string()
                },
                &format!("Key{upper}"),
                upper as i32,
            );
            // Ctrl/Alt/Super combinations are not text input in Chromium; only
            // bare and shift-modified presses produce characters.
            if !chord.has(KeyModifier::Ctrl)
                && !chord.has(KeyModifier::Alt)
                && !chord.has(KeyModifier::Super)
            {
                spec.text = Some(if chord.has(KeyModifier::Shift) {
                    upper.to_string()
                } else {
                    ch.to_string()
                });
            }
            return Ok(spec);
        }
        // Punctuation and unicode: derive the code from the US layout where we
        // can, and always send the character as `text`.
        let code = punctuation_code(ch).unwrap_or_else(|| "Unidentified".to_string());
        let vk = punctuation_vk(ch);
        return Ok(KeySpec::new(&ch.to_string(), &code, vk).with_text(&ch.to_string()));
    }

    let spec = match key {
        "enter" => KeySpec::new("Enter", "Enter", 13),
        "tab" => KeySpec::new("Tab", "Tab", 9),
        "escape" => KeySpec::new("Escape", "Escape", 27),
        "backspace" => KeySpec::new("Backspace", "Backspace", 8),
        "delete" => KeySpec::new("Delete", "Delete", 46),
        "insert" => KeySpec::new("Insert", "Insert", 45),
        "home" => KeySpec::new("Home", "Home", 36),
        "end" => KeySpec::new("End", "End", 35),
        "pageup" => KeySpec::new("PageUp", "PageUp", 33),
        "pagedown" => KeySpec::new("PageDown", "PageDown", 34),
        "arrowup" => KeySpec::new("ArrowUp", "ArrowUp", 38),
        "arrowdown" => KeySpec::new("ArrowDown", "ArrowDown", 40),
        "arrowleft" => KeySpec::new("ArrowLeft", "ArrowLeft", 37),
        "arrowright" => KeySpec::new("ArrowRight", "ArrowRight", 39),
        "space" => KeySpec::new(" ", "Space", 32).with_text(" "),
        "capslock" => KeySpec::new("CapsLock", "CapsLock", 20),
        "contextmenu" => KeySpec::new("ContextMenu", "ContextMenu", 93),
        other if other.starts_with('f') && other.len() <= 3 => {
            let number: i32 = other[1..]
                .parse()
                .map_err(|_| format!("unknown key `{other}`"))?;
            if !(1..=12).contains(&number) {
                return Err(format!("unknown key `{other}`"));
            }
            KeySpec::new(&format!("F{number}"), &format!("F{number}"), 111 + number)
        }
        "numpadadd" => KeySpec::new("+", "NumpadAdd", 107).with_text("+").numpad(),
        "numpadsubtract" => KeySpec::new("-", "NumpadSubtract", 109)
            .with_text("-")
            .numpad(),
        "numpadmultiply" => KeySpec::new("*", "NumpadMultiply", 106)
            .with_text("*")
            .numpad(),
        "numpaddivide" => KeySpec::new("/", "NumpadDivide", 111)
            .with_text("/")
            .numpad(),
        "numpaddecimal" => KeySpec::new(".", "NumpadDecimal", 110)
            .with_text(".")
            .numpad(),
        "numpadenter" => KeySpec::new("Enter", "NumpadEnter", 13).numpad(),
        other => {
            if let Some(digit) = other.strip_prefix("numpad")
                && let Ok(number) = digit.parse::<i32>()
                && (0..=9).contains(&number)
            {
                return Ok(KeySpec::new(
                    &number.to_string(),
                    &format!("Numpad{number}"),
                    96 + number,
                )
                .with_text(&number.to_string())
                .numpad());
            }
            return Err(format!("unknown key `{other}`"));
        }
    };
    Ok(spec)
}

fn punctuation_code(ch: char) -> Option<String> {
    let code = match ch {
        ' ' => "Space",
        '-' => "Minus",
        '=' => "Equal",
        '[' => "BracketLeft",
        ']' => "BracketRight",
        '\\' => "Backslash",
        ';' => "Semicolon",
        '\'' => "Quote",
        ',' => "Comma",
        '.' => "Period",
        '/' => "Slash",
        '`' => "Backquote",
        _ => return None,
    };
    Some(code.to_string())
}

fn punctuation_vk(ch: char) -> i32 {
    // US-layout virtual key codes for the punctuation Chromium expects.
    match ch {
        ' ' => 32,
        '-' => 189,
        '=' => 187,
        '[' => 219,
        ']' => 221,
        '\\' => 220,
        ';' => 186,
        '\'' => 222,
        ',' => 188,
        '.' => 190,
        '/' => 191,
        '`' => 192,
        other => other.to_ascii_uppercase() as i32,
    }
}

/// Combined CDP modifier mask for a chord.
pub fn modifier_mask(chord: &KeyChord) -> i32 {
    chord
        .modifiers
        .iter()
        .fold(0, |mask, modifier| mask | modifier.cdp_mask())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_letters_with_implicit_shift() {
        let chord = parse_chord("ctrl+shift+t").unwrap();
        assert_eq!(chord.key, "t");
        assert!(chord.has(KeyModifier::Ctrl));
        assert!(chord.has(KeyModifier::Shift));

        // `A` and `a` are the same chord: the desktop parser lowercases too, so
        // the model can write either without changing what is typed.
        let upper = parse_chord("A").unwrap();
        assert_eq!(upper.key, "a");
        assert!(!upper.has(KeyModifier::Shift));
    }

    #[test]
    fn accepts_aliases_the_model_likely_uses() {
        for (input, expected) in [
            ("Return", "enter"),
            ("Escape", "escape"),
            ("Page_Down", "pagedown"),
            ("down", "arrowdown"),
            ("Del", "delete"),
            ("plus", "+"),
        ] {
            assert_eq!(parse_chord(input).unwrap().key, expected, "{input}");
        }
    }

    #[test]
    fn rejects_unknown_keys_and_modifiers() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("hyper+a").is_err());
        // Parsing validates structure only, like the desktop parser: unknown key
        // names are rejected when a CDP spec is built.
        assert_eq!(parse_chord("nonsense").unwrap().key, "nonsense");
        assert!(key_spec(&parse_chord("nonsense").unwrap()).is_err());
        assert_eq!(parse_chord("f13").unwrap().key, "f13");
        assert!(key_spec(&parse_chord("f13").unwrap()).is_err());
        assert!(key_spec(&parse_chord("f12").unwrap()).is_ok());
    }

    #[test]
    fn builds_printable_specs_with_text() {
        let spec = key_spec(&parse_chord("a").unwrap()).unwrap();
        assert_eq!(spec.code, "KeyA");
        assert_eq!(spec.vk, 'A' as i32);
        assert_eq!(spec.text.as_deref(), Some("a"));

        let shifted = key_spec(&parse_chord("shift+a").unwrap()).unwrap();
        assert_eq!(shifted.key, "A");
        assert_eq!(shifted.text.as_deref(), Some("A"));

        // Ctrl combinations must not emit text or Chromium inserts characters.
        let ctrl = key_spec(&parse_chord("ctrl+a").unwrap()).unwrap();
        assert_eq!(ctrl.text, None);
    }

    #[test]
    fn builds_function_and_navigation_specs() {
        assert_eq!(key_spec(&parse_chord("F5").unwrap()).unwrap().vk, 116);
        let down = key_spec(&parse_chord("ArrowDown").unwrap()).unwrap();
        assert_eq!(down.code, "ArrowDown");
        assert_eq!(down.vk, 40);
        let enter = key_spec(&parse_chord("enter").unwrap()).unwrap();
        assert_eq!((enter.code.as_str(), enter.text.clone()), ("Enter", None));
    }

    #[test]
    fn punctuation_uses_us_layout_codes() {
        let slash = key_spec(&parse_chord("/").unwrap()).unwrap();
        assert_eq!(slash.code, "Slash");
        assert_eq!(slash.text.as_deref(), Some("/"));
        let plus = key_spec(&parse_chord("plus").unwrap()).unwrap();
        assert_eq!(plus.text.as_deref(), Some("+"));
    }

    #[test]
    fn modifier_mask_matches_cdp_bits() {
        let chord = parse_chord("ctrl+shift+alt+super+a").unwrap();
        assert_eq!(modifier_mask(&chord), 2 | 8 | 1 | 4);
    }
}
