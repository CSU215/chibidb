//! A simple Bloom filter over SSTable keys.
//!
//! Encoded as `[bit array][k]`, where `k` is the number of hash probes. An
//! empty filter (only the `k` byte) conservatively answers "maybe" so a table
//! with no keys never wrongly rejects one.

/// Builds a filter from all keys going into one SSTable.
pub struct BloomBuilder {
    hashes: Vec<u64>,
    bits_per_key: usize,
}

impl BloomBuilder {
    pub fn new(bits_per_key: usize) -> Self {
        Self { hashes: Vec::new(), bits_per_key: bits_per_key.max(1) }
    }

    pub fn add(&mut self, key: &[u8]) {
        self.hashes.push(hash(key));
    }

    pub fn key_count(&self) -> usize {
        self.hashes.len()
    }

    /// Produces the encoded filter.
    pub fn finish(&self) -> Vec<u8> {
        let probes = probes(self.bits_per_key);
        if self.hashes.is_empty() {
            return vec![probes as u8];
        }
        let bytes = (self.hashes.len() * self.bits_per_key).div_ceil(8).max(8);
        let bits = bytes * 8;
        let mut bitset = vec![0u8; bytes];
        for &h in &self.hashes {
            let mut pos = h % bits as u64;
            let delta = (h >> 17) | (h << 15);
            for _ in 0..probes {
                bitset[(pos / 8) as usize] |= 1 << (pos % 8);
                pos = pos.wrapping_add(delta) % bits as u64;
            }
        }
        bitset.push(probes as u8);
        bitset
    }
}

/// A parsed filter that can reject keys that are definitely absent.
pub struct BloomFilter {
    data: Vec<u8>,
}

impl BloomFilter {
    pub fn decode(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// Whether the filter holds no keys (so every lookup must proceed).
    pub fn is_empty(&self) -> bool {
        self.data.len() <= 1
    }

    /// `false` means the key is definitely absent; `true` means it may be.
    pub fn maybe_contains(&self, key: &[u8]) -> bool {
        if self.is_empty() {
            return true;
        }
        let probes = *self.data.last().expect("non-empty filter") as usize;
        let bits = (self.data.len() - 1) * 8;
        let h = hash(key);
        let mut pos = h % bits as u64;
        let delta = (h >> 17) | (h << 15);
        for _ in 0..probes {
            if self.data[(pos / 8) as usize] & (1 << (pos % 8)) == 0 {
                return false;
            }
            pos = pos.wrapping_add(delta) % bits as u64;
        }
        true
    }
}

fn probes(bits_per_key: usize) -> usize {
    ((bits_per_key as f64) * 0.69).round().clamp(1.0, 30.0) as usize
}

/// FNV-1a over the key bytes.
fn hash(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
