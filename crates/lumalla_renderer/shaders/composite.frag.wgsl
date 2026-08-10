struct Push {
    dest: vec4f,
    output_size: vec2f,
    force_opaque: f32,
}

var<push_constant> pc: Push;

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var tex_sampler: sampler;

struct In {
    @location(0) uv: vec2f,
}

@fragment
fn main(input: In) -> @location(0) vec4f {
    var color = textureSample(tex, tex_sampler, input.uv);
    if pc.force_opaque > 0.5 {
        color.a = 1.0;
    }
    return color;
}
