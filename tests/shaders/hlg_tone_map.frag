#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec3 hlg = texture(source_tex, texcoord).rgb;
    bvec3 low = lessThanEqual(hlg, vec3(0.5));
    vec3 linear_low = hlg * hlg / 3.0;
    vec3 linear_high = (exp((hlg - vec3(0.55991073)) / 0.17883277) + 0.28466892) / 12.0;
    color = vec4(mix(linear_high, linear_low, low), 1.0);
}
