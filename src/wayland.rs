//! Wayland client connection, globals binding, and XDG Shell window lifecycle.

use std::io::{Read, Write};
use std::os::fd::AsFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};

use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_output::{self, Subpixel, WlOutput},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, Capability, WlSeat},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::error::WaylandError;
use crate::event_loop::AppState;
use crate::input::mouse::{MouseModifiers, encode_mouse_event};
use crate::input::pointer::{PointerContext, PointerController, PointerOutcome};
pub use crate::input::pointer::{find_url_at_col, find_url_in_grid, x11_button_index};

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

// --- Init & Registry ---

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let comp = registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ());
                    state.wayland.compositor = Some(comp);
                    state.wayland.init_window(qh);
                    state.try_init_fractional_scale(qh);
                    state.try_init_viewport(qh);
                }
                "xdg_wm_base" => {
                    let xdg = registry.bind::<XdgWmBase, _, _>(name, 1, qh, ());
                    state.wayland.xdg_wm_base = Some(xdg);
                    state.wayland.init_window(qh);
                    state.try_init_fractional_scale(qh);
                    state.try_init_viewport(qh);
                }
                "wl_seat" => {
                    let seat = registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ());
                    state.wayland.seat = Some(seat);
                    state.wayland.init_text_input(qh);
                    state.wayland.init_data_device(qh);
                }
                "zwp_text_input_manager_v3" => {
                    let manager = registry.bind::<ZwpTextInputManagerV3, _, _>(name, 1, qh, ());
                    state.wayland.text_input_manager = Some(manager);
                    state.wayland.init_text_input(qh);
                }
                "wl_data_device_manager" => {
                    let manager = registry.bind::<WlDataDeviceManager, _, _>(name, 3, qh, ());
                    state.wayland.data_device_manager = Some(manager);
                    state.wayland.init_data_device(qh);
                }
                "wp_cursor_shape_manager_v1" => {
                    let manager = registry.bind::<WpCursorShapeManagerV1, _, _>(name, 1, qh, ());
                    state.wayland.cursor_shape_manager = Some(manager);
                    state.try_init_cursor_shape(qh);
                }
                "wp_fractional_scale_manager_v1" => {
                    let manager =
                        registry.bind::<WpFractionalScaleManagerV1, _, _>(name, 1, qh, ());
                    state.wayland.fractional_scale_manager = Some(manager);
                    state.try_init_fractional_scale(qh);
                }
                "wp_viewporter" => {
                    let viewporter = registry.bind::<WpViewporter, _, _>(name, 1, qh, ());
                    state.wayland.viewporter = Some(viewporter);
                    state.try_init_viewport(qh);
                }
                "wl_output" => {
                    let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                    state.wayland.outputs.push(output);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Geometry { subpixel, .. } = event
            && let wayland_client::WEnum::Value(sub) = subpixel
        {
            state.wayland.output_subpixel = Some(sub);
            let enable_subpixel = state.is_subpixel_preferred()
                && state.renderer.as_ref().is_some_and(|r| r.has_dual_source);
            let bgr = state.is_bgr_subpixel();
            if state.font_mgr.subpixel != enable_subpixel || state.font_mgr.bgr != bgr {
                state.font_mgr.subpixel = enable_subpixel;
                state.font_mgr.bgr = bgr;
                state.atlas.clear();
                state.needs_redraw = true;
            }
        }
    }
}

impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && state.frame_callback.as_ref() == Some(proxy)
        {
            state.frame_callback = None;
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.handle_preferred_scale(scale, conn);
        }
    }
}

impl Dispatch<WpViewporter, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// --- Window & XDG Shell ---

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _state: &mut Self,
        proxy: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            let _ = state.wayland.transition_window_to(WindowState::Configured);
            // Defer the resize until the queue is drained: back-to-back configure pairs then
            // collapse into one size change instead of scrolling content on a transient size.
            state.configure_pending = true;
        }
    }
}

