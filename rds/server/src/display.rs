//! Display side of a session: the Wayland frame source, the IronRDP display
//! handler, and the H.264 encoder that feeds EGFX.
//!
//! Two constraints from the crates we sit on shape the whole design:
//!
//! * [`DesktopBackend::next_frame`] blocks (it is a channel receive with a
//!   timeout), so it cannot be called from an async task without stalling the
//!   runtime. It runs on a dedicated OS thread — the *pump* — which therefore
//!   owns the backend, because `DesktopBackend` is neither `Clone` nor `Sync`.
//!   Everything the pump needs from the connection (the EGFX handle, the
//!   suppression flag, the encoder) lives behind a std mutex in
//!   [`DisplayShared`].
//! * IronRDP's server never drains the EGFX pipeline: it builds the pipeline
//!   from our [`GfxServerFactory`], hands the handle to the display through
//!   [`DisplayContext::gfx_handle`], and then only forwards
//!   [`ServerEvent::Egfx`] messages we produce — there is no call to
//!   `drain_output`, `create_surface` or `map_surface_to_output` anywhere in
//!   `ironrdp-server/src/server.rs`. Surface creation, output mapping, draining
//!   and the `SvcMessage` framing are ours.
//!
//! For the bitmap tiers (RemoteFX, QOI/QOI-Z) the pump simply publishes
//! [`DisplayUpdate::Bitmap`] updates with `PixelFormat::RgbA32`; the server's
//! own update encoder picks the negotiated codec and encodes them, so no
//! encoder is needed here at all.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use ironrdp_displaycontrol::pdu::{DisplayControlCapabilities, DisplayControlMonitorLayout};
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayContext, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerError, ServerErrorExt, ServerEvent, ServerResult,
};
#[cfg(feature = "egfx")]
use ironrdp_server::{EgfxServerMessage, GfxServerFactory, GfxServerHandle, ServerEventSender};
#[cfg(feature = "egfx")]
use ironrdp_svc::ChannelFlags;

use uflinux_rds_codec::{CodecError, CodecTier, H264Backend, MIN_DIMENSION, RgbaFrame};
use uflinux_rds_wayland::{
    BackendError, DesktopBackend, DirtyRect, Frame, KeyboardEvent as WlKeyboardEvent,
    PointerEvent as WlPointerEvent,
};

use crate::degrade::{Action as DegradeAction, Degrader};
use crate::pacing::Pacer;

/// How long the pump waits for a frame before checking for queued commands.
///
/// Input events are forwarded from this loop, so this is also the worst-case
/// extra input latency. 25 ms is below the 33 ms frame period of the default
/// 30 fps cap, so it never costs a frame.
const POLL: Duration = Duration::from_millis(25);

/// How many display updates may sit undelivered before frames are dropped.
///
/// One frame produces one update per dirty rectangle, so this is sized for a
/// busy frame from a client that is a round-trip behind. Dropping is the right
/// response to a full queue: the compositor already coalesces frames, and a
/// stale frame is worth less than a live cursor.
const UPDATE_QUEUE_DEPTH: usize = 16;

/// How long `display_suppressed` must stay set before we stop sending frames.
///
/// `mstsc` pulses `SuppressOutput` during its handshake; honouring the flag
/// blindly leaves the client with a half-initialised surface (see the caveat in
/// `DisplayContext`'s documentation), so the gate only engages after the flag
/// has been steady for this long.
const SUPPRESS_DEBOUNCE: Duration = Duration::from_millis(1_000);

/// H.264 quantisation parameter used for the AVC420 tiers.
///
/// Middle ground between bandwidth and text legibility; the RemoteFX quant
/// setting is a different scale (0..=63 over ten subbands) and is not reused
/// here.
pub const AVC_QUANTIZATION_PARAMETER: u8 = 20;

/// Command the pump thread accepts from the connection side.
#[derive(Debug)]
pub enum PumpCommand {
    /// Forward a keyboard event to the compositor.
    Keyboard(WlKeyboardEvent),
    /// Forward a pointer event to the compositor.
    Pointer(WlPointerEvent),
    /// Resize the desktop (a client that asked for its own desktop size, or an
    /// operator-driven resolution change).
    Resize {
        /// New width in pixels.
        width: u32,
        /// New height in pixels.
        height: u32,
    },
    /// Force the next H.264 frame to be an IDR (after a client-side refresh).
    KeyFrame,
    /// Leave the loop; the caller tears the backend down.
    Stop,
}

/// Sender side of the pump's command queue.
pub type PumpSender = std::sync::mpsc::Sender<PumpCommand>;

/// Builds the encoder for a tier, or `None` when this machine cannot provide
/// one (no render node, no loadable OpenH264).
///
/// A factory rather than an encoder because a mid-session downgrade has to
/// build the replacement tier's encoder at the moment it becomes necessary.
pub type EncoderFactory = Box<dyn Fn(CodecTier) -> Option<Box<dyn H264Backend>> + Send>;

/// The queue the display handler reads from.
///
/// Bounded on purpose: the pump uses `try_send` and drops the frame when a slow
/// client has not drained the queue, which keeps a stalled connection from
/// turning every compositor update into heap growth.
pub type UpdateQueue = Arc<AsyncMutex<mpsc::Receiver<DisplayUpdate>>>;

/// Where the pump sends `ServerEvent::Egfx` messages.
///
/// `RdpServer::new` calls [`ServerEventSender::set_sender`] on every object that
/// registered as an event sink — including the gfx factory (`server.rs:2195`),
/// which is therefore what captures the sender and hands it to the pump thread.
/// The display itself is never given one.
pub type EventSenderSlot = Arc<std::sync::Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

/// State shared between the pump thread and the connection side.
pub struct DisplayShared {
    size: DesktopSize,
    max_size: DesktopSize,
    awaiting_reactivation: bool,
    #[cfg(feature = "egfx")]
    gfx: Option<GfxServerHandle>,
    #[cfg(feature = "egfx")]
    surface: Option<SurfaceState>,
    /// The tier in effect. The pump rewrites this on a downgrade so the session
    /// can notice and re-report the codec to the broker.
    tier: CodecTier,
    /// Number of times the tier was downgraded mid-session.
    downgrades: u32,
    /// Set by the pump when the compositor is gone and no further frame can
    /// arrive.
    failed: Option<String>,
    suppression: Option<Arc<AtomicBool>>,
    suppressed_since: Option<Instant>,
    first_frame_sent: bool,
}

// `GfxServerHandle` is not `Debug`, so the derive cannot be used here.
impl std::fmt::Debug for DisplayShared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DisplayShared")
            .field("size", &self.size)
            .field("max_size", &self.max_size)
            .field("tier", &self.tier)
            .field("downgrades", &self.downgrades)
            .field("failed", &self.failed)
            .field("first_frame_sent", &self.first_frame_sent)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "egfx")]
#[derive(Debug, Clone, Copy)]
struct SurfaceState {
    id: u16,
    width: u16,
    height: u16,
}

