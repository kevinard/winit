//! Opt-in data-device transport on the window's own Wayland connection.
//! Callbacks only wake consumers. Pipe contents and application policy belong
//! to the caller, outside the native dispatch thread.
use super::{state::WinitState, window::WindowRequests, WindowId};
use calloop::ping::Ping;
use sctk::{
    compositor::SurfaceData,
    data_device_manager::DataDeviceManagerState,
    shm::{
        slot::{Buffer, SlotPool},
        Shm,
    },
    reexports::client::{
        backend::ObjectId,
        globals::GlobalList,
        protocol::{
            wl_compositor::WlCompositor, wl_shm::Format,
            wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
            wl_data_offer::WlDataOffer, wl_data_source::WlDataSource, wl_seat::WlSeat,
            wl_surface::WlSurface,
        },
        Proxy, QueueHandle,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    os::fd::OwnedFd,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

mod handlers;
mod pipes;
pub use pipes::{read_pipe, write_pipe, PipeTransfer};
const MAX_LIVE: usize = 32;

/// Copy and Move are the supported negotiated actions. Ask is not implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragAction {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DragActions {
    pub copy: bool,
    pub move_files: bool,
}
impl DragActions {
    fn wire(self) -> DndAction {
        let mut actions = DndAction::empty();
        if self.copy {
            actions |= DndAction::Copy;
        }
        if self.move_files {
            actions |= DndAction::Move;
        }
        actions
    }
    fn from_wire(actions: DndAction) -> Self {
        Self {
            copy: actions.contains(DndAction::Copy),
            move_files: actions.contains(DndAction::Move),
        }
    }
    fn contains(self, action: DragAction) -> bool {
        match action {
            DragAction::Copy => self.copy,
            DragAction::Move => self.move_files,
        }
    }
}
impl DragAction {
    fn wire(self) -> DndAction {
        match self {
            Self::Copy => DndAction::Copy,
            Self::Move => DndAction::Move,
        }
    }
    fn from_wire(action: DndAction) -> Option<Self> {
        match action {
            DndAction::Copy => Some(Self::Copy),
            DndAction::Move => Some(Self::Move),
            _ => None,
        }
    }
}

/// Stable state, including a latched Drop that is not erased by Leave.
#[derive(Debug, Clone)]
pub struct OfferSnapshot {
    pub id: u64,
    /// Surface-local logical coordinates, before an application's own scale.
    pub position: (f64, f64),
    pub actions: DragActions,
    pub selected: Option<DragAction>,
    pub dropped: bool,
    pub left: bool,
    pub finished: bool,
    /// Actual source on this client and seat, never a MIME marker supplied by a peer.
    pub local_source: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    pub id: u64,
    pub dropped: bool,
    pub finished: bool,
    pub cancelled: bool,
    pub selected: Option<DragAction>,
}

pub type Changed = Arc<dyn Fn() + Send + Sync>;
/// Must return promptly, moving the owned descriptor to a bounded I/O worker.
/// File-reference receivers own mutations; source completion never deletes data.
pub type SendData = Arc<dyn Fn(String, OwnedFd) + Send + Sync>;

struct Endpoint {
    hub: Hub,
    window: WindowId,
    surface: WlSurface,
    requests: Weak<WindowRequests>,
    ping: Ping,
    changed: Changed,
    closed: AtomicBool,
    offers: Mutex<HashMap<u64, Offer>>,
    presses: Mutex<(usize, VecDeque<Arc<Press>>)>,
}

/// Exact native left press, captured in client input dispatch order. A release
/// or newer press invalidates it; asynchronous preparation cannot borrow a later grab.
#[derive(Clone)]
pub struct Origin {
    endpoint: Weak<Endpoint>,
    press: Arc<Press>,
}
impl std::fmt::Debug for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Native drag origin")
    }
}