/// Resolves new window dimensions for an `xdg_toplevel.configure` event.
///
/// When the compositor sends positive dimensions, they are scheduled as the new size.
/// If either dimension is zero, the client decides its own dimensions (matching WezTerm
/// and Foot): active live dimensions are preserved, superseding any stale queued sizes.
#[must_use]
pub(crate) fn handle_toplevel_configure_size(
    width: i32,
    height: i32,
    live_width: u32,
    live_height: u32,
) -> [u32; 2] {
    if width > 0 && height > 0 {
        [width as u32, height as u32]
    } else {
        [live_width, live_height]
    }
}

impl Dispatch<XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states: _,
            } => {
                state.pending_size = Some(handle_toplevel_configure_size(
                    width,
                    height,
                    state.wayland.width,
                    state.wayland.height,
                ));
            }
            xdg_toplevel::Event::Close => {
                state.wayland.close_requested = true;
                let _ = state.wayland.transition_window_to(WindowState::Closed);
                state.running = false;
            }
            _ => {}
        }
    }
}

// --- Seat & Input ---

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(Capability::Keyboard) && state.wayland.keyboard.is_none() {
                let keyboard = proxy.get_keyboard(qh, ());
                state.wayland.keyboard = Some(keyboard);
            }
            if caps.contains(Capability::Pointer) {
                if state.wayland.pointer.is_none() {
                    let pointer = proxy.get_pointer(qh, ());
                    state.wayland.pointer = Some(pointer);
                    state.try_init_cursor_shape(qh);
                }
            } else {
                if let Some(device) = state.wayland.cursor_shape_device.take() {
                    device.destroy();
                }
                if let Some(pointer) = state.wayland.pointer.take() {
                    pointer.release();
                }
                let outcome = state.pointer.handle_leave();
                if outcome.needs_redraw {
                    state.needs_redraw = true;
                }
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.keyboard.set_keymap_from_fd(fd, size as usize);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.session.terminal.focus_reporting {
                        state.session.write_input_bytes(b"\x1b[I");
                    }
                    state.ime.active = true;
                    state.last_ime_cursor_rect.set(None);
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.enable();
                        text_input.set_content_type(
                            zwp_text_input_v3::ContentHint::None,
                            zwp_text_input_v3::ContentPurpose::Terminal,
                        );
                        state.update_ime_cursor_area();
                    }
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.session.terminal.focus_reporting {
                        state.session.write_input_bytes(b"\x1b[O");
                    }
                    state.ime.clear();
                    state.last_ime_cursor_rect.set(None);
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.disable();
                        text_input.commit();
                    }
                    state.needs_redraw = true;
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.last_serial = serial;
                let pressed = key_state == KeyState::Pressed;
                if pressed
                    && let Some(action) =
                        state.keyboard.check_action(key, &state.config.keybindings)
                {
                    state.handle_key_action(action, Some(qh), Some(_conn));
                } else if let Some(bytes) = state.keyboard.handle_key_event(key, pressed, false) {
                    if pressed
                        && state.config.auto_scroll()
                        && !state.session.terminal.grid.is_alt_screen()
                    {
                        state.session.terminal.grid.scroll_viewport_bottom();
                    }
                    state.session.write_input_bytes(&bytes);
                }
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.last_serial = serial;
                state
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                state.update_hover_state();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_serial = serial;
                state.last_serial = serial;
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state
                    .pointer
                    .handle_enter(surface_x, surface_y, serial, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Leave { .. } => {
                let outcome = state.pointer.handle_leave();
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state.pointer.handle_motion(surface_x, surface_y, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                time,
                serial,
            } => {
                state.last_serial = serial;
                let pressed = button_state == ButtonState::Pressed;
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state
                    .pointer
                    .handle_button(button, pressed, time, serial, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state.pointer.handle_axis(value, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::AxisStop { .. } => {
                let outcome = state.pointer.handle_axis_stop();
                state.dispatch_pointer_outcome(outcome, conn);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeManagerV1,
        _event: wp_cursor_shape_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeDeviceV1,
        _event: wp_cursor_shape_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl AppState {
    /// Builds transient layout and metrics context for pointer operations with separate field borrows.
    pub(crate) fn make_pointer_context<'a>(
        terminal: &'a crate::parser::Terminal,
        font_mgr: &'a crate::font::FontManager,
        config: &'a crate::config::Config,
        keyboard: &'a crate::input::KeyboardHandler,
        wayland: &'a WaylandState,
        hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
    ) -> PointerContext<'a> {
        let scale = if wayland.is_fractional_scale_active() {
            wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        PointerContext::new(
            &terminal.grid,
            font_mgr.metrics,
            scale,
            [config.padding_x(), config.padding_y()],
            keyboard.modifiers(),
            &terminal.mouse,
            f64::from(config.scroll_multiplier()),
            terminal.grid.is_alt_screen(),
            hyperlink_resolver,
        )
    }

    /// Builds transient layout and metrics context for pointer operations.
    pub(crate) fn pointer_ctx<'a>(
        &'a self,
        hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
    ) -> PointerContext<'a> {
        Self::make_pointer_context(
            &self.session.terminal,
            &self.font_mgr,
            &self.config,
            &self.keyboard,
            &self.wayland,
            hyperlink_resolver,
        )
    }

    /// Dispatches declarative actions produced by [`PointerController`] to PTY, compositor, and system.
    pub(crate) fn dispatch_pointer_outcome(&mut self, outcome: PointerOutcome, conn: &Connection) {
        if let Some(bytes) = outcome.pty_write {
            self.write_pty_blocking(&bytes);
        }
        if let Some(shape) = outcome.cursor_shape
            && let Some(device) = &self.wayland.cursor_shape_device
        {
            device.set_shape(self.pointer_serial, shape);
        }
        if let Some(url) = outcome.open_url {
            std::thread::spawn(move || {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            });
        }
        if outcome.paste_primary {
            self.paste_clipboard(Some(conn));
        }
        if let Some(lines) = outcome.scroll_lines {
            if lines < 0 {
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_up(lines.unsigned_abs() as usize);
            } else {
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_down(lines as usize);
            }
            let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
            let ctx = Self::make_pointer_context(
                &self.session.terminal,
                &self.font_mgr,
                &self.config,
                &self.keyboard,
                &self.wayland,
                Some(&resolver),
            );
            let (_, shape_update) = self.pointer.update_hover_state(&ctx);
            if let Some(shape) = shape_update
                && let Some(device) = &self.wayland.cursor_shape_device
                && self.pointer_serial != 0
            {
                device.set_shape(self.pointer_serial, shape);
            }
        }
        if outcome.needs_redraw {
            self.needs_redraw = true;
        }
    }

    /// Initializes the cursor shape device if protocol is available and updates shape if focused.
    pub fn try_init_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.wayland.cursor_shape_device.is_none() {
            self.wayland.init_cursor_shape(qh);
            if self.wayland.cursor_shape_device.is_some()
                && self.pointer.is_in_surface()
                && self.pointer_serial != 0
            {
                self.pointer.current_cursor_shape = None;
                self.update_cursor_shape();
            }
        }
    }

    /// Updates the Wayland cursor shape based on whether a hyperlink is hovered or mouse tracking is active.
    /// Aligns with Kitty: Shape::Pointer on hyperlinks, Shape::Default when an app tracks the pointer, Shape::Text for text selection.
    pub fn update_cursor_shape(&mut self) {
        if !self.pointer.is_in_surface() || self.pointer_serial == 0 {
            return;
        }
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        let shape = self.pointer.expected_cursor_shape(&ctx);
        if self.pointer.current_cursor_shape() == Some(shape) {
            return;
        }
        if let Some(device) = &self.wayland.cursor_shape_device {
            device.set_shape(self.pointer_serial, shape);
            self.pointer.current_cursor_shape = Some(shape);
        }
    }

    /// Updates the hovered hyperlink and cursor shape based on current pointer coordinates.
    pub fn update_hover_state(&mut self) {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = Self::make_pointer_context(
            &self.session.terminal,
            &self.font_mgr,
            &self.config,
            &self.keyboard,
            &self.wayland,
            Some(&resolver),
        );
        let (span_changed, shape_update) = self.pointer.update_hover_state(&ctx);
        if span_changed {
            self.needs_redraw = true;
        }
        if let Some(shape) = shape_update
            && let Some(device) = &self.wayland.cursor_shape_device
            && self.pointer_serial != 0
        {
            device.set_shape(self.pointer_serial, shape);
        }
    }

    /// Returns the resolved URL at the current pointer position if one exists.
    #[must_use]
    pub fn url_at_pointer(&self) -> Option<String> {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        self.pointer.url_at_pointer(&ctx)
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the surface-relative pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, surface_x: f64, surface_y: f64) -> (usize, usize, usize) {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        PointerController::cell_at_coords(surface_x, surface_y, &ctx)
    }

    /// Encodes a pointer event for the application, or `None` when the event stays local.
    ///
    /// Mouse reports are suppressed while tracking is disabled and while Shift is held,
    /// which is the conventional override that hands the pointer back to text selection.
    #[doc(hidden)]
    pub fn mouse_report_bytes(&self, button: u8, pressed: bool, motion: bool) -> Option<Vec<u8>> {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        if !ctx.mouse_mode.is_reporting() {
            return None;
        }
        let modifiers = self.keyboard.modifiers();
        if modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = PointerController::cell_at_coords(
            self.pointer.mouse_pos[0],
            self.pointer.mouse_pos[1],
            &ctx,
        );
        encode_mouse_event(
            ctx.mouse_mode.encoding,
            button,
            col,
            screen_row,
            pressed,
            motion,
            MouseModifiers {
                shift: modifiers.shift,
                alt: modifiers.alt,
                ctrl: modifiers.ctrl,
            },
        )
    }
}

// --- Text Input (IME) ---

impl Dispatch<ZwpTextInputManagerV3, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.active = true;
                }
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.clear();
                    state.needs_redraw = true;
                }
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.ime.stage_preedit(text, cursor_begin, cursor_end);
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime.stage_commit(text);
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.ime.stage_delete(before_length, after_length);
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let (delete, commit) = state.ime.apply_done();
                if let Some((before, after)) = delete {
                    for _ in 0..before {
                        state.session.write_input_bytes(b"\x08");
                    }
                    for _ in 0..after {
                        state.session.write_input_bytes(b"\x1b[3~");
                    }
                }
                if let Some(text) = commit {
                    state.session.write_input_bytes(text.as_bytes());
                }
                state.needs_redraw = true;
            }
            _ => {}
        }
    }
}

