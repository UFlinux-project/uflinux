use super::*;

/// The Performance page owns one decision: whether the kernel applies its
/// runtime workarounds for CPU vulnerabilities. The switch shows what the
/// system was configured to do, and the status line compares that with the
/// command line of the kernel that is running now, since the boot entries are
/// read by the firmware before anything is running.
pub fn page(app: &Rc<App>, parent: &gtk::Box) {
    parent.append(&description("UFLinux starts with the processor's runtime workarounds for CPU vulnerabilities switched off, which keeps the full speed of the machine. Enable them if this system runs code from untrusted sources directly on it, such as a shared login server or a browser opened on unknown websites."));
    let status = description("");
    parent.append(&status);
    let target = status.downgrade();
    app.bind_for(Surface::Full, &status, move |state| {
        if let Some(label) = target.upgrade() {
            label.set_text(&status_text(state));
        }
    });
    toggle(
        app,
        Surface::Full,
        parent,
        "Workarounds for CPU vulnerabilities",
        |s| s.boot.configured == Mitigations::Auto,
        |enabled| Action::System {
            operation: SystemAction::SetMitigations { enabled },
        },
    );
    section(parent, "What this costs");
    parent.append(&description("With the workarounds off, speculative execution is unthrottled: file, network and compression work typically runs at full speed. With them on, the same work can take longer, from a few percent up to a third on older processors, depending on the workload and the CPU generation. The kernel chooses the cheapest workaround it believes it needs, so the exact cost is not reported here."));
    let note = description("");
    parent.append(&note);
    let target = note.downgrade();
    app.bind_for(Surface::Full, &note, move |state| {
        if let Some(label) = target.upgrade() {
            label.set_text(if state.boot.available {
                "The setting is stored in /etc/uflinux/boot.conf and written into the boot entries when you change it. Changes require administrator authorization."
            } else {
                "Read the boot entries to change this setting."
            });
        }
    });
}

fn status_text(state: &Snapshot) -> String {
    let wanted = state.boot.configured.label();
    if !state.boot.available {
        return format!(
            "Configured: {wanted}. {}",
            if state.boot.reason.is_empty() {
                "The boot entries could not be read."
            } else {
                &state.boot.reason
            }
        );
    }
    match state.boot.applied {
        Some(applied) if state.boot.reboot_required => format!(
            "Configured: {wanted}. The system is still running with the workarounds {}; restart to apply the change.",
            match applied {
                Mitigations::Off => "off",
                Mitigations::Auto => "on",
            }
        ),
        Some(applied) => format!(
            "Configured: {wanted}. The system is running with the workarounds {}.",
            match applied {
                Mitigations::Off => "off",
                Mitigations::Auto => "on",
            }
        ),
        None => format!("Configured: {wanted}. {}", state.boot.reason),
    }
}
