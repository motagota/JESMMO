// The bay/river water surface: one translucent plane at sea level (see
// World._build_water). The look is deliberately muddy-estuary, not
// tropical blue — the Brisbane River and Moreton Bay mouth are silt brown
// (same reasoning as GroundPaint._RIVER_COLOR, which now reads as the bed
// showing through this surface).
//
// Everything is computed per-fragment off the depth buffer, so the shoreline
// needs no geometry or mask data: where the ground behind the surface is
// close below the waterline (shallow), the water fades out and lightens
// toward silt; where it drops away (the real below-sea channel bathymetry),
// it darkens and goes opaque. The waterline therefore hugs the real rendered
// terrain at whichever LOD is resident — coarse backdrop or streamed 5m
// tiles — with zero coupling to the streamer.
//
// Depth is measured VERTICALLY (waterline minus reconstructed ground
// height), not along the view ray: the DEM fills the river/bay NoData
// footprint at exactly 0.0m — the water SURFACE — so the plane floats only
// ~a scene unit above most of the "bed" and an along-ray measure would
// read the whole channel as a grazing-angle-dependent thin film (verified:
// it rendered as see-through silt with the bed dominating). The constants
// below are correspondingly small: the fill footprint must already count
// as fully-covered water.
shader_type spatial;
render_mode blend_mix, depth_draw_never, cull_back, specular_schlick_ggx;

// Silt-laden shallows vs the deeper channel. shallow_color intentionally
// sits near GroundPaint._RIVER_COLOR so the fade from wet bank to shallow
// water is continuous.
// Suspended silt scatters a lot of light — even the "deep" color stays a
// readable mid brown-green; near-black water read as an oil slick, not an
// estuary (verified on an early render).
uniform vec3 shallow_color : source_color = vec3(0.32, 0.28, 0.17);
uniform vec3 deep_color : source_color = vec3(0.17, 0.19, 0.14);
// Must track Main._build_environment's sky/fog color — the fresnel blends
// toward it so grazing-angle water reads as reflected sky.
uniform vec3 sky_color : source_color = vec3(0.55, 0.63, 0.72);
// Vertical scene-units of water at which the murk color saturates. Small on
// purpose: with ~0.9 of column over the streamed 0m fill (and ~1.2 over the
// coarse backdrop's un-biased fill), both tiers must land on the saturated
// end so the streamed-ring boundary doesn't draw itself as a murk step.
uniform float murk_depth = 0.15;
// Alpha ramps 0->full over this much vertical depth: a soft waterline a few
// metres wide on a typical bank slope instead of a hard polygon edge.
uniform float shore_fade = 0.25;
// 0.25 reads as a slow estuary chop; 0.35 looked like open-ocean swell.
uniform float wave_strength = 0.25;
uniform sampler2D depth_tex : hint_depth_texture, filter_nearest;

varying vec3 wpos; // world-space — wave phase must not depend on the camera

void vertex() {
    wpos = (MODEL_MATRIX * vec4(VERTEX, 1.0)).xyz;
}

// One directional sine's contribution to the surface gradient, killed as
// its phase approaches a radian per pixel (fwidth). The cutoff must be
// HARD and early: gentler falloffs (distance LOD, then exp(-fwidth^2/2))
// both left whichever wave sat near the pixel grid's Nyquist rate alive at
// half strength, and its specular/fresnel glints rendered as a stippled
// moiré lattice at every distance — the surviving pattern is always the
// one the screen can just barely not resolve. Below ~15px per period the
// wave is gone; `killed` reports the culled amplitude so the fragment can
// convert it into roughness instead (the microfacets are still there,
// they're just sub-pixel now).
vec2 wave(vec2 p, vec2 dir, float freq, float speed, float amp, float t, inout float killed) {
    float ph = dot(p, dir) * freq + t * speed;
    // Bend the crests: a slow phase wobble along the crest direction turns
    // ruler-straight sine lines into scallops. Without it the sun's glitter
    // path picked out each straight crest as a parallel bright band and the
    // surface read as corrugated iron from any elevated view.
    ph += sin(dot(p, vec2(-dir.y, dir.x)) * freq * 0.35 + t * speed * 0.7) * 1.2;
    float aa = 1.0 - smoothstep(0.2, 0.45, fwidth(ph));
    killed += amp * (1.0 - aa);
    return dir * cos(ph) * amp * aa;
}

// Analytic gradient of a few summed directional sines (frequencies in
// rad/m: wavelengths ~6m down to ~1m). Cheap, tiles the whole 25.6km plane
// without a texture, and being a gradient it perturbs the normal exactly.
vec2 wave_grad(vec2 p, float t, inout float killed) {
    vec2 g = vec2(0.0);
    g += wave(p, vec2(0.80, 0.60), 1.10, 1.30, 1.00, t, killed);
    g += wave(p, vec2(-0.62, 0.78), 1.90, 1.70, 0.60, t, killed);
    g += wave(p, vec2(0.20, -0.98), 3.70, 2.30, 0.35, t, killed);
    g += wave(p, vec2(-0.97, -0.24), 6.10, 2.90, 0.20, t, killed);
    return g;
}

void fragment() {
    // World-space position of the opaque scene behind this fragment
    // (Godot 4.3+ reversed-z is baked into INV_PROJECTION_MATRIX, so this
    // reconstruction is version-correct for Forward+).
    float d = texture(depth_tex, SCREEN_UV).r;
    vec4 behind = INV_PROJECTION_MATRIX * vec4(SCREEN_UV * 2.0 - 1.0, d, 1.0);
    behind.xyz /= behind.w;
    float ground_y = (INV_VIEW_MATRIX * vec4(behind.xyz, 1.0)).y;
    float depth_v = max(wpos.y - ground_y, 0.0);

    float murk = 1.0 - exp(-depth_v / murk_depth);
    vec3 col = mix(shallow_color, deep_color, murk);

    float killed = 0.0;
    vec2 g = wave_grad(wpos.xz, TIME, killed) * wave_strength;
    vec3 n_world = normalize(vec3(-g.x, 1.0, -g.y));
    NORMAL = normalize((VIEW_MATRIX * vec4(n_world, 0.0)).xyz);

    // 0.15 base sky mix even looking straight down: silt-laden water
    // diffusely scatters skylight, and without it the surface rendered as
    // a near-black sheet under this scene's modest lighting.
    float fresnel = pow(1.0 - clamp(dot(NORMAL, VIEW), 0.0, 1.0), 5.0);
    ALBEDO = mix(col, sky_color, 0.15 + fresnel * 0.6);
    // Shallow water is see-through (the silt-brown bed shows), deep water is
    // nearly opaque, grazing angles read as solid reflection.
    ALPHA = clamp(depth_v / shore_fade, 0.0, 1.0)
        * mix(mix(0.45, 0.92, murk), 1.0, fresnel);
    // Waves the AA culled become microfacet roughness (Toksvig-style): the
    // surface stays believably choppy at distance without per-pixel glints.
    // The 0.2 floor matters too — at 0.1 the sun drew saturated white
    // streaks along every resolved crest when seen from above (silt-laden
    // water is never that glassy anyway).
    ROUGHNESS = clamp(0.2 + killed * wave_strength * 0.5, 0.2, 0.5);
    SPECULAR = 0.25;
}
