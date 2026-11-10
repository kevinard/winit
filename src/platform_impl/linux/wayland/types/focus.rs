//! User-input-bound activation of another window on this Wayland connection.
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use sctk::reexports::client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface};
use sctk::reexports::client::{Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::XdgActivationTokenV1, xdg_activation_v1::XdgActivationV1,
};
use sctk::shell::xdg::window::Window;
use sctk::shell::WaylandSurface;

use super::xdg_activation::XdgActivationTokenData;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::window::{WindowRequests, WindowState};
use crate::platform_impl::wayland::WindowId;

const INPUT_AGE: Duration = Duration::from_secs(2);
const TOKEN_LIFETIME: Duration = Duration::from_secs(1);
const PENDING_LIMIT: usize = 64;

#[derive(Clone, Default)]
pub struct Input(Arc<Mutex<Option<RecentInput>>>);

#[derive(Clone)]
struct RecentInput {
    window: Weak<Mutex<WindowState>>,
    requests: Weak<WindowRequests>,
    seat: WlSeat,
    serial: u32,
    at: Instant,
}

impl Input {
    pub fn record(
        &self,
        window: &Arc<Mutex<WindowState>>,
        requests: &Arc<WindowRequests>,
        seat: &WlSeat,
        serial: u32,
    ) {
        *self.0.lock().unwrap() = Some(RecentInput {
            window: Arc::downgrade(window),
            requests: Arc::downgrade(requests),
            seat: seat.clone(),
            serial,
            at: Instant::now(),
        });
    }

    pub fn capture(&self) -> Option<Request> {
        // Never hold this slot while locking window state: input dispatch may
        // record a pointer gesture while it already holds that window's state.
        let input = self.0.lock().unwrap().clone()?;
        if input.at.elapsed() > INPUT_AGE || !input.seat.is_alive() {
            return None;
        }
        let requests = input.requests.upgrade()?;
        if requests.closed.load(Ordering::Acquire) {
            return None;
        }
        let window = input.window.upgrade()?;
        let window = window.lock().unwrap();
        if !window.has_seat_focus(&input.seat.id()) {
            return None;
        }
        Some(Request {
            source: window.window.clone(),
            seat: input.seat,
            serial: input.serial,
            at: input.at,
        })
    }
}

pub struct Request {
    source: Window,
    seat: WlSeat,
    serial: u32,
    at: Instant,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("User-input focus request")
    }
}

pub struct Target {
    pub window_id: WindowId,
    pub requests: Weak<WindowRequests>,
    pub surface: WlSurface,
}

impl Target {
    fn is_live(&self) -> bool {
        self.requests.upgrade().is_some_and(|requests| !requests.closed.load(Ordering::Acquire))
            && self.surface.is_alive()
    }
}

struct Pending {
    target: Target,
    source: Window,
    token: XdgActivationTokenV1,
    timer: RegistrationToken,
    at: Instant,
}

#[derive(Default)]
pub struct Requests {
    next: u64,
    pending: HashMap<u64, Pending>,
    targets: HashMap<WindowId, u64>,
}

impl Requests {
    pub fn start(
        &mut self,
        global: &XdgActivationV1,
        queue: &QueueHandle<WinitState>,
        handle: &LoopHandle<'static, WinitState>,
        target: Target,
        input: Request,
    ) {
        if input.at.elapsed() > INPUT_AGE
            || !target.is_live()
            || !input.seat.is_alive()
            || !input.source.wl_surface().is_alive()
        {
            return;
        }
        if let Some(previous) = self.targets.get(&target.window_id).copied() {
            self.cancel(previous, handle);
        }
        if self.pending.len() >= PENDING_LIMIT {
            if let Some(oldest) =
                self.pending.iter().min_by_key(|(_, pending)| pending.at).map(|(serial, _)| *serial)
            {
                self.cancel(oldest, handle);
            }
        }
        let Some(serial) = self.next.checked_add(1) else {
            return;
        };
        self.next = serial;
        let at = Instant::now();
        let Ok(timer) =
            handle.insert_source(Timer::from_deadline(at + TOKEN_LIFETIME), move |_, _, state| {
                if let Some(activation) = state.xdg_activation.as_mut() {
                    activation.focus.expire(serial);
                }
                TimeoutAction::Drop
            })
        else {
            return;
        };
        let token = global.get_activation_token(queue, XdgActivationTokenData::Focus(serial));
        token.set_serial(input.serial, &input.seat);
        token.set_surface(input.source.wl_surface());
        token.commit();
        let window_id = target.window_id;
        self.pending.insert(serial, Pending { target, source: input.source, token, timer, at });
        self.targets.insert(window_id, serial);
    }

    fn take(&mut self, serial: u64) -> Option<Pending> {
        let pending = self.pending.remove(&serial)?;
        if self.targets.get(&pending.target.window_id) == Some(&serial) {
            self.targets.remove(&pending.target.window_id);
        }
        Some(pending)
    }

    fn cancel(&mut self, serial: u64, handle: &LoopHandle<'static, WinitState>) {
        if let Some(pending) = self.take(serial) {
            handle.remove(pending.timer);
            if pending.token.is_alive() {
                pending.token.destroy();
            }
        }
    }

    pub fn retire(&mut self, window: WindowId, handle: &LoopHandle<'static, WinitState>) {
        if let Some(serial) = self.targets.get(&window).copied() {
            self.cancel(serial, handle);
        }
    }

    fn expire(&mut self, serial: u64) {
        if let Some(pending) = self.take(serial) {
            if pending.token.is_alive() {
                pending.token.destroy();
            }
        }
    }

    pub fn finish(
        &mut self,
        serial: u64,
        token: String,
        global: &XdgActivationV1,
        handle: &LoopHandle<'static, WinitState>,
    ) {
        let Some(pending) = self.take(serial) else {
            return;
        };
        handle.remove(pending.timer);
        // Wayland delivery can run before an overdue timer after an event-loop
        // stall. The deadline, rather than callback ordering, grants authority.
        if pending.at.elapsed() < TOKEN_LIFETIME
            && pending.target.is_live()
            && pending.source.wl_surface().is_alive()
        {
            global.activate(token, &pending.target.surface);
        }
        // The source role stays alive through the activation request. A normal
        // reply or the one-second timer then releases it, even after UI closure.
    }
}
