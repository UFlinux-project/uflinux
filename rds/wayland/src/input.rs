//! Input events and their translation into wayland virtual-device requests.
//!
//! `zwp_virtual_keyboard_v1` takes raw `KEY_*` codes (Linux `input-event-codes`),
//! while RDP clients send X11 keysyms. [`keysym_to_keycode`] performs that
//! translation; the compositor owns the active xkb layout, so the mapping is
//! layout agnostic and case insensitive (`'A'` and `'a'` are the same physical
//! key — the shift modifier travels separately).

use std::time::UNIX_EPOCH;

/// A pointer action coming from an RDP client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    /// Warp the cursor to an absolute desktop position, in pixels.
    Move {
        /// Horizontal position.
        x: u16,
        /// Vertical position.
        y: u16,
    },
    /// Move the cursor by the given deltas.
    MoveRelative {
        /// Horizontal displacement.
        x: i32,
        /// Vertical displacement.
        y: i32,
    },
    /// Press or release a button.
    Button {
        /// X11 button number for left, middle, right or a side button.
        button: u16,
        /// `true` for press, `false` for release.
        pressed: bool,
    },
    /// Scroll wheel, in classic 120 unit notches. Positive away from the user.
    Wheel {
        /// Horizontal axis.
        axis_x: i32,
        /// Vertical axis.
        axis_y: i32,
    },
}

impl PointerEvent {
    /// The desktop position of a [`Self::Move`], if this is one.
    pub fn position(&self) -> Option<(u16, u16)> {
        match self {
            Self::Move { x, y } => Some((*x, *y)),
            _ => None,
        }
    }
}

/// X11 core button numbers, as used by [`PointerEvent::Button`].
pub mod button {
    /// Left button.
    pub const LEFT: u16 = 1;
    /// Middle button.
    pub const MIDDLE: u16 = 2;
    /// Right button.
    pub const RIGHT: u16 = 3;
    /// Wheel up (also expressible as [`PointerEvent::Wheel`] with a positive
    /// `axis_y`).
    pub const WHEEL_UP: u16 = 4;
    /// Wheel down.
    pub const WHEEL_DOWN: u16 = 5;
    /// First side button.
    pub const SIDE: u16 = 8;
    /// Second side button.
    pub const EXTRA: u16 = 9;

    /// The Linux BTN_* code expected by the virtual pointer protocol.
    pub(crate) fn evdev(button: u16) -> Option<u32> {
        Some(match button {
            LEFT => 0x110,
            RIGHT => 0x111,
            MIDDLE => 0x112,
            SIDE => 0x113,
            EXTRA => 0x114,
            _ => return None,
        })
    }
}

