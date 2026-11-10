//! Raw data-device dispatch keeps dropped offers independent of the next enter.
use super::*;
use sctk::{
    globals::GlobalData,
    reexports::client::{
        event_created_child,
        protocol::{
            wl_data_device, wl_data_device_manager::WlDataDeviceManager, wl_data_offer,
            wl_data_source,
        },
        Connection, Dispatch, WEnum,
    },
};

sctk::reexports::client::delegate_dispatch!(WinitState:
    [WlDataDeviceManager: GlobalData] => DataDeviceManagerState);

impl Hub {
    fn update_offer(&self, proxy: &WlDataOffer, update: impl FnOnce(&mut OfferState)) {
        let incoming = self
            .0
            .state
            .lock()
            .unwrap()
            .offers
            .get(&proxy.id())
            .and_then(Weak::upgrade);
        if let Some(incoming) = incoming {
            update(&mut incoming.state.lock().unwrap());
            if let Some(endpoint) = incoming.endpoint.upgrade() {
                (endpoint.changed)();
            }
        }
    }

    fn current(&self, seat: &WlSeat) -> Option<Offer> {
        self.0
            .state
            .lock()
            .unwrap()
            .current
            .get(&seat.id())
            .cloned()
    }

    fn enter(
        &self,
        seat: &WlSeat,
        proxy: WlDataOffer,
        serial: u32,
        surface: WlSurface,
        x: f64,
        y: f64,
    ) {
        let mut hub = self.0.state.lock().unwrap();
        hub.pending.remove(&proxy.id());
        hub.offers.retain(|_, offer| {
            offer
                .upgrade()
                .is_some_and(|offer| !offer.state.lock().unwrap().snapshot.finished)
        });
        let endpoint = hub
            .endpoints
            .get(&super::super::make_wid(&surface))
            .and_then(Weak::upgrade);
        let Some(endpoint) = endpoint.filter(|endpoint| !endpoint.closed.load(Ordering::Acquire))
        else {
            proxy.accept(serial, None);
            proxy.set_actions(DndAction::empty(), DndAction::empty());
            if proxy.is_alive() {
                proxy.destroy();
            }
            return;
        };
        if hub.offers.len() >= MAX_LIVE {
            proxy.accept(serial, None);
            proxy.destroy();
            return;
        }
        let Ok(id) = hub.next() else {
            proxy.destroy();
            return;
        };
        let local_source = hub.sources.values().find_map(|source| {
            let status = source.snapshot();
            (source.0.seat == seat.id() && !status.dropped && !status.finished).then_some(status.id)
        });
        let Some(data) = proxy.data::<OfferData>() else {
            proxy.destroy();
            return;
        };
        let data = data.0.lock().unwrap();
        let offer = Offer(Arc::new(Incoming {
            endpoint: Arc::downgrade(&endpoint),
            state: Mutex::new(OfferState {
                snapshot: OfferSnapshot {
                    id,
                    position: (x, y),
                    actions: data.actions,
                    selected: data.selected,
                    dropped: false,
                    left: false,
                    finished: false,
                    local_source,
                },
                native: NativeOffer {
                    proxy: proxy.clone(),
                    serial,
                },
                accepted: false,
                negotiation: None,
            }),
        }));
        drop(data);
        hub.offers.insert(proxy.id(), Arc::downgrade(&offer.0));
        let old = hub.current.insert(seat.id(), offer.clone());
        drop(hub);
        if let Some(old) = old {
            self.leave_offer(old);
        }
        endpoint.offers.lock().unwrap().insert(id, offer);
        (endpoint.changed)();
    }

    pub(super) fn leave_offer(&self, offer: Offer) {
        let (id, ended) = {
            let mut state = offer.0.state.lock().unwrap();
            state.snapshot.left = true;
            if !state.snapshot.dropped {
                state.snapshot.finished = true;
                if state.native.proxy.is_alive() {
                    state.native.proxy.destroy();
                }
            }
            (state.snapshot.id, state.snapshot.finished)
        };
        if let Some(endpoint) = offer.0.endpoint.upgrade() {
            if ended {
                endpoint.offers.lock().unwrap().remove(&id);
            }
            (endpoint.changed)();
        }
    }

