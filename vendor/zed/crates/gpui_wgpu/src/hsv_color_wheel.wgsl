struct GlobalParams {
    viewport_size: vec2<f32>,
    premultiplied_alpha: u32,
    pad: u32,
}

@group(0) @binding(0) var<uniform> globals: GlobalParams;

struct Bounds {
    origin: vec2<f32>,
    size: vec2<f32>,
}

struct ContentMask {
    bounds: Bounds,
}

struct HsvColorWheel {
    order: u32,
    pad: u32,
    bounds: Bounds,
    content_mask: ContentMask,
    hue: f32,
    saturation: f32,
    value: f32,
    opacity: f32,
    ring_outer_radius: f32,
    ring_inner_radius: f32,
    triangle_radius: f32,
    geometry_pad: f32,
}

@group(1) @binding(0) var<storage, read> b_hsv_color_wheels: array<HsvColorWheel>;

const PI: f32 = 3.14159265359;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) wheel_id: u32,
    @location(1) clip_distances: vec4<f32>,
}

fn device_position(position: vec2<f32>) -> vec4<f32> {
    let normalized =
        position / globals.viewport_size * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0);
    return vec4<f32>(normalized, 0.0, 1.0);
}

@vertex
fn vs_hsv_color_wheel(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> VertexOutput {
    let wheel = b_hsv_color_wheels[instance_index];
    let unit = vec2<f32>(
        f32(vertex_index & 1u),
        0.5 * f32(vertex_index & 2u),
    );
    let position = wheel.bounds.origin + unit * wheel.bounds.size;

    var out: VertexOutput;
    out.position = device_position(position);
    out.wheel_id = instance_index;
    out.clip_distances = vec4<f32>(
        position.x - wheel.content_mask.bounds.origin.x,
        wheel.content_mask.bounds.origin.x + wheel.content_mask.bounds.size.x - position.x,
        position.y - wheel.content_mask.bounds.origin.y,
        wheel.content_mask.bounds.origin.y + wheel.content_mask.bounds.size.y - position.y,
    );
    return out;
}

fn hue_to_rgb(hue: f32) -> vec3<f32> {
    let r = abs(hue * 6.0 - 3.0) - 1.0;
    let g = 2.0 - abs(hue * 6.0 - 2.0);
    let b = 2.0 - abs(hue * 6.0 - 4.0);
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn over(below: vec4<f32>, above: vec4<f32>) -> vec4<f32> {
    let alpha = above.a + below.a * (1.0 - above.a);
    if (alpha <= 0.0) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(
        (above.rgb * above.a + below.rgb * below.a * (1.0 - above.a)) / alpha,
        alpha,
    );
}

fn layer(color: vec4<f32>, rgb: vec3<f32>, mask: f32) -> vec4<f32> {
    return over(color, vec4<f32>(rgb, clamp(mask, 0.0, 1.0)));
}

@fragment
fn fs_hsv_color_wheel(input: VertexOutput) -> @location(0) vec4<f32> {
    if (any(input.clip_distances < vec4<f32>(0.0))) {
        return vec4<f32>(0.0);
    }

    let wheel = b_hsv_color_wheels[input.wheel_id];
    let half_size = wheel.bounds.size * 0.5;
    let min_dimension = min(wheel.bounds.size.x, wheel.bounds.size.y);
    let uv = (input.position.xy - wheel.bounds.origin - half_size) / (min_dimension * 0.5);
    let distance = length(uv);
    let angle = atan2(uv.y, uv.x);
    var final_color = vec4<f32>(0.0);

    // Red is at the left and hue advances clockwise in screen coordinates.
    let pixel_hue = fract(0.5 + angle / (2.0 * PI) + 1.0);
    let ring_fe = max(fwidth(distance), 0.0001);
    let ring_mask =
        smoothstep(wheel.ring_inner_radius, wheel.ring_inner_radius + ring_fe, distance) *
        (1.0 - smoothstep(
            wheel.ring_outer_radius - ring_fe,
            wheel.ring_outer_radius,
            distance,
        ));
    final_color = layer(final_color, hue_to_rgb(pixel_hue), ring_mask);

    // Rotate the triangle so its saturated-hue vertex always points at the
    // selected position on the hue ring.
    let radius = wheel.triangle_radius;
    let selected_angle = (wheel.hue - 0.5) * 2.0 * PI;
    let angle_cos = cos(selected_angle);
    let angle_sin = sin(selected_angle);
    let triangle_point = vec2<f32>(
        angle_cos * uv.x + angle_sin * uv.y,
        -angle_sin * uv.x + angle_cos * uv.y,
    );
    let weight_hue = (2.0 * triangle_point.x / radius + 1.0) / 3.0;
    let non_hue_weight = 1.0 - weight_hue;
    let white_minus_black = triangle_point.y / (radius * 0.8660254);
    let weight_white = (non_hue_weight + white_minus_black) * 0.5;
    let weight_black = non_hue_weight - weight_white;
    let triangle_rgb =
        vec3<f32>(weight_white) + weight_hue * hue_to_rgb(wheel.hue);
    let minimum_weight = min(weight_black, min(weight_white, weight_hue));
    let triangle_fe = max(fwidth(minimum_weight), 0.0001);
    let triangle_mask = smoothstep(0.0, triangle_fe, minimum_weight);
    final_color = layer(final_color, triangle_rgb, triangle_mask);

    // Selected hue line.
    var angle_difference = abs(angle - selected_angle);
    if (angle_difference > PI) {
        angle_difference = 2.0 * PI - angle_difference;
    }
    let arc_distance = angle_difference * distance;
    let arc_fe = max(fwidth(arc_distance), 0.0001);
    let line_mask = 1.0 - smoothstep(0.006 - arc_fe, 0.006, arc_distance);
    let border_mask = 1.0 - smoothstep(0.011 - arc_fe, 0.011, arc_distance);
    final_color = layer(final_color, vec3<f32>(0.05), border_mask * ring_mask);
    final_color = layer(final_color, vec3<f32>(1.0), line_mask * ring_mask);

    // Selected saturation/value cursor.
    let vertex_black = vec2<f32>(-radius * 0.5, -radius * 0.8660254);
    let vertex_white = vec2<f32>(-radius * 0.5, radius * 0.8660254);
    let vertex_hue = vec2<f32>(radius, 0.0);
    let weights = vec3<f32>(
        1.0 - wheel.value,
        (1.0 - wheel.saturation) * wheel.value,
        wheel.saturation * wheel.value,
    );
    let local_target =
        weights.x * vertex_black + weights.y * vertex_white + weights.z * vertex_hue;
    let cursor_position = vec2<f32>(
        angle_cos * local_target.x - angle_sin * local_target.y,
        angle_sin * local_target.x + angle_cos * local_target.y,
    );
    let cursor_distance = length(uv - cursor_position);
    let cursor_fe = max(fwidth(cursor_distance), 0.0001);
    let outer_mask = 1.0 - smoothstep(0.032 - cursor_fe, 0.032, cursor_distance);
    let line_cursor = 1.0 - smoothstep(0.026 - cursor_fe, 0.026, cursor_distance);
    let inner_mask = 1.0 - smoothstep(0.018 - cursor_fe, 0.018, cursor_distance);
    let before_cursor = final_color;
    final_color = layer(final_color, vec3<f32>(0.05), outer_mask);
    final_color = layer(final_color, vec3<f32>(1.0), line_cursor);
    final_color = mix(final_color, before_cursor, inner_mask);

    let alpha = final_color.a * wheel.opacity;
    let multiplier = select(1.0, alpha, globals.premultiplied_alpha != 0u);
    return vec4<f32>(final_color.rgb * multiplier, alpha);
}
