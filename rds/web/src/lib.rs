//! WebAssembly RDP client for the UFLinux RDS web gateway.
//!
//! The session page controller lives in Rust and drives IronRDP’s session,
//! input and clipboard machinery through a small [`wasm_bindgen`] façade.
//!
//! # Why the bridge objects are driven through `Reflect`
//!
//! `ironrdp-web` builds its JS bindings with `iron_remote_desktop::make_bridge!`
//! inside a *private* `wasm_bridge` module. The concrete bridge classes
//! (`Session`, `SessionBuilder`, `DeviceEvent`, `InputTransaction`,
//! `ClipboardData`, `IronError`, …) therefore exist only on the **JavaScript**
//! side: they are not nameable from this crate's Rust code. (In
//! `iron-remote-desktop` those same identifiers are *traits*.)
//!
//! The only concrete, nameable wasm-bindgen types are `DesktopSize`,
//! `Extension` and the `IronErrorKind`/`RotationUnit` enums, which we use
//! directly. Everything else is reached through the ES module namespace object
//! that the browser obtains via `import('/assets/uflinux_rds_web.js')` and hands
//! to us as [`ConnectOptions::bridge`]. We then look up constructors and methods
//! with [`js_sys::Reflect`] and invoke them with [`js_sys::Function::apply`].
//!
//! This keeps the Rust side free of `unsafe` and free of any assumption about
//! IronRDP's internal type layout: we only rely on the documented JS surface.

use std::sync::Once;

// Force-link `ironrdp-web`: its `make_bridge!` bindings live in a private
// module and nothing in this crate's Rust code names them, so without an
// explicit reference the linker prunes the object file and the JS-side
// `SessionBuilder`/`Session`/`DeviceEvent`/… classes never appear in the wasm.
extern crate ironrdp_web;

