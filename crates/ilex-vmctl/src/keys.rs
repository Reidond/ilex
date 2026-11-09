//! Keyboard input: QEMU `QKeyCode` names for characters and key combos
//! (US layout).

use anyhow::{Result, bail};

/// One key to press: its `QKeyCode` and whether Shift has to be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub qcode: &'static str,
    pub shift: bool,
}

const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
/// Shifted digit row: `)!@#$%^&*(` for 0..9.
const DIGIT_SYMBOLS: &str = ")!@#$%^&*(";

/// The key stroke that types `c`.
pub fn stroke_for_char(c: char) -> Result<Stroke> {
    let plain = |qcode| Stroke { qcode, shift: false };
    let shifted = |qcode| Stroke { qcode, shift: true };
    Ok(match c {
        'a'..='z' => plain(LETTERS[(c as u8 - b'a') as usize]),
        'A'..='Z' => shifted(LETTERS[(c as u8 - b'A') as usize]),
        '0'..='9' => plain(DIGITS[(c as u8 - b'0') as usize]),
        ' ' => plain("spc"),
        '\n' | '\r' => plain("ret"),
        '\t' => plain("tab"),
        '-' => plain("minus"),
        '_' => shifted("minus"),
        '=' => plain("equal"),
        '+' => shifted("equal"),
        '[' => plain("bracket_left"),
        '{' => shifted("bracket_left"),
        ']' => plain("bracket_right"),
        '}' => shifted("bracket_right"),
        '\\' => plain("backslash"),
        '|' => shifted("backslash"),
        ';' => plain("semicolon"),
        ':' => shifted("semicolon"),
        '\'' => plain("apostrophe"),
        '"' => shifted("apostrophe"),
        ',' => plain("comma"),
        '<' => shifted("comma"),
        '.' => plain("dot"),
        '>' => shifted("dot"),
        '/' => plain("slash"),
        '?' => shifted("slash"),
        '`' => plain("grave_accent"),
        '~' => shifted("grave_accent"),
        _ => match DIGIT_SYMBOLS.find(c) {
            Some(i) => shifted(DIGITS[i]),
            None => bail!("cannot type {c:?}: no key for it on a US keyboard"),
        },
    })
}

/// Parse `ctrl+alt+t`, `Super`, `Return`, `shift+Tab`, `F11` into `QKeyCode`
/// names: modifiers first (in the order given), the last name is the key.
pub fn parse_combo(combo: &str) -> Result<Vec<&'static str>> {
    let mut keys = Vec::new();
    for part in combo.split('+') {
        let name = part.trim();
        if name.is_empty() {
            bail!("empty key name in `{combo}` (write a literal plus as `plus`)");
        }
        keys.push(key_name(name)?);
    }
    Ok(keys)
}

fn key_name(name: &str) -> Result<&'static str> {
    let lower = name.to_ascii_lowercase();
    let lower = lower.replace('-', "_");
    // Single characters: letters and digits map directly; punctuation goes
    // through the typing table (ignoring Shift).
    if lower.chars().count() == 1 {
        let c = lower.chars().next().unwrap();
        return Ok(stroke_for_char(c)?.qcode);
    }
    Ok(match lower.as_str() {
        "ctrl" | "control" | "ctrl_l" | "control_l" => "ctrl",
        "ctrl_r" | "control_r" => "ctrl_r",
        "alt" | "alt_l" | "option" => "alt",
        "alt_r" | "altgr" => "alt_r",
        "shift" | "shift_l" => "shift",
        "shift_r" => "shift_r",
        "super" | "super_l" | "meta" | "meta_l" | "win" | "windows" | "cmd" | "command" => "meta_l",
        "super_r" | "meta_r" => "meta_r",
        "enter" | "return" | "ret" => "ret",
        "esc" | "escape" => "esc",
        "space" | "spc" => "spc",
        "tab" => "tab",
        "backspace" | "bksp" => "backspace",
        "delete" | "del" => "delete",
        "insert" | "ins" => "insert",
        "home" => "home",
        "end" => "end",
        "pageup" | "page_up" | "pgup" | "prior" => "pgup",
        "pagedown" | "page_down" | "pgdn" | "next" => "pgdn",
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        "menu" => "menu",
        "capslock" | "caps_lock" | "caps" => "caps_lock",
        "print" | "printscreen" | "print_screen" | "sysrq" => "print",
        "plus" => "equal", // Shift+Equal; the caller adds shift if it wants a '+'
        "minus" => "minus",
        "f1" => "f1",
        "f2" => "f2",
        "f3" => "f3",
        "f4" => "f4",
        "f5" => "f5",
        "f6" => "f6",
        "f7" => "f7",
        "f8" => "f8",
        "f9" => "f9",
        "f10" => "f10",
        "f11" => "f11",
        "f12" => "f12",
        _ => bail!("unknown key `{name}` (examples: ctrl+alt+t, Return, Escape, Super, Tab, F11, a)"),
    })
}