impl AppState {
    /// Updates the Wayland `text-input-v3` cursor bounding box so the IME popup window tracks the cursor.
    /// Aligns with Kitty (`glfw/wl_text_input.c`): deduplicates cursor rectangle bounds and only commits when coordinates change.
    pub fn update_ime_cursor_area(&self) {
        if !self.ime.active {
            return;
        }
        let Some(text_input) = &self.wayland.text_input else {
            return;
        };
        let (x, y, w, h) = crate::input::ime::calculate_cursor_rect(
            &self.session.terminal.grid,
            self.font_mgr.metrics,
            [self.config.padding_x(), self.config.padding_y()],
        );
        let scale = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        let logical_x = (f64::from(x) / scale).round() as i32;
        let logical_y = (f64::from(y) / scale).round() as i32;
        let logical_w = (f64::from(w) / scale).round().max(1.0) as i32;
        let logical_h = (f64::from(h) / scale).round().max(1.0) as i32;
        let new_rect = (logical_x, logical_y, logical_w, logical_h);

        if self.last_ime_cursor_rect.get() != Some(new_rect) {
            self.last_ime_cursor_rect.set(Some(new_rect));
            text_input.set_cursor_rectangle(logical_x, logical_y, logical_w, logical_h);
            text_input.commit();
        }
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
        let bracketed = self.session.terminal.bracketed_paste;
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types)
                && let Ok((read_fd, write_fd)) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            {
                offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                drop(write_fd);

                if let Some(c) = conn {
                    let _ = c.flush();
                }

                if let Ok(pty_fd) = self.session.pty.try_clone_master() {
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
            } else if let Ok(pty_fd) = self.session.pty.try_clone_master() {
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

pub mod clipboard {
    pub use super::best_text_mime;
}

pub mod seat {
    pub use super::{find_url_at_col, find_url_in_grid, x11_button_index};
}

pub mod window {
    pub use super::WindowState;
}

pub mod init {}
pub mod text_input {}
pub mod xdg {}
