//! Clipboard adapter: data-device selection offers, MIME negotiation, and text transfer.

use std::io::Write;
use std::os::fd::AsFd;

use nix::fcntl::OFlag;
use wayland_client::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
};
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::event_loop::AppState;

use super::WaylandState;

impl WaylandState {
    /// Creates and initializes the `wl_data_device` instance once the manager and seat are available.
    pub fn init_data_device<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WlDataDevice, ()> + 'static,
    {
        if self.data_device.is_some() {
            return;
        }
        let Some(manager) = &self.data_device_manager else {
            return;
        };
        let Some(seat) = &self.seat else {
            return;
        };

        let device = manager.get_data_device(seat, qh, ());
        self.data_device = Some(device);
    }
}

// --- Clipboard ---

/// Selects the highest-fidelity UTF-8 text MIME type advertised by an offer.
#[must_use]
pub fn best_text_mime(mimes: &[String]) -> Option<&str> {
    for candidate in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
        if let Some(found) = mimes.iter().find(|m| m.as_str() == candidate) {
            return Some(found.as_str());
        }
    }
    None
}

impl Dispatch<WlDataDeviceManager, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state.pending_offers.len() >= 4
                    && let Some(stale) = state.pending_offers.first()
                {
                    stale.offer.destroy();
                    state.pending_offers.remove(0);
                }
                state.pending_offers.push(crate::wayland::OfferData {
                    offer: id,
                    mime_types: Vec::new(),
                });
            }
            wl_data_device::Event::Selection { id } => {
                // The previous selection offer is dead once a new one arrives.
                if let Some(previous) = state.wayland.current_offer.take() {
                    previous.offer.destroy();
                }
                state.wayland.current_offer = id.and_then(|offer| {
                    state
                        .pending_offers
                        .iter()
                        .position(|o| o.offer == offer)
                        .map(|idx| state.pending_offers.swap_remove(idx))
                });
                if state.wayland.current_offer.is_none() {
                    for stale in state.pending_offers.drain(..) {
                        stale.offer.destroy();
                    }
                }
            }
            _ => {}
        }
    }

    // data_offer creates a server-owned proxy before its MIME and selection events arrive.
    wayland_client::event_created_child!(AppState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataSource, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type: _, fd } => {
                let text = state.clipboard_text.clone();
                std::thread::spawn(move || {
                    let mut file = std::fs::File::from(fd);
                    if let Some(text) = text {
                        let _ = file.write_all(text.as_bytes());
                    }
                });
            }
            wl_data_source::Event::Cancelled => {
                if let Some(source) = state.wayland.data_source.take() {
                    source.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Some(data) = state.pending_offers.iter_mut().find(|o| &o.offer == proxy) {
                data.mime_types.push(mime_type);
            } else if let Some(current) = &mut state.wayland.current_offer
                && &current.offer == proxy
            {
                current.mime_types.push(mime_type);
            }
        }
    }
}

impl AppState {
    /// Sets the clipboard content internally and offers it through the Wayland data device.
    pub fn set_clipboard_text(&mut self, text: String, qh: Option<&QueueHandle<Self>>) {
        self.session
            .terminal
            .set_clipboard_content(Some(text.clone()));
        self.clipboard_text = Some(text);

        if let (Some(qh), Some(manager), Some(device)) = (
            qh,
            &self.wayland.data_device_manager,
            &self.wayland.data_device,
        ) {
            let source = manager.create_data_source(qh, ());
            source.offer("text/plain;charset=utf-8".to_string());
            source.offer("text/plain".to_string());
            source.offer("UTF8_STRING".to_string());
            device.set_selection(Some(&source), self.last_serial);
            self.wayland.data_source = Some(source);
        }
    }

    /// Copies the currently selected text to the Wayland clipboard and internal buffer.
    pub fn copy_selection(&mut self, qh: Option<&QueueHandle<Self>>) {
        let text = self
            .pointer
            .selection
            .extract_text(&self.session.terminal.grid);
        if text.is_empty() {
            return;
        }

        self.set_clipboard_text(text, qh);
    }

    /// Pastes text from the Wayland clipboard into the terminal PTY.
    pub fn paste_clipboard(&mut self, conn: Option<&Connection>) {
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types)
                && let Ok((read_fd, write_fd)) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            {
                offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                drop(write_fd);

                if let Some(c) = conn {
                    let _ = c.flush();
                }

                self.session.paste_from_fd(read_fd);
            }
            return;
        }

        // Fall back to the internal clipboard buffer when no offer is available.
        if let Some(text) = self.clipboard_text.clone() {
            self.session.paste(text.as_bytes());
        }
    }
}
