use super::*;
use settings_core::bus;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

/// The Remote Desktop page mirrors the uflinux-rds broker. Every control sends the
/// `Action` that owns the field it edits and is redrawn from the published `Rdp`
/// block, so the daemon stays the single writer of the broker configuration. The
/// codec controls share one draft because `SetRdpCodecs` always writes the whole
/// policy at once.
pub fn page(app: &Rc<App>, parent: &gtk::Box) {
    let (status_group, body) = status(app);
    let connectivity = connectivity(app);
    let certificate = certificate(app);
    let codecs = codec_section(app);
    let sessions = session_section(app);
    parent.append(&status_group);
    for group in [&connectivity, &certificate, &codecs, &sessions] {
        parent.append(group);
    }

    // Everything except the headline is meaningless without the package.
    let sections = vec![connectivity, certificate, codecs, sessions];
    app.bind_for(Surface::Full, parent, move |state| {
        body.set_visible(state.rdp.installed);
        for group in &sections {
            group.set_visible(state.rdp.installed);
        }
    });
}

/// Headline, the "not installed" explanation and the last broker error. The second
/// column holds the service switch and is hidden together with the other sections.
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
        let rdp = &state.rdp;
        if let Some(label) = target.upgrade() {
            label.set_text(&if !rdp.installed {
                "The Remote Desktop service is not installed on this system.".into()
            } else if rdp.enabled {
                format!(
                    "Remote Desktop is on, accepting connections on port {}.",
                    rdp.rdp_port
                )
            } else {
                "Remote Desktop is off.".into()
            });
        }
        if let Some(label) = trouble.upgrade() {
            label.set_text(&rdp.error.clone().unwrap_or_default());
            label.set_visible(rdp.error.is_some());
        }
    });

    let body = column();
    body.set_visible(false);
    group.append(&body);
    body.append(&description(
        "Anyone allowed by the system can connect to this machine's desktop over the network.",
    ));
    toggle(
        app,
        Surface::Full,
        &body,
        "Share this desktop",
        |s| s.rdp.enabled,
        |on| Action::RdpSystem {
            operation: if on {
                RdpSystemAction::Enable
            } else {
                RdpSystemAction::Disable
            },
        },
    );
    body.append(&description(
        "Turning this on enables the service at boot and starts it now.",
    ));
    (group, body)
}

