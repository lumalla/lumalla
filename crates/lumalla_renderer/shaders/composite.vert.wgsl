struct Push {
    dest: vec4f,
    src_uv: vec4f,
    output_size: vec2f,
    force_opaque: f32,
}

var<push_constant> pc: Push;

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) dest_uv: vec2f,
}

@vertex
fn main(@builtin(vertex_index) vi: u32) -> Out {
    var dest_uv = vec2f(0.0, 0.0);
    switch vi {
        case 1u: {
            dest_uv = vec2f(2.0, 0.0);
        }
        case 2u: {
            dest_uv = vec2f(0.0, 2.0);
        }
        default: {}
    }
    let x = pc.dest.x + dest_uv.x * pc.dest.z;
    let y = pc.dest.y + dest_uv.y * pc.dest.w;
    let ndc_x = (x / pc.output_size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (y / pc.output_size.y) * 2.0;
    let tex_uv = vec2f(
        pc.src_uv.x + dest_uv.x * (pc.src_uv.z - pc.src_uv.x),
        pc.src_uv.y + dest_uv.y * (pc.src_uv.w - pc.src_uv.y),
    );
    return Out(vec4f(ndc_x, ndc_y, 0.0, 1.0), tex_uv, dest_uv);
}
