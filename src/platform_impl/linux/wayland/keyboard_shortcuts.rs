//! Focus-scoped shortcut inhibition on the existing Wayland connection.
use super::{state::WinitState, WindowId};
mod policy;
mod recording;
mod recording_native;
pub use policy::Status;
use policy::{focused_status, Permission};
pub use recording::{CancelReason as RecordingCancelReason, State as RecordingState};
pub(crate) use recording::{Key, Modifiers, ALT, CTRL, LOGO, NUM, SHIFT};
pub use recording_native::Recording;
use sctk::reexports::client::{
    backend::ObjectId,
    globals::GlobalList,
    protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use sctk::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor},
};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, Weak,
    },
};

/// Notification must only wake a consumer; it must not block or perform I/O.
pub type Changed = Arc<dyn Fn() + Send + Sync>;
struct Slot {
    status: AtomicU8,
    changed: Changed,
}
impl Slot {
    fn status(&self) -> Status {
        match self.status.load(Ordering::Acquire) {
            0 => Status::Unfocused,
            1 => Status::Pending,
            2 => Status::Active,
            3 => Status::Inactive,
            _ => Status::Closed,
        }
    }
}
/// Retaining a clone cannot keep inhibition alive after its window retires.
#[derive(Clone)]
pub struct KeyboardShortcutsInhibitor(Arc<Lifetime>);
impl KeyboardShortcutsInhibitor {
    pub fn status(&self) -> Status {
        self.0.slot.status()
    }
}
impl fmt::Debug for KeyboardShortcutsInhibitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("KeyboardShortcutsInhibitor")
            .field(&self.status())
            .finish()
    }
}
struct Lifetime {
    hub: Weak<Mutex<Inner>>,
    window: WindowId,
    generation: u64,
    slot: Arc<Slot>,
    wake: calloop::ping::Ping,
}
impl Drop for Lifetime {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            Hub(hub).retire(self.window, Some(self.generation));
        }
        self.wake.ping();
    }
}
struct Seat {
    seat: WlSeat,
    keyboard: bool,
    focus: Option<WindowId>,
    held: recording::Held,
    modifiers: Option<recording::Modifiers>,
    group: Option<u32>,
    map_ready: bool,
}
struct Native {
    proxy: Inhibitor,
    permission: Permission,
}
impl Drop for Native {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}
struct Entry {
    generation: u64,
    surface: WlSurface,
    slot: Arc<Slot>,
    inhibitors: HashMap<ObjectId, Native>,
    recording: Option<recording_native::Active>,
}
struct Inner {
    manager: Manager,
    qh: QueueHandle<WinitState>,
    seats: HashMap<ObjectId, Seat>,
    windows: HashMap<WindowId, Entry>,
    generation: u64,
    closed: bool,
}
#[derive(Clone)]
pub(crate) struct Hub(Arc<Mutex<Inner>>);
#[derive(Clone)]
pub(crate) struct Data {
    hub: Weak<Mutex<Inner>>,
    window: WindowId,
    generation: u64,
    seat: ObjectId,
}
fn mark(slot: &Arc<Slot>, status: Status, notifications: &mut Vec<Changed>) {
    // Store while holding the hub lock. Delayed notifications can only wake a
    // reader of newer state, never resurrect Active after a concurrent close.
    if slot.status.swap(status as u8, Ordering::AcqRel) != status as u8 {
        notifications.push(slot.changed.clone());
    }
}
fn notify(notifications: Vec<Changed>) {
    for slot in notifications {
        slot();
    }
}
fn refresh(inner: &Inner) -> Vec<Changed> {
    let mut notifications = Vec::new();
    for (&window, entry) in &inner.windows {
        let status = focused_status(
            inner
                .seats
                .iter()
                .filter(|(_, seat)| seat.keyboard && seat.focus == Some(window))
                .map(|(id, _)| entry.inhibitors.get(id).map(|i| i.permission)),
        );
        mark(&entry.slot, status, &mut notifications);
    }
    notifications
}
fn native(
    inner: &Inner,
    hub: Weak<Mutex<Inner>>,
    window: WindowId,
    entry: &Entry,
    seat: &Seat,
) -> Native {
    Native {
        proxy: inner.manager.inhibit_shortcuts(
            &entry.surface,
            &seat.seat,
            &inner.qh,
            Data {
                hub,
                window,
                generation: entry.generation,
                seat: seat.seat.id(),
            },
        ),
        permission: Permission::Pending,
    }
}
impl Hub {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<WinitState>) -> Option<Self> {
        Some(Self(Arc::new(Mutex::new(Inner {
            manager: globals.bind(qh, 1..=1, ()).ok()?,
            qh: qh.clone(),
            seats: HashMap::new(),
            windows: HashMap::new(),
            generation: 0,
            closed: false,
        }))))
    }
    pub fn add_seat(&self, seat: WlSeat) {
        let mut inner = self.0.lock().unwrap();
        if !inner.closed {
            inner.seats.entry(seat.id()).or_insert(Seat {
                seat,
                keyboard: false,
                focus: None,
                held: recording::Held::default(),
                modifiers: None,
                group: None,
                map_ready: false,
            });
        }
    }
    pub fn keyboard(&self, id: &ObjectId, available: bool) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            let Some(seat) = inner.seats.get_mut(id) else {
                return;
            };
            if seat.keyboard == available {
                return;
            }
            seat.keyboard = available;
            if !available {
                seat.focus = None;
                seat.held.leave();
                seat.modifiers = None;
                seat.group = None;
                seat.map_ready = false;
            }
            if available {
                let seat = &inner.seats[id];
                let additions: Vec<_> = inner
                    .windows
                    .iter()
                    .map(|(&window, entry)| {
                        (
                            window,
                            native(&inner, Arc::downgrade(&self.0), window, entry, seat),
                        )
                    })
                    .collect();
                for (window, inhibitor) in additions {
                    inner
                        .windows
                        .get_mut(&window)
                        .unwrap()
                        .inhibitors
                        .insert(id.clone(), inhibitor);
                }
            } else {
                for entry in inner.windows.values_mut() {
                    entry.inhibitors.remove(id);
                }
            }
            let mut notifications = Vec::new();
            recording_native::cancel_all(
                &mut inner,
                recording::CancelReason::ProtectionChanged,
                &mut notifications,
            );
            notifications.extend(refresh(&inner));
            notifications
        };
        notify(notifications);
    }
    pub fn remove_seat(&self, id: &ObjectId) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            inner.seats.remove(id);
            for entry in inner.windows.values_mut() {
                entry.inhibitors.remove(id);
            }
            let mut notifications = Vec::new();
            recording_native::cancel_all(
                &mut inner,
                recording::CancelReason::ProtectionChanged,
                &mut notifications,
            );
            notifications.extend(refresh(&inner));
            notifications
        };
        notify(notifications);
    }
    pub fn enter(&self, id: &ObjectId, window: WindowId, keys: &[u8]) {
        let notifications =
            {
                let mut inner = self.0.lock().unwrap();
                if let Some(seat) = inner.seats.get_mut(id) {
                    if seat.keyboard {
                        seat.focus = Some(window);
                        seat.modifiers = None;
                        seat.held.enter(keys.chunks_exact(4).filter_map(|b| {
                            u32::from_ne_bytes(b.try_into().unwrap()).checked_add(8)
                        }));
                        if keys.len() % 4 != 0
                            || keys
                                .chunks_exact(4)
                                .any(|b| u32::from_ne_bytes(b.try_into().unwrap()) > u32::MAX - 8)
                        {
                            seat.held.invalidate();
                        }
                    }
                }
                let mut notifications = Vec::new();
                recording_native::cancel_all(
                    &mut inner,
                    recording::CancelReason::ProtectionChanged,
                    &mut notifications,
                );
                notifications.extend(refresh(&inner));
                notifications
            };
        notify(notifications);
    }
    pub fn leave(&self, id: &ObjectId, window: WindowId) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            if let Some(seat) = inner.seats.get_mut(id) {
                if seat.focus == Some(window) {
                    seat.focus = None;
                    seat.held.leave();
                }
            }
            let mut notifications = Vec::new();
            recording_native::cancel_all(
                &mut inner,
                recording::CancelReason::ProtectionChanged,
                &mut notifications,
            );
            notifications.extend(refresh(&inner));
            notifications
        };
        notify(notifications);
    }
    pub fn enable(
        &self,
        window: WindowId,
        surface: WlSurface,
        wake: calloop::ping::Ping,
        changed: Changed,
    ) -> Option<KeyboardShortcutsInhibitor> {
        let (endpoint, notifications) = {
            let mut inner = self.0.lock().unwrap();
            if inner.closed || inner.windows.contains_key(&window) {
                return None;
            }
            let generation = inner.generation.checked_add(1)?;
            inner.generation = generation;
            let slot = Arc::new(Slot {
                status: AtomicU8::new(Status::Unfocused as u8),
                changed,
            });
            let mut entry = Entry {
                generation,
                surface,
                slot: slot.clone(),
                inhibitors: HashMap::new(),
                recording: None,
            };
            for (id, seat) in &inner.seats {
                if seat.keyboard {
                    entry.inhibitors.insert(
                        id.clone(),
                        native(&inner, Arc::downgrade(&self.0), window, &entry, seat),
                    );
                }
            }
            inner.windows.insert(window, entry);
            let endpoint = KeyboardShortcutsInhibitor(Arc::new(Lifetime {
                hub: Arc::downgrade(&self.0),
                window,
                generation,
                slot,
                wake: wake.clone(),
            }));
            (endpoint, refresh(&inner))
        };
        wake.ping();
        notify(notifications);
        Some(endpoint)
    }
    pub fn retire(&self, window: WindowId, generation: Option<u64>) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            let mut notifications = Vec::new();
            if inner
                .windows
                .get(&window)
                .is_some_and(|entry| generation.is_none_or(|g| entry.generation == g))
            {
                let mut entry = inner.windows.remove(&window).unwrap();
                recording_native::close_entry(&mut entry, &mut notifications);
                mark(&entry.slot, Status::Closed, &mut notifications);
            }
            // Ordinary windows are tracked before they request an inhibitor.
            // Retire their memo too, so a reused native ID cannot borrow focus.
            if generation.is_none() {
                for seat in inner.seats.values_mut() {
                    if seat.focus == Some(window) {
                        seat.focus = None;
                    }
                }
            }
            notifications
        };
        notify(notifications);
    }
    pub fn shutdown(&self) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            if inner.closed {
                return;
            }
            inner.closed = true;
            let mut notifications = Vec::new();
            for (_, mut entry) in inner.windows.drain() {
                recording_native::close_entry(&mut entry, &mut notifications);
                mark(&entry.slot, Status::Closed, &mut notifications);
            }
            inner.seats.clear();
            inner.manager.destroy();
            notifications
        };
        notify(notifications);
    }
}
impl Dispatch<Inhibitor, Data> for WinitState {
    fn event(
        _: &mut Self,
        proxy: &Inhibitor,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        data: &Data,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(hub) = data.hub.upgrade() else {
            return;
        };
        let notifications = {
            let mut inner = hub.lock().unwrap();
            let Some(entry) = inner
                .windows
                .get_mut(&data.window)
                .filter(|entry| entry.generation == data.generation)
            else {
                return;
            };
            let Some(inhibitor) = entry
                .inhibitors
                .get_mut(&data.seat)
                .filter(|i| i.proxy.id() == proxy.id())
            else {
                return;
            };
            let permission = match event {
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => Permission::Allowed,
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => Permission::Denied,
                _ => return,
            };
            let mut notifications = Vec::new();
            if inhibitor.permission != permission {
                inhibitor.permission = permission;
                recording_native::cancel_all(
                    &mut inner,
                    recording::CancelReason::ProtectionChanged,
                    &mut notifications,
                );
            }
            notifications.extend(refresh(&inner));
            notifications
        };
        notify(notifications);
    }
}
wayland_client::delegate_noop!(WinitState: ignore Manager);
