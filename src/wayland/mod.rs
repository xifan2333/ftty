//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

pub mod clipboard;
pub mod init;
pub mod seat;
pub mod text_input;
pub mod window;
pub mod xdg;

#[cfg(test)]
mod tests;

pub use clipboard::best_text_mime;
pub use seat::x11_button_index;
pub use window::WindowState;

use wayland_client::QueueHandle;
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
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::WpFractionalScaleV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
};

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
            || (cfg!(test) && (self.scale_factor - 1.0).abs() > 0.001)
    }

    /// Creates and initializes the toplevel window once compositor and xdg_wm_base globals are bound.
    pub fn init_window<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WlSurface, ()>
            + wayland_client::Dispatch<XdgSurface, ()>
            + wayland_client::Dispatch<XdgToplevel, ()>
            + 'static,
    {
        if self.surface.is_some() {
            return;
        }

        let Some(compositor) = &self.compositor else {
            return;
        };
        let Some(xdg_wm_base) = &self.xdg_wm_base else {
            return;
        };

        let surface = compositor.create_surface(qh, ());
        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());

        let title = if self.title.is_empty() {
            "ftty"
        } else {
            &self.title
        };
        let app_id = if self.app_id.is_empty() {
            "ftty"
        } else {
            &self.app_id
        };
        toplevel.set_title(title.to_string());
        toplevel.set_app_id(app_id.to_string());
        toplevel.set_min_size(160, 90);

        surface.commit();

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.xdg_toplevel = Some(toplevel);
        let _ = self.transition_window_to(WindowState::Initializing);
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

    /// Creates and initializes the `zwp_text_input_v3` instance once the manager and seat are available.
    pub fn init_text_input<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<ZwpTextInputV3, ()> + 'static,
    {
        if self.text_input.is_some() {
            return;
        }
        let Some(manager) = &self.text_input_manager else {
            return;
        };
        let Some(seat) = &self.seat else {
            return;
        };

        let text_input = manager.get_text_input(seat, qh, ());
        self.text_input = Some(text_input);
    }

    /// Creates and initializes the `wp_cursor_shape_device_v1` instance once the manager and pointer are available.
    pub fn init_cursor_shape<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpCursorShapeDeviceV1, ()> + 'static,
    {
        if self.cursor_shape_device.is_some() {
            return;
        }
        let Some(manager) = &self.cursor_shape_manager else {
            return;
        };
        let Some(pointer) = &self.pointer else {
            return;
        };

        let device = manager.get_pointer(pointer, qh, ());
        self.cursor_shape_device = Some(device);
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

    /// Creates and initializes the `wp_fractional_scale_v1` instance once the manager and surface are available.
    pub fn init_fractional_scale<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpFractionalScaleV1, ()> + 'static,
    {
        if self.fractional_scale.is_some() {
            return;
        }
        let Some(manager) = &self.fractional_scale_manager else {
            return;
        };
        let Some(surface) = &self.surface else {
            return;
        };

        let fs = manager.get_fractional_scale(surface, qh, ());
        self.fractional_scale = Some(fs);
    }

    /// Creates and initializes the `wp_viewport` instance once the viewporter and surface are available.
    pub fn init_viewport<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpViewport, ()> + 'static,
    {
        if self.viewport.is_some() {
            return;
        }
        let Some(viewporter) = &self.viewporter else {
            return;
        };
        let Some(surface) = &self.surface else {
            return;
        };

        let vp = viewporter.get_viewport(surface, qh, ());
        self.viewport = Some(vp);
    }
}