/// One window's endpoint. Keep its notices alive while an outgoing source is
/// awaiting completion, even if the originating window has closed.
#[derive(Clone)]
pub struct DragDrop(Arc<Endpoint>);
impl std::fmt::Debug for DragDrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wayland drag endpoint")
    }
}

#[derive(Default)]
struct Announcement {
    formats: Vec<String>,
    actions: DragActions,
    selected: Option<DragAction>,
}
#[derive(Default)]
struct OfferData(Mutex<Announcement>);
struct DeviceData(WlSeat);
struct SourceData;
struct NativeOffer {
    proxy: WlDataOffer,
    serial: u32,
}
struct OfferState {
    snapshot: OfferSnapshot,
    native: NativeOffer,
    accepted: bool,
    negotiation: Option<(Option<String>, DragActions, Option<DragAction>)>,
}
struct Incoming {
    endpoint: Weak<Endpoint>,
    state: Mutex<OfferState>,
}
#[derive(Clone)]
pub struct Offer(Arc<Incoming>);
impl std::fmt::Debug for Offer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(f)
    }
}

struct SourceState {
    snapshot: SourceSnapshot,
    native: Option<WlDataSource>,
}
struct Outgoing {
    endpoint: Arc<Endpoint>,
    seat: ObjectId,
    state: Mutex<SourceState>,
    send: Mutex<Option<SendData>>,
    /// The drag icon's surface and pixels, released when the drag ends.
    icon: Mutex<Option<(WlSurface, Buffer)>>,
}
#[derive(Clone)]
pub struct Source(Arc<Outgoing>);
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(f)
    }
}

struct Press {
    window: WindowId,
    serial: u32,
    held: AtomicBool,
    consumed: AtomicBool,
    seat: ObjectId,
}
struct Seat {
    proxy: WlSeat,
    device: Option<WlDataDevice>,
    press: Option<Arc<Press>>,
}
struct State {
    next: u64,
    enabled: bool,
    seats: HashMap<ObjectId, Seat>,
    endpoints: HashMap<WindowId, Weak<Endpoint>>,
    sources: HashMap<ObjectId, Source>,
    offers: HashMap<ObjectId, Weak<Incoming>>,
    current: HashMap<ObjectId, Offer>,
    pending: HashMap<ObjectId, WlDataOffer>,
}
struct Shared {
    manager: DataDeviceManagerState,
    queue: QueueHandle<WinitState>,
    state: Mutex<State>,
    /// Drag icons: their surfaces and the shared memory their pixels use.
    compositor: WlCompositor,
    shm: Option<Shm>,
    icons: Mutex<Option<SlotPool>>,
}

/// A picture that follows the pointer while dragging, in straight
/// (not premultiplied) RGBA rows, `scale` buffer pixels per logical pixel.
/// `hotspot` is the logical point of the picture that sits under the pointer.
#[derive(Clone, Debug)]
pub struct DragIcon {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub rgba: Vec<u8>,
    pub hotspot: (i32, i32),
}

const MAX_ICON_SIDE: u32 = 1024;

impl DragIcon {
    fn valid(&self) -> bool {
        (1..=MAX_ICON_SIDE).contains(&self.width)
            && (1..=MAX_ICON_SIDE).contains(&self.height)
            && (1..=4).contains(&self.scale)
            && self.width % self.scale == 0
            && self.height % self.scale == 0
            && self.rgba.len() == (self.width * self.height * 4) as usize
    }
}
#[derive(Clone)]
pub(crate) struct Hub(Arc<Shared>);

fn error(message: &str) -> io::Error {
    io::Error::other(message)
}
impl State {
    fn next(&mut self) -> io::Result<u64> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| error("Drag identities exhausted"))?;
        Ok(self.next)
    }
}

