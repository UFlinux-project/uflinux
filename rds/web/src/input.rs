//! Translation from DOM `KeyboardEvent.code` values to the PS/2 set-1 scancode
//! encoding that IronRDP's `DeviceEvent::keyPressed/keyReleased` expect.
//!
//! IronRDP packs a scancode into a `u16` where the low byte is the make code
//! and the extended flag lives in `0xE000` (see `ironrdp_input::Scancode::from_u16`:
//! `extended = scancode & 0xE000 == 0xE000`). Extended keys therefore use the
//! `0xE0XX` form below. Keeping this table in Rust means the JS layer never
//! hand-rolls scancodes.

/// Make code for a `KeyboardEvent.code`, or `None` for an unmapped key.
///
/// Returned value already includes the extended flag where applicable.
#[must_use]
pub fn scancode_for_code(code: &str) -> Option<u16> {
    // Non-extended keys first (most common), then the extended block.
    let basic: u16 = match code {
        "Escape" => 0x01,
        "Digit1" => 0x02,
        "Digit2" => 0x03,
        "Digit3" => 0x04,
        "Digit4" => 0x05,
        "Digit5" => 0x06,
        "Digit6" => 0x07,
        "Digit7" => 0x08,
        "Digit8" => 0x09,
        "Digit9" => 0x0A,
        "Digit0" => 0x0B,
        "Minus" => 0x0C,
        "Equal" => 0x0D,
        "Backspace" => 0x0E,
        "Tab" => 0x0F,
        "KeyQ" => 0x10,
        "KeyW" => 0x11,
        "KeyE" => 0x12,
        "KeyR" => 0x13,
        "KeyT" => 0x14,
        "KeyY" => 0x15,
        "KeyU" => 0x16,
        "KeyI" => 0x17,
        "KeyO" => 0x18,
        "KeyP" => 0x19,
        "BracketLeft" => 0x1A,
        "BracketRight" => 0x1B,
        "Enter" => 0x1C,
        "ControlLeft" => 0x1D,
        "KeyA" => 0x1E,
        "KeyS" => 0x1F,
        "KeyD" => 0x20,
        "KeyF" => 0x21,
        "KeyG" => 0x22,
        "KeyH" => 0x23,
        "KeyJ" => 0x24,
        "KeyK" => 0x25,
        "KeyL" => 0x26,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "ShiftLeft" => 0x2A,
        "Backslash" => 0x2B,
        "KeyZ" => 0x2C,
        "KeyX" => 0x2D,
        "KeyC" => 0x2E,
        "KeyV" => 0x2F,
        "KeyB" => 0x30,
        "KeyN" => 0x31,
        "KeyM" => 0x32,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "ShiftRight" => 0x36,
        "NumpadMultiply" => 0x37,
        "AltLeft" => 0x38,
        "Space" => 0x39,
        "CapsLock" => 0x3A,
        "F1" => 0x3B,
        "F2" => 0x3C,
        "F3" => 0x3D,
        "F4" => 0x3E,
        "F5" => 0x3F,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "NumLock" => 0x45,
        "ScrollLock" => 0x46,
        "Numpad7" => 0x47,
        "Numpad8" => 0x48,
        "Numpad9" => 0x49,
        "NumpadSubtract" => 0x4A,
        "Numpad4" => 0x4B,
        "Numpad5" => 0x4C,
        "Numpad6" => 0x4D,
        "NumpadAdd" => 0x4E,
        "Numpad1" => 0x4F,
        "Numpad2" => 0x50,
        "Numpad3" => 0x51,
        "Numpad0" => 0x52,
        "NumpadDecimal" => 0x53,
        "IntlBackslash" => 0x56,
        "F11" => 0x57,
        "F12" => 0x58,
        _ => {
            return extended_scancode(code);
        }
    };
    Some(basic)
}

/// Extended keys: `0xE000 | make_code`.
fn extended_scancode(code: &str) -> Option<u16> {
    const EXT: u16 = 0xE000;
    let code: u16 = match code {
        "ControlRight" => 0x1D,
        "NumpadDivide" => 0x35,
        "AltRight" => 0x38,
        "Home" => 0x47,
        "ArrowUp" => 0x48,
        "PageUp" => 0x49,
        "ArrowLeft" => 0x4B,
        "ArrowRight" => 0x4D,
        "End" => 0x4F,
        "ArrowDown" => 0x50,
        "PageDown" => 0x51,
        "Insert" => 0x52,
        "Delete" => 0x53,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "NumpadEnter" => 0x1C,
        _ => return None,
    };
    Some(EXT | code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_key_has_no_extended_flag() {
        assert_eq!(scancode_for_code("KeyA"), Some(0x1E));
        assert_eq!(scancode_for_code("Enter"), Some(0x1C));
        assert_eq!(scancode_for_code("Space"), Some(0x39));
    }

    #[test]
    fn extended_keys_carry_the_e000_flag() {
        assert_eq!(scancode_for_code("ArrowUp"), Some(0xE048));
        assert_eq!(scancode_for_code("Delete"), Some(0xE053));
        assert_eq!(scancode_for_code("ControlRight"), Some(0xE01D));
        assert_eq!(scancode_for_code("NumpadEnter"), Some(0xE01C));
    }

    #[test]
    fn extended_flag_matches_ironrdp_convention() {
        // Mirrors ironrdp_input::Scancode::from_u16.
        for code in ["ArrowUp", "Delete", "MetaLeft", "AltRight"] {
            let sc = scancode_for_code(code).unwrap();
            assert_eq!(sc & 0xE000, 0xE000, "{code} must be extended");
        }
    }

    #[test]
    fn unknown_code_is_none() {
        assert_eq!(scancode_for_code("NonexistentKey"), None);
    }
}
