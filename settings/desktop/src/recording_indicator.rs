use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;

use crate::recording_bus;

pub struct Indicator {
    button: gtk::Button,
    menu: gtk::PopoverMenu,
    watch: Option<gio::WatcherId>,
    timer: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Indicator {
    pub fn new() -> Self {
        let button = gtk::Button::builder()
            .visible(false)
            .tooltip_text("Stop recording")
            .action_name("recording.stop")
            .css_classes(["uflinux-recording"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label("Stop recording")]);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        row.append(&gtk::Image::from_icon_name("media-playback-stop-symbolic"));
        let elapsed = gtk::Label::new(Some("0:00"));
        row.append(&elapsed);
        button.set_child(Some(&row));

        let actions = gio::Menu::new();
        actions.append(Some("Pause recording"), Some("recording.pause"));
        actions.append(Some("Stop recording"), Some("recording.stop"));
        let menu = gtk::PopoverMenu::from_model(Some(&actions));
        menu.set_parent(&button);
        let click = gtk::GestureClick::new();
        click.set_button(3);
        let popup = menu.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(menu) = popup.upgrade() {
                menu.popup();
            }
        });
        button.add_controller(click);

        let timer = Rc::new(RefCell::new(None));
        let watch = gio::bus_watch_name(
            gio::BusType::Session,
            recording_bus::NAME,
            gio::BusNameWatcherFlags::NONE,
            {
                let button = button.downgrade();
                let elapsed = elapsed.downgrade();
                let timer = timer.clone();
                move |bus, _, owner| {
                    let Some(button) = button.upgrade() else {
                        return;
                    };
                    let actions = gio::DBusActionGroup::get(&bus, Some(owner), recording_bus::PATH);
                    actions.connect_action_added(Some("stop"), {
                        let weak = button.downgrade();
                        let elapsed = elapsed.clone();
                        let timer = timer.clone();
                        move |actions, _| {
                            let Some(button) = weak.upgrade() else { return };
                            button.set_visible(true);
                            let started = actions
                                .action_state("started")
                                .and_then(|state| state.get::<i64>())
                                .unwrap_or_else(glib::monotonic_time);
                            stop_timer(&timer);
                            let elapsed = elapsed.clone();
                            let update = move || {
                                let Some(elapsed) = elapsed.upgrade() else {
                                    return;
                                };
                                let seconds = (glib::monotonic_time().saturating_sub(started)
                                    / 1_000_000)
                                    .max(0);
                                elapsed.set_text(&format!("{}:{:02}", seconds / 60, seconds % 60));
                            };
                            update();
                            timer.replace(Some(glib::timeout_add_local(
                                Duration::from_secs(1),
                                move || {
                                    update();
                                    glib::ControlFlow::Continue
                                },
                            )));
                        }
                    });
                    button.insert_action_group("recording", Some(&actions));
                    // Querying the group starts asynchronous discovery.
                    actions.list_actions();
                }
            },
            {
                let button = button.downgrade();
                let menu = menu.downgrade();
                let timer = timer.clone();
                move |_, _| {
                    stop_timer(&timer);
                    if let Some(menu) = menu.upgrade() {
                        menu.popdown();
                    }
                    if let Some(button) = button.upgrade() {
                        button.set_visible(false);
                        button.insert_action_group("recording", gio::ActionGroup::NONE);
                    }
                }
            },
        );
        Self {
            button,
            menu,
            watch: Some(watch),
            timer,
        }
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }
}

fn stop_timer(timer: &RefCell<Option<glib::SourceId>>) {
    if let Some(timer) = timer.take() {
        timer.remove();
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        if let Some(watch) = self.watch.take() {
            gio::bus_unwatch_name(watch);
        }
        stop_timer(&self.timer);
        self.menu.popdown();
        self.menu.unparent();
    }
}