/// RDP port, browser client, its port, and HTTPS.
fn connectivity(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Connectivity");
    group.append(&description(
        "Both ports are reachable from every network this machine is joined to, so a non-standard port only hides the service.",
    ));
    let warning = note("");
    warning.set_visible(false);
    group.append(&warning);

    let (rdp_port, _) = port_field(
        &group,
        "RDP port",
        &app.state.borrow().rdp.rdp_port.to_string(),
    );
    let (web_port, web_row) = port_field(
        &group,
        "Browser client port",
        &app.state.borrow().rdp.web_port.to_string(),
    );

    // Inline validation mirrors the daemon, so a mistake is visible before the
    // write is refused.
    let weak = Rc::downgrade(app);
    let live = warning.downgrade();
    let rdp_text = rdp_port.clone();
    let web_text = web_port.clone();
    rdp_port.connect_changed(move |_| {
        check_ports(&weak, &live, &rdp_text, &web_text);
    });
    let weak = Rc::downgrade(app);
    let live = warning.downgrade();
    let rdp_text = rdp_port.clone();
    let web_text = web_port.clone();
    web_port.connect_changed(move |_| {
        check_ports(&weak, &live, &rdp_text, &web_text);
    });

    let apply = gtk::Button::with_label(&tr("Apply port settings"));
    apply.set_halign(gtk::Align::Start);
    let weak = Rc::downgrade(app);
    let trouble = warning.downgrade();
    let rdp_entry = rdp_port.clone();
    let web_entry = web_port.clone();
    apply.connect_clicked(move |button| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let (Some(rdp), Some(web)) = (parse_port(&rdp_entry.text()), parse_port(&web_entry.text()))
        else {
            show_warning(&trouble, "A port between 1 and 65535 is required");
            return;
        };
        let problem = {
            let state = app.state.borrow();
            port_pair_problem(rdp, web, state.rdp.web_enabled, state.rdp.https)
        };
        match problem {
            Some(text) => show_warning(&trouble, &text),
            None => {
                hide_warning(&trouble);
                app.execute(
                    Action::SetRdpPorts {
                        rdp_port: rdp,
                        web_port: web,
                    },
                    Some(button.clone().upcast()),
                );
            }
        }
    });
    group.append(&apply);

    // `SetRdpWeb` carries the port as well, so the toggle sends whatever is in the
    // field rather than resetting it to the published value.
    let published = Rc::downgrade(app);
    let web_entry = web_port.clone();
    toggle(
        app,
        Surface::Full,
        &group,
        "Browser client",
        |s| s.rdp.web_enabled,
        move |enabled| {
            let current = published
                .upgrade()
                .map(|app| app.state.borrow().rdp.web_port);
            Action::SetRdpWeb {
                enabled,
                port: parse_port(&web_entry.text())
                    .or(current)
                    .unwrap_or(DEFAULT_RDP_WEB_PORT),
            }
        },
    );
    group.append(&description(
        "The browser client is reached over HTTPS and never uses H.264, so it always falls back to QOI or RemoteFX.",
    ));
    toggle(
        app,
        Surface::Full,
        &group,
        "HTTPS for the browser client",
        |s| s.rdp.https,
        |enabled| Action::SetRdpHttps { enabled },
    );
    group.append(&description(
        "Turning HTTPS off serves the browser client over plain HTTP, which sends the password unencrypted.",
    ));

    // Both entries follow the daemon unless the user is mid-edit, and the web port
    // only matters while the browser client is on.
    let rdp_target = rdp_port.downgrade();
    let web_target = web_port.downgrade();
    let row_target = web_row.downgrade();
    app.bind_for(Surface::Full, &group, move |state| {
        if let Some(entry) = rdp_target.upgrade()
            && !entry.has_focus()
        {
            entry.set_text(&state.rdp.rdp_port.to_string());
        }
        if let Some(entry) = web_target.upgrade()
            && !entry.has_focus()
        {
            entry.set_text(&state.rdp.web_port.to_string());
        }
        if let Some(row) = row_target.upgrade() {
            row.set_sensitive(state.rdp.web_enabled);
        }
    });
    group
}