impl Hub {
    pub fn shutdown(&self) {
        let (sources, offers, pending, seats) = {
            let mut state = self.0.state.lock().unwrap();
            let sources: Vec<_> = state.sources.values().cloned().collect();
            let offers: Vec<_> = state.offers.values().filter_map(Weak::upgrade).collect();
            let pending = std::mem::take(&mut state.pending);
            let seats = std::mem::take(&mut state.seats);
            state.current.clear();
            state.offers.clear();
            state.endpoints.clear();
            (sources, offers, pending, seats)
        };
        for source in sources {
            source.cancel();
        }
        for offer in offers {
            let mut state = offer.state.lock().unwrap();
            state.snapshot.finished = true;
            if state.native.proxy.is_alive() {
                state.native.proxy.destroy();
            }
        }
        for offer in pending.into_values() {
            if offer.is_alive() {
                offer.destroy();
            }
        }
        for seat in seats.into_values() {
            if let Some(device) = seat.device {
                if device.is_alive() {
                    device.release();
                }
            }
        }
    }

    pub fn bind(
        globals: &GlobalList,
        queue: &QueueHandle<WinitState>,
        compositor: WlCompositor,
    ) -> Option<Self> {
        let manager = DataDeviceManagerState::bind(globals, queue).ok()?;
        if manager.data_device_manager().version() < 3 {
            return None;
        }
        Some(Self(Arc::new(Shared {
            manager,
            queue: queue.clone(),
            compositor,
            // Without shared memory, drags still work, without an icon.
            shm: Shm::bind(globals, queue).ok(),
            icons: Mutex::new(None),
            state: Mutex::new(State {
                next: 0,
                enabled: false,
                seats: HashMap::new(),
                endpoints: HashMap::new(),
                sources: HashMap::new(),
                offers: HashMap::new(),
                current: HashMap::new(),
                pending: HashMap::new(),
            }),
        })))
    }

    pub fn add_seat(&self, proxy: WlSeat) {
        let mut state = self.0.state.lock().unwrap();
        if state.seats.contains_key(&proxy.id()) || state.seats.len() >= MAX_LIVE {
            return;
        }
        let device = state.enabled.then(|| {
            self.0.manager.data_device_manager().get_data_device(
                &proxy,
                &self.0.queue,
                DeviceData(proxy.clone()),
            )
        });
        state.seats.insert(
            proxy.id(),
            Seat {
                proxy,
                device,
                press: None,
            },
        );
    }

    pub fn remove_seat(&self, seat: &ObjectId) {
        let (sources, incoming, device) = {
            let mut state = self.0.state.lock().unwrap();
            let device = state.seats.remove(seat).and_then(|seat| seat.device);
            let incoming = state.current.remove(seat);
            let sources: Vec<_> = state
                .sources
                .values()
                .filter(|source| &source.0.seat == seat)
                .cloned()
                .collect();
            (sources, incoming, device)
        };
        for source in sources {
            source.cancel();
        }
        if let Some(offer) = incoming {
            self.leave_offer(offer);
        }
        if let Some(device) = device {
            if device.is_alive() {
                device.release();
            }
        }
    }

    pub fn pointer(&self, window: WindowId, seat: &WlSeat, serial: u32, held: bool) {
        let mut state = self.0.state.lock().unwrap();
        let endpoint = state.endpoints.get(&window).and_then(Weak::upgrade);
        let seat_id = seat.id();
        if let Some(seat) = state.seats.get_mut(&seat_id) {
            if held {
                if let Some(previous) = seat.press.take() {
                    previous.held.store(false, Ordering::Release);
                }
                let press = Arc::new(Press {
                    window,
                    serial,
                    held: AtomicBool::new(true),
                    consumed: AtomicBool::new(false),
                    seat: seat_id,
                });
                seat.press = Some(press.clone());
                if let Some(endpoint) = endpoint {
                    let mut queue = endpoint.presses.lock().unwrap();
                    if queue.1.len() >= 64 {
                        queue.1.pop_front();
                        queue.0 = queue.0.saturating_add(1);
                    }
                    queue.1.push_back(press);
                }
            } else if let Some(press) = &seat.press {
                press.held.store(false, Ordering::Release);
            }
        }
    }