    fn source(&self, proxy: &WlDataSource) -> Option<Source> {
        self.0
            .state
            .lock()
            .unwrap()
            .sources
            .get(&proxy.id())
            .cloned()
    }
}

impl Dispatch<wl_data_device::WlDataDevice, DeviceData> for WinitState {
    event_created_child!(WinitState, wl_data_device::WlDataDevice, [0 => (WlDataOffer, OfferData::default())]);

    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        data: &DeviceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(hub) = state.drag_drop.clone() else {
            return;
        };
        match event {
            wl_data_device::Event::DataOffer { id } => {
                let mut state = hub.0.state.lock().unwrap();
                if state.pending.len() >= MAX_LIVE {
                    id.destroy();
                } else {
                    state.pending.insert(id.id(), id);
                }
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id: Some(offer),
            } => {
                hub.enter(&data.0, offer, serial, surface, x, y);
            }
            wl_data_device::Event::Enter { id: None, .. } => {}
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(offer) = hub.current(&data.0) {
                    let proxy = offer.0.state.lock().unwrap().native.proxy.clone();
                    hub.update_offer(&proxy, |state| state.snapshot.position = (x, y));
                }
            }
            wl_data_device::Event::Drop => {
                if let Some(offer) = hub.current(&data.0) {
                    let proxy = offer.0.state.lock().unwrap().native.proxy.clone();
                    hub.update_offer(&proxy, |state| state.snapshot.dropped = true);
                }
            }
            wl_data_device::Event::Leave => {
                let offer = hub.0.state.lock().unwrap().current.remove(&data.0.id());
                if let Some(offer) = offer {
                    hub.leave_offer(offer);
                }
            }
            // This opt-in data device never owns clipboard selections.
            wl_data_device::Event::Selection { id: Some(offer) } => {
                hub.0.state.lock().unwrap().pending.remove(&offer.id());
                if offer.is_alive() {
                    offer.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferData> for WinitState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !proxy.is_alive() {
            return;
        }
        let mut announcement = data.0.lock().unwrap();
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                if mime_type.len() <= 256
                    && announcement.formats.len() < 128
                    && !announcement.formats.contains(&mime_type)
                {
                    announcement.formats.push(mime_type);
                }
            }
            wl_data_offer::Event::SourceActions {
                source_actions: WEnum::Value(actions),
            } => {
                announcement.actions = DragActions::from_wire(actions);
            }
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => {
                announcement.selected = DragAction::from_wire(action);
            }
            _ => {}
        }
        let actions = announcement.actions;
        let selected = announcement.selected;
        drop(announcement);
        if let Some(hub) = &state.drag_drop {
            hub.update_offer(proxy, |state| {
                state.snapshot.actions = actions;
                if !state.snapshot.dropped {
                    state.snapshot.selected = selected;
                }
            });
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for WinitState {
    fn event(
        state: &mut Self,
        proxy: &WlDataSource,
        event: wl_data_source::Event,
        _: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(source) = state.drag_drop.as_ref().and_then(|hub| hub.source(proxy)) else {
            return;
        };
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let send = source.0.send.lock().unwrap().clone();
                if let Some(send) = send {
                    send(mime_type, fd);
                }
            }
            wl_data_source::Event::Cancelled => source.end(true),
            wl_data_source::Event::DndFinished => source.end(false),
            wl_data_source::Event::DndDropPerformed => {
                source.0.state.lock().unwrap().snapshot.dropped = true;
                (source.0.endpoint.changed)();
            }
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => {
                source.0.state.lock().unwrap().snapshot.selected = DragAction::from_wire(action);
                (source.0.endpoint.changed)();
            }
            _ => {}
        }
    }
}
