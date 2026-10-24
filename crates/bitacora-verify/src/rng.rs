//! Deterministic generators: SplitMix64 to seed, xoshiro256** to generate.
//!
//! Own implementation so the output never changes with a dependency version.

use bitacora::format::BLOCK_SIZE;

/// SplitMix64 (Steele, Lea and Flood), used only to expand seeds.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// One SplitMix64 step from `x`: a cheap 64-bit mixer for deriving keys.
fn mix(x: u64) -> u64 {
    SplitMix64::new(x).next_u64()
}

/// xoshiro256** (Blackman and Vigna), seeded through SplitMix64.
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        Rng {
            s: [sm.next_u64(), sm.next_u64(), sm.next_u64(), sm.next_u64()],
        }
    }

    /// An independent generator for sub-stream `stream` of `seed`.
    pub fn stream(seed: u64, stream: u64) -> Self {
        Rng::new(mix(seed ^ mix(stream)))
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..n` (multiply-shift); `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

/// Content of version `version` of `lba` under `seed`: `f(seed, lba, version)`
/// of contract section 12.
pub fn block_data(seed: u64, lba: u64, version: u64) -> [u8; BLOCK_SIZE] {
    let mut rng = Rng::new(mix(mix(mix(seed) ^ lba) ^ version));
    let mut out = [0u8; BLOCK_SIZE];
    for word in out.chunks_exact_mut(8) {
        word.copy_from_slice(&rng.next_u64().to_le_bytes());
    }
    out
}