/// Linux input-event codes (`KEY_*`) understood by `zwp_virtual_keyboard_v1`.
pub mod keycode {
    #![allow(missing_docs)]
    pub const ESC: u32 = 1;
    pub const NUM_1: u32 = 2;
    pub const NUM_2: u32 = 3;
    pub const NUM_3: u32 = 4;
    pub const NUM_4: u32 = 5;
    pub const NUM_5: u32 = 6;
    pub const NUM_6: u32 = 7;
    pub const NUM_7: u32 = 8;
    pub const NUM_8: u32 = 9;
    pub const NUM_9: u32 = 10;
    pub const NUM_0: u32 = 11;
    pub const MINUS: u32 = 12;
    pub const EQUAL: u32 = 13;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const Q: u32 = 16;
    pub const W: u32 = 17;
    pub const E: u32 = 18;
    pub const R: u32 = 19;
    pub const T: u32 = 20;
    pub const Y: u32 = 21;
    pub const U: u32 = 22;
    pub const I: u32 = 23;
    pub const O: u32 = 24;
    pub const P: u32 = 25;
    pub const LEFTBRACKET: u32 = 26;
    pub const RIGHTBRACKET: u32 = 27;
    pub const ENTER: u32 = 28;
    pub const LEFTCTRL: u32 = 29;
    pub const A: u32 = 30;
    pub const S: u32 = 31;
    pub const D: u32 = 32;
    pub const F: u32 = 33;
    pub const G: u32 = 34;
    pub const H: u32 = 35;
    pub const J: u32 = 36;
    pub const K: u32 = 37;
    pub const L: u32 = 38;
    pub const SEMICOLON: u32 = 39;
    pub const APOSTROPHE: u32 = 40;
    pub const GRAVE: u32 = 41;
    pub const LEFTSHIFT: u32 = 42;
    pub const BACKSLASH: u32 = 43;
    pub const Z: u32 = 44;
    pub const X: u32 = 45;
    pub const C: u32 = 46;
    pub const V: u32 = 47;
    pub const B: u32 = 48;
    pub const N: u32 = 49;
    pub const M: u32 = 50;
    pub const COMMA: u32 = 51;
    pub const DOT: u32 = 52;
    pub const SLASH: u32 = 53;
    pub const RIGHTSHIFT: u32 = 54;
    pub const KPASTERISK: u32 = 55;
    pub const LEFTALT: u32 = 56;
    pub const SPACE: u32 = 57;
    pub const CAPSLOCK: u32 = 58;
    pub const F1: u32 = 59;
    pub const F2: u32 = 60;
    pub const F3: u32 = 61;
    pub const F4: u32 = 62;
    pub const F5: u32 = 63;
    pub const F6: u32 = 64;
    pub const F7: u32 = 65;
    pub const F8: u32 = 66;
    pub const F9: u32 = 67;
    pub const F10: u32 = 68;
    pub const NUMLOCK: u32 = 69;
    pub const SCROLLLOCK: u32 = 70;
    pub const KP7: u32 = 71;
    pub const KP8: u32 = 72;
    pub const KP9: u32 = 73;
    pub const KPMINUS: u32 = 74;
    pub const KP4: u32 = 75;
    pub const KP5: u32 = 76;
    pub const KP6: u32 = 77;
    pub const KPPLUS: u32 = 78;
    pub const KP1: u32 = 79;
    pub const KP2: u32 = 80;
    pub const KP3: u32 = 81;
    pub const KP0: u32 = 82;
    pub const KPDOT: u32 = 83;
    pub const F11: u32 = 87;
    pub const F12: u32 = 88;
    pub const KENTER: u32 = 96;
    pub const RIGHTCTRL: u32 = 97;
    pub const KPSLASH: u32 = 98;
    pub const SYSRQ: u32 = 99;
    pub const RIGHTALT: u32 = 100;
    pub const HOME: u32 = 102;
    pub const UP: u32 = 103;
    pub const PAGEUP: u32 = 104;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const END: u32 = 107;
    pub const DOWN: u32 = 108;
    pub const PAGEDOWN: u32 = 109;
    pub const INSERT: u32 = 110;
    pub const DELETE: u32 = 111;
    pub const PAUSE: u32 = 119;
    pub const LEFTMETA: u32 = 125;
    pub const RIGHTMETA: u32 = 126;
    pub const MENU: u32 = 139;
}