/// The display handler registered with IronRDP.
///
/// `with_display_handler` takes ownership, so the session keeps a [`clone]
/// handle: every field is shared state, and a clone sees the tier, size and
/// failure reason the pump writes.
pub struct SessionDisplay {
    shared: Arc<std::sync::Mutex<DisplayShared>>,
    queue: UpdateQueue,
    updates: mpsc::Sender<DisplayUpdate>,
    events: EventSenderSlot,
    /// The frame pump, once the desktop has been started. A session that is
    /// still waiting for its user to authenticate has none: the desktop is
    /// only started for someone who proved who they are, so the display
    /// exists (and answers size queries) before anything is drawing into it.
    pump: Arc<std::sync::Mutex<Option<PumpSender>>>,
}

impl Clone for SessionDisplay {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            queue: Arc::clone(&self.queue),
            updates: self.updates.clone(),
            events: Arc::clone(&self.events),
            pump: Arc::clone(&self.pump),
        }
    }
}

// `GfxServerHandle` is not `Debug`, so the derive cannot be used here.
impl std::fmt::Debug for SessionDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionDisplay")
            .field("tier", &self.tier())
            .finish_non_exhaustive()
    }
}

impl SessionDisplay {
    /// New display for a backend that already reports its size.
    pub fn new(backend: &DesktopBackend, tier: CodecTier) -> Self {
        let (width, height) = backend.size();
        Self::from_size(width, height, tier)
    }

    /// New display that starts out at `width` x `height` with no pump attached.
    ///
    /// This is what lets a session answer the client's size queries before the
    /// desktop exists: the geometry is whatever the compositor is about to be
    /// started at, so the first frame matches the advertised size.
    pub fn from_size(width: u32, height: u32, tier: CodecTier) -> Self {
        let size = clamp_size(width, height);
        let (updates, receiver) = mpsc::channel(UPDATE_QUEUE_DEPTH);
        Self {
            shared: Arc::new(std::sync::Mutex::new(DisplayShared {
                size,
                max_size: size,
                awaiting_reactivation: false,
                #[cfg(feature = "egfx")]
                gfx: None,
                #[cfg(feature = "egfx")]
                surface: None,
                tier,
                downgrades: 0,
                failed: None,
                suppression: None,
                suppressed_since: None,
                first_frame_sent: false,
            })),
            queue: Arc::new(AsyncMutex::new(receiver)),
            updates,
            events: Arc::new(std::sync::Mutex::new(None)),
            pump: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The command queue, once the desktop has been started.
    pub fn pump(&self) -> Option<PumpSender> {
        self.pump.lock().ok()?.clone()
    }

    /// Hands the pump to the display, so a later client resize reaches the
    /// compositor. Returns `false` if a pump was already attached.
    pub fn attach_pump(&self, sender: PumpSender) -> bool {
        let mut slot = match self.pump.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(sender);
        true
    }

    /// The queue the pump pushes into and the display handler reads from.
    pub fn update_queue(&self) -> UpdateQueue {
        Arc::clone(&self.queue)
    }

    /// The event-sender slot to hand the pump so it can reach the connection.
    pub fn event_slot(&self) -> EventSenderSlot {
        Arc::clone(&self.events)
    }

    /// The queue's sender half, for the pump's [`PumpConfig::updates`].
    pub fn update_sender(&self) -> mpsc::Sender<DisplayUpdate> {
        self.updates.clone()
    }

    /// The shared state, for the pump's [`PumpConfig::shared`].
    pub fn shared_arc(&self) -> Arc<std::sync::Mutex<DisplayShared>> {
        Arc::clone(&self.shared)
    }

    /// The EGFX factory to register, when the feature is on.
    #[cfg(feature = "egfx")]
    pub fn factory(&self) -> SessionGfxFactory {
        SessionGfxFactory {
            handle: Arc::new(std::sync::Mutex::new(None)),
            events: Arc::clone(&self.events),
        }
    }

    /// The tier currently in effect (changes after a mid-session downgrade).
    pub fn tier(&self) -> CodecTier {
        self.lock().tier
    }

    /// Number of mid-session downgrades so far.
    pub fn downgrades(&self) -> u32 {
        self.lock().downgrades
    }

    /// The desktop size the display is currently serving.
    pub fn size(&self) -> DesktopSize {
        self.lock().size
    }

    /// Reason the frame source died, if it has.
    pub fn failure(&self) -> Option<String> {
        self.lock().failed.clone()
    }

    /// Whether at least one frame reached the client. The session uses this to
    /// tell "the client connected and watched a desktop" apart from "the client
    /// vanished during negotiation", which is what decides the linger window.
    pub fn served_frame(&self) -> bool {
        self.lock().first_frame_sent
    }

    /// Ask the encoder for an IDR, e.g. after a client that missed a key frame
    /// sends a RefreshRectangles.
    pub fn request_key_frame(&self, sender: &PumpSender) {
        let _ = sender.send(PumpCommand::KeyFrame);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DisplayShared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Clamp a backend size into something a `DesktopSize` can hold.
fn clamp_size(width: u32, height: u32) -> DesktopSize {
    DesktopSize {
        width: u16::try_from(width.max(1)).unwrap_or(u16::MAX),
        height: u16::try_from(height.max(1)).unwrap_or(u16::MAX),
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for SessionDisplay {
    async fn size(&mut self) -> DesktopSize {
        self.lock().size
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        let current = self.lock().size;
        debug!(
            requested = %format!("{}x{}", client_size.width, client_size.height),
            current = %format!("{}x{}", current.width, current.height),
            "client asked for its own desktop size"
        );
        // The desktop is started from the credential path, so a pump may or may
        // not be attached yet. Either way the size is recorded here; when a pump
        // exists the compositor is resized too, and the pump repaints the whole
        // surface after a resize, so the client's first frame is complete.
        match self.request_size_no_pump(client_size) {
            Some(adopted) => {
                if let Some(sender) = self.pump() {
                    let _ = sender.send(PumpCommand::Resize {
                        width: u32::from(adopted.width),
                        height: u32::from(adopted.height),
                    });
                }
                adopted
            }
            None => self.lock().size,
        }
    }

    async fn updates(
        &mut self,
        ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let mut shared = self.lock();
        shared.suppression = Some(Arc::clone(&ctx.display_suppressed));
        shared.suppressed_since = None;
        shared.awaiting_reactivation = false;
        #[cfg(feature = "egfx")]
        {
            shared.gfx = ctx.gfx_handle.clone();
        }
        drop(shared);
        Ok(Box::new(QueueUpdates {
            queue: Arc::clone(&self.queue),
        }))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let [monitor] = layout.monitors() else {
            return;
        };
        let (width, height) = monitor.dimensions();
        if u64::from(width) * u64::from(height)
            > DisplayControlCapabilities::single_monitor().max_monitor_area()
        {
            debug!(
                width,
                height, "client resize exceeds the advertised pixel budget"
            );
            return;
        }
        // The decoder validates the width. H.264 4:2:0 also needs an even height.
        let height = height & !1;
        if let Some(sender) = self.pump() {
            let _ = sender.send(PumpCommand::Resize { width, height });
        }
    }
}

impl SessionDisplay {
    /// Resize without a pump command queue (initial negotiation only).
    fn request_size_no_pump(&self, client_size: DesktopSize) -> Option<DesktopSize> {
        let mut shared = self.lock();
        // IronRDP also asks during reactivation. Keep the new geometry
        // that the frame pump already supplied for that handshake.
        if shared.suppression.is_some() || client_size.width == 0 || client_size.height == 0 {
            return None;
        }
        let adopted = clamp_size(
            u32::from(client_size.width.min(shared.max_size.width)),
            u32::from(client_size.height.min(shared.max_size.height)),
        );
        shared.size = adopted;
        #[cfg(feature = "egfx")]
        {
            shared.surface = None;
        }
        Some(adopted)
    }
}

/// The per-connection stream of display updates: a view onto the shared queue.
///
/// IronRDP calls `updates()` again after every Deactivation-Reactivation
/// sequence, so each call must hand out a working reader; the receiver stays in
/// the shared slot and every reader borrows it, which also means updates queued
/// during a reactivation survive it.
struct QueueUpdates {
    queue: UpdateQueue,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for QueueUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        let mut guard = self.queue.lock().await;
        match guard.recv().await {
            Some(update) => Ok(Some(update)),
            // The pump only closes the queue when the session is over, and
            // returning an error is how a display says "stop polling me".
            None => Err(ServerError::reason(
                "display",
                "the session's frame queue closed",
            )),
        }
    }
}

/// The EGFX factory IronRDP calls once per connection.
///
/// `build_server_with_handle` is implemented so that *we* keep the
/// `GfxServerHandle`: the server would otherwise only keep the DVC bridge, and
/// the bridge's `server()` accessor is not enough for us because the display
/// (which lives on the pump thread) has to reach the pipeline to create surfaces
/// and drain output.
///
/// It is also the only object that can learn the connection's event sender:
/// `RdpServer::new` calls [`ServerEventSender::set_sender`] on the factory
/// before the connection starts, and the display never gets one.
#[cfg(feature = "egfx")]
pub struct SessionGfxFactory {
    handle: Arc<std::sync::Mutex<Option<GfxServerHandle>>>,
    events: EventSenderSlot,
}

// `GfxServerHandle` is not `Debug`, so the derive cannot be used here.
#[cfg(feature = "egfx")]
impl std::fmt::Debug for SessionGfxFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionGfxFactory")
            .field(
                "attached",
                &self.handle.lock().is_ok_and(|guard| guard.is_some()),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "egfx")]
impl ServerEventSender for SessionGfxFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
    }
}

#[cfg(feature = "egfx")]
impl GfxServerFactory for SessionGfxFactory {
    fn build_gfx_handler(
        &self,
        _ctx: ironrdp_server::GfxContext,
    ) -> Box<dyn ironrdp_egfx::server::GraphicsPipelineHandler> {
        Box::new(Handler)
    }

    fn build_server_with_handle(
        &self,
        _ctx: ironrdp_server::GfxContext,
    ) -> Option<(ironrdp_server::GfxDvcBridge, GfxServerHandle)> {
        let mut pipeline = ironrdp_egfx::server::GraphicsPipelineServer::new(Box::new(Handler));
        pipeline.set_max_frames_in_flight(30);
        let server = Arc::new(std::sync::Mutex::new(pipeline));
        *self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&server));
        Some((
            ironrdp_server::GfxDvcBridge::new(Arc::clone(&server)),
            server,
        ))
    }
}