/// Certificate source, the two custom paths, and the privileged apply step.
fn certificate(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Certificate");
    group.append(&description(
        "The certificate is what browser clients see when they connect. A self-signed certificate shows a warning the first time each browser is used.",
    ));
    let modes = [
        ("self-signed", CertificateMode::SelfSigned.label()),
        ("custom", CertificateMode::Custom.label()),
    ];
    let drop = choose(
        &group,
        "Certificate",
        &modes,
        mode_id(&app.state.borrow().rdp.certificate_mode),
    );

    let (certificate, cert_row) = path_field(
        app,
        &group,
        "Certificate file",
        "Choose certificate",
        app.state
            .borrow()
            .rdp
            .custom_certificate_path
            .as_deref()
            .unwrap_or(""),
    );
    let (key, key_row) = path_field(
        app,
        &group,
        "Private key file",
        "Choose private key",
        app.state
            .borrow()
            .rdp
            .custom_key_path
            .as_deref()
            .unwrap_or(""),
    );
    group.append(&description(
        "The service reads the key as its own user, so keep both files outside your home directory.",
    ));

    // The paths are useless until the mode is Custom, so the rows follow the
    // dropdown rather than the published state; otherwise a custom pair could never
    // be typed in before it is applied.
    let reveal: Rc<dyn Fn(bool)> = {
        let cert_row = cert_row.clone();
        let key_row = key_row.clone();
        Rc::new(move |custom| {
            cert_row.set_visible(custom);
            key_row.set_visible(custom);
        })
    };
    reveal(app.state.borrow().rdp.certificate_mode == CertificateMode::Custom);
    let syncing = Rc::new(Cell::new(false));
    let busy = Rc::new(Cell::new(false));
    // A choice that has not been written yet, so a publish does not undo it.
    let chosen = Rc::new(Cell::new(None));
    let shown = reveal.clone();
    let guard = syncing.clone();
    let picked = chosen.clone();
    drop.connect_selected_notify(move |drop| {
        if !guard.get() {
            picked.set(Some(drop.selected()));
            shown(drop.selected() == 1);
        }
    });

    let apply = gtk::Button::with_label(&tr("Apply certificate"));
    apply.set_halign(gtk::Align::Start);
    apply.add_css_class("suggested-action");
    let weak = Rc::downgrade(app);
    let mode = drop.clone();
    let cert = certificate.clone();
    let private = key.clone();
    let in_flight = busy.clone();
    apply.connect_clicked(move |button| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let custom = mode.selected() == 1;
        let certificate = cert.text().trim().to_string();
        let key_path = private.text().trim().to_string();
        if custom && (certificate.is_empty() || key_path.is_empty()) {
            app.error("A certificate file and a private key file are both required");
            return;
        }
        app.error("");
        in_flight.set(true);
        button.set_sensitive(false);
        let weak = Rc::downgrade(&app);
        let button = button.clone();
        let done = in_flight.clone();
        // The helper installs whatever the daemon has already published, so the
        // write has to finish before the privileged step is asked for.
        glib::MainContext::default().spawn_local(async move {
            let write = bus::execute(&Action::SetRdpCertificate {
                mode: if custom {
                    CertificateMode::Custom
                } else {
                    CertificateMode::SelfSigned
                },
                certificate_path: custom.then_some(certificate),
                key_path: custom.then_some(key_path),
            })
            .await;
            let applied = if write.is_ok() {
                bus::execute(&Action::RdpSystem {
                    operation: RdpSystemAction::ApplyCertificate,
                })
                .await
            } else {
                write
            };
            if let Err(error) = applied
                && let Some(app) = weak.upgrade()
            {
                app.error(&error);
            }
            done.set(false);
            button.set_sensitive(true);
        });
    });
    group.append(&apply);

    let target = drop.downgrade();
    let cert_target = certificate.downgrade();
    let key_target = key.downgrade();
    let apply_target = apply.downgrade();
    let flag = syncing.clone();
    let pending = busy.clone();
    let shown = reveal.clone();
    let written = chosen.clone();
    app.bind_for(Surface::Full, &group, move |state| {
        let want = match state.rdp.certificate_mode {
            CertificateMode::SelfSigned => 0,
            CertificateMode::Custom => 1,
        };
        // Follow the daemon, except for a mode the user picked but has not applied.
        let shown_index = match written.get() {
            Some(pick) if pick != want => pick,
            _ => {
                written.set(None);
                want
            }
        };
        if let Some(drop) = target.upgrade()
            && drop.selected() != shown_index
        {
            flag.set(true);
            drop.set_selected(shown_index);
            flag.set(false);
        }
        shown(shown_index == 1);
        // The material is only read while the secure listener is up.
        let usable = state.rdp.https;
        if let Some(entry) = cert_target.upgrade() {
            if !entry.has_focus() {
                entry.set_text(state.rdp.custom_certificate_path.as_deref().unwrap_or(""));
            }
            cert_row.set_sensitive(usable);
        }
        if let Some(entry) = key_target.upgrade() {
            if !entry.has_focus() {
                entry.set_text(state.rdp.custom_key_path.as_deref().unwrap_or(""));
            }
            key_row.set_sensitive(usable);
        }
        if let Some(button) = apply_target.upgrade()
            && !pending.get()
        {
            button.set_sensitive(usable);
        }
    });
    group
}