/// X11 keysym values that RDP clients and IronRDP's keyboard handling use.
pub mod keysym {
    #![allow(missing_docs)]
    pub const SPACE: u32 = 0x20;
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const ENTER: u32 = 0xff0d;
    pub const PAUSE: u32 = 0xff13;
    pub const SCROLL_LOCK: u32 = 0xff14;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const PRINT: u32 = 0xff61;
    pub const INSERT: u32 = 0xff63;
    pub const MENU: u32 = 0xff67;
    pub const NUM_LOCK: u32 = 0xff7f;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const KP_HOME: u32 = 0xff95;
    pub const KP_LEFT: u32 = 0xff96;
    pub const KP_UP: u32 = 0xff97;
    pub const KP_RIGHT: u32 = 0xff98;
    pub const KP_DOWN: u32 = 0xff99;
    pub const KP_PRIOR: u32 = 0xff9a;
    pub const KP_NEXT: u32 = 0xff9b;
    pub const KP_END: u32 = 0xff9c;
    pub const KP_BEGIN: u32 = 0xff9d;
    pub const KP_INSERT: u32 = 0xff9e;
    pub const KP_DELETE: u32 = 0xff9f;
    pub const KP_MULTIPLY: u32 = 0xffaa;
    pub const KP_ADD: u32 = 0xffab;
    pub const KP_SEPARATOR: u32 = 0xffac;
    pub const KP_SUBTRACT: u32 = 0xffad;
    pub const KP_DECIMAL: u32 = 0xffae;
    pub const KP_DIVIDE: u32 = 0xffaf;
    pub const KP_0: u32 = 0xffb0;
    pub const KP_1: u32 = 0xffb1;
    pub const KP_2: u32 = 0xffb2;
    pub const KP_3: u32 = 0xffb3;
    pub const KP_4: u32 = 0xffb4;
    pub const KP_5: u32 = 0xffb5;
    pub const KP_6: u32 = 0xffb6;
    pub const KP_7: u32 = 0xffb7;
    pub const KP_8: u32 = 0xffb8;
    pub const KP_9: u32 = 0xffb9;
    pub const F1: u32 = 0xffbe;
    pub const F2: u32 = 0xffbf;
    pub const F3: u32 = 0xffc0;
    pub const F4: u32 = 0xffc1;
    pub const F5: u32 = 0xffc2;
    pub const F6: u32 = 0xffc3;
    pub const F7: u32 = 0xffc4;
    pub const F8: u32 = 0xffc5;
    pub const F9: u32 = 0xffc6;
    pub const F10: u32 = 0xffc7;
    pub const F11: u32 = 0xffc8;
    pub const F12: u32 = 0xffc9;
    pub const F13: u32 = 0xffca;
    pub const SHIFT_LEFT: u32 = 0xffe1;
    pub const SHIFT_RIGHT: u32 = 0xffe2;
    pub const CONTROL_LEFT: u32 = 0xffe3;
    pub const CONTROL_RIGHT: u32 = 0xffe4;
    pub const CAPS_LOCK: u32 = 0xffe5;
    pub const META_LEFT: u32 = 0xffe7;
    pub const META_RIGHT: u32 = 0xffe8;
    pub const ALT_LEFT: u32 = 0xffe9;
    pub const ALT_RIGHT: u32 = 0xffea;
    pub const SUPER_L: u32 = 0xffeb;
    pub const SUPER_R: u32 = 0xffec;
    pub const DELETE: u32 = 0xffff;
    pub const ISO_LEVEL3_SHIFT: u32 = 0xfe03;
}

