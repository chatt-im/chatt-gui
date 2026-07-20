#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec2 size = vec2(textureSize(source_tex, 0));
    vec2 pixel = texcoord * size - vec2(0.5);
    vec2 base = floor(pixel);
    vec2 weight = fract(pixel);
    vec4 a = texelFetch(source_tex, ivec2(base), 0);
    vec4 b = texelFetch(source_tex, ivec2(base) + ivec2(1, 0), 0);
    color = mix(a, b, weight.x);
}
