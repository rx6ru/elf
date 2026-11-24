// ELF HUD: one flat white shape that changes form through a dictation.
//   listening  - a long, lean waveform: one large wave in the middle that stays in place, with
//                smaller waves rising out of it and flowing outward to both sides, thinning to
//                fine rounded tips. Without speech it rests as a short, low, slow swell; speech
//                wakes it, and loudness sets how tall the waves rise.
//   processing - the body smooths and expands into two lobes; the weight flows from one lobe
//                to the other and back, in a loop
//   done       - the lobes gather into one round drop, which then shrinks away
//   error      - the body settles into a calm flat line in a soft tint, then draws in and goes
// Every input arrives already smoothed by springs on the CPU.

struct U {
    rect: vec4<f32>, // slot x, y, w, h in target pixels
    a: vec4<f32>,    // wave phase (0..TAU, one turn = one lifetime), px per point, history phase (0..1), unused
    w: vec4<f32>,    // blob weight, error weight, slosh (-1..1), gather (0..1)
    v: vec4<f32>,    // loudness, low-mid energy, high energy, slow breath (-1..1)
    x: vec4<f32>,    // vanish (0..1), intro scale (0..1), error retract (0..1), alive (0..1)
    h: array<vec4<f32>, 5>, // loudness history, newest first, one sample per SNAP seconds
    ink: vec4<f32>,
    err: vec4<f32>,
}
@group(0) @binding(0) var<uniform> u: U;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

const TAU: f32 = 6.2831853;
// Shape units are points: the overlay is 225 x 120 of them.
const HALF_LEN: f32 = 76.0;
const WAVES: i32 = 4;
// Seconds a wave takes from birth to the tip.
const LIFETIME: f32 = 2.3;
const SNAP: f32 = 0.16;
const MEMORY: i32 = 20;

fn rot(p: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(c * p.x + s * p.y, -s * p.x + c * p.y);
}

fn sample_memory(i: i32) -> f32 {
    let j = u32(clamp(i, 0, MEMORY - 1));
    return u.h[j >> 2u][j & 3u];
}

// Loudness `x` snapshots ago, cubic between the samples so nothing creases.
fn memory(x: f32) -> f32 {
    let xc = max(x, 0.0);
    let i = i32(floor(xc));
    let f = xc - floor(xc);
    let a = sample_memory(i - 1);
    let b = sample_memory(i);
    let c = sample_memory(i + 1);
    let d = sample_memory(i + 2);
    let m = b + 0.5 * f * (c - a + f * (2.0 * a - 5.0 * b + 4.0 * c - d + f * (3.0 * (b - c) + d - a)));
    return clamp(m, 0.0, 1.0);
}

// A smooth bump of height `b` and half-width `a` centred on `c`; a higher `round` makes a
// softer, bell-like foot, and a higher `flat` a broader, rounder top.
fn bump(x: f32, c: f32, a: f32, b: f32, round: f32, flat: f32) -> f32 {
    let t = abs(x - c) / a;
    return b * pow(max(1.0 - pow(t, flat), 0.0), round);
}

// Listening: half the thickness of the body at `x`. A stack of rounded bumps fused into one
// liquid body: a large one in the middle that stays in place, and WAVES per side that are born
// inside it, flow outward and shrink as they go, on a thin spine that ends in fine rounded
// tips. Each wave carries the loudness the voice had when it was born. The bumps are joined by
// a smooth sum of cubes, so where they meet the outline curves and never creases.
fn half_height(x: f32, calm: f32) -> f32 {
    let ax = abs(x);
    let alive = u.x.w;
    let loud = u.v.x;
    let high = u.v.z;
    // At rest the body is shorter and low; waking stretches it out to its full size.
    let reach = mix(0.66, 1.0, alive);
    let rest = 11.0 + 1.2 * u.v.w;
    // Calm (the start of processing) swells the central wave into a smooth lens the full length.
    let peak = mix(mix(rest, 14.0 + 25.0 * loud, alive), 24.0, calm);
    let span = mix(23.0 + 6.0 * loud + 3.0 * u.v.y, HALF_LEN * reach, calm);
    var cubes = pow(bump(ax, 0.0, span, peak, mix(1.5, 0.5, calm), mix(2.7, 2.0, calm)), 3.0);
    let tip = 2.0;
    let end = HALF_LEN * reach;
    let e = clamp((ax - (end - tip)) / tip, 0.0, 1.0);
    cubes += pow(tip * sqrt(1.0 - e * e), 3.0);
    for (var k = 0; k < WAVES; k++) {
        // s: how far this wave has travelled, 0 at birth, 1 at the tip.
        let s = fract(u.a.x / TAU + f32(k) / f32(WAVES));
        let past = memory(s * LIFETIME / SNAP - u.a.z);
        let size = (4.0 + (5.0 + 16.0 * past) * alive) * (1.0 + 0.6 * high) * (1.0 - calm)
            * smoothstep(0.02, 0.32, s) * pow(1.0 - s, 0.9);
        cubes += pow(bump(ax, (8.0 + 66.0 * s) * reach, 11.0 + 0.3 * size, size, 1.3, 2.0), 3.0);
    }
    return pow(cubes, 1.0 / 3.0);
}

