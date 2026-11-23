//! DOM controller for the remote desktop page.

use super::{Client, ConnectOptions, connect, init};
use js_sys::{JSON, Reflect};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{
    AddEventListenerOptions, ClipboardEvent, Document, Element, Event, EventTarget,
    HtmlCanvasElement, KeyboardEvent, PointerEvent, ResizeObserver, WheelEvent, Window,
};

type Timer = RefCell<Option<(i32, Closure<dyn FnMut()>)>>;

struct Console {
    window: Window,
    document: Document,
    canvas: HtmlCanvasElement,
    screen: Element,
    client: RefCell<Option<Client>>,
    local_text: RefCell<String>,
    remote_text: RefCell<String>,
    resize_timer: Timer,
    toast_timer: Timer,
}

#[wasm_bindgen(js_name = startConsole)]
pub async fn start_console(bridge: JsValue) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("Missing browser window")?;
    let document = window.document().ok_or("Missing session document")?;
    let canvas = document
        .get_element_by_id("screen")
        .ok_or("Missing session canvas")?
        .dyn_into::<HtmlCanvasElement>()?;
    let screen = document
        .get_element_by_id("screen-wrap")
        .ok_or("Missing session display")?;
    let console = Rc::new(Console {
        window,
        document,
        canvas,
        screen,
        client: RefCell::new(None),
        local_text: RefCell::default(),
        remote_text: RefCell::default(),
        resize_timer: Timer::default(),
        toast_timer: Timer::default(),
    });
    console.bind()?;
    if let Err(error) = console.connect(bridge).await {
        console.disconnect(
            &error
                .as_string()
                .unwrap_or_else(|| "Could not start the RDP session.".into()),
        );
    }
    Ok(())
}

impl Console {
    fn text(&self, id: &str, message: &str) {
        if let Some(element) = self.document.get_element_by_id(id) {
            element.set_text_content(Some(message));
        }
    }

    fn state(&self, state: &str, label: &str) {
        if let Some(pill) = self.document.get_element_by_id("state") {
            let _ = pill.set_attribute("data-state", state);
            pill.set_text_content(Some(label));
        }
    }

    fn disconnect(&self, reason: &str) {
        self.client.borrow_mut().take();
        self.state("error", "disconnected");
        self.text(
            "overlay-reason",
            if reason.is_empty() {
                "Connection closed."
            } else {
                reason
            },
        );
        if let Some(overlay) = self.document.get_element_by_id("overlay") {
            let _ = overlay.remove_attribute("hidden");
        }
    }

    fn notify(&self, message: &str) {
        let Some(toast) = self.document.get_element_by_id("toast") else {
            return;
        };
        toast.set_text_content(Some(message));
        let _ = toast.class_list().add_1("show");
        self.timeout(&self.toast_timer, 1800, move || {
            let _ = toast.class_list().remove_1("show");
        });
    }