    // Process logical Escape in native dispatch order, before a later Drop can
    // overtake the application's queued keyboard message. Accepted drops retain
    // their receipt and work; only this seat's undropped gestures are revoked.
    pub fn escape(&self, seat: &ObjectId) {
        let sources: Vec<_> = {
            let state = self.0.state.lock().unwrap();
            if let Some(press) = state.seats.get(seat).and_then(|seat| seat.press.as_ref()) {
                press.consumed.store(true, Ordering::Release);
            }
            state
                .sources
                .values()
                .filter(|source| {
                    let snapshot = source.snapshot();
                    &source.0.seat == seat && !snapshot.dropped && !snapshot.finished
                })
                .cloned()
                .collect()
        };
        // Source teardown takes the hub lock, so it must run outside that lock.
        for source in sources {
            source.cancel();
        }
    }

    pub fn endpoint(
        &self,
        window: WindowId,
        surface: WlSurface,
        requests: Weak<WindowRequests>,
        ping: Ping,
        changed: Changed,
    ) -> Option<DragDrop> {
        let mut state = self.0.state.lock().unwrap();
        state
            .endpoints
            .retain(|_, endpoint| endpoint.strong_count() > 0);
        if state.endpoints.len() >= MAX_LIVE
            || state
                .endpoints
                .get(&window)
                .and_then(Weak::upgrade)
                .is_some()
        {
            return None;
        }
        state.enabled = true;
        for seat in state.seats.values_mut() {
            if seat.device.is_none() {
                seat.device = Some(self.0.manager.data_device_manager().get_data_device(
                    &seat.proxy,
                    &self.0.queue,
                    DeviceData(seat.proxy.clone()),
                ));
            }
        }
        let endpoint = Arc::new(Endpoint {
            hub: self.clone(),
            window,
            surface,
            requests,
            ping,
            changed,
            closed: AtomicBool::new(false),
            offers: Mutex::new(HashMap::new()),
            presses: Mutex::new((0, VecDeque::new())),
        });
        state.endpoints.insert(window, Arc::downgrade(&endpoint));
        endpoint.ping.ping();
        Some(DragDrop(endpoint))
    }

    pub fn close_window(&self, window: WindowId) {
        let (endpoint, sources) = {
            let mut state = self.0.state.lock().unwrap();
            let endpoint = state
                .endpoints
                .remove(&window)
                .and_then(|weak| weak.upgrade());
            for seat in state.seats.values_mut() {
                if seat
                    .press
                    .as_ref()
                    .is_some_and(|press| press.window == window)
                {
                    if let Some(press) = &seat.press {
                        press.held.store(false, Ordering::Release);
                    }
                    seat.press = None;
                }
            }
            let sources: Vec<_> = state
                .sources
                .values()
                .filter(|source| source.0.endpoint.window == window)
                .cloned()
                .collect();
            (endpoint, sources)
        };
        if let Some(endpoint) = endpoint {
            endpoint.closed.store(true, Ordering::Release);
            let offers: Vec<_> = endpoint.offers.lock().unwrap().values().cloned().collect();
            for offer in offers {
                if !offer.snapshot().dropped {
                    let _ = offer.accept(None, DragActions::default(), None);
                }
            }
        }
        for source in sources {
            if !source.snapshot().dropped {
                source.cancel();
            }
        }
    }
}

impl DragDrop {
    /// Consume one press record for EVERY client left-button press, in dispatch
    /// order, including presses that do not start a drag. Expired/overflowed
    /// records return None and never substitute a newer press's authority.
    pub fn capture_origin(&self) -> Option<Origin> {
        let mut queue = self.0.presses.lock().unwrap();
        if queue.0 > 0 {
            queue.0 -= 1;
            return None;
        }
        let press = queue.1.pop_front()?;
        if !press.held.load(Ordering::Acquire) || press.consumed.load(Ordering::Acquire) {
            return None;
        }
        Some(Origin {
            endpoint: Arc::downgrade(&self.0),
            press,
        })
    }

