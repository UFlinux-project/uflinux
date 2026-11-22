// uflinux-screencapture: GNOME-style screenshot and recording entry point.
// SPDX-License-Identifier: GPL-3.0-only

mod backend;
mod clipboard;
mod flash;
mod output;
mod overlay;
mod prefs;
mod recording;
#[path = "../../shared/recording_bus.rs"]
mod recording_bus;
mod selection;
mod shell;
mod still;
#[path = "../../shared/wayland_capture.rs"]
mod wayland_capture;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use backend::{Job, Outcome, Recorder, Target};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::MonitorExt;
use settings_core::{CONTAINERS, CaptureFormat, CaptureTarget, MAX_DELAY_SECONDS, ScreenCapture};

const APP_ID: &str = "org.uflinux.ScreenCapture";

const USAGE: &str = "\
uflinux-screencapture - screenshot and screen recording

Usage:
  uflinux-screencapture [options]                Show the capture chooser
  uflinux-screencapture --screenshot [options]   Take a screenshot
  uflinux-screencapture --record [options]       Start a recording

Options:
  --target screen|window|area   What to capture (default: from settings)
  --delay SECONDS               Extra delay before capturing (max 30)
  --stop                        Stop the active recording
  --silent                      Do not show a notification on failure
  -h, --help                    Show this help

Screenshots are written to the folder configured in Settings and copied to the
clipboard. A recording is controlled from the taskbar.";

/// What the process was asked to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Screenshot,
    Record,
    Stop,
}

/// Command line state.
struct Options {
    mode: Option<Mode>,
    target: Option<CaptureTarget>,
    delay: u8,
    silent: bool,
}

thread_local! {
    /// The recorder of this process, so that a shutdown signal can stop it.
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    static RESULT: Cell<ExitCode> = const { Cell::new(ExitCode::SUCCESS) };
    static CONTROLS: RefCell<Option<recording::Controls>> = const { RefCell::new(None) };
}

fn main() -> ExitCode {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("uflinux-screencapture: {message}");
                eprintln!();
                eprintln!("{USAGE}");
            }
            return ExitCode::from(2);
        }
    };

    if options.mode == Some(Mode::Stop) {
        return match recording::stop() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("uflinux-screencapture: {error}");
                ExitCode::from(1)
            }
        };
    }

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        gdk::set_allowed_backends("wayland");
    }
    if gtk::init().is_err() || !gtk4_layer_shell::is_supported() {
        return startup_error(
            "Screen capture requires a Wayland session with layer shell",
            options.silent,
        );
    }
    let Some(monitor) = shell::pointed_monitor() else {
        return startup_error("No screen available", options.silent);
    };
    let mut prefs = prefs::load();
    if let Some(target) = options.target {
        prefs.target = target;
    }
    shell::add_css();
    watch_shutdown_signals();

    let main_loop = glib::MainLoop::new(None, false);
    let mode = options.mode;
    let target = prefs.target;
    let loop_ref = main_loop.clone();
    let monitor_for_choice = monitor.clone();
    let on_choice = move |choice: overlay::Choice| {
        let silent = options.silent;
        if matches!(choice, overlay::Choice::Cancel) {
            loop_ref.quit();
            return;
        }
        let (prefs, directory) = match refresh() {
            Ok(state) => state,
            Err(error) => {
                finish(&loop_ref, Outcome::Failed(error), None, silent);
                return;
            }
        };
        match choice {
            overlay::Choice::Still(source) => run_still(
                &loop_ref,
                &prefs,
                &directory,
                options.delay,
                source,
                &monitor_for_choice,
                silent,
            ),
            overlay::Choice::Video(source) => {
                run_recording(&loop_ref, &prefs, &directory, source, silent)
            }
            overlay::Choice::Cancel => {}
        }
    };

    let record_target_needs_choice = mode == Some(Mode::Record) && target == CaptureTarget::Window;
    if record_target_needs_choice && options.target.is_some() {
        return startup_error(
            "Window recording is unavailable on labwc. Choose Screen or Area.",
            options.silent,
        );
    }
    let direct_recording = mode.map(|mode| mode == Mode::Record);
    if target == CaptureTarget::Screen
        && let Some(recording) = direct_recording
    {
        let source = Target::Screen(monitor.connector().map(String::from).unwrap_or_default());
        // Start inside the loop so an immediate startup error can quit it.
        glib::idle_add_local_once(move || {
            on_choice(if recording {
                overlay::Choice::Video(source)
            } else {
                overlay::Choice::Still(source)
            })
        });
    } else {
        overlay::show(
            &monitor,
            if record_target_needs_choice {
                CaptureTarget::Area
            } else {
                target
            },
            backend::backend_available(),
            direct_recording,
            on_choice,
        );
    }

    main_loop.run();
    RESULT.with(Cell::get)
}

/// Re-reads the settings and makes sure the output folder exists. The chooser
/// can stay open for a while, so a change made in the meantime is honoured.
fn refresh() -> Result<(ScreenCapture, PathBuf), String> {
    let prefs = prefs::load();
    let directory = output::screenshot_dir(&prefs.screenshot_dir);
    output::ensure_dir(&directory)?;
    Ok((prefs, directory))
}

fn startup_error(message: &str, silent: bool) -> ExitCode {
    eprintln!("uflinux-screencapture: {message}");
    if !silent {
        notify("Capture failed", message);
    }
    ExitCode::from(1)
}

