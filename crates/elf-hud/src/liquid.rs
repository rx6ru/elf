//! The compositor that paints the HUD shape into effect slots.
//! Each slot carries `effect_data([zoom, 0, 0, 0])`, so one window can show it at any size.
//! The raw voice analysis is turned into calm motion here: every value the shader sees
//! comes out of a spring, so the shape swells and settles instead of twitching.
//! Two voice signals stay apart: activity (is someone speaking) wakes the waveform, and
//! loudness sets how tall its waves rise.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use rune::compose::{Compositor, DeviceKey, Frame};
use rune::render::EffectSite;

use crate::Phase;
use crate::ink::{self, Ink};
use std::path::{Path, PathBuf};

use crate::mic::{Mic, Voice};

pub const LIQUID_MARK: u32 = 1;

/// Phase shared between the thread that drives ELF and the render loop.
#[derive(Clone, Default)]
pub struct SharedPhase(Arc<AtomicU8>);

impl SharedPhase {
    pub fn set(&self, p: Phase) { self.0.store(p as u8, Ordering::Relaxed) }
    fn get(&self) -> Phase {
        match self.0.load(Ordering::Relaxed) {
            1 => Phase::Processing,
            2 => Phase::Done,
            3 => Phase::Error,
            _ => Phase::Listening,
        }
    }
}


/// A float shared with the render loop: written by the thread that drives the HUD.
#[derive(Clone)]
pub struct SharedF32(Arc<AtomicU32>);

impl SharedF32 {
    pub fn new(v: f32) -> Self { Self(Arc::new(AtomicU32::new(v.to_bits()))) }
    pub fn set(&self, v: f32) { self.0.store(v.to_bits(), Ordering::Relaxed) }
    pub fn get(&self) -> f32 { f32::from_bits(self.0.load(Ordering::Relaxed)) }
}

const TAU: f32 = std::f32::consts::TAU;
/// Seconds a side wave takes from its birth to the tip; the shader's LIFETIME must match.
const LIFETIME: f32 = 2.3;
/// Loudness history: MEMORY samples, one per SNAP seconds, newest first.
const SNAP: f32 = 0.16;
const MEMORY: usize = 20;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    rect: [f32; 4],
    a: [f32; 4],
    w: [f32; 4],
    v: [f32; 4],
    x: [f32; 4],
    h: [[f32; 4]; MEMORY / 4],
    ink: [f32; 4],
    err: [f32; 4],
}

/// Uniform blocks sit at 256-byte offsets so each slot in a frame keeps its own values.
const SLOT: u64 = 256;
const SLOTS: u64 = 8;

#[derive(Clone, Copy, Default)]
struct Spring { x: f32, v: f32 }

