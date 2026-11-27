use super::*;
use std::{cell::RefCell, rc::Rc};

/// The Printing page is the only way to reach CUPS and SANE on UFLinux. Every
/// control sends the printing `Action` that owns the field it edits and is
/// redrawn from the published `Printing` block, so the daemon stays the single
/// writer and the page never has to guess whether cupsd actually came up.
pub fn page(app: &Rc<App>, parent: &gtk::Box) {
    let (status_group, body) = status(app);
    let printers = printer_section(app);
    let discovery = discovery_section(app);
    let scanners = scanner_section(app);
    parent.append(&status_group);
    for group in [&printers, &discovery, &scanners] {
        parent.append(group);
    }

    // Without the package there is nothing to switch on, and every section
    // below the headline would be an empty box.
    let sections = vec![
        body.upcast::<gtk::Widget>(),
        printers.upcast(),
        discovery.upcast(),
        scanners.upcast(),
    ];
    app.bind_for(Surface::Full, parent, move |state| {
        for widget in &sections {
            widget.set_visible(state.printing.installed);
        }
    });
}

/// Headline, the last helper error, the master switch and the service controls.
fn status(app: &Rc<App>) -> (gtk::Box, gtk::Box) {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Status");
    let headline = description("");
    group.append(&headline);
    let problem = note("");
    problem.set_visible(false);
    group.append(&problem);
    let target = headline.downgrade();
    let trouble = problem.downgrade();
    app.bind_for(Surface::Full, &group, move |state| {
        let printing = &state.printing;
        if let Some(label) = target.upgrade() {
            label.set_text(&if !printing.installed {
                "Printing and scanning are not installed on this system.".into()
            } else if !printing.enabled {
                "Printing is off. No print service is running and no port is open.".into()
            } else if !printing.running {
                "Printing is enabled at boot but the service is not running right now.".into()
            } else {
                let printers = match printing.queues.len() {
                    0 => "no printers are configured".to_string(),
                    1 => "1 printer is configured".to_string(),
                    count => format!("{count} printers are configured"),
                };
                format!(
                    "Printing is on, {printers}, {}.",
                    match printing.scanners.len() {
                        0 => "no scanners detected".to_string(),
                        1 => "1 scanner detected".to_string(),
                        count => format!("{count} scanners detected"),
                    }
                )
            });
        }
        if let Some(label) = trouble.upgrade() {
            label.set_text(&printing.error.clone().unwrap_or_default());
            label.set_visible(printing.error.is_some());
        }
    });

    let body = column();
    body.set_visible(false);
    group.append(&body);
    body.append(&description(
        "Turning this on starts the print service and network discovery, adds your user to the printer and scanner device groups, and shows the Document Scanner app.",
    ));
    toggle(
        app,
        Surface::Full,
        &body,
        "Enable printing",
        |s| s.printing.enabled,
        |on| Action::PrintingSystem {
            operation: if on {
                PrintingSystemAction::Enable
            } else {
                PrintingSystemAction::Disable
            },
        },
    );
    body.append(&description(
        "Device group membership and the Document Scanner shortcut take effect at the next sign-in.",
    ));

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    controls.set_halign(gtk::Align::Start);
    body.append(&controls);
    let start = button(app, "Start service", || Action::PrintingSystem {
        operation: PrintingSystemAction::Start,
    });
    let stop = button(app, "Stop service", || Action::PrintingSystem {
        operation: PrintingSystemAction::Stop,
    });
    controls.append(&start);
    controls.append(&stop);
    let start_ref = start.downgrade();
    let stop_ref = stop.downgrade();
    app.bind_for(Surface::Full, &controls, move |state| {
        if let Some(start) = start_ref.upgrade() {
            start.set_sensitive(state.printing.enabled && !state.printing.running);
        }
        if let Some(stop) = stop_ref.upgrade() {
            stop.set_sensitive(state.printing.running);
        }
    });
    (group, body)
}

