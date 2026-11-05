//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

use std::io::Write;
use std::os::fd::AsFd;

use nix::fcntl::OFlag;

use wayland_client::protocol::{
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::WlKeyboard,
    wl_output::{Subpixel, WlOutput},
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

use crate::error::WaylandError;
use crate::event_loop::AppState;
pub use crate::input::pointer::{find_url_at_col, find_url_in_grid, x11_button_index};

mod ime;
mod input;
mod pointer;
mod registry;
mod window;

// --- Window Lifecycle State Machine ---

/// Formal window lifecycle states for Wayland surfaces and XDG Shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowState {
    /// Window surface is unmapped or not yet created.
    #[default]
    Unmapped,
    /// Globals bound and surface requested, awaiting initial configure.
    Initializing,
    /// Received initial configure event from compositor, ready for rendering.
    Configured,
    /// Surface actively rendering and presenting frames to compositor.
    Active,
    /// Window close requested or surface destroyed.
    Closed,
}

impl WindowState {
    /// Human-readable label for error diagnostics and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmapped => "Unmapped",
            Self::Initializing => "Initializing",
            Self::Configured => "Configured",
            Self::Active => "Active",
            Self::Closed => "Closed",
        }
    }

    /// Validates whether transitioning from `self` to `next` is permitted.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Unmapped, Self::Initializing)
                | (Self::Initializing, Self::Configured)
                | (Self::Configured, Self::Active)
                | (Self::Active, Self::Active)
                | (Self::Active, Self::Configured)
                | (Self::Configured, Self::Configured)
                | (Self::Initializing, Self::Initializing)
                | (_, Self::Closed)
        )
    }

    /// Transitions to the next state, or returns [`WaylandError::InvalidStateTransition`].
    ///
    /// # Errors
    /// Returns an error if the transition violates the protocol state machine.
    pub fn transition_to(&mut self, next: Self) -> Result<(), WaylandError> {
        if self.can_transition_to(next) {
            *self = next;
            Ok(())
        } else {
            Err(WaylandError::InvalidStateTransition {
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}

// --- Wayland State ---

/// An incoming clipboard selection offer together with its advertised MIME types.
#[derive(Debug, Clone)]
pub struct OfferData {
    pub offer: WlDataOffer,
    pub mime_types: Vec<String>,
}

/// Tracks the lifecycle and handles of Wayland client globals and window surfaces.
#[derive(Debug, Default)]
pub struct WaylandState {
    pub compositor: Option<WlCompositor>,
    pub xdg_wm_base: Option<XdgWmBase>,
    pub seat: Option<WlSeat>,
    pub keyboard: Option<WlKeyboard>,
    pub pointer: Option<WlPointer>,
    pub text_input_manager: Option<ZwpTextInputManagerV3>,
    pub text_input: Option<ZwpTextInputV3>,
    pub cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    pub cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    pub fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
    pub fractional_scale: Option<WpFractionalScaleV1>,
    pub viewporter: Option<WpViewporter>,
    pub viewport: Option<WpViewport>,
    pub scale_factor: f64,
    pub preferred_scale_120: u32,
    pub data_device_manager: Option<WlDataDeviceManager>,
    pub data_device: Option<WlDataDevice>,
    pub data_source: Option<WlDataSource>,
    pub current_offer: Option<OfferData>,

    pub surface: Option<WlSurface>,
    pub xdg_surface: Option<XdgSurface>,
    pub xdg_toplevel: Option<XdgToplevel>,

    pub outputs: Vec<WlOutput>,
    pub output_subpixel: Option<Subpixel>,

    pub width: u32,
    pub height: u32,
    pub app_id: String,
    pub title: String,
    pub configured: bool,
    pub window_state: WindowState,
    pub close_requested: bool,
}

impl WaylandState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: 720,
            height: 480,
            scale_factor: 1.0,
            preferred_scale_120: 120,
            output_subpixel: Some(Subpixel::HorizontalRgb),
            app_id: "ftty".to_string(),
            title: "ftty".to_string(),
            ..Default::default()
        }
    }

    /// Returns `true` if both fractional scale and viewport extensions are active on the surface.
    #[must_use]
    pub fn is_fractional_scale_active(&self) -> bool {
        (self.fractional_scale.is_some() && self.viewport.is_some())
            || (self.fractional_scale.is_none() && (self.scale_factor - 1.0).abs() > 0.001)
    }

    /// Transitions the window lifecycle state machine.
    ///
    /// # Errors
    /// Returns [`crate::error::WaylandError::InvalidStateTransition`] if the transition violates the lifecycle model.
    pub fn transition_window_to(
        &mut self,
        next: WindowState,
    ) -> Result<(), crate::error::WaylandError> {
        self.window_state.transition_to(next)?;
        self.configured = matches!(
            self.window_state,
            WindowState::Configured | WindowState::Active
        );
        Ok(())
    }

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

pub mod clipboard {
    pub use super::best_text_mime;
}

pub mod seat {
    pub use super::{find_url_at_col, find_url_in_grid, x11_button_index};
}

pub mod init {}
pub mod text_input {}
pub mod xdg {}