/// `KEY_*` code for every printable ASCII keysym from `' '` (0x20) to `'~'`
/// (0x7e); `0` marks a keysym without a physical key. Shifted forms resolve to
/// their unshifted key because the shift modifier is sent separately.
const ASCII_KEYCODES: [u32; 0x7f - 0x20] = [
    keycode::SPACE,        // ' '
    keycode::NUM_1,        // '!'
    keycode::APOSTROPHE,   // '"'
    keycode::NUM_3,        // '#'
    keycode::NUM_4,        // '$'
    keycode::NUM_5,        // '%'
    keycode::NUM_7,        // '&'
    keycode::APOSTROPHE,   // '\''
    keycode::NUM_9,        // '('
    keycode::NUM_0,        // ')'
    keycode::NUM_8,        // '*'
    keycode::EQUAL,        // '+'
    keycode::COMMA,        // ','
    keycode::MINUS,        // '-'
    keycode::DOT,          // '.'
    keycode::SLASH,        // '/'
    keycode::NUM_0,        // '0'
    keycode::NUM_1,        // '1'
    keycode::NUM_2,        // '2'
    keycode::NUM_3,        // '3'
    keycode::NUM_4,        // '4'
    keycode::NUM_5,        // '5'
    keycode::NUM_6,        // '6'
    keycode::NUM_7,        // '7'
    keycode::NUM_8,        // '8'
    keycode::NUM_9,        // '9'
    keycode::SEMICOLON,    // ':'
    keycode::SEMICOLON,    // ';'
    keycode::COMMA,        // '<'
    keycode::EQUAL,        // '='
    keycode::DOT,          // '>'
    keycode::SLASH,        // '?'
    keycode::NUM_2,        // '@'
    keycode::A,            // 'A'
    keycode::B,            // 'B'
    keycode::C,            // 'C'
    keycode::D,            // 'D'
    keycode::E,            // 'E'
    keycode::F,            // 'F'
    keycode::G,            // 'G'
    keycode::H,            // 'H'
    keycode::I,            // 'I'
    keycode::J,            // 'J'
    keycode::K,            // 'K'
    keycode::L,            // 'L'
    keycode::M,            // 'M'
    keycode::N,            // 'N'
    keycode::O,            // 'O'
    keycode::P,            // 'P'
    keycode::Q,            // 'Q'
    keycode::R,            // 'R'
    keycode::S,            // 'S'
    keycode::T,            // 'T'
    keycode::U,            // 'U'
    keycode::V,            // 'V'
    keycode::W,            // 'W'
    keycode::X,            // 'X'
    keycode::Y,            // 'Y'
    keycode::Z,            // 'Z'
    keycode::LEFTBRACKET,  // '['
    keycode::BACKSLASH,    // '\\'
    keycode::RIGHTBRACKET, // ']'
    keycode::NUM_6,        // '^'
    keycode::MINUS,        // '_'
    keycode::GRAVE,        // '`'
    keycode::A,            // 'a'
    keycode::B,            // 'b'
    keycode::C,            // 'c'
    keycode::D,            // 'd'
    keycode::E,            // 'e'
    keycode::F,            // 'f'
    keycode::G,            // 'g'
    keycode::H,            // 'h'
    keycode::I,            // 'i'
    keycode::J,            // 'j'
    keycode::K,            // 'k'
    keycode::L,            // 'l'
    keycode::M,            // 'm'
    keycode::N,            // 'n'
    keycode::O,            // 'o'
    keycode::P,            // 'p'
    keycode::Q,            // 'q'
    keycode::R,            // 'r'
    keycode::S,            // 's'
    keycode::T,            // 't'
    keycode::U,            // 'u'
    keycode::V,            // 'v'
    keycode::W,            // 'w'
    keycode::X,            // 'x'
    keycode::Y,            // 'y'
    keycode::Z,            // 'z'
    keycode::LEFTBRACKET,  // '{'
    keycode::BACKSLASH,    // '|'
    keycode::RIGHTBRACKET, // '}'
    keycode::GRAVE,        // '~'
];

