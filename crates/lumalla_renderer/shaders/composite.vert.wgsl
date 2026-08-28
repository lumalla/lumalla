struct Push {
    dest: vec4f,
    output_size: vec2f,
    force_opaque: f32,
}

var<push_constant> pc: Push;

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn main(@builtin(vertex_index) vi: u32) -> Out {
    var tex_uv = vec2f(0.0, 0.0);
    switch vi {
        case 1u: {
            tex_uv = vec2f(2.0, 0.0);
        }
        case 2u: {
            tex_uv = vec2f(0.0, 2.0);
        }
        default: {}
    }
    let x = pc.dest.x + tex_uv.x * pc.dest.z;
    let y = pc.dest.y + tex_uv.y * pc.dest.w;
    let ndc_x = (x / pc.output_size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (y / pc.output_size.y) * 2.0;
    return Out(vec4f(ndc_x, ndc_y, 0.0, 1.0), tex_uv);
}