    fn timeout(&self, timer: &Timer, delay: i32, callback: impl FnOnce() + 'static) {
        if let Some((handle, _callback)) = timer.borrow_mut().take() {
            self.window.clear_timeout_with_handle(handle);
        }
        let callback = Closure::once(callback);
        if let Ok(handle) = self
            .window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                delay,
            )
        {
            timer.replace(Some((handle, callback)));
        }
    }

    fn viewport(&self) -> (u32, u32) {
        let rect = self.canvas.get_bounding_client_rect();
        let ratio = self.window.device_pixel_ratio();
        let width = ((rect.width() * ratio).round() as u32).clamp(200, 4096) & !1;
        let height = ((rect.height() * ratio).round() as u32).clamp(200, 4096);
        (width, height.min(3840 * 2400 / width) & !1)
    }

    fn point(&self, event: &PointerEvent) -> (f64, f64) {
        let rect = self.canvas.get_bounding_client_rect();
        if rect.width() == 0.0 || rect.height() == 0.0 {
            return (0.0, 0.0);
        }
        (
            (f64::from(event.client_x()) - rect.left()) * f64::from(self.canvas.width())
                / rect.width(),
            (f64::from(event.client_y()) - rect.top()) * f64::from(self.canvas.height())
                / rect.height(),
        )
    }

    fn resize(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.timeout(&self.resize_timer, 300, move || {
            if let Some(console) = weak.upgrade() {
                let (width, height) = console.viewport();
                if (width, height) != (console.canvas.width(), console.canvas.height())
                    && let Some(client) = console.client.borrow().as_ref()
                {
                    client.resize(width.into(), height.into());
                }
            }
        });
    }

    async fn connect(self: &Rc<Self>, bridge: JsValue) -> Result<(), JsValue> {
        let storage = self
            .window
            .session_storage()?
            .ok_or("Session storage is unavailable.")?;
        let stored = storage
            .get_item("uflinux-rds")?
            .ok_or("No active sign-in. Open this page from the login screen.")?;
        storage.remove_item("uflinux-rds")?;
        let sign_in =
            JSON::parse(&stored).map_err(|_| "Stored sign-in is unreadable. Sign in again.")?;
        let field = |name| {
            Reflect::get(&sign_in, &JsValue::from_str(name))
                .ok()
                .and_then(|value| value.as_string())
                .unwrap_or_default()
        };

        init("warn");
        let (width, height) = self.viewport();
        self.canvas.set_width(width);
        self.canvas.set_height(height);
        self.text("status", "Connecting…");

        let weak = Rc::downgrade(self);
        let disconnected = Closure::<dyn FnMut(String)>::new(move |reason: String| {
            if let Some(console) = weak.upgrade() {
                console.disconnect(&reason);
            }
        });
        let weak = Rc::downgrade(self);
        let error = Closure::<dyn FnMut(i32, String)>::new(move |_, message: String| {
            if let Some(console) = weak.upgrade() {
                console.text("status", &message);
            }
        });
        let canvas = self.canvas.clone();
        let cursor = Closure::<dyn FnMut(String, JsValue, f64, f64)>::new(
            move |kind: String, data: JsValue, x: f64, y: f64| {
                let css = match (kind.as_str(), data.as_string()) {
                    ("hidden", _) => "none".to_owned(),
                    ("url", Some(url)) => {
                        let x = if x.is_finite() { x } else { 0.0 };
                        let y = if y.is_finite() { y } else { 0.0 };
                        format!("url(\"{url}\") {x} {y}, default")
                    }
                    _ => "default".to_owned(),
                };
                let _ = canvas.style().set_property("cursor", &css);
            },
        );
        let weak = Rc::downgrade(self);
        let clipboard = Closure::<dyn FnMut(String)>::new(move |text: String| {
            if let Some(console) = weak.upgrade() {
                console.remote_text.replace(text);
                console.notify("remote clipboard ready — press Ctrl+C to keep it");
            }
        });
        let weak = Rc::downgrade(self);
        let clipboard_request = Closure::<dyn FnMut()>::new(move || {
            if let Some(console) = weak.upgrade()
                && let Some(client) = console.client.borrow().as_ref()
                && !console.local_text.borrow().is_empty()
            {
                let _ = client.clipboard_send_text(&console.local_text.borrow());
            }
        });
        let options = ConnectOptions {
            ws_url: field("ws_url"),
            token: field("token"),
            username: field("username"),
            password: field("password"),
            destination: field("destination"),
            bridge,
            canvas: self.canvas.clone().into(),
            on_disconnected: disconnected.into_js_value(),
            on_error: error.into_js_value(),
            on_pointer_cursor: cursor.into_js_value(),
            on_clipboard_text: clipboard.into_js_value(),
            on_clipboard_request: clipboard_request.into_js_value(),
        };
        let mut client = connect(options).await?;
        client
            .start()
            .map_err(|_| "The session ended before it started.")?;
        self.client.replace(Some(client));
        self.state("connected", "connected");
        self.text("status", "");
        let _ = self.canvas.focus();
        self.resize();
        Ok(())
    }

    fn listen<E: JsCast + 'static>(
        self: &Rc<Self>,
        target: &EventTarget,
        name: &str,
        handle: impl Fn(&Rc<Self>, E) + 'static,
    ) -> Result<(), JsValue> {
        let console = Rc::clone(self);
        let callback = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            if let Ok(event) = event.dyn_into::<E>() {
                handle(&console, event);
            }
        });
        let options = AddEventListenerOptions::new();
        options.set_passive(false);
        target.add_event_listener_with_callback_and_add_event_listener_options(
            name,
            callback.as_ref().unchecked_ref(),
            &options,
        )?;
        callback.forget(); // Page-lifetime DOM listener.
        Ok(())
    }

    fn button(
        self: &Rc<Self>,
        id: &str,
        handle: impl Fn(&Rc<Self>) + 'static,
    ) -> Result<(), JsValue> {
        if let Some(button) = self.document.get_element_by_id(id) {
            self.listen(&button, "click", move |console, _: Event| handle(console))?;
        }
        Ok(())
    }

    fn bind(self: &Rc<Self>) -> Result<(), JsValue> {
        self.listen(&self.canvas, "pointermove", |c, event: PointerEvent| {
            if let Some(client) = c.client.borrow().as_ref() {
                let (x, y) = c.point(&event);
                let _ = client.pointer_move(x, y, event.buttons());
            }
        })?;
        for name in ["pointerdown", "pointerup", "pointercancel"] {
            self.listen(&self.canvas, name, move |c, event: PointerEvent| {
                if let Some(client) = c.client.borrow().as_ref() {
                    let pressed = name == "pointerdown";
                    if pressed {
                        event.prevent_default();
                        let _ = c.canvas.focus();
                    }
                    let (x, y) = c.point(&event);
                    let _ = client.button(pressed, event.button().into(), x, y);
                }
            })?;
        }
        self.listen(&self.canvas, "wheel", |c, event: WheelEvent| {
            if let Some(client) = c.client.borrow().as_ref() {
                event.prevent_default();
                let _ = client.wheel(event.delta_y(), event.delta_x(), event.delta_mode());
            }
        })?;
        self.listen(&self.canvas, "contextmenu", |_, event: Event| {
            event.prevent_default()
        })?;
        for (name, pressed) in [("keydown", true), ("keyup", false)] {
            self.listen(&self.document, name, move |c, event: KeyboardEvent| {
                if pressed
                    && event
                        .target()
                        .and_then(|target| target.dyn_into::<Element>().ok())
                        .is_some_and(|element| {
                            matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "BUTTON")
                        })
                {
                    return;
                }
                if let Some(client) = c.client.borrow().as_ref()
                    && client
                        .key(
                            &event.code(),
                            pressed,
                            event.get_modifier_state("ScrollLock"),
                            event.get_modifier_state("NumLock"),
                            event.get_modifier_state("CapsLock"),
                            event.get_modifier_state("KanaLock"),
                        )
                        .unwrap_or(false)
                {
                    event.prevent_default();
                }
            })?;
        }
        self.listen(&self.window, "blur", |c, _: Event| {
            if let Some(client) = c.client.borrow().as_ref() {
                let _ = client.release_all_inputs();
            }
        })?;
        self.listen(&self.document, "paste", |c, event: ClipboardEvent| {
            if let Some(client) = c.client.borrow().as_ref()
                && let Some(clipboard) = event.clipboard_data()
                && let Ok(text) = clipboard.get_data("text/plain")
                && !text.is_empty()
                && client.clipboard_send_text(&text).is_ok()
            {
                event.prevent_default();
                c.local_text.replace(text);
                c.notify("clipboard sent to session");
            }
        })?;
        self.listen(&self.document, "copy", |c, event: ClipboardEvent| {
            let text = c.remote_text.borrow();
            if c.client.borrow().is_some()
                && !text.is_empty()
                && let Some(clipboard) = event.clipboard_data()
                && clipboard.set_data("text/plain", &text).is_ok()
            {
                event.prevent_default();
            }
        })?;
        self.button("paste", |c| {
            if let Some(client) = c.client.borrow().as_ref() {
                client.clipboard_paste();
            }
        })?;
        self.button("fullscreen", |c| {
            if c.document.fullscreen_element().is_some() {
                c.document.exit_fullscreen();
            } else {
                let _ = c.screen.request_fullscreen();
            }
        })?;
        self.button("send-break", |c| {
            if let Some(client) = c.client.borrow().as_ref() {
                for (key, pressed) in [
                    ("ControlLeft", true),
                    ("AltLeft", true),
                    ("Delete", true),
                    ("Delete", false),
                    ("AltLeft", false),
                    ("ControlLeft", false),
                ] {
                    let _ = client.key(key, pressed, false, false, false, false);
                }
                c.notify("ctrl-alt-del sent");
            }
        })?;
        self.button("disconnect", |c| {
            if let Some(client) = c.client.borrow().as_ref() {
                client.stop();
            }
        })?;
        self.button("reconnect", |c| {
            let _ = c.window.location().assign("/");
        })?;
        self.listen(&self.window, "resize", |c, _: Event| c.resize())?;
        self.listen(&self.document, "fullscreenchange", |c, _: Event| c.resize())?;
        let console = Rc::clone(self);
        let resize = Closure::<dyn FnMut()>::new(move || console.resize());
        if let Ok(observer) = ResizeObserver::new(resize.as_ref().unchecked_ref()) {
            observer.observe(&self.screen);
            // Keep the observer and callback alive as long as the page.
            std::mem::forget((observer, resize));
        }
        Ok(())
    }
}