/// EGFX pipeline handler.
///
/// Every callback here runs *while the pipeline mutex is held* — the DVC
/// processor locks the same `GfxServerHandle` the pump uses — so nothing in this
/// implementation may lock that handle, or a std mutex deadlock follows. The
/// pump therefore learns everything it needs by reading the pipeline when it
/// publishes, not from callbacks.
#[cfg(feature = "egfx")]
#[derive(Debug)]
struct Handler;

#[cfg(feature = "egfx")]
impl ironrdp_egfx::server::GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, _pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, _negotiated: &ironrdp_egfx::pdu::CapabilitySet) {}
}

/// The frame pump: owns the backend, publishes frames, drives H.264.
pub struct FramePump {
    thread: Option<std::thread::JoinHandle<Option<DesktopBackend>>>,
    sender: PumpSender,
}

/// Everything the pump needs that is not the backend itself.
pub struct PumpConfig {
    /// Shared display state (size, EGFX handle, suppression).
    pub shared: Arc<std::sync::Mutex<DisplayShared>>,
    /// Queue to publish updates into.
    pub updates: mpsc::Sender<DisplayUpdate>,
    /// Connection event sender, as captured by the gfx factory.
    pub events: EventSenderSlot,
    /// Frame-rate gate.
    pub pacer: Pacer,
    /// Tier to start with.
    pub tier: CodecTier,
    /// Bitmap tier to report and use until H.264 is available.
    pub bitmap_tier: CodecTier,
    /// Disconnect if a native client cannot use H.264.
    pub require_h264: bool,
    /// Encoder for the initial tier, when the tier needs one.
    pub encoder: Option<Box<dyn H264Backend>>,
    /// Builds replacement encoders for a downgrade.
    pub encoder_factory: EncoderFactory,
    /// Downgrade-once state machine.
    pub degrader: Degrader,
}

impl FramePump {
    /// Starts the pump thread.
    ///
    /// On failure the backend is dropped, whose `Drop` terminates the
    /// compositor, so a failed spawn cannot leak a nested Wayland server.
    pub fn spawn(
        backend: DesktopBackend,
        config: PumpConfig,
    ) -> Result<(Self, PumpSender), BackendError> {
        let (sender, receiver) = std::sync::mpsc::channel::<PumpCommand>();
        let sink = Sink::new(config);
        match std::thread::Builder::new()
            .name("rdp-frame-pump".to_owned())
            .spawn(move || run(backend, receiver, sink))
        {
            Ok(thread) => Ok((
                Self {
                    thread: Some(thread),
                    sender: sender.clone(),
                },
                sender,
            )),
            Err(error) => {
                warn!(error = %error, "cannot start the frame pump");
                Err(BackendError::Spawn {
                    command: "rdp-frame-pump".to_owned(),
                    reason: format!("cannot start the frame pump thread: {error}"),
                })
            }
        }
    }

    /// Command queue for input injection and resizes.
    pub fn sender(&self) -> &PumpSender {
        &self.sender
    }

    /// Stops the loop and returns the backend so the caller can shut it down
    /// (which is what actually terminates the compositor). `None` means the pump
    /// thread panicked, in which case the backend was dropped with it.
    pub fn join(mut self) -> Option<DesktopBackend> {
        let _ = self.sender.send(PumpCommand::Stop);
        let thread = self.thread.take()?;
        match thread.join() {
            Ok(backend) => backend,
            Err(_) => {
                warn!("frame pump panicked; the compositor was dropped with it");
                None
            }
        }
    }
}

