struct Push {
    dest: vec4f,
    src_uv: vec4f,
    output_size: vec2f,
    force_opaque: f32,
}

var<push_constant> pc: Push;

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var tex_sampler: sampler;

struct In {
    @location(0) uv: vec2f,
    @location(1) dest_uv: vec2f,
}

@fragment
fn main(input: In) -> @location(0) vec4f {
    // The vertex stage emits an oversized triangle; only the unit-square dest UV
    // region belongs to the dest rect. Without this discard, clamped edge
    // samples smear one triangle-leg past the bottom/right of every layer
    // and leave a trail when the cursor moves up/left.
    if input.dest_uv.x < 0.0 || input.dest_uv.y < 0.0 || input.dest_uv.x > 1.0 || input.dest_uv.y > 1.0 {
        discard;
    }
    var color = textureSample(tex, tex_sampler, input.uv);
    if pc.force_opaque > 0.5 {
        color.a = 1.0;
    }
    return color;
}
