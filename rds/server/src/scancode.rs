//! PC set-1 scancodes (what IronRDP gives us) to X11 keysyms (what the
//! compositor's virtual keyboard wants).
//!
//! IronRDP hands the input handler a raw `code: u8` plus an `extended` flag
//! rather than a keysym, and the nested compositor is driven by
//! `zwp_virtual_keyboard_v1`, which takes Linux `KEY_*` codes. The wayland
//! crate already owns the keysym→`KEY_*` half of that mapping
//! ([`uflinux_rds_wayland::keysym_to_keycode`]), so this module only has to
//! cover scancode→keysym.
//!
//! Going through keysyms rather than building a scancode→`KEY_*` table is what
//! makes the `extended` flag usable: `0x47` is numpad-7 unextended and `Home`
//! extended, and the two have different `KEY_*` codes (`KEY_KP7` and
//! `KEY_HOME`), so the disambiguation has to happen before the keysym lookup.
//!
//! Printable keys map to the lowercase ASCII keysym: shift is a separate
//! modifier and the compositor's keymap applies it, which is exactly what
//! [`uflinux_rds_wayland::keysym_to_keycode`] expects. Keysyms outside
//! Latin-1 and the named keys below are not produced — a client sending a
//! Compose key, an F13 key or a Unicode keysym gets the event dropped instead
//! of a wrong key, because the compositor's own table stops at the same place.

use uflinux_rds_wayland::keysym;

/// The X11 keysym for one PC set-1 scancode, or `None` if we cannot name it.
///
/// `extended` is the `KBDEXT` flag from the RDP keyboard event: it selects the
/// right-hand modifiers, the navigation cluster and the numpad Enter/`/`.
pub fn keysym_for(code: u8, extended: bool) -> Option<u32> {
    if extended {
        return Some(match code {
            0x1C => keysym::KP_ENTER,
            0x1D => keysym::CONTROL_RIGHT,
            0x35 => keysym::KP_DIVIDE,
            0x37 => keysym::PRINT,
            0x38 => keysym::ALT_RIGHT,
            0x47 => keysym::HOME,
            0x48 => keysym::UP,
            0x49 => keysym::PAGE_UP,
            0x4B => keysym::LEFT,
            0x4D => keysym::RIGHT,
            0x4F => keysym::END,
            0x50 => keysym::DOWN,
            0x51 => keysym::PAGE_DOWN,
            0x52 => keysym::INSERT,
            0x53 => keysym::DELETE,
            0x5B => keysym::SUPER_L,
            0x5C => keysym::SUPER_R,
            0x5D => keysym::MENU,
            _ => return None,
        });
    }
    Some(match code {
        0x01 => keysym::ESCAPE,
        0x02 => ascii(b'1'),
        0x03 => ascii(b'2'),
        0x04 => ascii(b'3'),
        0x05 => ascii(b'4'),
        0x06 => ascii(b'5'),
        0x07 => ascii(b'6'),
        0x08 => ascii(b'7'),
        0x09 => ascii(b'8'),
        0x0A => ascii(b'9'),
        0x0B => ascii(b'0'),
        0x0C => ascii(b'-'),
        0x0D => ascii(b'='),
        0x0E => keysym::BACKSPACE,
        0x0F => keysym::TAB,
        // Letters, lowercase: shift is a separate modifier and the compositor's
        // keymap applies it.
        0x10 => ascii(b'q'),
        0x11 => ascii(b'w'),
        0x12 => ascii(b'e'),
        0x13 => ascii(b'r'),
        0x14 => ascii(b't'),
        0x15 => ascii(b'y'),
        0x16 => ascii(b'u'),
        0x17 => ascii(b'i'),
        0x18 => ascii(b'o'),
        0x19 => ascii(b'p'),
        0x1A => ascii(b'['),
        0x1B => ascii(b']'),
        0x1C => keysym::ENTER,
        0x1D => keysym::CONTROL_LEFT,
        0x1E => ascii(b'a'),
        0x1F => ascii(b's'),
        0x20 => ascii(b'd'),
        0x21 => ascii(b'f'),
        0x22 => ascii(b'g'),
        0x23 => ascii(b'h'),
        0x24 => ascii(b'j'),
        0x25 => ascii(b'k'),
        0x26 => ascii(b'l'),
        0x27 => ascii(b';'),
        0x28 => ascii(b'\''),
        0x29 => ascii(b'`'),
        0x2A => keysym::SHIFT_LEFT,
        0x2B => ascii(b'\\'),
        0x2C => ascii(b'z'),
        0x2D => ascii(b'x'),
        0x2E => ascii(b'c'),
        0x2F => ascii(b'v'),
        0x30 => ascii(b'b'),
        0x31 => ascii(b'n'),
        0x32 => ascii(b'm'),
        0x33 => ascii(b','),
        0x34 => ascii(b'.'),
        0x35 => ascii(b'/'),
        0x36 => keysym::SHIFT_RIGHT,
        // Numpad (NumLock-on forms; the navigation forms arrive extended).
        0x37 => keysym::KP_MULTIPLY,
        0x38 => keysym::ALT_LEFT,
        0x39 => keysym::SPACE,
        0x3A => keysym::CAPS_LOCK,
        0x3B => keysym::F1,
        0x3C => keysym::F2,
        0x3D => keysym::F3,
        0x3E => keysym::F4,
        0x3F => keysym::F5,
        0x40 => keysym::F6,
        0x41 => keysym::F7,
        0x42 => keysym::F8,
        0x43 => keysym::F9,
        0x44 => keysym::F10,
        0x45 => keysym::NUM_LOCK,
        0x46 => keysym::SCROLL_LOCK,
        0x47 => keysym::KP_7,
        0x48 => keysym::KP_8,
        0x49 => keysym::KP_9,
        0x4A => keysym::KP_SUBTRACT,
        0x4B => keysym::KP_4,
        0x4C => keysym::KP_5,
        0x4D => keysym::KP_6,
        0x4E => keysym::KP_ADD,
        0x4F => keysym::KP_1,
        0x50 => keysym::KP_2,
        0x51 => keysym::KP_3,
        0x52 => keysym::KP_0,
        0x53 => keysym::KP_DECIMAL,
        0x57 => keysym::F11,
        0x58 => keysym::F12,
        _ => return None,
    })
}