impl Spring {
    fn to(&mut self, target: f32, dt: f32, stiffness: f32, damping: f32) {
        let n = (dt / (1.0 / 240.0)).ceil().max(1.0) as usize;
        let h = dt / n as f32;
        for _ in 0..n {
            let a = stiffness * (target - self.x) - damping * self.v;
            self.v += a * h;
            self.x += self.v * h;
        }
    }
    /// Rises at the first stiffness and falls at the second, both near critically damped.
    fn envelope(&mut self, target: f32, dt: f32, rise: f32, fall: f32) {
        let k = if target > self.x { rise } else { fall };
        self.to(target, dt, k, 1.9 * k.sqrt());
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Flat colours, sRGB. The ink is a clean warm white (#F7F7F5) over dark screens and near-black
/// (#161616) over light ones; errors get a soft tint that reads on each.
const INK_WHITE: [f32; 4] = [0.969, 0.969, 0.961, 1.0];
const INK_DARK: [f32; 4] = [0.0863, 0.0863, 0.0863, 1.0];
const ERR_ON_DARK: [f32; 4] = [0.949, 0.663, 0.608, 1.0];
const ERR_ON_LIGHT: [f32; 4] = [0.71, 0.25, 0.20, 1.0];
/// The ink crossfade settles in about 350 ms: a critically damped spring of this stiffness.
const INK_STIFFNESS: f32 = 130.0;

fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

struct Gpu {
    key: Option<DeviceKey>,
    buffer: wgpu::Buffer,
    group: wgpu::BindGroup,
    layout: wgpu::PipelineLayout,
    shader: wgpu::ShaderModule,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

pub struct LiquidCompositor {
    mic: Mic,
    phase: SharedPhase,
    /// 0 = white ink, 1 = dark ink: what the ink is heading for, and where it is now.
    ink_target: SharedF32,
    ink: Spring,
    ink_started: bool,
    /// Extra size on top of the effect's zoom: the overlay is drawn at this multiple of its
    /// 225 x 120 design size.
    zoom: SharedF32,
    /// Loudness while speech is going on (the waves' height), and the spectrum split in two.
    loud: Spring,
    body: Spring,
    high: Spring,
    /// 0 resting, 1 awake: follows the voice activity signal, not the loudness.
    alive: Spring,
    bounce: Spring,
    onsets_seen: u32,
    memory: [f32; MEMORY],
    memory_clock: f32,
    wave_phase: f32,
    slosh_phase: f32,
    breath_phase: f32,
    blob: Spring,
    err: Spring,
    slosh: Spring,
    gather: Spring,
    intro: Spring,
    vanish: f32,
    retract: f32,
    /// The phase seen last frame, and seconds since it began.
    last: Phase,
    age: f32,
    gpu: Option<Gpu>,
}

impl LiquidCompositor {
    pub fn new(mic: Mic, phase: SharedPhase) -> Self {
        Self {
            onsets_seen: mic.get().onsets,
            mic, phase,
            ink_target: SharedF32::new(0.0),
            ink: Spring::default(),
            ink_started: false,
            zoom: SharedF32::new(1.0),
            loud: Spring::default(),
            body: Spring::default(),
            high: Spring::default(),
            alive: Spring::default(),
            bounce: Spring::default(),
            memory: [0.0; MEMORY],
            memory_clock: 0.0,
            wave_phase: 0.0,
            slosh_phase: 0.0,
            breath_phase: 0.0,
            blob: Spring::default(),
            err: Spring::default(),
            slosh: Spring::default(),
            gather: Spring::default(),
            intro: Spring::default(),
            vanish: 0.0,
            retract: 0.0,
            last: Phase::Listening,
            age: 0.0,
            gpu: None,
        }
    }

    /// Where the ink colour (0 white, 1 dark) and the size come from.
    pub fn with_look(mut self, ink: SharedF32, zoom: SharedF32) -> Self {
        self.ink_target = ink;
        self.zoom = zoom;
        self
    }

    pub(crate) fn step(&mut self, dt: f32) {
        // The first frame starts at the right ink, with no fade from white.
        if !std::mem::replace(&mut self.ink_started, true) {
            self.ink.x = self.ink_target.get();
        }
        self.ink.to(self.ink_target.get(), dt, INK_STIFFNESS, 2.0 * INK_STIFFNESS.sqrt());
        let phase = self.phase.get();
        if phase != self.last {
            if phase == Phase::Listening { self.intro = Spring::default(); }
            self.last = phase;
            self.age = 0.0;
        } else {
            self.age += dt;
        }
        let listening = phase == Phase::Listening;
        let voice = self.mic.get();
        let gate = if listening { 1.0 } else { 0.0 };

        // Waking and resting are slow and eased both ways; loudness only counts while awake,
        // so a loud noise that is not speech never moves the shape.
        self.alive.envelope(voice.activity * gate, dt, 26.0, 10.0);
        let live = voice.activity * gate;
        self.loud.envelope(voice.level * live, dt, 140.0, 16.0);
        let b = voice.bands;
        self.body.envelope((b[1] + b[2] + b[3]) / 3.0 * live, dt, 120.0, 16.0);
        self.high.envelope((b[6] + b[7]) / 2.0 * live, dt, 120.0, 16.0);

        // A syllable gives the central wave a soft push; the spring makes it one gentle bounce.
        if voice.onsets != self.onsets_seen {
            self.onsets_seen = voice.onsets;
            if listening && voice.active { self.bounce.v += 4.0 * voice.onset_strength; }
        }
        self.bounce.to(0.0, dt, 90.0, 9.0);

        // The waves flow slowly at rest and at full speed when awake.
        let flow = TAU / LIFETIME * (0.3 + 0.7 * self.alive.x.clamp(0.0, 1.0));
        self.wave_phase = (self.wave_phase + dt * flow) % TAU;
        self.breath_phase = (self.breath_phase + dt * TAU / 4.5) % TAU;
        self.slosh_phase = (self.slosh_phase + dt * TAU / 1.9) % TAU;

        self.memory_clock += dt;
        while self.memory_clock >= SNAP {
            self.memory_clock -= SNAP;
            self.memory.rotate_right(1);
            self.memory[0] = self.loud.x.max(0.0);
        }

        let blob = matches!(phase, Phase::Processing | Phase::Done);
        self.blob.to(if blob { 1.0 } else { 0.0 }, dt, 22.0, 8.0);
        self.err.to(if phase == Phase::Error { 1.0 } else { 0.0 }, dt, 40.0, 12.6);
        let (stiff, damp) = if phase == Phase::Processing { (14.0, 7.5) } else { (60.0, 15.5) };
        self.slosh.to(if phase == Phase::Processing { 1.0 } else { 0.0 }, dt, stiff, damp);
        self.gather.to(if phase == Phase::Done { 1.0 } else { 0.0 }, dt, 26.0, 10.2);
        self.intro.to(1.0, dt, 40.0, 11.0);

        // Vanishing runs on the clock, then resets once the shape is gone from view.
        self.vanish = match phase {
            Phase::Done => smoothstep((self.age - 0.55) / 0.75),
            _ if self.blob.x < 0.02 => 0.0,
            _ => self.vanish,
        };
        self.retract = match phase {
            Phase::Error => smoothstep((self.age - 1.0) / 0.6),
            _ if self.err.x < 0.02 => 0.0,
            _ => self.retract,
        };
    }

    /// This frame's shader inputs for a slot at `rect` (target pixels), `px` pixels per point.
    fn uniforms(&self, rect: [f32; 4], px: f32, linear: bool) -> Uniforms {
        let mut h = [[0.0; 4]; MEMORY / 4];
        for (i, m) in self.memory.iter().enumerate() { h[i / 4][i % 4] = *m; }
        let loud = (self.loud.x + 0.2 * self.bounce.x).clamp(0.0, 1.0);
        let dark = self.ink.x.clamp(0.0, 1.0);
        Uniforms {
            rect,
            a: [self.wave_phase, px, self.memory_clock / SNAP, 0.0],
            w: [self.blob.x, self.err.x, self.slosh.x * self.slosh_phase.sin(), self.gather.x],
            v: [loud, self.body.x.max(0.0), self.high.x.max(0.0), self.breath_phase.sin()],
            x: [self.vanish, self.intro.x.max(0.0), self.retract, self.alive.x.clamp(0.0, 1.0)],
            h,
            ink: encode(mix(INK_WHITE, INK_DARK, dark), linear),
            err: encode(mix(ERR_ON_DARK, ERR_ON_LIGHT, dark), linear),
        }
    }
}

fn make_gpu(device: &wgpu::Device, key: Option<DeviceKey>) -> Gpu {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("elf liquid"),
        source: wgpu::ShaderSource::Wgsl(include_str!("liquid.wgsl").into()),
    });
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("elf liquid uniforms"),
        size: SLOT * SLOTS,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let size = NonZeroU64::new(size_of::<Uniforms>() as u64);
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            count: None,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: true, min_binding_size: size },
        }],
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &buffer, offset: 0, size }),
        }],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    Gpu { key, buffer, group, layout, shader, pipelines: HashMap::new() }
}