impl Drop for FramePump {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.sender.send(PumpCommand::Stop);
            let _ = thread.join();
        }
    }
}

fn run(
    mut backend: DesktopBackend,
    receiver: std::sync::mpsc::Receiver<PumpCommand>,
    mut sink: Sink,
) -> Option<DesktopBackend> {
    let mut pending = None;
    let mut resize_target = None;
    loop {
        let mut requested = None;
        // Drain everything queued first: input must not wait behind a frame
        // wait, and a burst of input should cost one pass, not one per event.
        while let Ok(command) = receiver.try_recv() {
            match command {
                PumpCommand::Keyboard(event) => {
                    if let Err(error) = backend.inject_keyboard(event) {
                        sink.fail(&error);
                        return Some(backend);
                    }
                }
                PumpCommand::Pointer(event) => {
                    if let Err(error) = backend.inject_pointer(event) {
                        sink.fail(&error);
                        return Some(backend);
                    }
                }
                PumpCommand::Resize { width, height } => requested = Some((width, height)),
                PumpCommand::KeyFrame => sink.request_key_frame(),
                PumpCommand::Stop => return Some(backend),
            }
        }

        if let Some((width, height)) = requested
            && resize_target != Some((width, height))
            && (resize_target.is_some() || backend.size() != (width, height))
        {
            match backend.resize(width, height) {
                Ok(()) => {
                    resize_target = Some((width, height));
                    pending = None;
                    sink.resync = true;
                }
                Err(error) => warn!(width, height, %error, "compositor rejected desktop resize"),
            }
        }

        match backend.next_frame(POLL) {
            Ok(Some(frame)) => {
                if let Some(target) = resize_target {
                    if (frame.width, frame.height) != target {
                        continue;
                    }
                    resize_target = None;
                }
                if pending.replace(frame).is_some() {
                    // The new damage is relative to the frame we still owe.
                    sink.resync = true;
                }
            }
            Ok(None) => {}
            Err(error) => {
                sink.fail(&error);
                break;
            }
        }
        if let Some(frame) = &pending {
            let frames_sent = sink.frame_count;
            sink.publish(frame);
            if sink.frame_count != frames_sent && !sink.resync {
                pending = None;
            }
        }
    }
    Some(backend)
}

/// The pump's publishing side.
struct Sink {
    shared: Arc<std::sync::Mutex<DisplayShared>>,
    updates: mpsc::Sender<DisplayUpdate>,
    #[cfg(feature = "egfx")]
    events: EventSenderSlot,
    pacer: Pacer,
    tier: CodecTier,
    bitmap_tier: CodecTier,
    require_h264: bool,
    encoder: Option<Box<dyn H264Backend>>,
    // Only read by the AVC420 path, which exists solely in an egfx build.
    #[cfg_attr(not(feature = "egfx"), allow(dead_code))]
    encoder_factory: EncoderFactory,
    #[cfg_attr(not(feature = "egfx"), allow(dead_code))]
    degrader: Degrader,
    started: Instant,
    frame_count: u64,
    /// A frame was dropped (full queue, encoder hiccup, resize), so the client's
    /// copy is known stale and the next update must cover the whole desktop (or
    /// be an IDR) rather than a diff.
    resync: bool,
}

impl Sink {
    fn new(config: PumpConfig) -> Self {
        Self {
            shared: config.shared,
            updates: config.updates,
            #[cfg(feature = "egfx")]
            events: config.events,
            pacer: config.pacer,
            tier: config.tier,
            bitmap_tier: config.bitmap_tier,
            require_h264: config.require_h264,
            encoder: config.encoder,
            encoder_factory: config.encoder_factory,
            degrader: config.degrader,
            started: Instant::now(),
            frame_count: 0,
            resync: true,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DisplayShared> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request_key_frame(&mut self) {
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.request_key_frame();
        }
        self.resync = true;
    }

    /// Records a backend error as terminal.
    fn fail(&self, error: &BackendError) {
        let mut shared = self.lock();
        if shared.failed.is_none() {
            shared.failed = Some(error.to_string());
        }
        debug!(error = %error, "frame source stopped");
    }

    /// Send the new geometry before publishing any pixels at that size.
    /// IronRDP calls updates() again when the client is ready for those pixels.
    fn prepare_frame(&mut self, frame: &Frame) -> bool {
        let size = clamp_size(frame.width, frame.height);
        let mut shared = self.lock();
        if shared.awaiting_reactivation {
            return false;
        }
        if shared.size == size {
            return true;
        }
        if shared.suppression.is_none()
            || self.updates.try_send(DisplayUpdate::Resize(size)).is_err()
        {
            return false;
        }
        shared.size = size;
        shared.awaiting_reactivation = true;
        #[cfg(feature = "egfx")]
        {
            shared.surface = None;
        }
        drop(shared);
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.reset();
        }
        self.resync = true;
        false
    }

    /// Publishes one compositor frame.
    fn publish(&mut self, frame: &Frame) {
        if !self.prepare_frame(frame) {
            return;
        }
        let now = Instant::now();
        {
            let mut shared = self.lock();
            if suppression_engaged(&mut shared, now) {
                trace!("client suppressed output");
                return;
            }
        }
        if !self.pacer.poll(now) {
            return;
        }
        if self.tier.requires_h264() {
            match self.client_supports_avc() {
                Some(true) => {
                    let mut shared = self.lock();
                    let switched = shared.tier != self.tier;
                    shared.tier = self.tier;
                    drop(shared);
                    self.resync |= switched;
                    self.publish_avc420(frame, now);
                    return;
                }
                capability if self.require_h264 => {
                    if capability == Some(false) || now.duration_since(self.started) >= Duration::from_secs(5) {
                        self.lock().failed = Some("The client did not negotiate H.264 AVC420".to_owned());
                    }
                    return;
                }
                _ => self.lock().tier = self.bitmap_tier,
            }
        }
        match bitmap_updates(frame, self.resync) {
            Some(updates) => {
                let total = updates.len();
                let mut sent = 0;
                for update in updates {
                    if self.updates.try_send(update).is_err() {
                        break;
                    }
                    sent += 1;
                }
                if sent == 0 {
                    trace!("display queue full; frame dropped");
                    self.resync = true;
                    return;
                }
                // A partial send leaves the client with half a frame: keep the
                // resync flag so the rectangles that never made it into the
                // queue come back with the next update as a full refresh.
                self.resync = sent < total;
                if self.resync {
                    trace!(
                        sent,
                        total, "display queue full mid-frame; next update resyncs"
                    );
                }
                self.mark_sent(now);
            }
            None => debug!(
                width = frame.width,
                height = frame.height,
                "frame cannot be expressed as an RDP bitmap; skipped"
            ),
        }
    }

    fn mark_sent(&mut self, now: Instant) {
        self.frame_count += 1;
        let mut shared = self.lock();
        if !shared.first_frame_sent {
            shared.first_frame_sent = true;
            debug!(elapsed = ?now.duration_since(self.started), "first frame delivered");
        }
    }