/// Encoder policy for the host and what the probe found.
fn codec_section(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Codecs");
    group.append(&description(
        "H.264 gives the smoothest picture but only native clients use it. A browser client never negotiates H.264 and always falls back to QOI or RemoteFX.",
    ));

    // One draft, because the daemon writes the whole policy per action.
    let draft = Rc::new(RefCell::new(app.state.borrow().rdp.codecs.clone()));
    let sender = Rc::downgrade(app);
    let send_draft = draft.clone();
    let send: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(app) = sender.upgrade() {
            let codecs = send_draft.borrow().clone();
            app.execute(
                Action::SetRdpCodecs {
                    allow_h264: codecs.allow_h264,
                    allow_remotefx: codecs.allow_remotefx,
                    hardware_acceleration: codecs.hardware_acceleration,
                },
                None,
            );
        }
    });

    codec_switch(
        app,
        &group,
        &draft,
        &send,
        "Allow H.264",
        |codecs| codecs.allow_h264,
        |codecs, value| codecs.allow_h264 = value,
    );
    codec_switch(
        app,
        &group,
        &draft,
        &send,
        "Allow RemoteFX",
        |codecs| codecs.allow_remotefx,
        |codecs, value| codecs.allow_remotefx = value,
    );

    let levels = [
        ("auto", HardwareAccel::Auto.label()),
        ("force", HardwareAccel::Always.label()),
        ("off", HardwareAccel::Off.label()),
    ];
    let drop = choose(
        &group,
        "Hardware acceleration",
        &levels,
        accel_id(app.state.borrow().rdp.codecs.hardware_acceleration),
    );
    let syncing = Rc::new(Cell::new(false));
    let guard = syncing.clone();
    let accel_draft = draft.clone();
    let send_accel = send.clone();
    drop.connect_selected_notify(move |drop| {
        if guard.get() {
            return;
        }
        if let Some((id, _)) = levels.get(drop.selected() as usize) {
            accel_draft.borrow_mut().hardware_acceleration = accel_from_id(id);
            send_accel();
        }
    });
    let target = drop.downgrade();
    let flag = syncing.clone();
    app.bind_for(Surface::Full, &group, move |state| {
        if let Some(drop) = target.upgrade() {
            let want = levels
                .iter()
                .position(|(id, _)| *id == accel_id(state.rdp.codecs.hardware_acceleration))
                .unwrap_or(0) as u32;
            if drop.selected() != want {
                flag.set(true);
                drop.set_selected(want);
                flag.set(false);
            }
        }
    });

    let device = description("");
    group.append(&device);
    let target = device.downgrade();
    app.bind_for(Surface::Full, &device, move |state| {
        if let Some(label) = target.upgrade() {
            label.set_text(&match state.rdp.codecs.detected_device.as_deref() {
                Some(device) => format!("Hardware encoder detected: {device}."),
                None => {
                    "No hardware encoder found. H.264 uses the software encoder, which costs CPU."
                        .into()
                }
            });
        }
    });

    // The draft follows the daemon on every publish, so a refused write is undone
    // here instead of looking applied.
    let follow = draft.clone();
    app.bind_for(Surface::Full, &group, move |state| {
        *follow.borrow_mut() = state.rdp.codecs.clone();
    });
    group
}

/// Session limit and the live roster.
fn session_section(app: &Rc<App>) -> gtk::Box {
    let group = column();
    group.add_css_class("settings-group");
    section(&group, "Sessions");
    group.append(&description(
        "Every session holds a logged-in desktop. More sessions means more memory and more CPU shared between them.",
    ));
    let limit = gtk::SpinButton::with_range(1., f64::from(MAX_RDP_SESSIONS), 1.);
    limit.set_increments(1., 1.);
    limit.set_value(f64::from(app.state.borrow().rdp.max_sessions));
    limit.set_valign(gtk::Align::Center);
    limit.set_width_chars(4);
    group.append(&row("Maximum sessions", &limit));
    let syncing = Rc::new(Cell::new(false));
    let guard = syncing.clone();
    let weak = Rc::downgrade(app);
    limit.connect_value_changed(move |spin| {
        if guard.get() {
            return;
        }
        if let Some(app) = weak.upgrade() {
            let value = spin.value_as_int().max(1);
            if i32::from(app.state.borrow().rdp.max_sessions) != value {
                app.execute(
                    Action::SetRdpMaxSessions {
                        max_sessions: value as u16,
                    },
                    None,
                );
            }
        }
    });
    let target = limit.downgrade();
    let flag = syncing.clone();
    app.bind_for(Surface::Full, &limit, move |state| {
        if let Some(spin) = target.upgrade() {
            let want = f64::from(state.rdp.max_sessions);
            if spin.value() != want {
                flag.set(true);
                spin.set_value(want);
                flag.set(false);
            }
        }
    });

    section(&group, "Connected now");
    let roster = column();
    group.append(&roster);
    let last = Rc::new(RefCell::new(String::new()));
    let list = roster.downgrade();
    let weak = Rc::downgrade(app);
    app.bind_for(Surface::Full, &roster, move |state| {
        let signature = state
            .rdp
            .sessions
            .iter()
            .map(|session| {
                format!(
                    "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                    session.id,
                    session.username,
                    session.web,
                    session.codec.label(),
                    session.codec_detail.clone().unwrap_or_default(),
                    session.connected_since_secs
                )
            })
            .collect::<Vec<_>>()
            .join("\u{2}");
        if *last.borrow() == signature {
            return;
        }
        last.replace(signature);
        let Some(list) = list.upgrade() else {
            return;
        };
        clear(&list);
        if state.rdp.sessions.is_empty() {
            list.append(&description("Nobody is connected right now."));
            return;
        }
        let Some(app) = weak.upgrade() else {
            return;
        };
        for session in &state.rdp.sessions {
            list.append(&session_row(&app, session));
        }
    });
    group
}

