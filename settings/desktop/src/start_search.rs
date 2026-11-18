//! Session-bus client for the file manager's search index.
use gio::prelude::*;
use glib::variant::ToVariant;
use serde::Deserialize;
use std::{cell::RefCell, rc::Rc, time::Duration};

const NAME: &str = "org.uflinux.Search1";
const PATH: &str = "/org/uflinux/Search";
const PAGE_SIZE: usize = 20;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub mime: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "reply")]
enum Reply {
    #[serde(rename = "search")]
    Search { hits: Vec<Hit> },
}

#[derive(Clone, Copy)]
enum Index {
    Apps,
    Files,
}

impl Index {
    const ALL: [Self; 2] = [Self::Apps, Self::Files];

    fn method(self) -> (&'static str, &'static str) {
        match self {
            Self::Apps => ("SearchApps", "search-apps"),
            Self::Files => ("SearchFiles", "search-files"),
        }
    }
}

#[derive(Default)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub pending: bool,
    pub unavailable: bool,
    call: Option<gio::Cancellable>,
}

#[derive(Default)]
struct Query {
    text: String,
    timer: Option<glib::SourceId>,
    apps: Results,
    files: Results,
}

impl Query {
    fn results(&mut self, index: Index) -> &mut Results {
        match index {
            Index::Apps => &mut self.apps,
            Index::Files => &mut self.files,
        }
    }
}

type Changed = Box<dyn Fn()>;

#[derive(Default)]
pub struct Search {
    query: RefCell<Query>,
    bus: RefCell<Option<gio::DBusConnection>>,
    changed: RefCell<Option<Changed>>,
    signals: RefCell<Option<gio::SignalSubscriptionId>>,
}

impl Search {
    pub fn on_changed(&self, callback: impl Fn() + 'static) {
        self.changed.replace(Some(Box::new(callback)));
    }

    fn notify(&self) {
        if let Some(changed) = self.changed.borrow().as_ref() {
            changed();
        }
    }

    pub fn results(&self, read: impl FnOnce(&Results, &Results)) {
        let query = self.query.borrow();
        read(&query.apps, &query.files);
    }

    pub fn set_text(self: &Rc<Self>, text: &str) {
        let text = text.trim();
        let previous = {
            let mut state = self.query.borrow_mut();
            if state.text == text {
                return;
            }
            std::mem::replace(
                &mut *state,
                Query {
                    text: text.to_owned(),
                    ..Query::default()
                },
            )
        };
        if let Some(timer) = previous.timer {
            timer.remove();
        }
        for call in [previous.apps.call, previous.files.call]
            .into_iter()
            .flatten()
        {
            call.cancel();
        }
        if text.is_empty() {
            return;
        }
        let mut state = self.query.borrow_mut();
        state.apps.pending = true;
        state.files.pending = true;
        let weak = Rc::downgrade(self);
        state.timer = Some(glib::timeout_add_local_once(
            Duration::from_millis(120),
            move || {
                if let Some(search) = weak.upgrade() {
                    search.query.borrow_mut().timer.take();
                    for index in Index::ALL {
                        search.request(index);
                    }
                }
            },
        ));
    }