/// Configured queues with their per-printer controls, plus the entry point for
/// adding a printer.
fn printer_section(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Printers");
    let list = column();
    group.append(&list);
    let last = Rc::new(RefCell::new(String::new()));
    let target = list.downgrade();
    let weak = Rc::downgrade(app);
    app.bind_for(Surface::Full, &list, move |state| {
        let signature = state
            .printing
            .queues
            .iter()
            .map(|queue| {
                format!(
                    "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                    queue.name,
                    queue.state.label(),
                    queue.uri,
                    queue.driver,
                    queue.kind.label(),
                    queue.is_default,
                    queue.accepting
                )
            })
            .collect::<Vec<_>>()
            .join("\u{2}");
        if *last.borrow() == signature {
            return;
        }
        last.replace(signature);
        let Some(list) = target.upgrade() else {
            return;
        };
        clear(&list);
        let Some(app) = weak.upgrade() else {
            return;
        };
        if state.printing.queues.is_empty() {
            list.append(&description("No printers are configured yet."));
        }
        for queue in &state.printing.queues {
            list.append(&printer_row(&app, queue));
        }
    });

    let add = gtk::Button::with_label(&tr("Add printer…"));
    add.set_halign(gtk::Align::Start);
    let weak = Rc::downgrade(app);
    add.connect_clicked(move |_| {
        if let Some(app) = weak.upgrade() {
            add_printer(&app, None);
        }
    });
    group.append(&add);
    group
}

/// One queue: what it is, how it is reached, and the verbs that make sense for
/// a queue. Pause and Resume are one button because a queue is either usable
/// or it is not.
fn printer_row(app: &Rc<App>, queue: &PrinterQueue) -> gtk::Box {
    let box_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_row.add_css_class("settings-row");
    let summary = column();
    summary.set_hexpand(true);
    let mut headline = format!("{} — {}", queue.name, queue.state.label());
    if queue.is_default {
        headline.push_str(" · default");
    }
    if !queue.accepting {
        headline.push_str(" · not accepting jobs");
    }
    summary.append(&description(&headline));
    let mut detail = queue.kind.label().to_string();
    for part in [&queue.driver, &queue.uri, &queue.detail] {
        if !part.is_empty() {
            detail.push_str(" · ");
            detail.push_str(part);
        }
    }
    summary.append(&muted(&detail));
    box_row.append(&summary);

    let usable = queue.state != PrinterState::Stopped && queue.accepting;
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_valign(gtk::Align::Center);
    box_row.append(&actions);

    let name = queue.name.clone();
    let weak = Rc::downgrade(app);
    let test = gtk::Button::with_label(&tr("Print test page"));
    test.set_sensitive(usable);
    let target = name.clone();
    test.connect_clicked(move |button| {
        if let Some(app) = weak.upgrade() {
            app.execute(
                Action::PrintingSystem {
                    operation: PrintingSystemAction::TestPage {
                        name: target.clone(),
                    },
                },
                Some(button.clone().upcast()),
            );
        }
    });
    actions.append(&test);

    let weak = Rc::downgrade(app);
    let pause_label = tr(if usable { "Pause" } else { "Resume" });
    let pause = gtk::Button::with_label(&pause_label);
    let target = name.clone();
    pause.connect_clicked(move |button| {
        if let Some(app) = weak.upgrade() {
            app.execute(
                Action::PrintingSystem {
                    operation: PrintingSystemAction::SetPrinterEnabled {
                        name: target.clone(),
                        enabled: !usable,
                    },
                },
                Some(button.clone().upcast()),
            );
        }
    });
    actions.append(&pause);

    if !queue.is_default {
        let weak = Rc::downgrade(app);
        let default = gtk::Button::with_label(&tr("Make default"));
        let target = name.clone();
        default.connect_clicked(move |button| {
            if let Some(app) = weak.upgrade() {
                app.execute(
                    Action::PrintingSystem {
                        operation: PrintingSystemAction::SetDefaultPrinter {
                            name: target.clone(),
                        },
                    },
                    Some(button.clone().upcast()),
                );
            }
        });
        actions.append(&default);
    }

    let weak = Rc::downgrade(app);
    let remove = gtk::Button::with_label(&tr("Remove"));
    let target = name;
    remove.connect_clicked(move |button| {
        if let Some(app) = weak.upgrade() {
            app.execute(
                Action::PrintingSystem {
                    operation: PrintingSystemAction::RemovePrinter {
                        name: target.clone(),
                    },
                },
                Some(button.clone().upcast()),
            );
        }
    });
    actions.append(&remove);
    box_row
}