    #[cfg(feature = "egfx")]
    fn client_supports_avc(&self) -> Option<bool> {
        let handle = self.lock().gfx.clone()?;
        let pipeline = handle.lock().ok()?;
        pipeline.is_ready().then(|| pipeline.supports_avc420())
    }

    #[cfg(not(feature = "egfx"))]
    fn client_supports_avc(&self) -> Option<bool> {
        Some(false)
    }

    #[cfg(feature = "egfx")]
    fn publish_avc420(&mut self, frame: &Frame, now: Instant) {
        let Some(handle) = self.lock().gfx.clone() else {
            return;
        };
        let Ok(pipeline) = handle.lock() else {
            self.give_up();
            return;
        };
        if !pipeline.is_ready() || pipeline.should_backpressure() {
            return;
        }
        drop(pipeline);

        let h264 = match self.encode(frame) {
            Ok(h264) => h264,
            Err(keep_running) => {
                if keep_running {
                    return;
                }
                self.give_up();
                return;
            }
        };
        if h264.is_empty() {
            return;
        }
        let regions = (!self.resync)
            .then(|| avc_regions(frame, AVC_QUANTIZATION_PARAMETER))
            .flatten()
            .unwrap_or_else(|| {
                vec![ironrdp_egfx::pdu::Avc420Region::full_frame(
                    u16::try_from(frame.width).unwrap_or(u16::MAX),
                    u16::try_from(frame.height).unwrap_or(u16::MAX),
                    AVC_QUANTIZATION_PARAMETER,
                )]
            });
        let timestamp_ms =
            u32::try_from(now.duration_since(self.started).as_millis()).unwrap_or(u32::MAX);

        // The handle is cloned out of `shared` before the pipeline mutex is
        // taken, and `shared` is re-locked only for short reads and writes: EGFX
        // runs its callbacks under the pipeline mutex, so holding `shared` across
        // a pipeline lock would invert the lock order with any callback that ever
        // takes it.
        let Ok(mut pipeline) = handle.lock() else {
            self.give_up();
            return;
        };
        let existing = self.lock().surface;
        let submitted = ensure_surface(&mut pipeline, existing, frame).and_then(|surface| {
            match pipeline.try_send_avc420_frame(surface.id, &h264, &regions, timestamp_ms) {
                Ok(_) => Some(surface),
                Err(error) => {
                    debug!(error = %error, "EGFX refused the frame");
                    None
                }
            }
        });
        let messages = if submitted.is_some() {
            drain(&mut pipeline)
        } else {
            Vec::new()
        };
        drop(pipeline);

        let mut shared = self.lock();
        match submitted {
            Some(surface) => {
                shared.surface = Some(SurfaceState {
                    id: surface.id,
                    width: u16::try_from(frame.width).unwrap_or(u16::MAX),
                    height: u16::try_from(frame.height).unwrap_or(u16::MAX),
                });
                drop(shared);
                self.resync = false;
                self.send_egfx(messages);
                self.mark_sent(now);
            }
            None => {
                shared.surface = None;
                drop(shared);
                self.request_key_frame();
                debug!("EGFX surface not ready; frame dropped");
            }
        }
    }

    #[cfg(not(feature = "egfx"))]
    fn publish_avc420(&mut self, _frame: &Frame, _now: Instant) {
        self.give_up();
    }

    /// Encodes one frame, applying the downgrade-once policy on failure.
    ///
    /// `Ok(vec)` is the Annex-B bitstream (possibly empty, meaning "nothing to
    /// send yet"); `Err(true)` means "skip this frame and stay alive",
    /// `Err(false)` means the session should end.
    #[cfg_attr(not(feature = "egfx"), allow(dead_code))]
    fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>, bool> {
        let width = frame.width;
        let height = frame.height;
        if !encodable(width, height) {
            debug!(width, height, "frame below the encoder's minimum size");
            return Ok(Vec::new());
        }
        let rgba = RgbaFrame::new(&frame.pixels, width, height);
        if let Err(error) = rgba.validate() {
            warn!(error = %error, "compositor frame is not encodable");
            return Ok(Vec::new());
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(self.handle_encoder_missing());
        };
        match encoder.encode(&rgba) {
            Ok(h264) => Ok(h264),
            Err(error) => {
                warn!(error = %error, tier = %self.tier, "H.264 encoder failed");
                if matches!(error, CodecError::InvalidFrame(_)) {
                    return Ok(Vec::new());
                }
                if self.require_h264 {
                    return Err(false);
                }
                match self.degrader.failure() {
                    DegradeAction::GiveUp { from } => {
                        warn!(tier = %from, "codec already at the lowest tier");
                        Err(false)
                    }
                    DegradeAction::DegradeTo(tier) => {
                        let replacement = (self.encoder_factory)(tier);
                        Err(!self.degrade_to(tier, replacement))
                    }
                }
            }
        }
    }

    /// Records a tier change; false when the new tier still has no encoder.
    #[cfg_attr(not(feature = "egfx"), allow(dead_code))]
    fn degrade_to(&mut self, tier: CodecTier, encoder: Option<Box<dyn H264Backend>>) -> bool {
        self.tier = tier;
        self.resync = true;
        self.encoder = encoder;
        let mut shared = self.lock();
        shared.tier = tier;
        shared.downgrades += 1;
        #[cfg(feature = "egfx")]
        {
            shared.surface = None;
        }
        debug!(tier = %tier, "changed the codec tier");
        self.encoder.is_some()
    }

    /// The tier needs an encoder and we do not have one.
    #[cfg_attr(not(feature = "egfx"), allow(dead_code))]
    fn handle_encoder_missing(&mut self) -> bool {
        if self.require_h264 {
            return false;
        }
        match self.degrader.failure() {
            DegradeAction::GiveUp { .. } => false,
            DegradeAction::DegradeTo(tier) => {
                let replacement = (self.encoder_factory)(tier);
                self.degrade_to(tier, replacement)
            }
        }
    }

    fn give_up(&mut self) {
        if self.require_h264 {
            let mut shared = self.lock();
            if shared.failed.is_none() {
                shared.failed = Some(format!("no encoder available for {}", self.tier));
            }
        } else {
            self.tier = self.bitmap_tier;
            self.encoder = None;
            self.resync = true;
            self.lock().tier = self.bitmap_tier;
        }
    }

    #[cfg(feature = "egfx")]
    fn send_egfx(&self, messages: Vec<ironrdp_svc::SvcMessage>) {
        let sender = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(sender) = sender else {
            trace!("no event sender yet; EGFX output dropped");
            return;
        };
        let event = ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages });
        if sender.send(event).is_err() {
            debug!("connection is gone; EGFX output dropped");
        }
    }
}

/// A frame the H.264 encoders can be handed.
///
/// The VAAPI drivers reject frames below 128px on either axis and OpenH264
/// needs even dimensions; both are checked here so the outcome is a logged skip
/// rather than an encoder error that would trigger a needless downgrade.
#[cfg_attr(not(feature = "egfx"), allow(dead_code))]
fn encodable(width: u32, height: u32) -> bool {
    width >= MIN_DIMENSION && height >= MIN_DIMENSION
}