/// Non-ASCII keysyms with an evdev equivalent, sorted by keysym.
const KEYSYM_MAP: &[(u32, u32)] = &[
    (keysym::ISO_LEVEL3_SHIFT, keycode::RIGHTALT),
    (keysym::BACKSPACE, keycode::BACKSPACE),
    (keysym::TAB, keycode::TAB),
    (keysym::ENTER, keycode::ENTER),
    (keysym::PAUSE, keycode::PAUSE),
    (keysym::SCROLL_LOCK, keycode::SCROLLLOCK),
    (keysym::ESCAPE, keycode::ESC),
    (keysym::HOME, keycode::HOME),
    (keysym::LEFT, keycode::LEFT),
    (keysym::UP, keycode::UP),
    (keysym::RIGHT, keycode::RIGHT),
    (keysym::DOWN, keycode::DOWN),
    (keysym::PAGE_UP, keycode::PAGEUP),
    (keysym::PAGE_DOWN, keycode::PAGEDOWN),
    (keysym::END, keycode::END),
    (keysym::PRINT, keycode::SYSRQ),
    (keysym::INSERT, keycode::INSERT),
    (keysym::MENU, keycode::MENU),
    (keysym::NUM_LOCK, keycode::NUMLOCK),
    (keysym::KP_ENTER, keycode::KENTER),
    (keysym::KP_HOME, keycode::KP7),
    (keysym::KP_LEFT, keycode::KP4),
    (keysym::KP_UP, keycode::KP5),
    (keysym::KP_RIGHT, keycode::KP6),
    (keysym::KP_DOWN, keycode::KP2),
    (keysym::KP_PRIOR, keycode::KP9),
    (keysym::KP_NEXT, keycode::KP3),
    (keysym::KP_END, keycode::KP1),
    (keysym::KP_BEGIN, keycode::KP2),
    (keysym::KP_INSERT, keycode::KP0),
    (keysym::KP_DELETE, keycode::KPDOT),
    (keysym::KP_MULTIPLY, keycode::KPASTERISK),
    (keysym::KP_ADD, keycode::KPPLUS),
    (keysym::KP_SEPARATOR, keycode::KPDOT),
    (keysym::KP_SUBTRACT, keycode::KPMINUS),
    (keysym::KP_DECIMAL, keycode::KPDOT),
    (keysym::KP_DIVIDE, keycode::KPSLASH),
    (keysym::KP_0, keycode::KP0),
    (keysym::KP_1, keycode::KP1),
    (keysym::KP_2, keycode::KP2),
    (keysym::KP_3, keycode::KP3),
    (keysym::KP_4, keycode::KP4),
    (keysym::KP_5, keycode::KP5),
    (keysym::KP_6, keycode::KP6),
    (keysym::KP_7, keycode::KP7),
    (keysym::KP_8, keycode::KP8),
    (keysym::KP_9, keycode::KP9),
    (keysym::F1, keycode::F1),
    (keysym::F2, keycode::F2),
    (keysym::F3, keycode::F3),
    (keysym::F4, keycode::F4),
    (keysym::F5, keycode::F5),
    (keysym::F6, keycode::F6),
    (keysym::F7, keycode::F7),
    (keysym::F8, keycode::F8),
    (keysym::F9, keycode::F9),
    (keysym::F10, keycode::F10),
    (keysym::F11, keycode::F11),
    (keysym::F12, keycode::F12),
    (keysym::SHIFT_LEFT, keycode::LEFTSHIFT),
    (keysym::SHIFT_RIGHT, keycode::RIGHTSHIFT),
    (keysym::CONTROL_LEFT, keycode::LEFTCTRL),
    (keysym::CONTROL_RIGHT, keycode::RIGHTCTRL),
    (keysym::CAPS_LOCK, keycode::CAPSLOCK),
    (keysym::META_LEFT, keycode::LEFTMETA),
    (keysym::META_RIGHT, keycode::RIGHTMETA),
    (keysym::ALT_LEFT, keycode::LEFTALT),
    (keysym::ALT_RIGHT, keycode::RIGHTALT),
    (keysym::SUPER_L, keycode::LEFTMETA),
    (keysym::SUPER_R, keycode::RIGHTMETA),
    (keysym::DELETE, keycode::DELETE),
];

/// Translate an X11 keysym into the `KEY_*` code expected by
/// `zwp_virtual_keyboard_v1::key`.
///
/// Returns `None` for keysyms without an evdev equivalent (dead keys,
/// composites, F13 and above, non-Latin Unicode keysyms).
pub fn keysym_to_keycode(keysym: u32) -> Option<u32> {
    // Printable ASCII, including both ASCII cases of every letter.
    if (keysym::SPACE..0x7f).contains(&keysym) {
        return match ASCII_KEYCODES[(keysym - 0x20) as usize] {
            0 => None,
            code => Some(code),
        };
    }
    KEYSYM_MAP
        .binary_search_by(|(k, _)| k.cmp(&keysym))
        .ok()
        .map(|i| KEYSYM_MAP[i].1)
}