/// Sharing, and the devices that were found but have no queue yet.
fn discovery_section(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Sharing and discovery");
    group.append(&description(
        "Discovery finds network printers advertised on the local network. Sharing offers your printers to other machines on it.",
    ));
    let state = description("");
    group.append(&state);
    let target = state.downgrade();
    app.bind_for(Surface::Full, &group, move |state| {
        if let Some(label) = target.upgrade() {
            label.set_text(&format!(
                "Network discovery is {}. Printers are {}.",
                if state.printing.discovery {
                    "running"
                } else {
                    "not running"
                },
                if state.printing.shared {
                    "shared"
                } else {
                    "not shared"
                }
            ));
        }
    });
    toggle(
        app,
        Surface::Full,
        &group,
        "Share printers on this network",
        |s| s.printing.shared,
        |shared| Action::PrintingSystem {
            operation: PrintingSystemAction::SetShared { shared },
        },
    );

    let list = column();
    group.append(&list);
    let last = Rc::new(RefCell::new(String::new()));
    let target = list.downgrade();
    let weak = Rc::downgrade(app);
    app.bind_for(Surface::Full, &list, move |state| {
        let signature = state
            .printing
            .devices
            .iter()
            .map(|device| format!("{}\u{1}{}", device.uri, device.description))
            .collect::<Vec<_>>()
            .join("\u{2}");
        if *last.borrow() == signature {
            return;
        }
        last.replace(signature);
        let Some(list) = target.upgrade() else {
            return;
        };
        clear(&list);
        if state.printing.devices.is_empty() {
            list.append(&description("No unconfigured devices were found."));
            return;
        }
        let Some(app) = weak.upgrade() else {
            return;
        };
        for device in &state.printing.devices {
            list.append(&discovered_row(&app, device));
        }
    });
    group
}

fn discovered_row(app: &Rc<App>, device: &DiscoveredDevice) -> gtk::Box {
    let box_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_row.add_css_class("settings-row");
    let summary = column();
    summary.set_hexpand(true);
    summary.append(&description(&format!(
        "{} · {}",
        device.description,
        device.kind.label()
    )));
    summary.append(&muted(&device.uri));
    box_row.append(&summary);
    let add = gtk::Button::with_label(&tr("Add…"));
    let weak = Rc::downgrade(app);
    let device = device.clone();
    add.connect_clicked(move |_| {
        if let Some(app) = weak.upgrade() {
            add_printer(&app, Some(&device));
        }
    });
    box_row.append(&add);
    box_row
}

/// The scanner list. A scanner needs no queue, so this section reports what
/// SANE found and points at the application that uses it.
fn scanner_section(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Scanners");
    group.append(&description(
        "Document Scanner opens any device listed here. A USB scanner has to be connected before the service starts.",
    ));
    let list = column();
    group.append(&list);
    let last = Rc::new(RefCell::new(String::new()));
    let target = list.downgrade();
    app.bind_for(Surface::Full, &list, move |state| {
        let signature = state
            .printing
            .scanners
            .iter()
            .map(|scanner| format!("{}\u{1}{}", scanner.name, scanner.title()))
            .collect::<Vec<_>>()
            .join("\u{2}");
        if *last.borrow() == signature {
            return;
        }
        last.replace(signature);
        let Some(list) = target.upgrade() else {
            return;
        };
        clear(&list);
        if state.printing.scanners.is_empty() {
            list.append(&description("No scanners were detected."));
            return;
        }
        for scanner in &state.printing.scanners {
            let line = column();
            line.append(&description(&format!("{} · {}", scanner.title(), scanner.kind.label())));
            line.append(&muted(&scanner.name));
            list.append(&line);
        }
    });
    let open = gtk::Button::with_label(&tr("Open Document Scanner"));
    open.set_halign(gtk::Align::Start);
    open.connect_clicked(|_| {
        // simple-scan is a session service of its own; the page must not wait
        // for it, and a missing package is not worth an error banner.
        let _ = std::process::Command::new("simple-scan").spawn();
    });
    group.append(&open);
    group
}

