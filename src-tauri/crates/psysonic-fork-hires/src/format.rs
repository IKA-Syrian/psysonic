//! Output formatter (design: `docs/fork-hires-engine-plan.md` §3.5): soft limiter, optional TPDF dither and
//! f32 to integer conversion, written as little-endian bytes.

/// The sample container the device is opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    F32,
    S16,
    /// 3 bytes per sample.
    S24Packed,
    /// 4 bytes per sample with `valid_bits` significant bits in the upper bits (24 = WASAPI 24-in-32).
    S32 {
        valid_bits: u8,
    },
}

impl OutputFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::F32 | Self::S32 { .. } => 4,
            Self::S24Packed => 3,
            Self::S16 => 2,
        }
    }

    /// Significant bits per sample. `F32` reports 32, which never needs dither.
    pub fn valid_bits(self) -> u8 {
        match self {
            Self::F32 => 32,
            Self::S16 => 16,
            Self::S24Packed => 24,
            Self::S32 { valid_bits } => valid_bits,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DitherMode {
    Auto,
    Off,
}

/// `Auto` dithers only when the container is shallower than 24 bits and the signal is no longer bit-exact:
/// DSP is active, or the source is deeper than the container.
pub fn should_dither(
    mode: DitherMode,
    target_valid_bits: u32,
    source_bits: u32,
    dsp_active: bool,
) -> bool {
    match mode {
        DitherMode::Off => false,
        DitherMode::Auto => {
            target_valid_bits < 24 && (dsp_active || source_bits > target_valid_bits)
        }
    }
}

/// Limiter threshold, -0.3 dBFS.
pub const LIMITER_THRESHOLD: f32 = 0.966_051;
/// Largest f32 below 1.0.
const BELOW_FULL_SCALE: f32 = 1.0 - 1.0 / 16_777_216.0;

/// Identity up to the threshold, then a tanh knee that approaches (and never reaches) full scale.
/// Value and slope are continuous at the threshold.
fn soft_limit(x: f32) -> f32 {
    let t = LIMITER_THRESHOLD as f64;
    let over = (x.abs() as f64 - t) / (1.0 - t);
    let y = ((t + (1.0 - t) * over.tanh()) as f32).min(BELOW_FULL_SCALE);
    y.copysign(x)
}

/// xorshift64*: small, deterministic, good enough for dither.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [-0.5, 0.5).
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 - 0.5
    }
}

pub struct Formatter {
    format: OutputFormat,
    dither: bool,
    limiter: bool,
    rng: Rng,
    limited: u64,
    gain: f64,
}

impl Formatter {
    /// Dither off, limiter on. Panics if an `S32` format has fewer than 16 or more than 32 valid bits.
    pub fn new(format: OutputFormat, seed: u64) -> Self {
        if let OutputFormat::S32 { valid_bits } = format {
            assert!(
                (16..=32).contains(&valid_bits),
                "S32 valid_bits must be 16..=32"
            );
        }
        Self {
            format,
            dither: false,
            limiter: true,
            rng: Rng::new(seed),
            limited: 0,
            gain: 1.0,
        }
    }

    pub fn format(&self) -> OutputFormat {
        self.format
    }

    /// TPDF dither before rounding; ignored for `F32`.
    pub fn set_dither(&mut self, on: bool) {
        self.dither = on;
    }

    /// The limiter is on by default. Turn it off in the bit-perfect state, where full-scale samples must pass
    /// unchanged.
    pub fn set_limiter(&mut self, on: bool) {
        self.limiter = on;
    }

    /// Linear gain applied to every input sample (as `x as f64 * gain`) before the limiter and the scaling to
    /// integer codes. Default 1.0, which leaves all output (including `F32`) bit-identical to no gain at all.
    /// The exclusive backend uses `1 / MASTER_HEADROOM` to undo rodio's fixed headroom attenuation.
    pub fn set_input_gain(&mut self, gain: f64) {
        self.gain = gain;
    }

    /// Samples the limiter has modified so far.
    pub fn limited_samples(&self) -> u64 {
        self.limited
    }

    /// Converts `input` and writes little-endian bytes to the front of `out`. Returns the bytes written.
    /// Panics if `out` is shorter than `input.len() * bytes_per_sample()`.
    pub fn write_le(&mut self, input: &[f32], out: &mut [u8]) -> usize {
        let width = self.format.bytes_per_sample();
        let total = input.len() * width;
        assert!(
            out.len() >= total,
            "output buffer too small: {} bytes for {total} needed",
            out.len()
        );
        for (&x, dst) in input.iter().zip(out.chunks_exact_mut(width)) {
            let x = self.limit(x as f64 * self.gain);
            match self.format {
                OutputFormat::F32 => dst.copy_from_slice(&(x as f32).to_le_bytes()),
                OutputFormat::S16 => dst.copy_from_slice(&(self.code(x, 16) as i16).to_le_bytes()),
                OutputFormat::S24Packed => {
                    dst.copy_from_slice(&self.code(x, 24).to_le_bytes()[..3])
                }
                OutputFormat::S32 { valid_bits } => {
                    let code = self.code(x, valid_bits as u32);
                    dst.copy_from_slice(&(code << (32 - valid_bits as u32)).to_le_bytes());
                }
            }
        }
        total
    }

    /// Works in f64 so the input gain loses no precision; the limiter itself runs in f32.
    fn limit(&mut self, x: f64) -> f64 {
        if self.limiter && x.abs() > LIMITER_THRESHOLD as f64 {
            self.limited += 1;
            soft_limit(x as f32) as f64
        } else {
            x
        }
    }

    /// Rounded, clamped integer code on a `bits`-deep signed grid.
    fn code(&mut self, x: f64, bits: u32) -> i32 {
        let scale = (1u64 << (bits - 1)) as f64;
        let mut v = x * scale;
        if self.dither {
            v += self.rng.uniform() + self.rng.uniform();
        }
        // `as i64` saturates and maps NaN to 0.
        (v.round() as i64).clamp(-(scale as i64), scale as i64 - 1) as i32
    }
}
