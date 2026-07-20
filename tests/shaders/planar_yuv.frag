#version 450
layout(set=0, binding=0) uniform sampler2D plane_y;
layout(set=0, binding=1) uniform sampler2D plane_u;
layout(set=0, binding=2) uniform sampler2D plane_v;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec3 yuv = vec3(texture(plane_y, texcoord).r,
                    texture(plane_u, texcoord).r - 0.5,
                    texture(plane_v, texcoord).r - 0.5);
    color = vec4(mat3(1.0, 1.0, 1.0, 0.0, -0.1873, 1.8556,
                      1.5748, -0.4681, 0.0) * yuv, 1.0);
}
