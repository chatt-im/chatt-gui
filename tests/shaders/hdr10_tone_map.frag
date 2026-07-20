#version 450
layout(set=0, binding=0) uniform sampler2D source_tex;
layout(location=0) in vec2 texcoord;
layout(location=0) out vec4 color;
void main() {
    vec3 pq = texture(source_tex, texcoord).rgb;
    vec3 power = pow(max(pq, vec3(0.0)), vec3(1.0 / 78.84375));
    vec3 linear = pow(max(power - vec3(0.8359375), vec3(0.0)) /
                      max(vec3(18.8515625) - vec3(18.6875) * power, vec3(0.0001)),
                      vec3(1.0 / 0.1593017578));
    color = vec4(linear / (linear + vec3(0.1)), 1.0);
}