/// Adding a printer is the one operation with more than two fields, so it gets
/// its own dialog. The address stays editable because a user can always type an
/// IPP URL that discovery has not seen yet.
fn add_printer(app: &Rc<App>, device: Option<&DiscoveredDevice>) {
    let (window, parent) = dialog(app, "Add printer");
    parent.append(&description(
        "Pick a discovered device or type an address such as ipp://192.0.2.10/ipp/print. Leave the driver empty to use the printer's built-in driverless support.",
    ));
    let name = entry(&parent, "Printer name", "");
    let uri = entry(&parent, "Address", "");
    let driver = entry(&parent, "Driver (optional)", "");
    if let Some(device) = device {
        name.set_text(&suggested_name(&device.description));
        uri.set_text(&device.uri);
    }
    let devices = app.state.borrow().printing.devices.clone();
    if devices.len() > 1 {
        // Index 0 is a no-op entry so that opening the dialog from a device row
        // does not immediately overwrite the address that row provided.
        let texts = devices
            .iter()
            .map(|device| format!("{} — {}", device.description, device.uri))
            .collect::<Vec<_>>();
        let mut labels = vec!["Another discovered device…"];
        labels.extend(texts.iter().map(String::as_str));
        let pick = gtk::DropDown::from_strings(&labels);
        parent.append(&row("Another discovered device", &pick));
        let address = uri.clone();
        let field = name.clone();
        pick.connect_selected_notify(move |pick| {
            let index = pick.selected();
            if index == 0 {
                return;
            }
            let Some(picked) = devices.get(index as usize - 1) else {
                return;
            };
            address.set_text(&picked.uri);
            field.set_text(&suggested_name(&picked.description));
        });
    }
    let trouble = note("");
    trouble.set_visible(false);
    parent.append(&trouble);
    let weak = Rc::downgrade(app);
    let window_for_click = window.clone();
    let name_field = name.clone();
    let uri_field = uri.clone();
    let driver_field = driver.clone();
    let trouble_ref = trouble.downgrade();
    let add = gtk::Button::with_label(&tr("Add printer"));
    add.set_halign(gtk::Align::Start);
    add.connect_clicked(move |button| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let queue = name_field.text().trim().to_string();
        let address = uri_field.text().trim().to_string();
        if let Some(text) = problem_with(&queue, &address) {
            if let Some(label) = trouble_ref.upgrade() {
                label.set_text(&text);
                label.set_visible(true);
            }
            return;
        }
        app.execute(
            Action::PrintingSystem {
                operation: PrintingSystemAction::AddPrinter {
                    name: queue,
                    uri: address,
                    driver: driver_field.text().trim().to_string(),
                },
            },
            Some(button.clone().upcast()),
        );
        window_for_click.close();
    });
    parent.append(&add);
    name.grab_focus();
    window.present();
}

/// Mirrors the helper's own checks so a mistake is visible before the write is
/// refused.
fn problem_with(name: &str, uri: &str) -> Option<String> {
    if name.is_empty() {
        return Some("A printer name is required".into());
    }
    if name.len() > 127
        || !name.starts_with(|first: char| first.is_ascii_alphanumeric())
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Some(
            "Use 1 to 127 letters, digits, dot, underscore or hyphen, starting with a letter or digit".into(),
        );
    }
    if uri.is_empty() {
        return Some("An address is required".into());
    }
    if uri.contains(char::is_whitespace) {
        return Some("The address cannot contain spaces".into());
    }
    let scheme = uri.split(':').next().unwrap_or("");
    if !matches!(
        scheme,
        "ipp" | "ipps"
            | "https"
            | "http"
            | "socket"
            | "lpd"
            | "smb"
            | "dnssd"
            | "beh"
            | "usb"
            | "hp"
            | "hpnp"
            | "hpusb"
            | "usbhp"
            | "direct"
            | "ippusb"
    ) {
        return Some(
            "The address must start with ipp://, https://, socket://, lpd://, smb://, dnssd:// or hp:".into(),
        );
    }
    None
}

/// Turns "EPSON WorkForce Pro WF-4820" into "WorkForce-Pro-WF-4820", the name a
/// user would otherwise have to type themselves.
fn suggested_name(description: &str) -> String {
    let mut out = String::new();
    for byte in description.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte as char);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    let mut out = out.chars().take(63).collect::<String>();
    if out.is_empty() {
        out.push_str("Printer");
    }
    out
}

/// Secondary text inside a row: same look as a description, but not translated,
/// because it is always data the machine produced.
fn muted(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.);
    label.add_css_class("description");
    label
}

fn note(text: &str) -> gtk::Label {
    let label = muted(text);
    label.remove_css_class("description");
    label.add_css_class("error");
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_mirrors_the_helper_name_and_address_rules() {
        assert!(problem_with("Office_Jet.1", "ipp://192.0.2.1/ipp/print").is_none());
        assert!(problem_with("", "ipp://h/p").is_some());
        assert!(problem_with("-Office", "ipp://h/p").is_some());
        assert!(problem_with("Office Jet", "ipp://h/p").is_some());
        assert!(problem_with("Office", "").is_some());
        assert!(problem_with("Office", "file:/etc/shadow").is_some());
        assert!(problem_with("Office", "ipp://host print").is_some());
        assert!(problem_with("Office", "dnssd://x").is_none());
    }

    #[test]
    fn a_description_becomes_a_usable_queue_name() {
        assert_eq!(
            suggested_name("EPSON WorkForce Pro WF-4820"),
            "EPSON-WorkForce-Pro-WF-4820"
        );
        assert_eq!(suggested_name("  --  "), "Printer");
        assert_eq!(suggested_name("").len(), 7);
        assert_eq!(suggested_name("HP LaserJet 1020").len(), 16);
    }
}