// Where the waveform ends on each side, as (left, right) x positions.
fn wave_ends() -> vec2<f32> {
    let end = HALF_LEN * mix(0.66, 1.0, u.x.w);
    return vec2<f32>(-end, end);
}

// Processing: two lobes. The heavy one is bigger and sinks a little; the weight flows across and
// back. Done: the lobes slide together into one round drop, then it shrinks to nothing.
struct Lobes { left: vec3<f32>, right: vec3<f32>, } // centre, half-width, half-height

fn lobes() -> Lobes {
    let shrink = 1.0 - u.x.x;
    let gather = u.w.w;
    let swing = u.w.z * (1.0 - gather);
    // Sized to sit inside the listening waveform's footprint, so the change of phase reads as a
    // transformation rather than a jump in size.
    let apart = 24.0 * (1.0 - smoothstep(0.0, 0.7, gather));
    let wide = mix(28.0, 21.0, gather);
    let tall = mix(20.0, 21.0, gather);
    let shift = 4.0 * swing;
    let l = 1.0 - 0.2 * swing;
    let r = 1.0 + 0.2 * swing;
    var o: Lobes;
    o.left = vec3<f32>(-apart + shift, wide * l, tall * l) * shrink;
    o.right = vec3<f32>(apart + shift, wide * r, tall * r) * shrink;
    return o;
}

fn lobe_h(x: f32, l: vec3<f32>) -> f32 {
    let t = (x - l.x) / max(l.y, 1e-3);
    return l.z * sqrt(max(1.0 - t * t, 0.0));
}

// The taller of the two lobes, rounded where they meet so the neck is a soft curve: a soft
// maximum that is exact wherever only one lobe exists. The rounding fades out as the lobes come
// together into one drop.
fn blob_h(x: f32) -> f32 {
    let o = lobes();
    let a = lobe_h(x, o.left);
    let b = lobe_h(x, o.right);
    let k = max(0.13 * (o.right.x - o.left.x), 1e-3);
    let m = max(a, b);
    return m + k * log(exp((a - m) / k) + exp((b - m) / k) - exp(-m / k));
}

fn blob_ends() -> vec2<f32> {
    let o = lobes();
    return vec2<f32>(o.left.x - o.left.y, o.right.x + o.right.y);
}

// Error: a calm flat line that breathes once, then draws in to nothing.
fn line_h(x: f32) -> f32 {
    let draw = 1.0 - u.x.z;
    let r = 3.6 * (1.0 + 0.08 * u.v.w) * draw;
    let half_len = 58.0 * draw;
    let e = clamp((abs(x) - (half_len - r)) / max(r, 1e-3), 0.0, 1.0);
    return r * sqrt(1.0 - e * e);
}

// Half-thickness of the whole body at `x`. Each form is laid out between its own two ends; the
// blend works on positions measured along that span (0 in the middle, 1 at an end), so the
// outline always closes with a round end and never ends in a cut.
fn body_h(x: f32, calm: f32, to_blob: f32, err: f32, grow: f32) -> f32 {
    let side = select(1, 0, x < 0.0);
    let span = abs(x) / abs(mix(mix(wave_ends() * grow, blob_ends(), to_blob), vec2<f32>(-line_end(), line_end()), err)[side]);
    let ew = wave_ends()[side] * grow;
    let eb = blob_ends()[side];
    let hw = half_height(span * ew / grow, calm) * grow;
    let hb = blob_h(span * eb);
    let hl = line_h(span * line_end());
    return mix(mix(hw, hb, to_blob), hl, err);
}

fn line_end() -> f32 { return 58.0 * (1.0 - u.x.z); }

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    var p = (pos.xy - u.rect.xy - u.rect.zw * 0.5) / u.a.y;
    let blob_w = clamp(u.w.x, 0.0, 1.0);
    let err = clamp(u.w.y, 0.0, 1.0);
    // The waveform smooths out first, then flows into the two lobes.
    let calm = max(smoothstep(0.0, 0.5, blob_w), err);
    let to_blob = smoothstep(0.25, 1.0, blob_w);
    // The heavy lobe dips: tilt the whole body a little while it sloshes.
    p = rot(p, 0.11 * u.w.z * (1.0 - u.w.w));

    // The intro scales the waveform up from a point, so it opens instead of popping in.
    let grow = max(u.x.y, 0.001);
    let ends = mix(mix(wave_ends() * grow, blob_ends(), to_blob), vec2<f32>(-line_end(), line_end()), err);

    let h0 = body_h(p.x - 0.1, calm, to_blob, err, grow);
    let h1 = body_h(p.x, calm, to_blob, err, grow);
    let h2 = body_h(p.x + 0.1, calm, to_blob, err, grow);
    // Distance to the outline, corrected for how steeply it runs.
    let slope = (h2 - h0) / 0.2;
    let dy = (abs(p.y) - h1) / sqrt(1.0 + slope * slope);
    let dx = select(p.x - ends.y, ends.x - p.x, p.x < 0.0);
    let d = length(max(vec2<f32>(dx, dy), vec2<f32>(0.0))) + min(max(dx, dy), 0.0);

    let cover = clamp(0.5 - d * u.a.y, 0.0, 1.0) * smoothstep(0.0, 0.05, u.x.y + blob_w + err);
    let ink = mix(u.ink, u.err, err);
    let a = cover * ink.a;
    return vec4<f32>(ink.rgb * a, a);
}
