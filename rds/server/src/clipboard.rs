//! Bridge Unicode text between CLIPRDR and the session's private Wayland socket.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::{IntoOwned, impl_as_any};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc::UnboundedSender;

const MAX_TEXT: usize = 256 * 1024;
const UNICODE: ClipboardFormatId = ClipboardFormatId::CF_UNICODETEXT;

#[derive(Clone, Debug)]
pub struct Endpoint {
    socket: PathBuf,
    uid: u32,
    gid: u32,
}

pub type EndpointSlot = Arc<Mutex<Option<Endpoint>>>;

impl Endpoint {
    pub fn new(socket: &Path, uid: u32, gid: u32) -> Self {
        Self {
            socket: socket.to_owned(),
            uid,
            gid,
        }
    }

    pub(super) fn runtime_dir(&self) -> &Path {
        self.socket.parent().expect("private Wayland socket")
    }

    pub(super) fn command(&self, program: &str) -> Command {
        let (uid, gid) = (self.uid, self.gid);
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("WAYLAND_DISPLAY", &self.socket)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: the closure only performs credential syscalls. A non-root
        // development session already runs as the account it connects to.
        unsafe {
            command.pre_exec(move || {
                if libc::geteuid() == 0
                    && (libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0)
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }

    fn read_text(&self) -> Option<String> {
        let mut child = self
            .command("/usr/bin/timeout")
            .args([
                "-s",
                "KILL",
                "1",
                "/usr/bin/wl-paste",
                "--no-newline",
                "--type",
                "text",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let mut bytes = Vec::new();
        let result = child
            .stdout
            .take()?
            .take(MAX_TEXT as u64 + 1)
            .read_to_end(&mut bytes);
        let status = child.wait().ok()?;
        if result.is_err() || bytes.len() > MAX_TEXT {
            return None;
        }
        status.success().then_some(())?;
        String::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
    }

    fn write_text(&self, text: &str) -> std::io::Result<Child> {
        let fd = nix::sys::memfd::memfd_create(
            c"rdp-clipboard",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )?;
        let mut file = File::from(fd);
        file.write_all(text.as_bytes())?;
        file.rewind()?;
        self.command("/usr/bin/wl-copy")
            .args(["--foreground", "--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::from(file))
            .spawn()
    }
}

pub struct Factory {
    endpoint: EndpointSlot,
    sender: Option<UnboundedSender<ServerEvent>>,
}

impl Factory {
    pub fn new(endpoint: EndpointSlot) -> Self {
        Self {
            endpoint,
            sender: None,
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl CliprdrServerFactory for Factory {}

impl CliprdrBackendFactory for Factory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend {
            endpoint: Arc::clone(&self.endpoint),
            sender: self.sender.clone(),
            cached: Default::default(),
            incoming: None,
            worker: None,
            pending: false,
        })
    }
}

#[derive(Debug)]
struct Backend {
    endpoint: EndpointSlot,
    sender: Option<UnboundedSender<ServerEvent>>,
    cached: Arc<Mutex<Option<String>>>,
    incoming: Option<mpsc::SyncSender<String>>,
    worker: Option<JoinHandle<()>>,
    pending: bool,
}

impl_as_any!(Backend);

fn send(sender: &UnboundedSender<ServerEvent>, message: ClipboardMessage) {
    let _ = sender.send(ServerEvent::Clipboard(message));
}

fn stop_child(child: &mut Option<Child>) {
    if let Some(mut child) = child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.incoming.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let Some(sender) = self.sender.clone() else {
            return;
        };
        let endpoint = Arc::clone(&self.endpoint);
        let cached = Arc::clone(&self.cached);
        let (incoming, requests) = mpsc::sync_channel::<String>(1);
        let worker = std::thread::Builder::new()
            .name("rds-clipboard".to_owned())
            .spawn(move || {
                let mut selection = None;
                loop {
                    let next = match requests.recv_timeout(Duration::from_millis(500)) {
                        Ok(text) => Some(text),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let Some(endpoint) = endpoint.lock().unwrap_or_else(|e| e.into_inner()).clone()
                    else {
                        continue;
                    };
                    if let Some(text) = next {
                        stop_child(&mut selection);
                        if let Ok(child) = endpoint.write_text(&text) {
                            selection = Some(child);
                            *cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
                        }
                        continue;
                    }
                    if selection
                        .as_mut()
                        .is_some_and(|child| child.try_wait().is_ok_and(|status| status.is_none()))
                    {
                        continue;
                    }
                    stop_child(&mut selection);
                    let value = endpoint.read_text();
                    let mut cached = cached.lock().unwrap_or_else(|e| e.into_inner());
                    if *cached != value {
                        let formats = if value.is_some() {
                            vec![ClipboardFormat::new(UNICODE)]
                        } else {
                            vec![]
                        };
                        *cached = value;
                        send(&sender, ClipboardMessage::SendInitiateCopy(formats));
                    }
                }
                stop_child(&mut selection);
            });
        if let Ok(worker) = worker {
            self.worker = Some(worker);
            self.incoming = Some(incoming);
        }
    }

    fn on_request_format_list(&mut self) {
        self.on_ready();
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        if !self.pending
            && self.incoming.is_some()
            && formats.iter().any(|format| format.id() == UNICODE)
            && let Some(sender) = &self.sender
        {
            self.pending = true;
            send(sender, ClipboardMessage::SendInitiatePaste(UNICODE));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let response = match (request.format, cached.as_deref()) {
            (UNICODE, Some(text)) => FormatDataResponse::new_unicode_string(text),
            _ => FormatDataResponse::new_error(),
        };
        if let Some(sender) = &self.sender {
            send(
                sender,
                ClipboardMessage::SendFormatData(response.into_owned()),
            );
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if !std::mem::take(&mut self.pending)
            || response.is_error()
            || response.data().len() > MAX_TEXT * 2
        {
            return;
        }
        if let Some(incoming) = &self.incoming
            && let Ok(text) = response.to_unicode_string()
            && text.len() <= MAX_TEXT
            && !text.contains('\0')
        {
            let _ = incoming.try_send(text);
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        if let Some(sender) = &self.sender {
            send(
                sender,
                ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(
                    request.stream_id,
                )),
            );
        }
    }

    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}
