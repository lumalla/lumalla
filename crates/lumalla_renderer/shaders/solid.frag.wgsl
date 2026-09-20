struct Push {
    dest: vec4f,
    color: vec4f,
    output_size: vec2f,
    half_width: f32,
    _pad: f32,
    p0: vec2f,
    p1: vec2f,
}

var<push_constant> pc: Push;

struct In {
    @location(0) dest_uv: vec2f,
}

fn dist_to_segment(p: vec2f, a: vec2f, b: vec2f) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 1e-6);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

@fragment
fn main(input: In) -> @location(0) vec4f {
    if input.dest_uv.x < 0.0 || input.dest_uv.y < 0.0 || input.dest_uv.x > 1.0 || input.dest_uv.y > 1.0 {
        discard;
    }
    if pc.half_width >= 0.0 {
        let p = vec2f(
            pc.dest.x + input.dest_uv.x * pc.dest.z,
            pc.dest.y + input.dest_uv.y * pc.dest.w,
        );
        // +0.5 covers pixel-center sampling / float error on thin strokes.
        if dist_to_segment(p, pc.p0, pc.p1) > (pc.half_width + 0.5) {
            discard;
        }
    }
    return pc.color;
}
