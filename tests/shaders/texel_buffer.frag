#version 450
#extension GL_EXT_texture_buffer : require

layout(set = 0, binding = 0) uniform samplerBuffer lookup_table;
layout(location = 0) out vec4 color;

void main()
{
    int last = textureSize(lookup_table) - 1;
    color = texelFetch(lookup_table, max(last, 0));
}
