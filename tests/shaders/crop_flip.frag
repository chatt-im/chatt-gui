#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec2 cropped = vec2(0.125, 0.875) + vec2(texcoord.x, 1.0 - texcoord.y) * vec2(0.75, -0.75);
    color = texture(source_tex, cropped);
}