fn pipeline(device: &wgpu::Device, g: &Gpu, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let premultiplied = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("elf liquid"),
        layout: Some(&g.layout),
        vertex: wgpu::VertexState { module: &g.shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
        fragment: Some(wgpu::FragmentState {
            module: &g.shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState { color: premultiplied, alpha: premultiplied }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// sRGB-encoded colour to what a pass of `format` expects.
fn encode(c: [f32; 4], linear: bool) -> [f32; 4] {
    if !linear { return c; }
    let l = |v: f32| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    [l(c[0]), l(c[1]), l(c[2]), c[3]]
}

impl<S: 'static> Compositor<S> for LiquidCompositor {
    fn animating(&self) -> bool { true }

    fn render(&mut self, f: &mut Frame<'_, S>) {
        let (device, queue, size) = (f.device, f.queue, f.size);
        self.step(f.dt.min(0.1) as f32);
        let key = f.device_key();
        let mut gpu = match self.gpu.take() {
            Some(g) if g.key == Some(key) => g,
            _ => make_gpu(device, Some(key)),
        };
        let this = &*self;
        let mut slot = 0u64;

        let clear = f.rt.clear_color();
        f.ui.prepare(device, queue, &mut f.rt.text.atlas, &f.rt.instances, size);
        f.ui.draw_with(f.encoder, f.target, clear, &mut |pass, s: &EffectSite<'_>| {
            if s.mark != LIQUID_MARK || slot >= SLOTS { return; }
            let linear = s.format.is_srgb() || s.format == wgpu::TextureFormat::Rgba16Float;
            let [x, y, sw, sh] = s.instance.rect;
            let zoom = if s.data[0] > 0.0 { s.data[0] } else { 1.0 };
            let u = this.uniforms([x - s.origin[0], y - s.origin[1], sw, sh], s.scale * zoom * this.zoom.get(), linear);
            let offset = slot * SLOT;
            slot += 1;
            queue.write_buffer(&gpu.buffer, offset, bytemuck::bytes_of(&u));
            if !gpu.pipelines.contains_key(&s.format) {
                let p = pipeline(device, &gpu, s.format);
                gpu.pipelines.insert(s.format, p);
            }
            pass.set_pipeline(&gpu.pipelines[&s.format]);
            pass.set_bind_group(0, &gpu.group, &[offset as u32]);
            pass.draw(0..3, 0..1);
        });
        self.gpu = Some(gpu);
    }
}


/// What the strip is drawn over, as sRGB: a dark screen, or a light or mid wallpaper.
fn backdrop(name: &str) -> [f32; 3] {
    match name {
        "light" => [0.86, 0.85, 0.82],
        "mid" => [0.50, 0.55, 0.62],
        _ => [0.02, 0.022, 0.026],
    }
}

/// Renders a scripted dictation off screen and writes sampled frames as PNGs, so the motion
/// can be judged frame by frame without a display. `voice` holds one analysis per 20 ms of a
/// recording; it plays during listening, then processing follows, then `end` (done or error).
/// A frame is written every `every` frames (60 per second).
pub fn render_strip(dir: &Path, voice: &[Voice], bg: &str, every: usize, end: Phase, ink_mode: Ink) -> Vec<PathBuf> {
    use rune::render::{Gpu as Headless, Offscreen};
    const W: u32 = 675;
    const H: u32 = 360;
    const PX: f32 = 3.0;
    const FPS: f32 = 60.0;

    std::fs::create_dir_all(dir).expect("create strip dir");
    let mic = Mic::default();
    let phase = SharedPhase::default();
    // Auto takes the ink from the backdrop, as the live overlay takes it from the screen.
    let dark = match ink_mode {
        Ink::White => false,
        Ink::Dark => true,
        Ink::Auto => ink::decide(false, ink::luminance(backdrop(bg))),
    };
    let ink_target = SharedF32::new(if dark { 1.0 } else { 0.0 });
    let mut comp = LiquidCompositor::new(mic.clone(), phase.clone()).with_look(ink_target, SharedF32::new(1.0));
    let hw = Headless::headless();
    let off = Offscreen::new(&hw.device, W, H);
    let format = rune::render::OFFSCREEN_FORMAT;
    let mut gpu = make_gpu(&hw.device, None);
    let pipe = pipeline(&hw.device, &gpu, format);
    gpu.pipelines.insert(format, pipe);
    let clear = encode([backdrop(bg)[0], backdrop(bg)[1], backdrop(bg)[2], 1.0], true);

    let listen = (voice.len() as f32 * 0.02).clamp(2.0, 20.0);
    let script = [(Phase::Listening, listen), (Phase::Processing, 3.6), (end, 2.0)];
    let total: f32 = script.iter().map(|s| s.1).sum();
    let dt = 1.0 / FPS;
    let mut out = Vec::new();
    for n in 0..(total * FPS) as usize {
        let t = n as f32 * dt;
        let mut at = 0.0;
        let mut now = end;
        for (p, len) in script {
            if t < at + len { now = p; break; }
            at += len;
        }
        phase.set(now);
        mic.set(voice.get((t / 0.02) as usize).copied().filter(|_| now == Phase::Listening).unwrap_or_default());
        comp.step(dt);
        if n % every != 0 { continue; }

        let u = comp.uniforms([0.0, 0.0, W as f32, H as f32], PX, true);
        hw.queue.write_buffer(&gpu.buffer, 0, bytemuck::bytes_of(&u));
        let mut enc = hw.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &off.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: clear[0] as f64, g: clear[1] as f64, b: clear[2] as f64, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&gpu.pipelines[&format]);
            pass.set_bind_group(0, &gpu.group, &[0]);
            pass.draw(0..3, 0..1);
        }
        off.copy(&mut enc);
        hw.queue.submit([enc.finish()]);
        let raw = dir.join(format!("{:03}.rgba", out.len()));
        std::fs::write(&raw, off.read(&hw.device)).expect("write frame");
        let png = raw.with_extension("png");
        let v = mic.get();
        let label = format!("{:?} {:.1}s  level {:.2}  act {:.2}", now, t, v.level, v.activity);
        let ok = std::process::Command::new("magick")
            .args(["-size", &format!("{W}x{H}"), "-depth", "8", &format!("rgba:{}", raw.display())])
            .args(["-fill", "#7a7a7a", "-pointsize", "13", "-gravity", "southwest", "-annotate", "+6+4", &label])
            .arg(&png)
            .status()
            .is_ok_and(|s| s.success());
        let _ = std::fs::remove_file(&raw);
        if ok { out.push(png); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp() -> (LiquidCompositor, SharedF32) {
        let ink = SharedF32::new(0.0);
        (LiquidCompositor::new(Mic::default(), SharedPhase::default()).with_look(ink.clone(), SharedF32::new(1.0)), ink)
    }

    #[test]
    fn the_ink_crossfades_in_about_350_ms_without_overshoot() {
        let (mut c, ink) = comp();
        c.step(1.0 / 60.0);
        ink.set(1.0);
        let mut last = 0.0;
        let mut at_350 = 0.0;
        for frame in 1..=60 {
            c.step(1.0 / 60.0);
            assert!(c.ink.x >= last && c.ink.x <= 1.0, "monotonic, no overshoot: {}", c.ink.x);
            last = c.ink.x;
            if frame == 21 {
                at_350 = c.ink.x;
            }
        }
        assert!(at_350 > 0.9 && at_350 < 1.0, "{at_350}");
        assert!(c.ink.x > 0.999);
        assert!(c.uniforms([0.0; 4], 1.0, false).ink[0] < 0.1, "dark ink once settled");
    }

    #[test]
    fn it_does_not_snap_and_it_starts_at_the_set_ink() {
        let (mut c, ink) = comp();
        c.step(1.0 / 60.0);
        ink.set(1.0);
        c.step(1.0 / 60.0);
        assert!(c.ink.x > 0.0 && c.ink.x < 0.2, "a frame in is a fraction of the way: {}", c.ink.x);

        let ink = SharedF32::new(1.0);
        let mut first = LiquidCompositor::new(Mic::default(), SharedPhase::default()).with_look(ink, SharedF32::new(1.0));
        first.step(1.0 / 60.0);
        assert_eq!(first.ink.x, 1.0, "the first frame is already dark");
    }

    #[test]
    fn the_error_tint_follows_the_ink() {
        let (mut c, _) = comp();
        c.step(1.0 / 60.0);
        let on_dark = c.uniforms([0.0; 4], 1.0, false).err;
        assert_eq!(on_dark, ERR_ON_DARK);
        c.ink.x = 1.0;
        let on_light = c.uniforms([0.0; 4], 1.0, false).err;
        assert!(on_light.iter().zip(ERR_ON_LIGHT).all(|(a, b)| (a - b).abs() < 1e-6), "{on_light:?}");
    }
}
