#version 450
#extension GL_ARB_texture_gather : require

layout(set = 0, binding = 0) uniform sampler2D source_image;
layout(location = 0) in vec2 texture_coord;
layout(location = 0) out vec4 color;

vec4 gather_inner(sampler2D source, vec2 position)
{
    return textureGather(source, position, 1)
         + textureGatherOffset(source, position, ivec2(1, -1), 2);
}

vec4 gather_taps(sampler2D source, vec2 position)
{
    return gather_inner(source, position);
}

void main()
{
    color = gather_taps(source_image, texture_coord);
}
