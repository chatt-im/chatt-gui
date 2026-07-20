#version 450
#define LUT_POS(x, lut_size) mix(0.5 / (lut_size), 1.0 - 0.5 / (lut_size), (x))
layout(location=0) out vec4 out_color;
layout(location=1) in vec2 texcoord0;
layout(location=2) in vec2 texcoord1;
layout(std140, binding=2) uniform UBO {
layout(offset=0) mat3 colormatrix;
layout(offset=48) mat2 texture_rot0;
layout(offset=80) mat2 texture_rot1;
};
layout(std430, push_constant) uniform PushC {
layout(offset=0) vec3 colormatrix_c;
layout(offset=16) vec2 texture_size0;
layout(offset=24) vec2 texture_off0;
layout(offset=32) vec2 pixel_size0;
layout(offset=40) vec2 texture_size1;
layout(offset=48) vec2 texture_off1;
layout(offset=56) vec2 pixel_size1;
};
layout(binding=0) uniform sampler2D texture0;
layout(binding=1) uniform sampler2D texture1;
void main() {
    vec2 pos0 = texture_rot0 * texcoord0 + texture_off0 * pixel_size0;
    vec2 pos1 = texture_rot1 * texcoord1 + texture_off1 * pixel_size1;
    vec4 color = vec4(texture(texture0, pos0).r, texture(texture1, pos1).rg, 1.0);
    color.rgb = mat3(colormatrix) * color.rgb + colormatrix_c;
    color.rgb += vec3(0.0 * (texture_size0.x + texture_size1.x));
    out_color = color;
}
