#version 450
layout(set=0, binding=0) uniform sampler2D plane_y;
layout(set=0, binding=1) uniform sampler2D plane_uv;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    float y = texture(plane_y, texcoord).r;
    vec2 uv = texture(plane_uv, texcoord).rg - vec2(0.5);
    color = vec4(y + 1.5748 * uv.y, y - 0.1873 * uv.x - 0.4681 * uv.y,
                 y + 1.8556 * uv.x, 1.0);
}
