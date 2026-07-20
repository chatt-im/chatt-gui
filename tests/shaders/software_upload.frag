#version 450
layout(set=1, binding=0) uniform sampler2D upload_plane;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() { color = texture(upload_plane, texcoord); }