/// `zwp_virtual_keyboard_v1::key` states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// Released.
    Released,
    /// Pressed.
    Pressed,
}

impl KeyState {
    /// The wire value of the state.
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Released => 0,
            Self::Pressed => 1,
        }
    }
}

/// XKB modifier bits, as used by `zwp_virtual_keyboard_v1::modifiers`.
pub mod modifier {
    /// Shift.
    pub const SHIFT: u32 = 1;
    /// CapsLock.
    pub const LOCK: u32 = 2;
    /// Control.
    pub const CONTROL: u32 = 4;
    /// Alt.
    pub const MOD1: u32 = 8;
    /// NumLock.
    pub const MOD2: u32 = 16;
    /// Super/Meta.
    pub const MOD4: u32 = 64;
}

/// The keyboard state the backend reports to the compositor.
///
/// RDP sends keysyms without an XKB modifier mask, so the backend tracks the
/// modifiers itself from the keys it has injected and forwards them with every
/// change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardState {
    /// Depressed (currently held) modifier bits.
    pub depressed: u32,
    /// Latched modifier bits.
    pub latched: u32,
    /// Locked (toggle) modifier bits, e.g. CapsLock.
    pub locked: u32,
    /// Layout group.
    pub group: u32,
}

impl KeyboardState {
    /// An empty state: no modifiers down, group 0.
    pub const fn empty() -> Self {
        Self {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        }
    }

    /// Update the state for a physical key and report whether the caller
    /// should send a `modifiers` request.
    ///
    /// Held modifier keys contribute to [`Self::depressed`]; CapsLock, NumLock
    /// and ScrollLock toggle [`Self::locked`].
    pub fn apply(&mut self, keycode: u32, state: KeyState) -> ModifiersChanged {
        let before = *self;
        let toggle = match keycode {
            keycode::CAPSLOCK => Some(modifier::LOCK),
            keycode::NUMLOCK => Some(modifier::MOD2),
            keycode::SCROLLLOCK => Some(modifier::MOD1),
            _ => None,
        };
        let held = match keycode {
            keycode::LEFTSHIFT | keycode::RIGHTSHIFT => modifier::SHIFT,
            keycode::LEFTCTRL | keycode::RIGHTCTRL => modifier::CONTROL,
            keycode::LEFTALT | keycode::RIGHTALT => modifier::MOD1,
            keycode::LEFTMETA | keycode::RIGHTMETA => modifier::MOD4,
            _ => 0,
        };
        match state {
            KeyState::Pressed => {
                self.depressed |= held;
                if let Some(bit) = toggle {
                    self.locked ^= bit;
                }
            }
            KeyState::Released => self.depressed &= !held,
        }
        ModifiersChanged {
            changed: before != *self,
        }
    }

    /// Replace the modifier state wholesale, e.g. from an RDP input packet
    /// that carries an explicit modifier flag byte.
    pub fn set_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) -> ModifiersChanged {
        let before = *self;
        self.depressed = depressed;
        self.latched = latched;
        self.locked = locked;
        self.group = group;
        ModifiersChanged {
            changed: before != *self,
        }
    }
}

/// Returned by [`KeyboardState::apply`] and [`KeyboardState::set_modifiers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModifiersChanged {
    /// `true` when the caller should send a `modifiers` request.
    pub changed: bool,
}

/// A keyboard action coming from an RDP client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyboardEvent {
    /// X11 keysym of the key, e.g. `0x61` for `a`.
    pub keysym: u32,
    /// Whether the key went down or up.
    pub pressed: bool,
}

impl KeyboardEvent {
    /// The `KEY_*` code to inject, or `None` for an untranslatable keysym.
    pub fn keycode(&self) -> Option<u32> {
        keysym_to_keycode(self.keysym)
    }

