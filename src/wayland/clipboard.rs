//! Clipboard and data-device protocol dispatch (`wl_data_device_manager`, `wl_data_device`, `wl_data_source`, `wl_data_offer`).

use std::io::{Read, Write};
use std::os::fd::AsFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};
use wayland_client::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
};
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::event_loop::AppState;

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
                if state.pending_offers.len() >= 4 {
                    state.pending_offers.remove(0);
                }
                state.pending_offers.push(crate::wayland::OfferData {
                    offer: id,
                    mime_types: Vec::new(),
                });
            }
            wl_data_device::Event::Selection { id } => {
                state.wayland.current_offer = id.and_then(|offer| {
                    state
                        .pending_offers
                        .iter()
                        .position(|o| o.offer == offer)
                        .map(|idx| state.pending_offers.swap_remove(idx))
                });
                if state.wayland.current_offer.is_none() {
                    state.pending_offers.clear();
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
                state.wayland.data_source = None;
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
        self.terminal.set_clipboard_content(Some(text.clone()));
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
        let text = self.selection.extract_text(&self.terminal.grid);
        if text.is_empty() {
            return;
        }

        self.set_clipboard_text(text, qh);
    }

    /// Pastes text from the Wayland clipboard into the terminal PTY.
    pub fn paste_clipboard(&mut self, conn: Option<&Connection>) {
        let bracketed = self.terminal.bracketed_paste;
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types)
                && let Ok((read_fd, write_fd)) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            {
                offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                drop(write_fd);

                if let Some(c) = conn {
                    let _ = c.flush();
                }

                if let Ok(pty_fd) = self.pty.try_clone_master() {
                    std::thread::spawn(move || {
                        // Limit paste payload to at most 10 MiB to prevent memory exhaustion
                        let mut reader = std::fs::File::from(read_fd).take(10 * 1024 * 1024);
                        let mut bytes = Vec::new();
                        if reader.read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                            let payload = if bracketed {
                                let mut wrapped = Vec::with_capacity(bytes.len() + 12);
                                wrapped.extend_from_slice(b"\x1b[200~");
                                wrapped.extend_from_slice(&bytes);
                                wrapped.extend_from_slice(b"\x1b[201~");
                                wrapped
                            } else {
                                bytes
                            };
                            // PTY master is nonblocking: write with poll readiness loop to avoid truncation
                            let mut to_write = &payload[..];
                            let start = std::time::Instant::now();
                            while !to_write.is_empty()
                                && start.elapsed() < std::time::Duration::from_secs(5)
                            {
                                let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                                if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                                    break;
                                }
                                match nix::unistd::write(&pty_fd, to_write) {
                                    Ok(0) => break,
                                    Ok(written) => to_write = &to_write[written..],
                                    Err(Errno::EINTR | Errno::EAGAIN) => continue,
                                    Err(_) => break,
                                }
                            }
                        }
                    });
                    return;
                }
            }
            return;
        }

        // Fallback to internal clipboard buffer if offer not available
        let fallback_text = self.clipboard_text.clone();
        if let Some(text) = fallback_text {
            let payload = if bracketed {
                let mut wrapped = Vec::with_capacity(text.len() + 12);
                wrapped.extend_from_slice(b"\x1b[200~");
                wrapped.extend_from_slice(text.as_bytes());
                wrapped.extend_from_slice(b"\x1b[201~");
                wrapped
            } else {
                text.into_bytes()
            };

            if payload.len() <= 4096 {
                self.write_pty_blocking(&payload);
            } else if let Ok(pty_fd) = self.pty.try_clone_master() {
                std::thread::spawn(move || {
                    let mut to_write = &payload[..];
                    let start = std::time::Instant::now();
                    while !to_write.is_empty()
                        && start.elapsed() < std::time::Duration::from_secs(5)
                    {
                        let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                        if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                            break;
                        }
                        match nix::unistd::write(&pty_fd, to_write) {
                            Ok(0) => break,
                            Ok(written) => to_write = &to_write[written..],
                            Err(Errno::EINTR | Errno::EAGAIN) => continue,
                            Err(_) => break,
                        }
                    }
                });
            } else {
                // If cloning PTY master failed, do not block the main event loop with an oversized write;
                // write bounded head chunk only.
                self.write_pty_blocking(&payload[..4096]);
            }
        }
    }
}
