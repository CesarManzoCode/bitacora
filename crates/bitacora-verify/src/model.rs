//! Reference model: the version of every block, from which the expected
//! content is regenerated with `block_data`.

use bitacora::format::BLOCK_SIZE;

use crate::rng::block_data;

pub struct Model {
    seed: u64,
    /// Version of each block; 0 = never written.
    versions: Vec<u32>,
}

impl Model {
    pub fn new(seed: u64, blocks: u64) -> Self {
        Model {
            seed,
            versions: vec![0; blocks as usize],
        }
    }

    pub fn blocks(&self) -> u64 {
        self.versions.len() as u64
    }

    pub fn version(&self, lba: u64) -> u32 {
        self.versions[lba as usize]
    }

    /// Records a new version of `lba` without generating its content.
    pub fn bump(&mut self, lba: u64) {
        self.versions[lba as usize] += 1;
    }

    /// Records a new version of `lba` and returns the content to write.
    pub fn write(&mut self, lba: u64) -> [u8; BLOCK_SIZE] {
        self.bump(lba);
        self.expected(lba)
    }

    /// Content a read of `lba` must return: zeros if never written.
    pub fn expected(&self, lba: u64) -> [u8; BLOCK_SIZE] {
        match self.version(lba) {
            0 => [0; BLOCK_SIZE],
            v => block_data(self.seed, lba, u64::from(v)),
        }
    }

    /// Whether one block read from `lba` matches the model.
    pub fn matches(&self, lba: u64, data: &[u8]) -> bool {
        match self.version(lba) {
            0 => data.len() == BLOCK_SIZE && data.iter().all(|&b| b == 0),
            _ => data == self.expected(lba),
        }
    }

    /// Blocks of a raw volume image that differ from the model; a missing
    /// tail counts as differing.
    pub fn diff_image(&self, image: &[u8]) -> u64 {
        (0..self.blocks())
            .filter(|&lba| {
                let start = lba as usize * BLOCK_SIZE;
                image
                    .get(start..start + BLOCK_SIZE)
                    .is_none_or(|block| !self.matches(lba, block))
            })
            .count() as u64
    }
}
