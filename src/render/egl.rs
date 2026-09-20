//! EGL context initialization, native Wayland window binding, and context teardown.

use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};
use wayland_egl::WlEglSurface;

use crate::error::RenderError;

pub(crate) struct EglContext {
    pub(crate) egl: egl::DynamicInstance<egl::EGL1_5>,
    pub(crate) display: egl::Display,
    pub(crate) context: Option<egl::Context>,
    pub(crate) surface: Option<egl::Surface>,
    pub(crate) initialized: bool,
    // Field order keeps the Wayland connection alive through native window destruction.
    pub(crate) window: WlEglSurface,
    _surface: WlSurface,
    _connection: Connection,
}

impl EglContext {
    pub(crate) fn new(
        surface: &WlSurface,
        connection: &Connection,
        size: [u32; 2],
    ) -> Result<Self, RenderError> {
        let [width, height] = native_size(size)?;
        let window = WlEglSurface::new(surface.id(), width, height)
            .map_err(|e| RenderError::EglSurface(format!("{e:?}")))?;
        // SAFETY: the library stays loaded in this instance for all EGL calls.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
        // SAFETY: connection owns the live libwayland display and is retained below.
        let display = unsafe { egl.get_display(connection.backend().display_ptr().cast()) }
            .ok_or_else(|| RenderError::EglDisplay("eglGetDisplay failed".to_string()))?;
        egl.initialize(display)
            .map_err(|e| RenderError::EglInit(format!("{e:?}")))?;
        // Own each handle as soon as it exists, including during failed initialization.
        let mut context = Self {
            egl,
            display,
            context: None,
            surface: None,
            initialized: false,
            window,
            _surface: surface.clone(),
            _connection: connection.clone(),
        };

        let init_result = (|| -> Result<(), RenderError> {
            context
                .egl
                .bind_api(egl::OPENGL_ES_API)
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
            let config = context
                .egl
                .choose_first_config(
                    display,
                    &[
                        egl::SURFACE_TYPE,
                        egl::WINDOW_BIT,
                        egl::RENDERABLE_TYPE,
                        egl::OPENGL_ES3_BIT,
                        egl::RED_SIZE,
                        8,
                        egl::GREEN_SIZE,
                        8,
                        egl::BLUE_SIZE,
                        8,
                        egl::ALPHA_SIZE,
                        8,
                        egl::NONE,
                    ],
                )
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?
                .ok_or(RenderError::NoSupportedConfig)?;
            context.context = Some(
                context
                    .egl
                    .create_context(
                        display,
                        config,
                        None,
                        &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
                    )
                    .map_err(|e| RenderError::ContextCreation(format!("{e:?}")))?,
            );
            // SAFETY: window wraps a live wl_surface on this EGL display.
            context.surface = Some(
                unsafe {
                    context.egl.create_window_surface(
                        display,
                        config,
                        context.window.ptr().cast_mut(),
                        None,
                    )
                }
                .map_err(|e| RenderError::SurfaceCreation(format!("{e:?}")))?,
            );
            context.make_current()?;
            // Frame callbacks pace drawing; swapping must not block PTY and signal dispatch.
            context
                .egl
                .swap_interval(display, 0)
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
            Ok(())
        })();

        if let Err(err) = init_result {
            let _ = context.egl.make_current(display, None, None, None);
            if let Some(surface) = context.surface {
                let _ = context.egl.destroy_surface(display, surface);
            }
            if let Some(ctx) = context.context {
                let _ = context.egl.destroy_context(display, ctx);
            }
            let _ = context.egl.terminate(display);
            return Err(err);
        }

        context.initialized = true;
        Ok(context)
    }

    pub(crate) fn make_current(&self) -> Result<(), RenderError> {
        self.egl
            .make_current(self.display, self.surface, self.surface, self.context)
            .map_err(|e| RenderError::MakeCurrent(format!("{e:?}")))
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        if let Some(surface) = self.surface {
            let _ = self.egl.destroy_surface(self.display, surface);
        }
        if let Some(context) = self.context {
            let _ = self.egl.destroy_context(self.display, context);
        }
        let _ = self.egl.terminate(self.display);
    }
}

pub fn native_size([width, height]: [u32; 2]) -> Result<[i32; 2], RenderError> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(RenderError::InvalidDimensions(width, height));
    }
    Ok([width as i32, height as i32])
}
