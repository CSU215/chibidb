//! Sorted string table data blocks.
//!
//! A block is a run of prefix-compressed key/value entries followed by an
//! array of restart offsets. Every `restart_interval` entries the key is
//! written in full (shared prefix length 0), which bounds the cost of binary
//! search and lets a reader reconstruct keys with only the previous entry as
//! context.

use std::cmp::Ordering;

use crate::storage::lsm::coding::{get_fixed32, get_varint32, put_fixed32, put_varint32};
use crate::{Error, Result};

/// Full keys every this many entries by default.
pub const DEFAULT_RESTART_INTERVAL: usize = 16;

/// Builds one block. Keys must be added in strictly ascending order.
pub struct BlockBuilder {
    buffer: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    last_key: Vec<u8>,
    restart_interval: usize,
}

impl BlockBuilder {
    pub fn new(restart_interval: usize) -> Self {
        Self {
            buffer: Vec::new(),
            restarts: vec![0],
            counter: 0,
            last_key: Vec::new(),
            restart_interval: restart_interval.max(1),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Approximate size of the finished block in bytes.
    pub fn current_size_estimate(&self) -> usize {
        self.buffer.len() + self.restarts.len() * 4 + 4
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        debug_assert!(
            self.last_key.is_empty() || self.last_key.as_slice() < key,
            "block keys must be added in ascending order"
        );
        let shared = if self.counter < self.restart_interval {
            common_prefix(&self.last_key, key)
        } else {
            self.restarts.push(self.buffer.len() as u32);
            self.counter = 0;
            0
        };
        let non_shared = key.len() - shared;
        put_varint32(&mut self.buffer, shared as u32);
        put_varint32(&mut self.buffer, non_shared as u32);
        put_varint32(&mut self.buffer, value.len() as u32);
        self.buffer.extend_from_slice(&key[shared..]);
        self.buffer.extend_from_slice(value);

        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
    }

    /// Appends the restart array and returns the encoded block, resetting the
    /// builder so it can be reused.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.buffer.is_empty() {
            self.restarts.clear();
        }
        for restart in &self.restarts {
            put_fixed32(&mut self.buffer, *restart);
        }
        put_fixed32(&mut self.buffer, self.restarts.len() as u32);
        let out = std::mem::take(&mut self.buffer);
        self.restarts = vec![0];
        self.counter = 0;
        self.last_key.clear();
        out
    }
}

/// A parsed block: a borrowed-free copy of the bytes plus its restart offsets.
pub struct Block {
    data: Vec<u8>,
    restarts: Vec<u32>,
    restart_array_start: usize,
}

impl Block {
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if data.len() < 4 {
            return Err(Error::Runtime("block is too short".into()));
        }
        let mut pos = data.len() - 4;
        let num_restarts = get_fixed32(&data, &mut pos)? as usize;
        let restart_bytes = num_restarts
            .checked_mul(4)
            .ok_or_else(|| Error::Runtime("block restart count overflows".into()))?;
        if data.len() < 4 + restart_bytes {
            return Err(Error::Runtime("block restart array is truncated".into()));
        }
        let restart_array_start = data.len() - 4 - restart_bytes;
        let mut restarts = Vec::with_capacity(num_restarts);
        let mut rpos = restart_array_start;
        for _ in 0..num_restarts {
            restarts.push(get_fixed32(&data, &mut rpos)?);
        }
        let mut previous: Option<u32> = None;
        for &restart in &restarts {
            if restart as usize >= restart_array_start.max(1) {
                return Err(Error::Runtime("block restart offset out of range".into()));
            }
            if let Some(prev) = previous
                && restart <= prev
            {
                return Err(Error::Runtime("block restart offsets are not sorted".into()));
            }
            previous = Some(restart);
        }
        Ok(Self { data, restarts, restart_array_start })
    }

    pub fn is_empty(&self) -> bool {
        self.restarts.is_empty()
    }

    /// The first key in the block, if any.
    pub fn first_key(&self) -> Result<Option<Vec<u8>>> {
        match self.restarts.first() {
            None => Ok(None),
            Some(&offset) => Ok(Some(self.entry_at(offset as usize)?.key)),
        }
    }

    /// Looks up an exact key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.restarts.is_empty() {
            return Ok(None);
        }
        // find the last restart whose key is <= `key`
        let mut left = 0usize;
        let mut right = self.restarts.len();
        while left < right {
            let mid = (left + right) / 2;
            let restart_key = self.entry_at(self.restarts[mid] as usize)?.key;
            if restart_key.as_slice() < key {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        let start = if left == 0 { 0 } else { self.restarts[left - 1] as usize };
        let mut current = Vec::new();
        let mut offset = start;
        while offset < self.restart_array_start {
            let entry = self.decode(offset)?;
            current.truncate(entry.shared);
            current.extend_from_slice(&self.data[entry.key_offset..entry.key_offset + entry.non_shared]);
            match current.as_slice().cmp(key) {
                Ordering::Less => offset = entry.next,
                Ordering::Equal => {
                    let value = self.data[entry.value_offset..entry.value_offset + entry.value_len].to_vec();
                    return Ok(Some(value));
                }
                Ordering::Greater => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Every entry in ascending key order.
    pub fn entries(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut current = Vec::new();
        let mut offset = 0usize;
        while offset < self.restart_array_start {
            let entry = self.decode(offset)?;
            current.truncate(entry.shared);
            current.extend_from_slice(&self.data[entry.key_offset..entry.key_offset + entry.non_shared]);
            let value = self.data[entry.value_offset..entry.value_offset + entry.value_len].to_vec();
            out.push((current.clone(), value));
            offset = entry.next;
        }
        Ok(out)
    }

    fn entry_at(&self, offset: usize) -> Result<DecodedEntry> {
        let entry = self.decode(offset)?;
        if entry.shared != 0 {
            return Err(Error::Runtime("restart entry must not share a prefix".into()));
        }
        let key = self.data[entry.key_offset..entry.key_offset + entry.non_shared].to_vec();
        Ok(DecodedEntry { key, ..entry })
    }

    fn decode(&self, offset: usize) -> Result<DecodedEntry> {
        if offset >= self.restart_array_start {
            return Err(Error::Runtime("block entry offset out of range".into()));
        }
        let mut pos = offset;
        let shared = get_varint32(&self.data, &mut pos)? as usize;
        let non_shared = get_varint32(&self.data, &mut pos)? as usize;
        let value_len = get_varint32(&self.data, &mut pos)? as usize;
        let key_offset = pos;
        let value_offset = key_offset + non_shared;
        let next = value_offset + value_len;
        if next > self.restart_array_start {
            return Err(Error::Runtime("block entry runs past the restart array".into()));
        }
        Ok(DecodedEntry { shared, non_shared, value_len, key_offset, value_offset, next, key: Vec::new() })
    }
}

struct DecodedEntry {
    shared: usize,
    non_shared: usize,
    value_len: usize,
    key_offset: usize,
    value_offset: usize,
    next: usize,
    key: Vec<u8>,
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let max = a.len().min(b.len());
    let mut i = 0;
    while i < max && a[i] == b[i] {
        i += 1;
    }
    i
}
