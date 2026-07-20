#version 450
layout(set=0, binding=0) uniform sampler2D chroma_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec2 texel = 1.0 / vec2(textureSize(chroma_tex, 0));
    vec2 chroma = (texture(chroma_tex, texcoord - vec2(texel.x, 0.0)).rg +
                   2.0 * texture(chroma_tex, texcoord).rg +
                   texture(chroma_tex, texcoord + vec2(texel.x, 0.0)).rg) * 0.25;
    color = vec4(chroma, 0.0, 1.0);
}
