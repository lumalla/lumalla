struct Push {
    dest: vec4f,
    src_uv: vec4f,
    output_size: vec2f,
    force_opaque: f32,
    buffer_transform: u32,
}

var<push_constant> pc: Push;

struct Out {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) dest_uv: vec2f,
}

fn buffer_uv(uv: vec2f) -> vec2f {
    switch pc.buffer_transform {
        case 1u: { return vec2f(1.0 - uv.y, uv.x); }
        case 2u: { return vec2f(1.0 - uv.x, 1.0 - uv.y); }
        case 3u: { return vec2f(uv.y, 1.0 - uv.x); }
        case 4u: { return vec2f(1.0 - uv.x, uv.y); }
        case 5u: { return vec2f(uv.y, uv.x); }
        case 6u: { return vec2f(uv.x, 1.0 - uv.y); }
        case 7u: { return vec2f(1.0 - uv.y, 1.0 - uv.x); }
        default: { return uv; }
    }
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
    let transformed_uv = vec2f(
        pc.src_uv.x + dest_uv.x * (pc.src_uv.z - pc.src_uv.x),
        pc.src_uv.y + dest_uv.y * (pc.src_uv.w - pc.src_uv.y),
    );
    return Out(vec4f(ndc_x, ndc_y, 0.0, 1.0), buffer_uv(transformed_uv), dest_uv);
}