use iron_remote_desktop::{DesktopSize, Extension};
use js_sys::{Array, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[cfg(target_arch = "wasm32")]
mod console;
pub mod error;
mod input;

pub use error::{
    KIND_ACCESS_DENIED, KIND_GENERAL, KIND_LOGON_FAILURE, KIND_NEGOTIATION_FAILURE,
    KIND_PROXY_CONNECT, KIND_RDCLEANPATH, KIND_WRONG_PASSWORD,
};

/// Guards `iron_remote_desktop::internal::setup` (panic hook + logger).
static SETUP: Once = Once::new();

/// Initialise logging and the wasm panic hook. Safe to call more than once.
///
/// `log_level` is a `tracing` level (`error`/`warn`/`info`/`debug`/`trace`); an
/// unrecognised value silently disables logging, matching IronRDP's behaviour.
#[wasm_bindgen]
pub fn init(log_level: &str) {
    SETUP.call_once(|| iron_remote_desktop::internal::setup(log_level));
}

// ---------------------------------------------------------------------------
// Error helpers exposed to JS
// ---------------------------------------------------------------------------

/// User-facing message for an `IronErrorKind` discriminant.
#[wasm_bindgen(js_name = errorMessage)]
#[must_use]
pub fn error_message(kind: i32) -> String {
    error::error_message(kind).to_owned()
}

/// Stable lowercase tag for an `IronErrorKind` discriminant.
#[wasm_bindgen(js_name = errorKindName)]
#[must_use]
pub fn error_kind_name(kind: i32) -> String {
    error::error_kind_name(kind).to_owned()
}

/// Reads `IronErrorKind` out of a caught `IronError` (or any thrown value).
///
/// Falls back to [`KIND_GENERAL`] when the value carries no `kind()` method.
#[wasm_bindgen(js_name = errorKind)]
#[must_use]
pub fn error_kind(err: &JsValue) -> i32 {
    error_kind_of(err)
}

// ---------------------------------------------------------------------------
// ConnectOptions
// ---------------------------------------------------------------------------

/// Options for [`connect`].
///
/// `bridge` must be the ES module namespace object returned by
/// `await import('/assets/uflinux_rds_web.js')`; `canvas` is the
/// `<canvas>` IronRDP renders into. The `on_*` fields are JS callbacks (or
/// `undefined`).
#[wasm_bindgen]
#[derive(Default, Clone)]
pub struct ConnectOptions {
    ws_url: String,
    token: String,
    username: String,
    password: String,
    destination: String,
    bridge: JsValue,
    canvas: JsValue,
    on_disconnected: JsValue,
    on_error: JsValue,
    on_pointer_cursor: JsValue,
    on_clipboard_text: JsValue,
    on_clipboard_request: JsValue,
}

#[wasm_bindgen]
impl ConnectOptions {
    /// Empty options; set the fields you need before calling [`connect`].
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    #[wasm_bindgen(getter, js_name = wsUrl)]
    pub fn ws_url(&self) -> String {
        self.ws_url.clone()
    }
    #[wasm_bindgen(setter, js_name = wsUrl)]
    pub fn set_ws_url(&mut self, value: String) {
        self.ws_url = value;
    }

    #[wasm_bindgen(getter)]
    pub fn token(&self) -> String {
        self.token.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_token(&mut self, value: String) {
        self.token = value;
    }

    #[wasm_bindgen(getter)]
    pub fn username(&self) -> String {
        self.username.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_username(&mut self, value: String) {
        self.username = value;
    }

    #[wasm_bindgen(getter)]
    pub fn password(&self) -> String {
        self.password.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_password(&mut self, value: String) {
        self.password = value;
    }

    #[wasm_bindgen(getter)]
    pub fn destination(&self) -> String {
        self.destination.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_destination(&mut self, value: String) {
        self.destination = value;
    }

    #[wasm_bindgen(getter)]
    pub fn bridge(&self) -> JsValue {
        self.bridge.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_bridge(&mut self, value: JsValue) {
        self.bridge = value;
    }

    #[wasm_bindgen(getter)]
    pub fn canvas(&self) -> JsValue {
        self.canvas.clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_canvas(&mut self, value: JsValue) {
        self.canvas = value;
    }

    #[wasm_bindgen(getter, js_name = onDisconnected)]
    pub fn on_disconnected(&self) -> JsValue {
        self.on_disconnected.clone()
    }
    #[wasm_bindgen(setter, js_name = onDisconnected)]
    pub fn set_on_disconnected(&mut self, value: JsValue) {
        self.on_disconnected = value;
    }

    #[wasm_bindgen(getter, js_name = onError)]
    pub fn on_error(&self) -> JsValue {
        self.on_error.clone()
    }
    #[wasm_bindgen(setter, js_name = onError)]
    pub fn set_on_error(&mut self, value: JsValue) {
        self.on_error = value;
    }

    #[wasm_bindgen(getter, js_name = onPointerCursor)]
    pub fn on_pointer_cursor(&self) -> JsValue {
        self.on_pointer_cursor.clone()
    }
    #[wasm_bindgen(setter, js_name = onPointerCursor)]
    pub fn set_on_pointer_cursor(&mut self, value: JsValue) {
        self.on_pointer_cursor = value;
    }

    #[wasm_bindgen(getter, js_name = onClipboardText)]
    pub fn on_clipboard_text(&self) -> JsValue {
        self.on_clipboard_text.clone()
    }
    #[wasm_bindgen(setter, js_name = onClipboardText)]
    pub fn set_on_clipboard_text(&mut self, value: JsValue) {
        self.on_clipboard_text = value;
    }

    #[wasm_bindgen(getter, js_name = onClipboardRequest)]
    pub fn on_clipboard_request(&self) -> JsValue {
        self.on_clipboard_request.clone()
    }
    #[wasm_bindgen(setter, js_name = onClipboardRequest)]
    pub fn set_on_clipboard_request(&mut self, value: JsValue) {
        self.on_clipboard_request = value;
    }
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

/// Builds an IronRDP session from `options` and returns a connected [`Client`].
///
/// The wasm module opens the WebSocket itself (from `wsUrl`), so there is no
/// reader/writer plumbing on the JS side. On failure the rejected value is a
/// short human-readable string.
#[wasm_bindgen]
pub async fn connect(options: ConnectOptions) -> Result<Client, JsValue> {
    let bridge = options.bridge.clone();
    if !bridge.is_object() {
        return Err(JsValue::from_str(
            "connect: options.bridge must be the wasm module namespace",
        ));
    }
    let canvas = options.canvas.clone();
    if !canvas.is_object() {
        return Err(JsValue::from_str(
            "connect: options.canvas must be a canvas element",
        ));
    }
    let (cw, ch) = canvas_size(&canvas);

    let mut b = construct(&bridge, "SessionBuilder", &[])?;
    b = call_method(&b, "username", &[JsValue::from_str(&options.username)])?;
    b = call_method(&b, "password", &[JsValue::from_str(&options.password)])?;
    b = call_method(
        &b,
        "destination",
        &[JsValue::from_str(&options.destination)],
    )?;
    b = call_method(&b, "proxyAddress", &[JsValue::from_str(&options.ws_url)])?;
    b = call_method(&b, "authToken", &[JsValue::from_str(&options.token)])?;
    b = call_method(&b, "renderCanvas", std::slice::from_ref(&canvas))?;

    let size = DesktopSize::create(cw, ch);
    b = call_method(&b, "desktopSize", &[size.into()])?;

    // Cursor: IronRDP calls the callback with (kind, data, hotspotX, hotspotY)
    // and requires a context object, so we always pass `null`.
    let cursor_cb = cursor_callback(&options.on_pointer_cursor);
    b = call_method(
        &b,
        "setCursorStyleCallback",
        std::slice::from_ref(&cursor_cb),
    )?;
    b = call_method(&b, "setCursorStyleCallbackContext", &[JsValue::NULL])?;

    // The clipboard backend only exists when a remote-changed callback is set,
    // so register both clipboard callbacks unconditionally.
    let clip_cb = clipboard_callback(&options.on_clipboard_text);
    b = call_method(
        &b,
        "remoteClipboardChangedCallback",
        std::slice::from_ref(&clip_cb),
    )?;
    let force_cb = force_clipboard_callback(&options.on_clipboard_request);
    b = call_method(
        &b,
        "forceClipboardUpdateCallback",
        std::slice::from_ref(&force_cb),
    )?;

    // display_control lets session.resize() renegotiate the geometry.
    let ext = Extension::create("display_control".to_owned(), JsValue::TRUE);
    b = call_method(&b, "extension", &[ext.into()])?;

    // The gateway's backend transport is already confidential, so the session
    // host answers X.224 with an empty SecurityProtocol and there is no TLS
    // handshake inside the connection. IronRDP only offers that "standard RDP
    // security" when it is asked for, and the wasm builder has no setter for
    // it, so it goes in as a raw extension. Without it the client sends a
    // request that only advertises TLS and the server refuses the negotiation.
    let rdp_security = Extension::create("enable_standard_rdp_security".to_owned(), JsValue::TRUE);
    b = call_method(&b, "extension", &[rdp_security.into()])?;

    let connect_result = call_method(&b, "connect", &[])?;
    let session = JsFuture::from(connect_result.dyn_into::<js_sys::Promise>()?)
        .await
        .map_err(|e| connect_failure_message(&e))?;

    Ok(Client {
        session,
        bridge,
        canvas,
        on_disconnected: options.on_disconnected.clone(),
        on_error: options.on_error.clone(),
        started: false,
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A connected RDP session, driven from JavaScript.
#[wasm_bindgen]
pub struct Client {
    session: JsValue,
    bridge: JsValue,
    canvas: JsValue,
    on_disconnected: JsValue,
    on_error: JsValue,
    started: bool,
}

#[wasm_bindgen]
impl Client {
    /// The canvas this client renders into.
    #[wasm_bindgen(getter)]
    pub fn canvas(&self) -> JsValue {
        self.canvas.clone()
    }

    /// Starts pumping the session. Resolves into `onDisconnected(reason)`; a
    /// failed run first reports `onError(kind, message)`. Idempotent.
    pub fn start(&mut self) -> Result<(), JsValue> {
        if self.started {
            return Ok(());
        }
        self.started = true;

        let session = self.session.clone();
        let on_disconnected = self.on_disconnected.clone();
        let on_error = self.on_error.clone();
        spawn_local(async move {
            let run = match method(&session, "run") {
                Ok(f) => f,
                Err(_) => return,
            };
            let promise = match run.apply(&session, &Array::new()) {
                Ok(p) => p,
                Err(_) => return,
            };
            let promise = match promise.dyn_into::<js_sys::Promise>() {
                Ok(p) => p,
                Err(_) => return,
            };
            match JsFuture::from(promise).await {
                Ok(info) => {
                    let reason = termination_reason(&info);
                    fire(&on_disconnected, &[JsValue::from_str(&reason)]);
                }
                Err(err) => {
                    let kind = error_kind_of(&err);
                    fire(
                        &on_error,
                        &[
                            JsValue::from(kind),
                            JsValue::from_str(error::error_message(kind)),
                        ],
                    );
                    fire(&on_disconnected, &[JsValue::from_str("disconnected")]);
                }
            }
        });
        Ok(())
    }

    /// Asks the session to shut down; `start()`'s callback reports the reason.
    pub fn stop(&self) {
        let _ = call_method(&self.session, "shutdown", &[]);
    }

    /// Renegotiates the desktop geometry (needs the `display_control`
    /// extension, which [`connect`] always sets). Zero sizes are ignored.
    pub fn resize(&self, width: f64, height: f64) {
        let w = clamp_u32(width);
        let h = clamp_u32(height);
        if w == 0 || h == 0 {
            return;
        }
        let none = JsValue::UNDEFINED;
        let _ = call_method(
            &self.session,
            "resize",
            &[
                JsValue::from_f64(f64::from(w)),
                JsValue::from_f64(f64::from(h)),
                none.clone(),
                none.clone(),
                none,
            ],
        );
    }

    /// The server's current desktop size as `{ width, height }`, if available.
    ///
    /// IronRDP's `DesktopSize` has no JS-visible fields, so the pair is
    /// rebuilt here from the struct's memory layout guarantees (two `u16`s
    /// exported as an opaque handle) via the `width`/`height` getters that
    /// wasm-bindgen generates for public fields.
    pub fn desktop_size(&self) -> Option<js_sys::Object> {
        let obj = call_method(&self.session, "desktopSize", &[]).ok()?;
        let width = Reflect::get(&obj, &JsValue::from_str("width"))
            .ok()
            .and_then(|v| v.as_f64())?;
        let height = Reflect::get(&obj, &JsValue::from_str("height"))
            .ok()
            .and_then(|v| v.as_f64())?;
        let entries = Array::of2(&JsValue::from_str("width"), &JsValue::from(width));
        let pair = Array::of2(&JsValue::from_str("height"), &JsValue::from(height));
        let entries = Array::of2(&entries, &pair);
        js_sys::Object::from_entries(&entries).ok()
    }

    /// Moves the pointer to desktop coordinates `(x, y)`.
    ///
    /// `buttons` is accepted for symmetry with the DOM pointer state but is
    /// unused: IronRDP's `mouseMove` carries position only.
    pub fn pointer_move(&self, x: f64, y: f64, _buttons: u16) -> Result<(), JsValue> {
        let (x, y) = (clamp_u16(x), clamp_u16(y));
        let ev = device_event(
            &self.bridge,
            "mouseMove",
            &[JsValue::from(x), JsValue::from(y)],
        )?;
        self.send_events(vec![ev])
    }

    /// Presses (`pressed == true`) or releases a mouse button at `(x, y)`.
    ///
    /// `kind` is the DOM `MouseEvent.button` id (0 left, 1 middle, 2 right,
    /// 3 X1, 4 X2), which maps one-to-one onto IronRDP's button numbering.
    pub fn button(&self, pressed: bool, kind: i32, x: f64, y: f64) -> Result<(), JsValue> {
        let (x, y) = (clamp_u16(x), clamp_u16(y));
        let move_ev = device_event(
            &self.bridge,
            "mouseMove",
            &[JsValue::from(x), JsValue::from(y)],
        )?;
        let factory = if pressed {
            "mouseButtonPressed"
        } else {
            "mouseButtonReleased"
        };
        let button = kind.clamp(0, 4) as u8;
        let button_ev = device_event(&self.bridge, factory, &[JsValue::from(button)])?;
        self.send_events(vec![move_ev, button_ev])
    }

    /// Sends a wheel gesture. `delta_mode` matches `WheelEvent.deltaMode`
    /// (0 pixels, 1 lines, 2 pages).
    pub fn wheel(&self, delta_y: f64, delta_x: f64, delta_mode: u32) -> Result<(), JsValue> {
        let unit = match delta_mode {
            1 => 1, // Line
            2 => 2, // Page
            _ => 0, // Pixel
        };
        let mut events = Vec::new();
        let vertical = clamp_i16(delta_y);
        if vertical != 0 {
            events.push(device_event(
                &self.bridge,
                "wheelRotations",
                &[JsValue::TRUE, JsValue::from(vertical), JsValue::from(unit)],
            )?);
        }
        let horizontal = clamp_i16(delta_x);
        if horizontal != 0 {
            events.push(device_event(
                &self.bridge,
                "wheelRotations",
                &[
                    JsValue::FALSE,
                    JsValue::from(horizontal),
                    JsValue::from(unit),
                ],
            )?);
        }
        if events.is_empty() {
            return Ok(());
        }
        self.send_events(events)
    }

    /// Sends a key press/release from a `KeyboardEvent.code`.
    ///
    /// Lock-key states are synchronised first. Returns `false` (and sends
    /// nothing) for an unmapped code.
    #[allow(clippy::too_many_arguments)]
    pub fn key(
        &self,
        code: &str,
        pressed: bool,
        scroll_lock: bool,
        num_lock: bool,
        caps_lock: bool,
        kana_lock: bool,
    ) -> Result<bool, JsValue> {
        let _ = call_method(
            &self.session,
            "synchronizeLockKeys",
            &[
                JsValue::from(scroll_lock),
                JsValue::from(num_lock),
                JsValue::from(caps_lock),
                JsValue::from(kana_lock),
            ],
        );
        let Some(scancode) = input::scancode_for_code(code) else {
            return Ok(false);
        };
        let factory = if pressed { "keyPressed" } else { "keyReleased" };
        let ev = device_event(&self.bridge, factory, &[JsValue::from(scancode)])?;
        self.send_events(vec![ev])?;
        Ok(true)
    }

    /// Applies a batch of pre-built `DeviceEvent` objects.
    pub fn send_input(&self, events: Array) -> Result<(), JsValue> {
        let mut vec = Vec::with_capacity(events.length() as usize);
        for event in events.iter() {
            vec.push(event);
        }
        self.send_events(vec)
    }

    /// Releases every pointer button and key.
    pub fn release_all_inputs(&self) -> Result<(), JsValue> {
        call_method(&self.session, "releaseAllInputs", &[]).map(|_| ())
    }

    /// Sends `text` to the remote clipboard.
    pub fn clipboard_send_text(&self, text: &str) -> Result<(), JsValue> {
        send_text(&self.session, &self.bridge, text)
    }

    /// Reads the local clipboard (when permitted) and sends it to the server.
    pub fn clipboard_paste(&self) {
        let Some(promise) = read_clipboard_text() else {
            return;
        };
        let session = self.session.clone();
        let bridge = self.bridge.clone();
        spawn_local(async move {
            if let Ok(value) = JsFuture::from(promise).await
                && let Some(text) = value.as_string()
            {
                let _ = send_text(&session, &bridge, &text);
            }
        });
    }

    /// Whether the server accepts unicode keyboard shortcuts (always false for
    /// the RDP backend, surfaced for the UI).
    #[wasm_bindgen(js_name = supportsUnicodeKeyboardShortcuts)]
    #[must_use]
    pub fn supports_unicode_keyboard_shortcuts(&self) -> bool {
        call_method(&self.session, "supportsUnicodeKeyboardShortcuts", &[])
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

impl Client {
    /// Bundles `events` into one `InputTransaction` and applies it.
    fn send_events(&self, events: Vec<JsValue>) -> Result<(), JsValue> {
        let txn = construct(&self.bridge, "InputTransaction", &[])?;
        for event in events {
            // addEvent mutates the transaction in place and returns undefined.
            call_method(&txn, "addEvent", &[event])?;
        }
        call_method(&self.session, "applyInputs", &[txn])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bridge helpers
// ---------------------------------------------------------------------------

/// Wraps a JS callback that IronRDP invokes as `(kind, data, hotspotX, hotspotY)`.
fn cursor_callback(on_cursor: &JsValue) -> JsValue {
    let on_cursor = on_cursor.clone();
    let closure =
        Closure::<dyn FnMut(JsValue, JsValue, JsValue, JsValue)>::new(move |kind, data, x, y| {
            fire(&on_cursor, &[kind, data, x, y]);
        });
    closure.into_js_value()
}

/// Wraps a JS callback fed the remote `ClipboardData` whenever it changes.
fn clipboard_callback(on_text: &JsValue) -> JsValue {
    let on_text = on_text.clone();
    let closure = Closure::<dyn FnMut(JsValue)>::new(move |data| {
        if let Some(text) = clipboard_text(&data) {
            fire(&on_text, &[JsValue::from_str(&text)]);
        }
    });
    closure.into_js_value()
}

/// Wraps a JS callback fired when the server asks for a clipboard update.
fn force_clipboard_callback(on_request: &JsValue) -> JsValue {
    let on_request = on_request.clone();
    let closure = Closure::<dyn FnMut()>::new(move || {
        fire(&on_request, &[]);
    });
    closure.into_js_value()
}

/// Extracts the best text payload from a `ClipboardData` bridge object.
fn clipboard_text(data: &JsValue) -> Option<String> {
    let items = call_method(data, "items", &[])
        .ok()?
        .dyn_into::<Array>()
        .ok()?;
    let mut fallback = None;
    for item in items.iter() {
        let Ok(mime_val) = call_method(&item, "mimeType", &[]) else {
            continue;
        };
        let Some(mime) = mime_val.as_string() else {
            continue;
        };
        if mime == "text/plain" {
            if let Some(text) = call_method(&item, "value", &[])
                .ok()
                .and_then(|v| v.as_string())
            {
                return Some(text);
            }
        } else if mime == "text/html" && fallback.is_none() {
            fallback = call_method(&item, "value", &[])
                .ok()
                .and_then(|v| v.as_string());
        }
    }
    fallback
}

/// Builds a `ClipboardData`, adds `text`, and hands it to the session.
fn send_text(session: &JsValue, bridge: &JsValue, text: &str) -> Result<(), JsValue> {
    let data = construct(bridge, "ClipboardData", &[])?;
    call_method(
        &data,
        "addText",
        &[JsValue::from_str("text/plain"), JsValue::from_str(text)],
    )?;
    let promise =
        call_method(session, "onClipboardPaste", &[data])?.dyn_into::<js_sys::Promise>()?;
    spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
    Ok(())
}

/// Reads the local clipboard text as a promise, if the API is available.
///
/// `navigator.clipboard` is absent in insecure contexts, so the whole chain is
/// walked through `Reflect` rather than typed web-sys casts.
fn read_clipboard_text() -> Option<js_sys::Promise> {
    let window = web_sys::window()?;
    let navigator = JsValue::from_str("navigator");
    let nav = Reflect::get(&window.into(), &navigator).ok()?;
    let clipboard = Reflect::get(&nav, &JsValue::from_str("clipboard")).ok()?;
    if clipboard.is_null() || clipboard.is_undefined() {
        return None;
    }
    let read = Reflect::get(&clipboard, &JsValue::from_str("readText")).ok()?;
    let read = read.dyn_into::<js_sys::Function>().ok()?;
    read.call0(&clipboard)
        .ok()?
        .dyn_into::<js_sys::Promise>()
        .ok()
}

/// `new bridge[class](...args)`.
fn construct(ns: &JsValue, class: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let ctor = Reflect::get(ns, &JsValue::from_str(class))?;
    let ctor = ctor.dyn_into::<js_sys::Function>()?;
    Reflect::construct(&ctor, &array(args))
}

/// `bridge.DeviceEvent[factory](...args)` — a static factory method.
fn device_event(bridge: &JsValue, factory: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let class = Reflect::get(bridge, &JsValue::from_str("DeviceEvent"))?;
    let func = Reflect::get(&class, &JsValue::from_str(factory))?;
    let func = func.dyn_into::<js_sys::Function>()?;
    func.apply(&class, &array(args))
}

/// Fetches a callable property `name` from `obj`.
fn method(obj: &JsValue, name: &str) -> Result<js_sys::Function, JsValue> {
    let func = Reflect::get(obj, &JsValue::from_str(name))?;
    func.dyn_into::<js_sys::Function>()
        .map_err(|_| JsValue::from_str(&format!("{name} is not callable")))
}

/// `obj.name(...args)`.
fn call_method(obj: &JsValue, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let func = method(obj, name)?;
    func.apply(obj, &array(args))
}

/// Calls `cb(...args)` when `cb` is a function; otherwise a no-op.
fn fire(cb: &JsValue, args: &[JsValue]) {
    if let Ok(func) = cb.clone().dyn_into::<js_sys::Function>() {
        let _ = func.apply(&JsValue::UNDEFINED, &array(args));
    }
}

/// Builds a JS array from a slice.
fn array(args: &[JsValue]) -> Array {
    let arr = Array::new();
    for arg in args {
        arr.push(arg);
    }
    arr
}

/// Reads `SessionTerminationInfo.reason()`, with a stable fallback.
fn termination_reason(info: &JsValue) -> String {
    call_method(info, "reason", &[])
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "disconnected".to_owned())
}

/// Reads `IronError.kind()`, defaulting to [`KIND_GENERAL`].
fn error_kind_of(err: &JsValue) -> i32 {
    if let Ok(func) = method(err, "kind")
        && let Ok(value) = func.apply(err, &Array::new())
        && let Some(number) = value.as_f64()
    {
        return number as i32;
    }
    KIND_GENERAL
}

/// Turns a failed `connect()` into a short, actionable message.
fn connect_failure_message(err: &JsValue) -> JsValue {
    let kind = error_kind_of(err);
    let base = error::error_message(kind);
    let message = if kind == KIND_GENERAL {
        format!("{base}. Check the credentials, or sign in again if the session expired.")
    } else {
        base.to_owned()
    };
    JsValue::from_str(&message)
}

/// Reads `canvas.width`/`canvas.height`, with sane defaults.
fn canvas_size(canvas: &JsValue) -> (u16, u16) {
    let width = Reflect::get(canvas, &JsValue::from_str("width"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    let height = Reflect::get(canvas, &JsValue::from_str("height"))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(768.0);
    (clamp_u16(width), clamp_u16(height))
}

fn clamp_u16(value: f64) -> u16 {
    if value.is_finite() {
        value.clamp(0.0, f64::from(u16::MAX)) as u16
    } else {
        0
    }
}

fn clamp_u32(value: f64) -> u32 {
    if value.is_finite() {
        value.clamp(0.0, f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

fn clamp_i16(value: f64) -> i16 {
    if value.is_finite() {
        value.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_constants_match_iron_error_kind_order() {
        assert_eq!(KIND_GENERAL, 0);
        assert_eq!(KIND_WRONG_PASSWORD, 1);
        assert_eq!(KIND_LOGON_FAILURE, 2);
        assert_eq!(KIND_ACCESS_DENIED, 3);
        assert_eq!(KIND_RDCLEANPATH, 4);
        assert_eq!(KIND_PROXY_CONNECT, 5);
        assert_eq!(KIND_NEGOTIATION_FAILURE, 6);
    }

    #[test]
    fn connect_options_defaults_are_empty() {
        let options = ConnectOptions::default();
        assert!(options.ws_url.is_empty());
        assert!(options.username.is_empty());
        assert!(options.password.is_empty());
        assert!(options.destination.is_empty());
        assert!(options.token.is_empty());
    }

    #[test]
    fn clamps_saturate() {
        assert_eq!(clamp_u16(-5.0), 0);
        assert_eq!(clamp_u16(70000.0), u16::MAX);
        assert_eq!(clamp_i16(99999.0), i16::MAX);
        assert_eq!(clamp_u32(f64::NAN), 0);
    }
}
