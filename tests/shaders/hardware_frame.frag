#version 450
layout(set=2, binding=0) uniform sampler2D imported_plane;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() { color = vec4(texture(imported_plane, texcoord).rgb, 1.0); }