/// A printable ASCII keysym is its own code point.
const fn ascii(byte: u8) -> u32 {
    byte as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use uflinux_rds_wayland::keysym_to_keycode;

    #[test]
    fn a_plain_letter_is_lowercase_ascii() {
        assert_eq!(keysym_for(0x1E, false), Some(u32::from(b'a')));
        assert_eq!(keysym_for(0x11, false), Some(u32::from(b'w')));
    }

    #[test]
    fn the_same_scancode_is_numpad_or_navigation() {
        assert_eq!(keysym_for(0x47, false), Some(keysym::KP_7));
        assert_eq!(keysym_for(0x47, true), Some(keysym::HOME));
        assert_eq!(keysym_for(0x53, false), Some(keysym::KP_DECIMAL));
        assert_eq!(keysym_for(0x53, true), Some(keysym::DELETE));
    }

    #[test]
    fn extended_selects_the_right_hand_modifiers() {
        assert_eq!(keysym_for(0x1D, false), Some(keysym::CONTROL_LEFT));
        assert_eq!(keysym_for(0x1D, true), Some(keysym::CONTROL_RIGHT));
        assert_eq!(keysym_for(0x38, false), Some(keysym::ALT_LEFT));
        assert_eq!(keysym_for(0x38, true), Some(keysym::ALT_RIGHT));
        assert_eq!(keysym_for(0x1C, true), Some(keysym::KP_ENTER));
        assert_eq!(keysym_for(0x35, true), Some(keysym::KP_DIVIDE));
    }

    #[test]
    fn unknown_scancodes_are_dropped() {
        assert_eq!(keysym_for(0x00, false), None);
        assert_eq!(keysym_for(0x7F, false), None);
        // F13 and the multimedia keys have no keysym in the compositor's table.
        assert_eq!(keysym_for(0x64, true), None);
    }

    /// Every keysym this module can produce must survive the compositor's own
    /// translation, or the event is dropped later with no diagnostic.
    #[test]
    fn every_produced_keysym_reaches_a_keycode() {
        for code in 0x01..=0x58u8 {
            for extended in [false, true] {
                let Some(symbol) = keysym_for(code, extended) else {
                    continue;
                };
                assert!(
                    keysym_to_keycode(symbol).is_some(),
                    "scancode {code:#04x} extended={extended} keysym {symbol:#x} has no keycode"
                );
            }
        }
    }
}