/// One live connection: who, how, which encoder, for how long, and the disconnect
/// button.
fn session_row(app: &Rc<App>, session: &RdpSession) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("settings-row");
    let details = column();
    details.set_hexpand(true);
    let name = gtk::Label::new(Some(&tr(&session.username)));
    name.set_xalign(0.);
    name.add_css_class("network-name");
    details.append(&name);
    let transport = if session.web {
        "Browser client"
    } else {
        "Native client"
    };
    let mut line = format!(
        "{transport} · {} · connected {}",
        session.codec.label(),
        duration_text(session.connected_since_secs)
    );
    if let Some(detail) = session.codec_detail.as_deref() {
        line = format!("{line} · {detail}");
    }
    details.append(&description(&line));
    row.append(&details);

    let drop = gtk::Button::with_label(&tr("Disconnect"));
    drop.set_valign(gtk::Align::Center);
    let id = session.id.clone();
    let weak = Rc::downgrade(app);
    drop.connect_clicked(move |button| {
        if let Some(app) = weak.upgrade() {
            app.execute(
                Action::RdpDisconnect {
                    session_id: id.clone(),
                },
                Some(button.clone().upcast()),
            );
        }
    });
    row.append(&drop);
    row
}

// ------------------------------------------------------------------ helpers

/// A labelled switch over the shared codec draft, like the Screen Capture page's
/// whole-section resubmit.
fn codec_switch(
    app: &Rc<App>,
    parent: &gtk::Box,
    draft: &Rc<RefCell<RdpCodecs>>,
    send: &Rc<dyn Fn()>,
    name: &str,
    get: fn(&RdpCodecs) -> bool,
    set: fn(&mut RdpCodecs, bool),
) {
    let control = gtk::Switch::new();
    control.set_valign(gtk::Align::Center);
    control.update_property(&[gtk::accessible::Property::Label(name)]);
    control.set_active(get(&draft.borrow()));
    parent.append(&row(name, &control));
    let guard = Rc::new(Cell::new(false));
    let send = send.clone();
    let draft = draft.clone();
    let flag = guard.clone();
    control.connect_state_set(move |_, value| {
        if !flag.get() {
            set(&mut draft.borrow_mut(), value);
            send();
        }
        glib::Propagation::Proceed
    });
    let target = control.downgrade();
    app.bind_for(Surface::Full, &control, move |state| {
        if let Some(control) = target.upgrade() {
            guard.set(true);
            control.set_active(get(&state.rdp.codecs));
            guard.set(false);
        }
    });
}

/// A labelled port entry that only accepts digits. Returns the entry and its row.
fn port_field(parent: &gtk::Box, name: &str, value: &str) -> (gtk::Entry, gtk::Box) {
    let field = gtk::Entry::builder().text(value).width_chars(8).build();
    field.set_valign(gtk::Align::Center);
    field.set_input_purpose(gtk::InputPurpose::Digits);
    let row = row(name, &field);
    parent.append(&row);
    (field, row)
}