/// Takes one screenshot, after a countdown when one is configured. The loop is
/// stopped when the backend is done, whatever the result.
fn run_still(
    main_loop: &glib::MainLoop,
    prefs: &ScreenCapture,
    directory: &Path,
    extra_delay: u8,
    target: Target,
    monitor: &gdk::Monitor,
    silent: bool,
) {
    let format = prefs.image_format;
    let path = output::unique_path(directory, &output::filename(format.extension()));
    let include_cursor = prefs.include_cursor;
    let copy_format = prefs.copy_to_clipboard.then_some(format);
    let delay = u64::from(prefs.delay_seconds) + u64::from(extra_delay);
    let flash = prefs.flash;
    let loop_ref = main_loop.clone();
    let monitor = monitor.clone();
    let start = move || {
        glib::MainContext::default().spawn_local(async move {
            let result =
                gio::spawn_blocking(move || still::capture(&target, &path, format, include_cursor))
                    .await;
            let outcome =
                result.unwrap_or_else(|_| Outcome::Failed("Capture was interrupted".into()));
            if flash && matches!(outcome, Outcome::Saved(_)) {
                flash::show(&monitor);
                glib::timeout_future(flash::DURATION).await;
            }
            finish(&loop_ref, outcome, copy_format, silent);
        });
    };
    if delay > 0 {
        glib::timeout_add_local_once(Duration::from_secs(delay), start);
    } else {
        start();
    }
}

/// Start the backend and publish its controls to the taskbar.
fn run_recording(
    main_loop: &glib::MainLoop,
    prefs: &ScreenCapture,
    directory: &Path,
    target: Target,
    silent: bool,
) {
    let controls = match recording::Controls::claim() {
        Ok(controls) => controls,
        Err(message) => {
            finish(main_loop, Outcome::Failed(message), None, silent);
            return;
        }
    };
    let extension = container_extension(prefs.container.as_deref().unwrap_or("mp4"));
    let path = output::unique_path(directory, &output::filename(extension));
    let job = Job {
        target,
        path,
        prefs: prefs.clone(),
    };
    let socket = output::socket_path();
    let loop_ref = main_loop.clone();
    let started = Recorder::start(&job, Some(&socket), move |exit| {
        let recorder = RECORDER.with(|slot| slot.borrow_mut().take());
        CONTROLS.with(|slot| slot.borrow_mut().take());
        let outcome = match recorder {
            Some(recorder) => recorder.outcome(exit),
            None => Outcome::Failed("Recording was interrupted".into()),
        };
        finish(&loop_ref, outcome, None, silent);
    });
    match started {
        Ok(recorder) => {
            controls.attach(&recorder);
            CONTROLS.with(|slot| *slot.borrow_mut() = Some(controls));
            RECORDER.with(|slot| *slot.borrow_mut() = Some(recorder));
        }
        Err(message) => finish(main_loop, Outcome::Failed(message), None, silent),
    }
}

/// Reports the result of a capture and stops the process.
fn finish(
    main_loop: &glib::MainLoop,
    outcome: Outcome,
    copy_format: Option<CaptureFormat>,
    silent: bool,
) {
    match outcome {
        Outcome::Saved(path) => {
            if let Some(format) = copy_format {
                if let Err(error) = clipboard::copy(&path, format) {
                    eprintln!("uflinux-screencapture: clipboard: {error}");
                }
            }
        }
        Outcome::Failed(message) => {
            RESULT.with(|status| status.set(ExitCode::from(1)));
            eprintln!("uflinux-screencapture: {message}");
            if !silent {
                notify("Capture failed", &message);
            }
        }
    }
    main_loop.quit();
}

/// Shows a desktop notification through notify-send, when available.
fn notify(title: &str, body: &str) {
    let Some(program) = glib::find_program_in_path("notify-send") else {
        return;
    };
    let _ = std::process::Command::new(program)
        .args(["-a", APP_ID, "-u", "normal"])
        .arg(title)
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Stops a running recording when the session manager terminates us. The
/// recorder finishes the file on its own and outlives this process.
fn watch_shutdown_signals() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        glib_unix::unix_signal_add_local_once(signal, || {
            RECORDER.with(|slot| {
                if let Some(recorder) = slot.borrow_mut().take() {
                    recorder.terminate();
                }
            });
            std::process::exit(0);
        });
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        mode: None,
        target: None,
        delay: 0,
        silent: false,
    };
    let mut args = args.peekable();
    if args.peek().is_none() {
        return Ok(options);
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Err(String::new());
            }
            "--screenshot" | "--screenshot-now" => options.mode = Some(Mode::Screenshot),
            "--record" => options.mode = Some(Mode::Record),
            "--stop" => options.mode = Some(Mode::Stop),
            "--silent" => options.silent = true,
            "--delay" => {
                let value = args.next().ok_or("--delay needs a value")?;
                let seconds: u8 = value
                    .parse()
                    .map_err(|_| format!("invalid delay: {value}"))?;
                if seconds > MAX_DELAY_SECONDS {
                    return Err(format!("delay must be at most {MAX_DELAY_SECONDS} seconds"));
                }
                options.delay = seconds;
            }
            "--target" => {
                let value = args.next().ok_or("--target needs a value")?;
                options.target = Some(parse_target(&value)?);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if options.target.is_some() && !matches!(options.mode, Some(Mode::Screenshot | Mode::Record)) {
        return Err("--target needs --screenshot or --record".into());
    }
    Ok(options)
}

fn parse_target(value: &str) -> Result<CaptureTarget, String> {
    match value {
        "screen" => Ok(CaptureTarget::Screen),
        "window" => Ok(CaptureTarget::Window),
        "area" => Ok(CaptureTarget::Area),
        other => Err(format!("unknown target: {other}")),
    }
}

/// The file extension to name a recording after. The backend chooses the
/// container from the output name, so it has to match the setting.
fn container_extension(container: &str) -> &str {
    if CONTAINERS.contains(&container) {
        container
    } else {
        "mp4"
    }
}