/// Whether the output gate is currently closed.
///
/// Split out of [`Sink`] so the debounce is testable without a compositor: the
/// flag is the client's `SuppressOutput` pulse, `first_frame_sent` guards the
/// handshake, and `suppressed_since` is the debounce clock.
fn suppression_engaged(shared: &mut DisplayShared, now: Instant) -> bool {
    let Some(flag) = shared.suppression.as_ref() else {
        return false;
    };
    if !flag.load(Ordering::Relaxed) {
        shared.suppressed_since = None;
        return false;
    }
    if !shared.first_frame_sent {
        // Deliver at least one frame before honouring the flag.
        return false;
    }
    let since = *shared.suppressed_since.get_or_insert(now);
    now.saturating_duration_since(since) >= SUPPRESS_DEBOUNCE
}

/// Makes sure a surface of the frame's size exists and is mapped.
#[cfg(feature = "egfx")]
fn ensure_surface(
    pipeline: &mut ironrdp_egfx::server::GraphicsPipelineServer,
    existing: Option<SurfaceState>,
    frame: &Frame,
) -> Option<SurfaceState> {
    let width = u16::try_from(frame.width).ok()?;
    let height = u16::try_from(frame.height).ok()?;
    if !pipeline.is_ready() {
        return None;
    }
    if let Some(surface) = existing
        && surface.width == width
        && surface.height == height
        && pipeline.get_surface(surface.id).is_some()
    {
        return Some(surface);
    }
    // Send the single headless output as the primary monitor. Clients such
    // as FreeRDP reject the implicit ResetGraphics with an empty monitor list.
    use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
    pipeline.resize_with_monitors(
        width,
        height,
        vec![Monitor {
            left: 0,
            top: 0,
            right: i32::from(width) - 1,
            bottom: i32::from(height) - 1,
            flags: MonitorFlags::PRIMARY,
        }],
    );
    let id = pipeline.create_surface(width, height)?;
    let _ = pipeline.map_surface_to_output(id, 0, 0);
    Some(SurfaceState { id, width, height })
}

/// Drains the pipeline's pending messages, framing them for the DVC channel.
#[cfg(feature = "egfx")]
fn drain(
    pipeline: &mut ironrdp_egfx::server::GraphicsPipelineServer,
) -> Vec<ironrdp_svc::SvcMessage> {
    let Some(channel_id) = pipeline.channel_id() else {
        return Vec::new();
    };
    // Each message is already wrapped in its own ZGFX segment. Keep those
    // boundaries when the DVC encoder adds its transport framing.
    match ironrdp_dvc::encode_dvc_messages(
        channel_id,
        pipeline.drain_output(),
        ChannelFlags::empty(),
    ) {
        Ok(svc) => svc,
        Err(error) => {
            warn!(error = %error, "cannot frame EGFX output");
            Vec::new()
        }
    }
}

/// Turns a compositor frame into bitmap updates, one per dirty rectangle.
///
/// `resync` (set after a dropped frame) sends the whole frame instead of the
/// diff, because the client's copy is then known to be stale.
///
/// Returns `None` when the frame cannot be expressed at all (a dimension that
/// does not fit a `u16`).
fn bitmap_updates(frame: &Frame, resync: bool) -> Option<Vec<DisplayUpdate>> {
    let width = u16::try_from(frame.width).ok()?;
    let height = u16::try_from(frame.height).ok()?;
    if resync {
        return Some(vec![DisplayUpdate::Bitmap(full_frame_update(
            frame, width, height,
        )?)]);
    }
    let mut updates = Vec::with_capacity(frame.dirty.len().max(1));
    for rect in &frame.dirty {
        if let Some(update) = sub_update(frame, rect) {
            updates.push(update);
        }
    }
    if updates.is_empty() {
        updates.push(DisplayUpdate::Bitmap(full_frame_update(
            frame, width, height,
        )?));
    }
    Some(updates)
}

/// The whole frame as one update. Cloning the pixels is a reference-count
/// bump: the frame's buffer is shared with the capture path.
fn full_frame_update(frame: &Frame, width: u16, height: u16) -> Option<BitmapUpdate> {
    Some(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(width)?,
        height: NonZeroU16::new(height)?,
        format: PixelFormat::RgbA32,
        data: frame.pixels.clone(),
        stride: NonZeroUsize::new(usize::from(width) * 4)?,
    })
}

/// One dirty rectangle, copied straight out of the shared frame pixels.
///
/// Only the bytes inside the rectangle are copied, and the update carries that
/// rectangle's own stride, so a cursor blink costs 64 bytes rather than a
/// second desktop.
///
/// `BitmapUpdate::sub` is not used here: it bounds-checks with plain `u16`
/// arithmetic, which panics on overflow in a debug build, and it inherits the
/// parent stride, so a small rectangle would still ship a whole row of the
/// desktop. Out-of-frame rectangles are rejected with `u32` math instead.
fn sub_update(frame: &Frame, rect: &DirtyRect) -> Option<DisplayUpdate> {
    let width = NonZeroU16::new(rect.width)?;
    let height = NonZeroU16::new(rect.height)?;
    if u32::from(rect.x) + u32::from(rect.width) > frame.width
        || u32::from(rect.y) + u32::from(rect.height) > frame.height
    {
        return None;
    }
    let row_bytes = usize::from(rect.width) * 4;
    let frame_stride = usize::try_from(frame.width).ok()?.checked_mul(4)?;
    let left = usize::from(rect.x) * 4;
    let mut data = Vec::with_capacity(usize::from(rect.height) * row_bytes);
    for row in 0..usize::from(rect.height) {
        let start = (usize::from(rect.y) + row)
            .checked_mul(frame_stride)?
            .checked_add(left)?;
        data.extend_from_slice(frame.pixels.get(start..start + row_bytes)?);
    }
    Some(DisplayUpdate::Bitmap(BitmapUpdate {
        x: rect.x,
        y: rect.y,
        width,
        height,
        format: PixelFormat::RgbA32,
        data: Bytes::from(data),
        stride: NonZeroUsize::new(row_bytes)?,
    }))
}

