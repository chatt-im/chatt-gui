#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec3 linear = pow(max(texture(source_tex, texcoord).rgb, vec3(0.0)), vec3(2.2));
    color = vec4(pow(linear / (linear + vec3(1.0)), vec3(1.0 / 2.2)), 1.0);
}
