//! One native attempt per protected window, with bounded source-owned results.
use super::*;
use recording::{Attempt, CancelReason, Key, Modifiers, State};
use sctk::reexports::client::protocol::wl_callback::{self, WlCallback};

struct ResultSlot {
    state: Mutex<State>,
    changed: Changed,
}
/// An owned one-shot recording attempt on a protected ordinary window.
#[derive(Clone)]
pub struct Recording(Arc<RecordingLifetime>);
impl Recording {
    /// Read the latest source-owned state, including an immutable terminal result.
    pub fn state(&self) -> State {
        self.0.slot.state.lock().unwrap().clone()
    }
    /// Revoke this attempt even while a stream or another clone remains alive.
    pub fn cancel(&self) {
        if let Some(hub) = self.0.hub.upgrade() {
            Hub(hub).cancel_recording(
                self.0.window,
                self.0.owner,
                self.0.generation,
                CancelReason::User,
            );
        }
        self.0.wake.ping();
    }
}
impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ShortcutRecording")
            .field(&self.state())
            .finish()
    }
}
struct RecordingLifetime {
    hub: Weak<Mutex<Inner>>,
    window: WindowId,
    owner: u64,
    generation: u64,
    slot: Arc<ResultSlot>,
    wake: calloop::ping::Ping,
}
impl Drop for RecordingLifetime {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            Hub(hub).cancel_recording(
                self.window,
                self.owner,
                self.generation,
                CancelReason::Dropped,
            );
        }
        self.wake.ping();
    }
}
pub(super) struct Active {
    generation: u64,
    policy: Attempt<ObjectId>,
    slot: Arc<ResultSlot>,
    serial: Option<u32>,
    post: Option<Modifiers>,
}
impl Active {
    fn resolve(&mut self) {
        if self.policy.pending_sequence().is_some() {
            self.policy.resolve(self.post.take());
            self.serial = None;
        }
    }
}
#[derive(Clone)]
pub(crate) struct Fence {
    hub: Weak<Mutex<Inner>>,
    window: WindowId,
    owner: u64,
    generation: u64,
    sequence: u64,
}
fn released(inner: &Inner, window: WindowId) -> bool {
    inner
        .seats
        .values()
        .filter(|s| s.keyboard && s.focus == Some(window))
        .all(|s| s.held.empty())
}
fn publish(entry: &mut Entry, notifications: &mut Vec<Changed>) {
    if let Some(active) = &entry.recording {
        let next = active.policy.state().clone();
        let mut state = active.slot.state.lock().unwrap();
        if *state != next {
            *state = next;
            notifications.push(active.slot.changed.clone());
        }
    }
    if entry
        .recording
        .as_ref()
        .is_some_and(|a| a.policy.state().is_terminal())
    {
        entry.recording = None;
    }
}
pub(super) fn cancel_all(
    inner: &mut Inner,
    reason: CancelReason,
    notifications: &mut Vec<Changed>,
) {
    for entry in inner.windows.values_mut() {
        if let Some(active) = &mut entry.recording {
            active.policy.cancel(reason);
        }
        publish(entry, notifications);
    }
}
pub(super) fn close_entry(entry: &mut Entry, notifications: &mut Vec<Changed>) {
    if let Some(active) = &mut entry.recording {
        active.policy.close();
    }
    publish(entry, notifications);
}
fn fence(inner: &Inner, data: Fence) {
    if let Some(backend) = inner.manager.backend().upgrade() {
        Connection::from_backend(backend)
            .display()
            .sync(&inner.qh, data);
    }
}
impl KeyboardShortcutsInhibitor {
    /// Arm one recording attempt only while actual native protection is Active.
    /// The callback may only wake a consumer. Input queued before the arm fence,
    /// keys already held and repeats are never recorded as a fresh choice.
    pub fn record(&self, changed: Changed) -> Option<Recording> {
        let hub = self.0.hub.upgrade()?;
        let endpoint = {
            let mut inner = hub.lock().unwrap();
            let entry = inner.windows.get(&self.0.window)?;
            if inner.closed
                || entry.generation != self.0.generation
                || entry.slot.status() != Status::Active
                || entry.recording.is_some()
            {
                return None;
            }
            if inner
                .seats
                .values()
                .filter(|s| s.keyboard && s.focus == Some(self.0.window))
                .any(|s| !s.held.valid() || s.modifiers.is_none())
            {
                return None;
            }
            inner.manager.backend().upgrade()?;
            let generation = inner.generation.checked_add(1)?;
            inner.generation = generation;
            let slot = Arc::new(ResultSlot {
                state: Mutex::new(State::Preparing),
                changed,
            });
            inner.windows.get_mut(&self.0.window)?.recording = Some(Active {
                generation,
                policy: Attempt::new(),
                slot: slot.clone(),
                serial: None,
                post: None,
            });
            fence(
                &inner,
                Fence {
                    hub: Arc::downgrade(&hub),
                    window: self.0.window,
                    owner: self.0.generation,
                    generation,
                    sequence: 0,
                },
            );
            Recording(Arc::new(RecordingLifetime {
                hub: Arc::downgrade(&hub),
                window: self.0.window,
                owner: self.0.generation,
                generation,
                slot,
                wake: self.0.wake.clone(),
            }))
        };
        self.0.wake.ping();
        Some(endpoint)
    }
}
impl Hub {
    fn cancel_recording(
        &self,
        window: WindowId,
        owner: u64,
        generation: u64,
        reason: CancelReason,
    ) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            let mut notifications = Vec::new();
            if let Some(entry) = inner
                .windows
                .get_mut(&window)
                .filter(|e| e.generation == owner)
            {
                if let Some(active) = entry
                    .recording
                    .as_mut()
                    .filter(|a| a.generation == generation)
                {
                    active.policy.cancel(reason);
                }
                publish(entry, &mut notifications);
            }
            notifications
        };
        notify(notifications);
    }
    pub fn keymap(&self, id: &ObjectId, ready: bool) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            let mut notifications = Vec::new();
            if let Some(seat) = inner.seats.get_mut(id) {
                seat.modifiers = None;
                seat.group = None;
                seat.map_ready = ready;
            }
            cancel_all(&mut inner, CancelReason::KeymapChanged, &mut notifications);
            notifications
        };
        notify(notifications);
    }
    /// Associate post-press modifiers by the native key serial. An unrelated
    /// modifier event seals the older snapshot before changing the live state.
    pub fn modifiers(&self, id: &ObjectId, serial: u32, modifiers: Option<Modifiers>, group: u32) {
        let notifications = {
            let mut inner = self.0.lock().unwrap();
            let mut notifications = Vec::new();
            let Some(seat) = inner.seats.get_mut(id) else {
                return;
            };
            let changed = seat.group.is_some_and(|old| old != group);
            seat.group = Some(group);
            seat.modifiers = if seat.map_ready { modifiers } else { None };
            if changed {
                cancel_all(&mut inner, CancelReason::KeymapChanged, &mut notifications);
            } else {
                for entry in inner.windows.values_mut() {
                    if let Some(active) = &mut entry.recording {
                        if active.policy.source() == Some(id)
                            && active.policy.pending_sequence().is_some()
                        {
                            if active.serial == Some(serial) {
                                active.post = modifiers;
                            } else {
                                active.resolve();
                            }
                        }
                    }
                    publish(entry, &mut notifications);
                }
            }
            notifications
        };
        notify(notifications);
    }
    pub fn consumes_repeat(&self, id: &ObjectId, window: WindowId, code: u32) -> bool {
        let inner = self.0.lock().unwrap();
        inner.seats.get(id).is_some_and(|s| s.held.consumes(code))
            || inner
                .windows
                .get(&window)
                .is_some_and(|e| e.recording.is_some())
    }
    pub fn key_event(
        &self,
        id: &ObjectId,
        window: WindowId,
        code: u32,
        down: bool,
        serial: u32,
        key: impl FnOnce() -> Option<Key>,
    ) -> bool {
        let (consumed, notifications) = {
            let mut inner = self.0.lock().unwrap();
            let mut notifications = Vec::new();
            let capture = inner
                .windows
                .get(&window)
                .is_some_and(|e| e.recording.is_some() && e.slot.status() == Status::Active);
            let Some(seat) = inner
                .seats
                .get_mut(id)
                .filter(|s| s.keyboard && s.focus == Some(window))
            else {
                return false;
            };
            let (fresh, consumed) = seat.held.edge(code, down, capture);
            let valid = seat.held.valid();
            let all_released = released(&inner, window);
            let mut request = None;
            if let Some(entry) = inner.windows.get_mut(&window) {
                if let Some(active) = &mut entry.recording {
                    active.resolve();
                    if !valid {
                        active.policy.cancel(CancelReason::InputLimit);
                    } else if down && fresh {
                        let key = key();
                        let pre = key.as_ref().map(|key| key.modifiers);
                        if let Some(sequence) = active.policy.press(id.clone(), key) {
                            active.serial = Some(serial);
                            active.post = pre;
                            request = Some((entry.generation, active.generation, sequence));
                        }
                    }
                    active.policy.released(all_released);
                }
                publish(entry, &mut notifications);
            }
            if let Some((owner, generation, sequence)) = request {
                fence(
                    &inner,
                    Fence {
                        hub: Arc::downgrade(&self.0),
                        window,
                        owner,
                        generation,
                        sequence,
                    },
                );
            }
            (consumed, notifications)
        };
        notify(notifications);
        consumed
    }
}
impl Dispatch<WlCallback, Fence> for WinitState {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        data: &Fence,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_callback::Event::Done { .. }) {
            return;
        }
        let Some(hub) = data.hub.upgrade() else {
            return;
        };
        let notifications = {
            let mut inner = hub.lock().unwrap();
            let mut notifications = Vec::new();
            let all_released = released(&inner, data.window);
            let focused: Vec<_> = if data.sequence == 0 {
                inner
                    .seats
                    .iter()
                    .filter(|(_, seat)| seat.keyboard && seat.focus == Some(data.window))
                    .map(|(id, _)| id.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let Some(entry) = inner
                .windows
                .get_mut(&data.window)
                .filter(|e| e.generation == data.owner)
            else {
                return;
            };
            let Some(active) = entry
                .recording
                .as_mut()
                .filter(|a| a.generation == data.generation)
            else {
                return;
            };
            if data.sequence == 0 {
                for id in focused {
                    if let Some(seat) = state.seats.get_mut(&id) {
                        seat.clear_shortcut_compose();
                    }
                }
                active.policy.armed(all_released);
            } else if active.policy.pending_sequence() == Some(data.sequence) {
                active.resolve();
                active.policy.released(all_released);
            }
            publish(entry, &mut notifications);
            notifications
        };
        notify(notifications);
    }
}