    pub fn offers(&self) -> Vec<Offer> {
        self.0.offers.lock().unwrap().values().cloned().collect()
    }

    /// Starts only from this window's currently held, unconsumed left-button
    /// serial. Source actions are declared once, before the native request.
    pub fn start(
        &self,
        origin: &Origin,
        formats: Vec<String>,
        actions: DragActions,
        send: SendData,
    ) -> io::Result<Source> {
        self.start_with_icon(origin, formats, actions, send, None)
    }

    /// As [`Self::start`], with `icon` following the pointer until the drag
    /// ends. An icon the compositor's shared memory cannot hold is left out.
    pub fn start_with_icon(
        &self,
        origin: &Origin,
        formats: Vec<String>,
        actions: DragActions,
        send: SendData,
        icon: Option<&DragIcon>,
    ) -> io::Result<Source> {
        if icon.is_some_and(|icon| !icon.valid()) {
            return Err(error("Invalid drag icon"));
        }
        if self.0.closed.load(Ordering::Acquire)
            || !self.0.surface.is_alive()
            || self
                .0
                .requests
                .upgrade()
                .is_none_or(|requests| requests.closed.load(Ordering::Acquire))
        {
            return Err(error("The drag origin has closed"));
        }
        if formats.is_empty()
            || formats.len() > MAX_LIVE
            || actions.wire().is_empty()
            || formats
                .iter()
                .any(|mime| mime.is_empty() || mime.len() > 256 || mime.as_bytes().contains(&0))
        {
            return Err(error("Invalid or excessive drag source formats/actions"));
        }
        let mut state = self.0.hub.0.state.lock().unwrap();
        if state.sources.len() >= MAX_LIVE {
            return Err(error("Too many active drag sources"));
        }
        if !origin
            .endpoint
            .upgrade()
            .is_some_and(|endpoint| Arc::ptr_eq(&endpoint, &self.0))
        {
            return Err(error("The pointer grab belongs to another window"));
        }
        let seat_id = origin.press.seat.clone();
        if !state
            .seats
            .get(&seat_id)
            .and_then(|seat| seat.press.as_ref())
            .is_some_and(|press| {
                Arc::ptr_eq(press, &origin.press)
                    && press.window == self.0.window
                    && press.held.load(Ordering::Acquire)
                    && !press.consumed.load(Ordering::Acquire)
            })
        {
            return Err(error("The original pointer grab has ended"));
        }
        let id = state.next()?;
        let seat = state.seats.get_mut(&seat_id).unwrap();
        let native = self
            .0
            .hub
            .0
            .manager
            .data_device_manager()
            .create_data_source(&self.0.hub.0.queue, SourceData);
        for mime in formats {
            native.offer(mime);
        }
        native.set_actions(actions.wire());
        let press = seat.press.as_ref().unwrap();
        let device = seat
            .device
            .as_ref()
            .ok_or_else(|| error("The drag data device is unavailable"))?;
        let icon = icon.and_then(|icon| self.icon_surface(icon));
        device.start_drag(
            Some(&native),
            &self.0.surface,
            icon.as_ref().map(|(surface, ..)| surface),
            press.serial,
        );
        // The icon's content is committed once it has the drag icon role.
        if let Some((surface, buffer, (x, y))) = &icon {
            let (x, y) = (*x, *y);
            if surface.version() >= 5 {
                surface.attach(Some(buffer.wl_buffer()), 0, 0);
                surface.offset(x, y);
            } else {
                surface.attach(Some(buffer.wl_buffer()), x, y);
            }
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            surface.commit();
        }
        press.consumed.store(true, Ordering::Release);
        let key = native.id();
        let source = Source(Arc::new(Outgoing {
            endpoint: self.0.clone(),
            seat: seat_id,
            state: Mutex::new(SourceState {
                native: Some(native),
                snapshot: SourceSnapshot {
                    id,
                    dropped: false,
                    finished: false,
                    cancelled: false,
                    selected: None,
                },
            }),
            send: Mutex::new(Some(send)),
            icon: Mutex::new(icon.map(|(surface, buffer, _)| (surface, buffer))),
        }));
        state.sources.insert(key, source.clone());
        self.0.ping.ping();
        Ok(source)
    }
}