    /// The state to inject.
    pub fn state(&self) -> KeyState {
        if self.pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        }
    }
}

/// A timestamp in milliseconds for the virtual-device requests. Wayland only
/// requires it to be non-decreasing per client; compositors expect a
/// `CLOCK_MONOTONIC`-ish value.
pub(crate) fn event_time() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_case_insensitive() {
        assert_eq!(keysym_to_keycode(0x61), Some(keycode::A));
        assert_eq!(keysym_to_keycode(0x41), Some(keycode::A));
        assert_eq!(keysym_to_keycode(0x7a), Some(keycode::Z));
        assert_eq!(keysym_to_keycode(0x5a), Some(keycode::Z));
        assert_eq!(keysym_to_keycode(0x71), Some(keycode::Q));
        assert_eq!(keysym_to_keycode(0x6d), Some(keycode::M));
    }

    #[test]
    fn digits_map_to_the_number_row() {
        assert_eq!(keysym_to_keycode(0x30), Some(keycode::NUM_0));
        assert_eq!(keysym_to_keycode(0x31), Some(keycode::NUM_1));
        assert_eq!(keysym_to_keycode(0x39), Some(keycode::NUM_9));
        // Shifted forms resolve to the unshifted key.
        assert_eq!(keysym_to_keycode(0x21), Some(keycode::NUM_1));
        assert_eq!(keysym_to_keycode(0x25), Some(keycode::NUM_5));
    }

    #[test]
    fn punctuation_and_whitespace() {
        assert_eq!(keysym_to_keycode(keysym::SPACE), Some(keycode::SPACE));
        assert_eq!(keysym_to_keycode(keysym::TAB), Some(keycode::TAB));
        assert_eq!(keysym_to_keycode(keysym::ENTER), Some(keycode::ENTER));
        assert_eq!(
            keysym_to_keycode(keysym::BACKSPACE),
            Some(keycode::BACKSPACE)
        );
        assert_eq!(keysym_to_keycode(keysym::ESCAPE), Some(keycode::ESC));
        assert_eq!(keysym_to_keycode(0x2f), Some(keycode::SLASH));
        assert_eq!(keysym_to_keycode(0x7b), Some(keycode::LEFTBRACKET));
        assert_eq!(keysym_to_keycode(0x7c), Some(keycode::BACKSLASH));
    }

    #[test]
    fn function_keys() {
        assert_eq!(keysym_to_keycode(keysym::F1), Some(keycode::F1));
        assert_eq!(keysym_to_keycode(0xffc9), Some(keycode::F12));
        // F13 has no evdev code that X11 and Linux agree on.
        assert_eq!(keysym_to_keycode(keysym::F13), None);
    }

    #[test]
    fn numpad() {
        assert_eq!(keysym_to_keycode(keysym::KP_0), Some(keycode::KP0));
        assert_eq!(keysym_to_keycode(keysym::KP_5), Some(keycode::KP5));
        assert_eq!(keysym_to_keycode(keysym::KP_9), Some(keycode::KP9));
        assert_eq!(keysym_to_keycode(keysym::KP_ENTER), Some(keycode::KENTER));
        assert_eq!(keysym_to_keycode(keysym::KP_ADD), Some(keycode::KPPLUS));
        assert_eq!(keysym_to_keycode(keysym::KP_DECIMAL), Some(keycode::KPDOT));
        assert_eq!(keysym_to_keycode(keysym::KP_BEGIN), Some(keycode::KP2));
        // A numpad keysym we deliberately do not map.
        assert_eq!(keysym_to_keycode(0xff80), None);
    }

    #[test]
    fn modifiers_and_navigation() {
        assert_eq!(
            keysym_to_keycode(keysym::SHIFT_LEFT),
            Some(keycode::LEFTSHIFT)
        );
        assert_eq!(
            keysym_to_keycode(keysym::CONTROL_RIGHT),
            Some(keycode::RIGHTCTRL)
        );
        assert_eq!(keysym_to_keycode(keysym::SUPER_L), Some(keycode::LEFTMETA));
        assert_eq!(keysym_to_keycode(keysym::HOME), Some(keycode::HOME));
        assert_eq!(keysym_to_keycode(keysym::DELETE), Some(keycode::DELETE));
        assert_eq!(keysym_to_keycode(keysym::PRINT), Some(keycode::SYSRQ));
    }

    #[test]
    fn unknown_keysyms_are_rejected() {
        assert_eq!(keysym_to_keycode(0xdead), None);
        assert_eq!(keysym_to_keycode(0), None);
        assert_eq!(keysym_to_keycode(0xff00), None); // dead grave
        assert_eq!(keysym_to_keycode(0x010000e9), None); // Unicode keysym
        assert_eq!(keysym_to_keycode(0x1000000), None);
    }

    /// `keysym_to_keycode` binary-searches `KEYSYM_MAP`, which silently returns
    /// wrong answers (or none) if the table is ever edited out of order.
    #[test]
    fn keysym_table_is_sorted_and_deduplicated() {
        assert!(
            KEYSYM_MAP.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "KEYSYM_MAP must be sorted by keysym without duplicates"
        );
        // Every entry must round-trip through the lookup.
        for &(keysym, expected) in KEYSYM_MAP {
            assert_eq!(keysym_to_keycode(keysym), Some(expected), "{keysym:#x}");
        }
    }

    #[test]
    fn keyboard_event_helpers() {
        let down = KeyboardEvent {
            keysym: 0x61,
            pressed: true,
        };
        assert_eq!(down.keycode(), Some(keycode::A));
        assert_eq!(down.state(), KeyState::Pressed);
        assert!(
            !KeyboardState::empty()
                .apply(keycode::A, down.state())
                .changed
        );
    }

    #[test]
    fn held_shift_tracks_depressed() {
        let mut state = KeyboardState::empty();
        assert!(state.apply(keycode::LEFTSHIFT, KeyState::Pressed).changed);
        assert_eq!(state.depressed, modifier::SHIFT);
        assert!(!state.apply(keycode::A, KeyState::Pressed).changed);
        assert!(state.apply(keycode::LEFTSHIFT, KeyState::Released).changed);
        assert_eq!(state.depressed, 0);
    }

    #[test]
    fn capslock_toggles_locked_only() {
        let mut state = KeyboardState::empty();
        assert!(state.apply(keycode::CAPSLOCK, KeyState::Pressed).changed);
        assert_eq!(state.locked, modifier::LOCK);
        assert_eq!(state.depressed, 0);
        assert!(!state.apply(keycode::CAPSLOCK, KeyState::Released).changed);
        assert!(state.apply(keycode::CAPSLOCK, KeyState::Pressed).changed);
        assert_eq!(state.locked, 0);
    }

    #[test]
    fn set_modifiers_reports_noop() {
        let mut state = KeyboardState::empty();
        assert!(!state.set_modifiers(0, 0, 0, 0).changed);
        assert!(state.set_modifiers(modifier::CONTROL, 0, 0, 0).changed);
    }

    #[test]
    fn pointer_event_position() {
        assert_eq!(PointerEvent::Move { x: 3, y: 4 }.position(), Some((3, 4)));
        assert_eq!(
            PointerEvent::Button {
                button: button::LEFT,
                pressed: true
            }
            .position(),
            None
        );
    }

    #[test]
    fn key_state_wire_values() {
        assert_eq!(KeyState::Released.as_u32(), 0);
        assert_eq!(KeyState::Pressed.as_u32(), 1);
    }

    #[test]
    fn event_time_does_not_go_backwards() {
        let a = event_time();
        let b = event_time();
        assert!(b >= a || a - b < 10, "{a} then {b}");
    }
}
