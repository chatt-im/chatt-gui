#version 450
layout(set=0, binding=0, r32f) coherent restrict uniform image2D storage_image;
layout(std430, set=0, binding=1) coherent restrict buffer Data {
    float value;
} data;
layout(location=0) out vec4 out_color;
void main() {
    out_color = imageLoad(storage_image, ivec2(0)) + vec4(data.value);
}
