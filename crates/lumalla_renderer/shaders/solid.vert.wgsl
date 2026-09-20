struct Push {
    dest: vec4f,
    color: vec4f,
    output_size: vec2f,
    // half_width < 0 → solid fill of dest; otherwise distance-to-segment line.
    half_width: f32,
    _pad: f32,
    p0: vec2f,
    p1: vec2f,
}

var<push_constant> pc: Push;

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) dest_uv: vec2f,
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
    return Out(vec4f(ndc_x, ndc_y, 0.0, 1.0), dest_uv);
}
