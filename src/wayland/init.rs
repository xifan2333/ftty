//! Wayland registry, compositor, surface, and frame callback dispatch.

use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_data_device_manager::WlDataDeviceManager,
    wl_output::{self, WlOutput},
    wl_registry::{self, WlRegistry},
    wl_seat::WlSeat,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;

use crate::event_loop::AppState;

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