/// Dirty rectangles as AVC420 regions, or `None` when they do not fit.
#[cfg(feature = "egfx")]
fn avc_regions(frame: &Frame, qp: u8) -> Option<Vec<ironrdp_egfx::pdu::Avc420Region>> {
    let width = u16::try_from(frame.width).ok()?;
    let height = u16::try_from(frame.height).ok()?;
    let mut regions = Vec::with_capacity(frame.dirty.len().max(1));
    for rect in &frame.dirty {
        let right = rect.x.saturating_add(rect.width);
        let bottom = rect.y.saturating_add(rect.height);
        if right > width || bottom > height {
            return None;
        }
        regions.push(ironrdp_egfx::pdu::Avc420Region::new(
            rect.x, rect.y, right, bottom, qp, 100,
        ));
    }
    if regions.is_empty() {
        regions.push(ironrdp_egfx::pdu::Avc420Region::full_frame(
            width, height, qp,
        ));
    }
    Some(regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, dirty: Vec<DirtyRect>) -> Frame {
        Frame {
            width,
            height,
            pixels: Bytes::from(vec![0u8; (width as usize) * (height as usize) * 4]),
            dirty,
        }
    }

    /// A frame whose pixel `n` is `n % 251`, so a copied rectangle can be
    /// checked byte by byte against the source.
    fn patterned_frame(width: u32, height: u32, dirty: Vec<DirtyRect>) -> Frame {
        let pixels: Vec<u8> = (0..(width as usize) * (height as usize) * 4)
            .map(|n| (n % 251) as u8)
            .collect();
        Frame {
            width,
            height,
            pixels: Bytes::from(pixels),
            dirty,
        }
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> DirtyRect {
        DirtyRect {
            x,
            y,
            width,
            height,
        }
    }

    fn shared() -> DisplayShared {
        DisplayShared {
            size: clamp_size(64, 64),
            max_size: clamp_size(1920, 1080),
            awaiting_reactivation: false,
            #[cfg(feature = "egfx")]
            gfx: None,
            #[cfg(feature = "egfx")]
            surface: None,
            tier: CodecTier::Qoi,
            downgrades: 0,
            failed: None,
            suppression: None,
            suppressed_since: None,
            first_frame_sent: false,
        }
    }

    #[test]
    fn clamp_size_floors_and_caps() {
        assert_eq!(clamp_size(0, 0).width, 1);
        assert_eq!(clamp_size(0, 0).height, 1);
        assert_eq!(clamp_size(1366, 768).height, 768);
        assert_eq!(clamp_size(u32::MAX, 10).width, u16::MAX);
    }

    #[test]
    fn a_frame_with_dirty_rects_becomes_one_update_per_rect() {
        let updates = bitmap_updates(
            &frame(64, 32, vec![rect(0, 0, 8, 8), rect(16, 8, 4, 4)]),
            false,
        )
        .expect("frame expressible");
        assert_eq!(updates.len(), 2);
        let DisplayUpdate::Bitmap(first) = &updates[0] else {
            panic!("expected a bitmap update");
        };
        assert_eq!((first.x, first.y), (0, 0));
        assert_eq!(first.format, PixelFormat::RgbA32);
        // A rectangle carries its own tightly packed rows, not the desktop's
        // stride: nothing outside the dirty region is sent.
        assert_eq!(first.data.len(), 8 * (8 * 4));
        assert_eq!(first.stride, NonZeroUsize::new(8 * 4).expect("non-zero"));
        let DisplayUpdate::Bitmap(second) = &updates[1] else {
            panic!("expected a bitmap update");
        };
        assert_eq!((second.x, second.y), (16, 8));
        assert_eq!(second.data.len(), 4 * (4 * 4));
        assert_eq!(second.stride, NonZeroUsize::new(4 * 4).expect("non-zero"));
    }

    #[test]
    fn a_rectangle_is_byte_identical_to_the_same_cut_of_the_full_frame() {
        // The old path built a whole-frame update and asked IronRDP for the
        // sub-rectangle; the new one copies the rows directly. Both must give
        // the client the same pixels.
        let frame = patterned_frame(64, 32, vec![rect(16, 8, 12, 6)]);
        let rect_update = match sub_update(&frame, &rect(16, 8, 12, 6)).expect("in frame") {
            DisplayUpdate::Bitmap(update) => update,
            _ => panic!("expected a bitmap update"),
        };
        let full = full_frame_update(&frame, 64, 32).expect("expressible");
        let reference = full
            .sub(
                16,
                8,
                NonZeroU16::new(12).expect("non-zero"),
                NonZeroU16::new(6).expect("non-zero"),
            )
            .expect("in bounds");
        assert_eq!((rect_update.x, rect_update.y), (reference.x, reference.y));
        assert_eq!(
            (rect_update.width, rect_update.height),
            (reference.width, reference.height)
        );
        assert_eq!(rect_update.format, reference.format);
        // `sub` keeps the parent stride, so compare row by row.
        let parent_stride = reference.stride.get();
        let row_bytes = usize::from(rect_update.width.get()) * 4;
        assert_eq!(
            rect_update.data.len(),
            usize::from(rect_update.height.get()) * row_bytes
        );
        for row in 0..usize::from(rect_update.height.get()) {
            let from = row * parent_stride;
            assert_eq!(
                &rect_update.data[row * row_bytes..row * row_bytes + row_bytes],
                &reference.data[from..from + row_bytes],
                "row {row}"
            );
        }
    }

    #[test]
    fn a_small_dirty_rect_copies_only_its_own_bytes() {
        // 1280x720 desktop, 8x8 dirty: the update must be 256 bytes, not 3.5 MB.
        let frame = patterned_frame(1280, 720, vec![rect(640, 360, 8, 8)]);
        let updates = bitmap_updates(&frame, false).expect("expressible");
        assert_eq!(updates.len(), 1);
        let DisplayUpdate::Bitmap(update) = &updates[0] else {
            panic!("expected a bitmap update");
        };
        assert_eq!(update.data.len(), 8 * 8 * 4);
        let expected = &frame.pixels[(360 * 1280 + 640) * 4..(360 * 1280 + 640) * 4 + 4];
        assert_eq!(&update.data[..4], expected);
        // The last row of the rectangle, to prove the rows come from the right
        // place and are tightly packed.
        let last_row = (367 * 1280 + 640) * 4;
        assert_eq!(
            &update.data[7 * 32..7 * 32 + 4],
            &frame.pixels[last_row..last_row + 4]
        );
    }

    #[test]
    fn a_frame_without_dirty_rects_sends_everything() {
        let updates = bitmap_updates(&frame(8, 8, Vec::new()), false).expect("expressible");
        assert_eq!(updates.len(), 1);
        let DisplayUpdate::Bitmap(update) = &updates[0] else {
            panic!("expected a bitmap update");
        };
        assert_eq!(update.data.len(), 8 * 8 * 4);
    }

    #[test]
    fn resync_sends_the_whole_frame_ignoring_the_diff() {
        let updates = bitmap_updates(
            &frame(16, 16, vec![rect(0, 0, 4, 4), rect(8, 8, 4, 4)]),
            true,
        )
        .expect("expressible");
        assert_eq!(updates.len(), 1);
        let DisplayUpdate::Bitmap(update) = &updates[0] else {
            panic!("expected a bitmap update");
        };
        assert_eq!(update.data.len(), 16 * 16 * 4);
    }

    #[test]
    fn a_dirty_rect_outside_the_frame_is_dropped_rather_than_panicking() {
        // 60000 + 60000 overflows u16, which is what BitmapUpdate::sub would do
        // in a debug build if the rectangle were passed straight through.
        let frame = frame(64, 64, Vec::new());
        assert!(sub_update(&frame, &rect(60_000, 60_000, 60_000, 60_000)).is_none());
        assert!(sub_update(&frame, &rect(0, 0, 0, 4)).is_none());
        assert!(sub_update(&frame, &rect(64, 0, 4, 4)).is_none());
        assert!(sub_update(&frame, &rect(0, 64, 4, 4)).is_none());
        assert!(sub_update(&frame, &rect(56, 56, 8, 8)).is_some());
    }

    #[test]
    fn a_frame_wider_than_u16_cannot_be_expressed() {
        let too_wide = Frame {
            width: 70_000,
            height: 8,
            pixels: Bytes::new(),
            dirty: Vec::new(),
        };
        assert!(bitmap_updates(&too_wide, false).is_none());
        assert!(bitmap_updates(&too_wide, true).is_none());
    }

    #[test]
    #[cfg(feature = "egfx")]
    fn dirty_rects_become_avc_regions_with_exclusive_bounds() {
        let regions = avc_regions(&frame(64, 32, vec![rect(4, 4, 16, 8)]), 18).expect("fits");
        assert_eq!(regions.len(), 1);
        assert_eq!((regions[0].left, regions[0].top), (4, 4));
        assert_eq!((regions[0].right, regions[0].bottom), (20, 12));
        assert_eq!(regions[0].quantization_parameter, 18);
    }

    #[test]
    #[cfg(feature = "egfx")]
    fn avc_regions_fall_back_to_the_whole_frame_and_reject_oversized_frames() {
        let regions = avc_regions(&frame(64, 32, Vec::new()), 20).expect("fits");
        assert_eq!(regions.len(), 1);
        assert_eq!((regions[0].right, regions[0].bottom), (64, 32));
        assert!(avc_regions(&frame(64, 32, vec![rect(60, 0, 8, 4)]), 20).is_none());
    }

    #[test]
    fn a_partial_send_keeps_the_resync_flag_set() {
        // A queue that fills up mid-frame leaves the client with half a frame.
        // Clearing the resync flag there loses every rectangle that never made
        // it in, until those pixels happen to change again.
        let (tx, mut rx) = mpsc::channel(1);
        let mut sink = Sink::new(PumpConfig {
            shared: Arc::new(std::sync::Mutex::new(shared())),
            updates: tx,
            events: Arc::new(std::sync::Mutex::new(None)),
            pacer: Pacer::new(0, Instant::now()),
            tier: CodecTier::Qoi,
            bitmap_tier: CodecTier::Qoi,
            require_h264: false,
            encoder: None,
            encoder_factory: Box::new(|_| None),
            degrader: Degrader::new(CodecTier::Qoi),
        });
        sink.resync = false;
        let dirty = vec![rect(0, 0, 8, 8), rect(16, 16, 8, 8)];

        sink.publish(&frame(64, 64, dirty.clone()));
        assert_eq!(sink.resync, true, "the second rectangle was dropped");
        assert!(rx.try_recv().is_ok(), "the first rectangle went out");

        // The queue has room again: the next update must be a whole frame, not
        // a diff of the rectangles that changed since the partial send.
        sink.publish(&frame(64, 64, dirty.clone()));
        let update = rx.try_recv().expect("an update");
        let DisplayUpdate::Bitmap(bitmap) = update else {
            panic!("expected a bitmap update");
        };
        assert_eq!(bitmap.data.len(), 64 * 64 * 4);
        assert!(!sink.resync, "a complete send clears the flag");
    }

    #[test]
    fn a_full_queue_resyncs_and_a_complete_send_clears_it() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut sink = Sink::new(PumpConfig {
            shared: Arc::new(std::sync::Mutex::new(shared())),
            updates: tx,
            events: Arc::new(std::sync::Mutex::new(None)),
            pacer: Pacer::new(0, Instant::now()),
            tier: CodecTier::Qoi,
            bitmap_tier: CodecTier::Qoi,
            require_h264: false,
            encoder: None,
            encoder_factory: Box::new(|_| None),
            degrader: Degrader::new(CodecTier::Qoi),
        });
        // The first frame is a resync and fits.
        sink.publish(&frame(64, 64, vec![rect(0, 0, 8, 8)]));
        assert!(!sink.resync);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn resize_is_sent_before_the_first_full_frame_at_the_new_size() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut state = shared();
        state.suppression = Some(Arc::new(AtomicBool::new(false)));
        let mut sink = Sink::new(PumpConfig {
            shared: Arc::new(std::sync::Mutex::new(state)),
            updates: tx,
            events: Arc::new(std::sync::Mutex::new(None)),
            pacer: Pacer::new(0, Instant::now()),
            tier: CodecTier::Qoi,
            bitmap_tier: CodecTier::Qoi,
            require_h264: false,
            encoder: None,
            encoder_factory: Box::new(|_| None),
            degrader: Degrader::new(CodecTier::Qoi),
        });
        let frame = frame(320, 240, vec![rect(0, 0, 8, 8)]);

        sink.publish(&frame);
        assert!(
            matches!(rx.try_recv().unwrap(), DisplayUpdate::Resize(size) if size == clamp_size(320, 240))
        );
        sink.publish(&frame);
        assert!(rx.try_recv().is_err());

        sink.lock().awaiting_reactivation = false;
        sink.publish(&frame);
        assert!(
            matches!(rx.try_recv().unwrap(), DisplayUpdate::Bitmap(bitmap)
            if bitmap.width.get() == 320 && bitmap.height.get() == 240)
        );
    }

    #[test]
    fn frames_below_the_encoder_minimum_are_not_encoded() {
        assert!(encodable(MIN_DIMENSION, MIN_DIMENSION));
        assert!(!encodable(MIN_DIMENSION - 1, 1080));
        assert!(!encodable(1920, 16));
    }

    #[test]
    fn suppression_needs_a_first_frame_and_a_steady_flag() {
        let mut shared = shared();
        let flag = Arc::new(AtomicBool::new(true));
        shared.suppression = Some(Arc::clone(&flag));
        let t0 = Instant::now();

        // No frame delivered yet: the handshake needs one, and the clock stays off.
        assert!(!suppression_engaged(&mut shared, t0));
        assert!(
            shared.suppressed_since.is_none(),
            "no clock before the first frame"
        );

        // The gate does not close the moment the flag arrives — it only starts
        // counting, so a brief SuppressOutput pulse cannot stall a session.
        shared.first_frame_sent = true;
        assert!(!suppression_engaged(&mut shared, t0));
        assert!(shared.suppressed_since.is_some(), "starts the clock");
        assert!(
            !suppression_engaged(&mut shared, t0 + SUPPRESS_DEBOUNCE / 2),
            "a short pulse is ignored"
        );
        assert!(
            suppression_engaged(&mut shared, t0 + SUPPRESS_DEBOUNCE),
            "a steady flag is honoured"
        );

        flag.store(false, Ordering::Relaxed);
        assert!(!suppression_engaged(
            &mut shared,
            t0 + SUPPRESS_DEBOUNCE * 4
        ));
        assert!(shared.suppressed_since.is_none(), "the clock resets");
    }

    #[test]
    fn no_suppression_flag_never_gates_output() {
        let mut shared = shared();
        shared.first_frame_sent = true;
        assert!(!suppression_engaged(&mut shared, Instant::now()));
    }
}
