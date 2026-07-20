#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec4 sample_value = texture(source_tex, texcoord);
    color = vec4(sample_value.a, sample_value.r, sample_value.g, sample_value.b);
}