impl DragDrop {
    /// A surface showing `icon`, with the offset that puts its hotspot under
    /// the pointer. `None` when shared memory is unavailable or exhausted.
    fn icon_surface(&self, icon: &DragIcon) -> Option<(WlSurface, Buffer, (i32, i32))> {
        let hub = &self.0.hub.0;
        let shm = hub.shm.as_ref()?;
        let mut pool = hub.icons.lock().unwrap();
        let bytes = (icon.width * icon.height * 4) as usize;
        if pool.is_none() {
            *pool = SlotPool::new(bytes, shm).ok();
        }
        let pool = pool.as_mut()?;
        let (buffer, canvas) = pool
            .create_buffer(
                icon.width as i32,
                icon.height as i32,
                4 * icon.width as i32,
                Format::Argb8888,
            )
            .ok()?;
        for (pixel, rgba) in canvas.chunks_exact_mut(4).zip(icon.rgba.chunks_exact(4)) {
            // Shared memory pixels are premultiplied ARGB, little endian.
            let alpha = u32::from(rgba[3]);
            let channel = |value: u8| u32::from(value) * alpha / 255;
            let color = (alpha << 24)
                | (channel(rgba[0]) << 16)
                | (channel(rgba[1]) << 8)
                | channel(rgba[2]);
            pixel.copy_from_slice(&color.to_le_bytes());
        }
        let surface =
            hub.compositor.create_surface(&hub.queue, SurfaceData::new(None, icon.scale as i32));
        surface.set_buffer_scale(icon.scale as i32);
        Some((surface, buffer, (-icon.hotspot.0, -icon.hotspot.1)))
    }
}

impl Source {
    pub fn snapshot(&self) -> SourceSnapshot {
        self.0.state.lock().unwrap().snapshot.clone()
    }
    pub fn cancel(&self) {
        self.end(true);
    }
    fn end(&self, cancelled: bool) {
        let native = {
            let mut state = self.0.state.lock().unwrap();
            if state.snapshot.finished {
                return;
            }
            state.snapshot.finished = true;
            state.snapshot.cancelled = cancelled;
            state.native.take()
        };
        if let Some(native) = native {
            self.0
                .endpoint
                .hub
                .0
                .state
                .lock()
                .unwrap()
                .sources
                .remove(&native.id());
            if native.is_alive() {
                native.destroy();
            }
        }
        self.0.send.lock().unwrap().take();
        if let Some((surface, _buffer)) = self.0.icon.lock().unwrap().take() {
            if surface.is_alive() {
                surface.destroy();
            }
        }
        self.0.endpoint.ping.ping();
        (self.0.endpoint.changed)();
    }
}