/// A labelled path entry with a file chooser next to it. Returns the entry and its
/// row so the caller can hide the pair together.
fn path_field(
    app: &Rc<App>,
    parent: &gtk::Box,
    name: &str,
    chooser: &str,
    value: &str,
) -> (gtk::Entry, gtk::Box) {
    let field = gtk::Entry::builder()
        .text(value)
        .hexpand(true)
        .width_chars(28)
        .build();
    field.set_valign(gtk::Align::Center);
    field.update_property(&[gtk::accessible::Property::Label(&tr(name))]);
    let browse = gtk::Button::with_label(&tr("Browse…"));
    browse.set_valign(gtk::Align::Center);
    let weak = Rc::downgrade(app);
    let title = chooser.to_string();
    let target = field.clone();
    browse.connect_clicked(move |_| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let title = title.clone();
        let field = target.clone();
        glib::MainContext::default().spawn_local(async move {
            let dialog = gtk::FileDialog::builder().title(&title).build();
            if let Ok(file) = dialog.open_future(app.parent().as_ref()).await
                && let Some(path) = file.path().map(|path| path.to_string_lossy().to_string())
            {
                field.set_text(&path);
            }
        });
    });
    let pair = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    pair.append(&field);
    pair.append(&browse);
    let row = row(name, &pair);
    parent.append(&row);
    (field, row)
}

/// A warning line that lives inside the group it belongs to.
fn note(text: &str) -> gtk::Label {
    let label = description(text);
    label.remove_css_class("description");
    label.add_css_class("error");
    label
}

fn show_warning(target: &glib::WeakRef<gtk::Label>, text: &str) {
    if let Some(label) = target.upgrade() {
        label.set_text(text);
        label.set_visible(true);
    }
}

fn hide_warning(target: &glib::WeakRef<gtk::Label>) {
    if let Some(label) = target.upgrade() {
        label.set_visible(false);
    }
}

/// Live validation for the two port fields, mirroring the daemon's own checks.
fn check_ports(
    app: &Weak<App>,
    warning: &glib::WeakRef<gtk::Label>,
    rdp_port: &gtk::Entry,
    web_port: &gtk::Entry,
) {
    let (Some(rdp), Some(web)) = (parse_port(&rdp_port.text()), parse_port(&web_port.text()))
    else {
        show_warning(warning, "A port between 1 and 65535 is required");
        return;
    };
    let Some(app) = app.upgrade() else {
        return;
    };
    let state = app.state.borrow();
    match port_pair_problem(rdp, web, state.rdp.web_enabled, state.rdp.https) {
        Some(problem) => show_warning(warning, &problem),
        None => hide_warning(warning),
    }
}

/// The daemon only refuses a clash while the secure browser listener is on,
/// because then both listeners compete for one port.
fn port_pair_problem(
    rdp_port: u16,
    web_port: u16,
    web_enabled: bool,
    https: bool,
) -> Option<String> {
    if web_enabled && https && rdp_port == web_port {
        Some("The RDP and web ports must differ while HTTPS is on".into())
    } else {
        None
    }
}

fn parse_port(text: &str) -> Option<u16> {
    let value: u32 = text.trim().parse().ok()?;
    if value == 0 || value > 65_535 {
        None
    } else {
        Some(value as u16)
    }
}

fn mode_id(mode: &CertificateMode) -> &'static str {
    match mode {
        CertificateMode::SelfSigned => "self-signed",
        CertificateMode::Custom => "custom",
    }
}

fn accel_id(accel: HardwareAccel) -> &'static str {
    match accel {
        HardwareAccel::Auto => "auto",
        HardwareAccel::Always => "force",
        HardwareAccel::Off => "off",
    }
}

fn accel_from_id(id: &str) -> HardwareAccel {
    match id {
        "force" => HardwareAccel::Always,
        "off" => HardwareAccel::Off,
        _ => HardwareAccel::Auto,
    }
}

/// "1 h 4 min", "4 min" or "30 s".
fn duration_text(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}
