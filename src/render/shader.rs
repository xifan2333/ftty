//! OpenGL ES 2.0 shader compilation and shader program linking.

use std::io;

use glow::HasContext;

pub const VERTEX_SHADER: &str = r#"
attribute vec2 a_position;
attribute vec2 a_tex_coords;
attribute vec4 a_color;
uniform vec2 u_viewport;
uniform vec2 u_atlas_size;
varying mediump vec2 v_tex_coords;
varying lowp vec4 v_color;
void main() {
    v_tex_coords = a_tex_coords / u_atlas_size;
    v_color = a_color;
    gl_Position = vec4(a_position / u_viewport * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
}
"#;

pub const FRAGMENT_SHADER: &str = r#"
precision mediump float;
varying mediump vec2 v_tex_coords;
varying lowp vec4 v_color;
uniform sampler2D u_texture;
// 0 = single-channel glyph coverage, 1 = RGBA kitty image placement.
uniform int u_image_mode;
void main() {
    if (u_image_mode == 1) {
        vec4 texel = texture2D(u_texture, v_tex_coords);
        gl_FragColor = vec4(texel.rgb, texel.a * v_color.a);
    } else {
        float alpha = v_tex_coords.x < 0.0 ? 1.0 : texture2D(u_texture, v_tex_coords).a;
        gl_FragColor = vec4(v_color.rgb, v_color.a * alpha);
    }
}
"#;

pub fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> io::Result<glow::Shader> {
    // SAFETY: callers hold the current EGL context.
    unsafe {
        let shader = gl.create_shader(kind).map_err(io::Error::other)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(io::Error::other(log));
        }
        Ok(shader)
    }
}

pub fn create_program(gl: &glow::Context) -> io::Result<glow::Program> {
    // SAFETY: initialization holds the current EGL context.
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
        let fragment = match compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT_SHADER) {
            Ok(shader) => shader,
            Err(error) => {
                gl.delete_shader(vertex);
                return Err(error);
            }
        };
        let program = match gl.create_program() {
            Ok(program) => program,
            Err(error) => {
                gl.delete_shader(vertex);
                gl.delete_shader(fragment);
                return Err(io::Error::other(error));
            }
        };
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.bind_attrib_location(program, 0, "a_position");
        gl.bind_attrib_location(program, 1, "a_tex_coords");
        gl.bind_attrib_location(program, 2, "a_color");
        gl.link_program(program);
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(io::Error::other(log));
        }
        Ok(program)
    }
}