impl Offer {
    pub fn snapshot(&self) -> OfferSnapshot {
        self.0.state.lock().unwrap().snapshot.clone()
    }
    pub fn has_format(&self, mime: &str) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .native
            .proxy
            .data::<OfferData>()
            .is_some_and(|data| {
                data.0
                    .lock()
                    .unwrap()
                    .formats
                    .iter()
                    .any(|offered| offered == mime)
            })
    }

    /// Update hover negotiation; a dropped offer's final action is immutable.
    pub fn accept(
        &self,
        mime: Option<String>,
        actions: DragActions,
        preferred: Option<DragAction>,
    ) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if state.snapshot.finished
            || state.snapshot.dropped
            || state.snapshot.left
            || !state.native.proxy.is_alive()
        {
            return Err(error("This drag offer has left or already dropped"));
        }
        if preferred.is_some_and(|action| !actions.contains(action))
            || mime.as_ref().is_some_and(|mime| {
                !state
                    .native
                    .proxy
                    .data::<OfferData>()
                    .is_some_and(|data| data.0.lock().unwrap().formats.contains(mime))
            })
        {
            return Err(error("Unsupported drag format/action"));
        }
        let negotiation = (mime.clone(), actions, preferred);
        if state.negotiation.as_ref() == Some(&negotiation) {
            return Ok(());
        }
        state.native.proxy.accept(state.native.serial, mime.clone());
        state.native.proxy.set_actions(
            actions.wire(),
            preferred.map_or(DndAction::empty(), DragAction::wire),
        );
        state.accepted = mime.is_some();
        state.negotiation = Some(negotiation);
        if let Some(endpoint) = self.0.endpoint.upgrade() {
            endpoint.ping.ping();
        }
        Ok(())
    }

    /// Requests a pipe; callers must read on a bounded I/O worker, never block
    /// native dispatch. The endpoint wakeup flushes the receive request.
    pub fn receive(&self, mime: String) -> io::Result<OwnedFd> {
        let state = self.0.state.lock().unwrap();
        if state.snapshot.finished
            || state.snapshot.left && !state.snapshot.dropped
            || !state.native.proxy.is_alive()
            || !state
                .native
                .proxy
                .data::<OfferData>()
                .is_some_and(|data| data.0.lock().unwrap().formats.contains(&mime))
        {
            return Err(error("This drag offer cannot provide the requested format"));
        }
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        state
            .native
            .proxy
            .receive(mime, std::os::fd::AsFd::as_fd(&write));
        drop(write);
        if let Some(endpoint) = self.0.endpoint.upgrade() {
            endpoint.ping.ping();
        }
        Ok(read)
    }

    /// End a claimed drop exactly once. Failure destroys without advertising
    /// success; no filesystem mutation or source deletion happens here.
    pub fn finish(&self, success: bool) -> io::Result<()> {
        let id = {
            let mut state = self.0.state.lock().unwrap();
            if state.snapshot.finished {
                return Err(error("This drag offer has already ended"));
            }
            if !state.snapshot.dropped {
                return Err(error("A hover cannot be finished as a drop"));
            }
            if success && (!state.accepted || state.snapshot.selected.is_none()) {
                return Err(error("An unaccepted drag cannot be finished successfully"));
            }
            if state.native.proxy.is_alive() {
                if success {
                    state.native.proxy.finish();
                }
                state.native.proxy.destroy();
            }
            state.snapshot.finished = true;
            state.snapshot.id
        };
        if let Some(endpoint) = self.0.endpoint.upgrade() {
            endpoint.offers.lock().unwrap().remove(&id);
            endpoint.ping.ping();
            (endpoint.changed)();
        }
        Ok(())
    }
}

#[cfg(test)]
mod icon_tests {
    use super::DragIcon;

    #[test]
    fn drag_icons_need_whole_scaled_sides_and_one_rgba_pixel_each() {
        let icon = |width, height, scale, bytes| DragIcon {
            width,
            height,
            scale,
            rgba: vec![0; bytes],
            hotspot: (0, 0),
        };
        assert!(icon(48, 32, 1, 48 * 32 * 4).valid());
        assert!(icon(96, 64, 2, 96 * 64 * 4).valid());
        assert!(!icon(95, 64, 2, 95 * 64 * 4).valid());
        assert!(!icon(48, 32, 1, 48 * 32 * 3).valid());
        assert!(!icon(0, 32, 1, 0).valid());
        assert!(!icon(2048, 1, 1, 2048 * 4).valid());
    }
}