    fn request(self: &Rc<Self>, index: Index) {
        let call = gio::Cancellable::new();
        let text = {
            let mut query = self.query.borrow_mut();
            if query.text.is_empty() || query.timer.is_some() {
                return;
            }
            let results = query.results(index);
            if let Some(previous) = results.call.replace(call.clone()) {
                previous.cancel();
            }
            query.text.clone()
        };
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let Some(search) = weak.upgrade() else { return };
            let bus = search.connection().await;
            if call.is_cancelled() {
                return;
            }
            let Ok(bus) = bus else {
                search.finish(index, &call, None);
                return;
            };
            let (method, tag) = index.method();
            let body = serde_json::json!({
                "method": tag, "text": text, "page_size": PAGE_SIZE,
            })
            .to_string();
            let weak = Rc::downgrade(&search);
            let token = call.clone();
            bus.call(
                Some(NAME),
                PATH,
                NAME,
                method,
                Some(&(body,).to_variant()),
                Some(glib::VariantTy::new("(s)").unwrap()),
                gio::DBusCallFlags::NONE,
                2000,
                Some(&call),
                move |reply| {
                    if let Some(search) = weak.upgrade() {
                        search.finish(index, &token, reply.ok().and_then(decode));
                    }
                },
            );
        });
    }

    async fn connection(self: &Rc<Self>) -> Result<gio::DBusConnection, glib::Error> {
        let existing = self.bus.borrow().clone().filter(|bus| !bus.is_closed());
        if let Some(bus) = existing {
            return Ok(bus);
        }
        let bus = gio::bus_get_future(gio::BusType::Session).await?;
        if self.bus.borrow().as_ref() != Some(&bus) {
            let weak = Rc::downgrade(self);
            if let (Some(previous), Some(id)) =
                (self.bus.borrow().as_ref(), self.signals.borrow_mut().take())
            {
                previous.signal_unsubscribe(id);
            }
            self.signals.replace(Some(bus.signal_subscribe(
                Some(NAME),
                Some(NAME),
                Some("Changed"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, parameters| {
                    #[derive(Deserialize)]
                    struct Changed {
                        kind: String,
                    }
                    let Some((body,)) = parameters.get::<(String,)>() else {
                        return;
                    };
                    let Ok(changed) = serde_json::from_str::<Changed>(&body) else {
                        return;
                    };
                    if let Some(search) = weak.upgrade() {
                        match changed.kind.as_str() {
                            "apps" => search.request(Index::Apps),
                            "files" => search.request(Index::Files),
                            _ => {}
                        }
                    }
                },
            )));
            self.bus.replace(Some(bus.clone()));
        }
        Ok(bus)
    }

    fn finish(&self, index: Index, token: &gio::Cancellable, hits: Option<Vec<Hit>>) {
        {
            let mut query = self.query.borrow_mut();
            let results = query.results(index);
            if results.call.as_ref() != Some(token) {
                return;
            }
            results.call.take();
            results.pending = false;
            results.unavailable = hits.is_none();
            results.hits = hits.unwrap_or_default();
        }
        self.notify();
    }
}

fn decode(reply: glib::Variant) -> Option<Vec<Hit>> {
    let (body,) = reply.get::<(String,)>()?;
    if body.len() > 2 * 1024 * 1024 {
        return None;
    }
    let Reply::Search { hits } = serde_json::from_str(&body).ok()?;
    Some(
        hits.into_iter()
            .take(PAGE_SIZE)
            .filter(|hit| std::path::Path::new(&hit.path).is_absolute())
            .collect(),
    )
}

impl Drop for Search {
    fn drop(&mut self) {
        if let (Some(bus), Some(id)) = (self.bus.get_mut().take(), self.signals.get_mut().take()) {
            bus.signal_unsubscribe(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_call_cannot_replace_the_current_group() {
        let search = Search::default();
        let old = gio::Cancellable::new();
        let current = gio::Cancellable::new();
        search.query.borrow_mut().files.call = Some(current.clone());
        search.query.borrow_mut().files.pending = true;
        search.finish(Index::Files, &old, None);
        search.results(|_, files| {
            assert!(files.pending);
            assert!(!files.unavailable);
        });

        let reply = (r#"{"reply":"search","hits":[{"path":"/home/test/report.txt","name":"report.txt","kind":"file"}]}"#,).to_variant();
        search.finish(Index::Files, &current, decode(reply));
        search.finish(Index::Files, &old, None);
        search.results(|_, files| {
            assert!(!files.pending);
            assert!(!files.unavailable);
            assert_eq!(files.hits[0].name, "report.txt");
        });
    }
}
