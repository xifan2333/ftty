//! Wayland protocol adapters: shared client state and the window lifecycle state machine.

use wayland_client::protocol::{
    wl_compositor::WlCompositor,
    wl_data_device::WlDataDevice,
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::WlDataOffer,
    wl_data_source::WlDataSource,
    wl_keyboard::WlKeyboard,
    wl_output::{Subpixel, WlOutput},
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
};
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
pub use crate::input::pointer::{find_url_at_col, find_url_in_grid, x11_button_index};

pub mod clipboard;
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
}

/// Compatibility path for the pointer grid helpers, which live in [`crate::input::pointer`].
pub mod seat {
    pub use super::{find_url_at_col, find_url_in_grid, x11_button_index};
}
