//! Capture the session's virtual output for clients that negotiate RDPSND.

use crate::clipboard::EndpointSlot;
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_rdpsnd::server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler};
use ironrdp_server::{RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

const CREDIT: usize = 8;
static FORMATS: [AudioFormat; 2] = [pcm(48_000), pcm(44_100)];

const fn pcm(rate: u32) -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * 4,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
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

impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(Playback {
            endpoint: Arc::clone(&self.endpoint),
            sender: self.sender.clone(),
            credit: Arc::new(AtomicUsize::new(CREDIT)),
            task: None,
        })
    }
}

#[derive(Debug)]
struct Playback {
    endpoint: EndpointSlot,
    sender: Option<UnboundedSender<ServerEvent>>,
    credit: Arc<AtomicUsize>,
    task: Option<JoinHandle<()>>,
}

impl RdpsndServerHandler for Playback {
    fn get_formats(&self) -> &[AudioFormat] {
        &FORMATS
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        self.stop();
        let sender = self.sender.clone().ok_or_else(|| -> Box<dyn RdpsndError> {
            Box::new(std::io::Error::other("no RDP audio sender"))
        })?;
        let endpoint = Arc::clone(&self.endpoint);
        let credit = Arc::clone(&self.credit);
        credit.store(CREDIT, Ordering::Relaxed);
        let rate = format.format().n_samples_per_sec;
        self.task = Some(tokio::spawn(async move {
            if let Err(error) = stream_audio(&endpoint, &sender, &credit, rate).await {
                tracing::debug!(%error, "remote audio output stopped");
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    fn wave_confirm(&mut self, _: u8, _: u16) {
        let _ = self
            .credit
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                (credit < CREDIT).then_some(credit + 1)
            });
    }
}

async fn stream_audio(
    slot: &EndpointSlot,
    sender: &UnboundedSender<ServerEvent>,
    credit: &AtomicUsize,
    rate: u32,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut capture = loop {
        let endpoint = slot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(endpoint) = endpoint {
            let socket = endpoint.runtime_dir().join("pulse/native");
            let mut server = std::ffi::OsString::from("unix:");
            server.push(socket);
            let mut command = endpoint.command("/usr/bin/parec");
            command
                .args([
                    "--raw",
                    "--device=@DEFAULT_MONITOR@",
                    "--format=s16le",
                    "--channels=2",
                    "--latency-msec=40",
                    "--process-time-msec=20",
                ])
                .arg(format!("--rate={rate}"))
                .arg("--server")
                .arg(server)
                .stdout(Stdio::piped());
            let mut child = tokio::process::Command::from(command)
                .kill_on_drop(true)
                .spawn()?;
            sleep(Duration::from_millis(150)).await;
            if child.try_wait()?.is_none() {
                break child;
            }
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other(
                "the private PulseAudio monitor is unavailable",
            ));
        }
        sleep(Duration::from_millis(200)).await;
    };

    let mut output = capture
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no audio pipe"))?;
    let mut samples = vec![0; (rate as usize / 50) * 4];
    let started = Instant::now();
    while output.read_exact(&mut samples).await.is_ok() {
        let available = credit.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_sub(1)
        });
        if available.is_ok()
            && sender
                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                    samples.clone(),
                    started.elapsed().as_millis() as u32,
                )))
                .is_err()
        {
            break;
        }
    }
    Ok(())
}
