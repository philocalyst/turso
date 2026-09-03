use crate::model::{ChunkHash, NodeFlags, StoreError};

const PRIME1: u32 = 0x9E3779B1;
const PRIME2: u32 = 0x85EBCA77;
const PRIME3: u32 = 0xC2B2AE3D;
const PRIME4: u32 = 0x27D4EB2F;
const PRIME5: u32 = 0x165667B1;

/// BLAKE3 XOF output truncated to 20 bytes per doltlite commit a29ad438.
pub fn blake3_chunk_hash(data: &[u8]) -> ChunkHash {
    let full = blake3::hash(data);
    let mut buf = [0u8; 20];
    buf.copy_from_slice(&full.as_bytes()[..20]);
    ChunkHash(buf)
}

pub fn split_decision(hash32: u32, level: u32) -> bool {
    assert!(level <= 32);
    let threshold = 1u32.checked_shl(32u32.wrapping_sub(level)).unwrap_or(0);
    hash32 < threshold
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub hash: ChunkHash,
    pub data: Vec<u8>,
}

pub struct WeibullChunker {
    buffer: Vec<u8>,
    front: usize,
    chunks: Vec<Chunk>,
    level: u32,
}

impl Default for WeibullChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl WeibullChunker {
    pub const MIN: usize = 512;
    pub const MAX: usize = 16_384;
    pub const L: usize = 4_096;

    pub fn with_level(level: u32) -> Self {
        assert!(level <= 32);
        WeibullChunker {
            buffer: Vec::with_capacity(Self::MAX),
            front: 0,
            chunks: Vec::new(),
            level,
        }
    }

    /// Level whose expected chunk size is 2^level: one position in 2^level
    /// splits, so floor(log2) of the target centers the distribution on it.
    fn level_for_target(target: usize) -> u32 {
        assert!(target >= 1);
        usize::BITS - 1 - target.leading_zeros()
    }

    pub fn new() -> Self {
        Self::with_level(Self::level_for_target(Self::L))
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);

        while self.buffer.len() - self.front >= Self::MAX {
            self.split_at_level(self.level);
        }
    }

    /// Tail merged across pushes, below the split threshold; `finish` emits it.
    pub fn remaining(&self) -> &[u8] {
        &self.buffer[self.front..]
    }

    /// Counts only what `push` split, never the tail `finish` emits.
    pub fn push_emitted_len(&self) -> usize {
        self.chunks.len()
    }

    /// Flushes the merge-not-emit buffer as one final chunk. `push` only ever
    /// emits chunks in `[MIN, MAX]`; this tail may be smaller than `MIN`
    /// because an undersized remainder is merged across pushes, never emitted.
    pub fn finish(mut self) -> Vec<Chunk> {
        if self.front < self.buffer.len() {
            let hash = blake3_chunk_hash(&self.buffer[self.front..]);
            self.chunks.push(Chunk {
                hash,
                data: self.buffer[self.front..].to_vec(),
            });
        }
        self.chunks
    }

    fn split_at_level(&mut self, level: u32) {
        // The caller only invokes this with a full buffer (len - front >= MAX),
        // so the scan always ends in a forced MAX split even if no hash boundary
        // fires.
        let start = self.front;
        let limit = self.buffer.len() - 32;
        let mut i = start + Self::MIN;
        let mut found = false;

        while i <= limit {
            if i >= start + Self::MAX || split_decision(xxhash32(&self.buffer[i..i + 32], 0), level)
            {
                let hash = blake3_chunk_hash(&self.buffer[start..i]);
                self.chunks.push(Chunk {
                    hash,
                    data: self.buffer[start..i].to_vec(),
                });
                self.front = i;
                found = true;
                break;
            }
            i += 1;
        }

        // Undersized tail stays in buffer across pushes — merged, not emitted as a
        // sub-minimum chunk. The caller must call finish() to emit any remainder.
        if !found && self.buffer.len() - self.front >= Self::MAX {
            let end = self.front + Self::MAX;
            let hash = blake3_chunk_hash(&self.buffer[self.front..end]);
            self.chunks.push(Chunk {
                hash,
                data: self.buffer[self.front..end].to_vec(),
            });
            self.front = end;
        }

        // Consumed-prefix bytes are dropped once the front cursor passes the
        // midpoint, so each byte is memmoved at most twice in total (amortized
        // linear instead of quadratic across chunk splits).
        if self.front >= self.buffer.len() / 2 {
            self.buffer.drain(0..self.front);
            self.front = 0;
        }
    }
}

pub const MAX_ITEMS: usize = 4096;

pub const NODE_MAGIC: u32 = 0x504E4F44;

#[derive(Debug, Clone)]
pub struct ProllyNode {
    pub flags: NodeFlags,
    pub counts: [u32; 2],
    pub items: Vec<(Vec<u8>, Vec<u8>)>,
}

impl ProllyNode {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&NODE_MAGIC.to_be_bytes());
        buf.extend_from_slice(&self.flags.0.to_be_bytes());
        let count = self.items.len();
        assert!(count <= MAX_ITEMS);
        buf.extend_from_slice(&(count as u16).to_be_bytes());
        buf.extend_from_slice(&self.counts[0].to_be_bytes());
        buf.extend_from_slice(&self.counts[1].to_be_bytes());
        for (key, value) in &self.items {
            // Lengths are stored as u16, so anything past 64 KiB would silently
            // truncate on encode; panic instead (crash > corrupt).
            assert!(key.len() <= u16::MAX as usize, "prolly key too long");
            assert!(value.len() <= u16::MAX as usize, "prolly value too long");
            buf.extend_from_slice(&(key.len() as u16).to_be_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
            buf.extend_from_slice(value);
        }
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, StoreError> {
        if buf.len() < 16 {
            return Err(StoreError::BadNodeMagic);
        }
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != NODE_MAGIC {
            return Err(StoreError::BadNodeMagic);
        }
        let flags = NodeFlags(u16::from_be_bytes([buf[4], buf[5]]));
        let count = u16::from_be_bytes([buf[6], buf[7]]) as usize;
        if count > MAX_ITEMS {
            return Err(StoreError::BadNodeMagic);
        }
        let mut counts = [0u32; 2];
        counts[0] = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        counts[1] = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);

        let mut items = Vec::with_capacity(count.min(MAX_ITEMS));
        let mut pos = 16;
        for _ in 0..count {
            if pos + 2 > buf.len() {
                return Err(StoreError::BadNodeMagic);
            }
            let klen = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
            pos += 2;
            if pos + klen > buf.len() {
                return Err(StoreError::BadNodeMagic);
            }
            let key = buf[pos..pos + klen].to_vec();
            pos += klen;

            if pos + 2 > buf.len() {
                return Err(StoreError::BadNodeMagic);
            }
            let vlen = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
            pos += 2;
            if pos + vlen > buf.len() {
                return Err(StoreError::BadNodeMagic);
            }
            let value = buf[pos..pos + vlen].to_vec();
            pos += vlen;
            items.push((key, value));
        }

        if pos != buf.len() {
            return Err(StoreError::BadNodeMagic);
        }

        Ok(ProllyNode {
            flags,
            counts,
            items,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub struct Builder {
    items: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Builder { items: Vec::new() }
    }

    /// Rejects out-of-order keys and full capacity; capacity rejection reuses
    /// the `UnorderedInsert` variant rather than adding one (no silent `as
    /// u16` truncation — the count is guarded above `MAX_ITEMS`).
    pub fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), StoreError> {
        if self.items.len() >= MAX_ITEMS {
            return Err(StoreError::UnorderedInsert);
        }
        if let Some((last_key, _)) = self.items.last() {
            if key <= *last_key {
                return Err(StoreError::UnorderedInsert);
            }
        }
        self.items.push((key, value));
        Ok(())
    }

    pub fn finish(self) -> ProllyNode {
        ProllyNode {
            flags: NodeFlags(0),
            counts: [0, 0],
            items: self.items,
        }
    }
}

pub struct Cursor {
    nodes: Vec<ProllyNode>,
    node_idx: usize,
    item_idx: usize,
}

impl Cursor {
    pub fn new(nodes: Vec<ProllyNode>) -> Self {
        Cursor {
            nodes,
            node_idx: 0,
            item_idx: 0,
        }
    }

    /// Seek to first item with key >= target.
    pub fn seek(&mut self, key: &[u8]) -> Option<(&[u8], &[u8])> {
        self.item_idx = 0;
        for ni in 0..self.nodes.len() {
            let node = &self.nodes[ni];
            for ii in 0..node.items.len() {
                if node.items[ii].0.as_slice() >= key {
                    self.node_idx = ni;
                    self.item_idx = ii;
                    return Some((node.items[ii].0.as_slice(), node.items[ii].1.as_slice()));
                }
            }
        }
        None
    }

    /// Returns the item after the current position.
    pub fn advance(&mut self) -> Option<(&[u8], &[u8])> {
        if self.node_idx >= self.nodes.len() {
            return None;
        }
        self.item_idx += 1;
        while self.node_idx < self.nodes.len() {
            let node = &self.nodes[self.node_idx];
            if self.item_idx < node.items.len() {
                let item = &node.items[self.item_idx];
                return Some((item.0.as_slice(), item.1.as_slice()));
            }
            self.node_idx += 1;
            self.item_idx = 0;
        }
        None
    }
}

pub struct MutMap {
    snapshot: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Default for MutMap {
    fn default() -> Self {
        Self::new()
    }
}

impl MutMap {
    pub fn new() -> Self {
        MutMap {
            snapshot: Vec::new(),
        }
    }

    pub fn snapshot(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.snapshot
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.snapshot
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
            .ok()
            .map(|i| self.snapshot[i].1.as_slice())
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        match self
            .snapshot
            .binary_search_by(|(k, _)| k.as_slice().cmp(&key))
        {
            Ok(i) => self.snapshot[i] = (key, value),
            Err(i) => self.snapshot.insert(i, (key, value)),
        }
    }

    pub fn delete(&mut self, key: &[u8]) -> bool {
        match self
            .snapshot
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
        {
            Ok(i) => {
                self.snapshot.remove(i);
                true
            }
            Err(_) => false,
        }
    }
}

pub fn xxhash32(data: &[u8], seed: u32) -> u32 {
    let len = data.len();
    let mut h32: u32;
    let mut index = 0;

    if len >= 16 {
        let mut v1 = seed.wrapping_add(PRIME1).wrapping_add(PRIME2);
        let mut v2 = seed.wrapping_add(PRIME2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME1);

        while index + 16 <= len {
            v1 = round(
                v1,
                u32::from_le_bytes([
                    data[index],
                    data[index + 1],
                    data[index + 2],
                    data[index + 3],
                ]),
            );
            v2 = round(
                v2,
                u32::from_le_bytes([
                    data[index + 4],
                    data[index + 5],
                    data[index + 6],
                    data[index + 7],
                ]),
            );
            v3 = round(
                v3,
                u32::from_le_bytes([
                    data[index + 8],
                    data[index + 9],
                    data[index + 10],
                    data[index + 11],
                ]),
            );
            v4 = round(
                v4,
                u32::from_le_bytes([
                    data[index + 12],
                    data[index + 13],
                    data[index + 14],
                    data[index + 15],
                ]),
            );
            index += 16;
        }

        h32 = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        h32 = seed.wrapping_add(PRIME5);
    }

    h32 = h32.wrapping_add(len as u32);

    while index + 4 <= len {
        // Standard xxHash32 folds word*P3 into the accumulator before the
        // rotation (doltlite prollyXXH32 agrees); multiplying the running sum
        // instead would diverge on any input that reaches this loop.
        h32 = h32
            .wrapping_add(
                u32::from_le_bytes([
                    data[index],
                    data[index + 1],
                    data[index + 2],
                    data[index + 3],
                ])
                .wrapping_mul(PRIME3),
            )
            .rotate_left(17)
            .wrapping_mul(PRIME4);
        index += 4;
    }

    while index < len {
        h32 = h32
            .wrapping_add((data[index] as u32).wrapping_mul(PRIME5))
            .rotate_left(11)
            .wrapping_mul(PRIME1);
        index += 1;
    }

    h32 ^= h32 >> 15;
    h32 = h32.wrapping_mul(PRIME2);
    h32 ^= h32 >> 13;
    h32 = h32.wrapping_mul(PRIME3);
    h32 ^= h32 >> 16;

    h32
}

#[inline]
fn round(acc: u32, input: u32) -> u32 {
    // The 16-byte-block lane uses input*PRIME2, not PRIME4: standard xxHash32
    // and doltlite prollyXXH32 agree, and the vendored golden vectors pin it.
    acc.wrapping_add(input.wrapping_mul(PRIME2))
        .rotate_left(13)
        .wrapping_mul(PRIME1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixed_payload(len: usize) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect()
    }

    #[test]
    fn chunk_blake3_known_vector() {
        let h = blake3_chunk_hash(b"");
        let expected_hex = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9";
        let expected = ChunkHash::from_hex(expected_hex).unwrap();
        assert_eq!(h, expected);
    }

    #[test]
    fn chunk_blake3_1mib_deterministic() {
        let data = vec![0xABu8; 1024 * 1024];
        assert_eq!(blake3_chunk_hash(&data), blake3_chunk_hash(&data));
    }

    #[test]
    fn chunk_blake3_prefix_differs() {
        // Truncation to 20 bytes must not lose sensitivity: two 1 MiB inputs that
        // differ only in the last byte hash to different chunks.
        let a = vec![0u8; 1024 * 1024];
        let mut b = a.clone();
        *b.last_mut().unwrap() = 1;
        assert_ne!(blake3_chunk_hash(&a), blake3_chunk_hash(&b));
    }

    #[test]
    fn chunk_xxhash32_doltlite_golden() {
        // Vendored from doltlite test/prolly_chunker_boundary_test.c
        // (master 5c67114c0374): prollyXXH32(seed 0) over the 8-byte big-endian
        // encoding of integer keys 0..31. Same primes and mixing as our xxhash32.
        let expected: [u32; 32] = [
            0xdeb39513, 0xf414a945, 0xd53c8ecb, 0xea0bee70, 0xf8cdd998, 0x17d5f46a, 0xb2edcb43,
            0x87a9925e, 0xa3301e1d, 0x4c537cb4, 0x8b4174dd, 0x5ece03f1, 0xbb28819c, 0x52f7a644,
            0x867ce427, 0xfd624c39, 0x15d91f09, 0x9efb9836, 0xd2b74cf3, 0x7d456185, 0xeaabb43d,
            0xa3f424a0, 0xff4b9b11, 0x0817d47a, 0x10d28df6, 0x08ab1330, 0x14c039bd, 0x04d7fa0e,
            0xe76e2dbd, 0x74e636d9, 0x3119a83e, 0xf5b8a0e0,
        ];
        for (key, want) in expected.iter().enumerate() {
            assert_eq!(xxhash32(&(key as u64).to_be_bytes(), 0), *want, "key {key}");
        }
        // The C test only pins <16-byte inputs (8-byte keys), which never reach
        // the 16-byte-block round(). Pin the block path too so the byte-for-byte
        // parity claim is tested: canonical xxHash32 values (prollyXXH32 is a
        // verbatim port), cross-checked against twox-hash 1.x.
        let block = (0u8..16).collect::<Vec<u8>>();
        assert_eq!(xxhash32(&block, 0), 0xb72837f4, "bytes 0..15");
        assert_eq!(xxhash32(&[0xABu8; 32], 0), 0xed4004c7, "32x0xAB");
        assert_eq!(xxhash32(b"Hello, world!", 0), 0x31b7405d);
    }

    #[test]
    fn chunk_weibull_boundary_golden() {
        // Golden boundary offsets for a 64 KiB mixed payload at level 10, gated
        // by the doltlite-parity xxhash32 decision rule (each 32-byte window
        // hashed with xxhash32(window, 0); split iff hash < 1 << (32-level)).
        // Level 10 is chosen because the correct hash fires both hash splits and
        // one forced MAX split and leaves a finish tail, so every branch of the
        // chunker is pinned. (The default level 12 fires no hash split on this
        // structured payload, so it would only pin the forced-split path.) The
        // doltlite tie for the hash itself lives in chunk_xxhash32_doltlite_golden.
        let payload = mixed_payload(64 * 1024);
        let mut chunker = WeibullChunker::with_level(10);
        chunker.push(&payload);
        let emitted = chunker.push_emitted_len();
        let chunks = chunker.finish();

        let expected_boundaries = [
            2374, 4829, 7284, 9739, 12194, 14649, 17104, 19559, 22014, 24469, 26924, 29379, 31834,
            34289, 36744, 39199, 41654, 44109, 60493,
        ];
        assert_eq!(emitted, expected_boundaries.len());
        let mut offset = 0usize;
        for (chunk, want) in chunks[..emitted].iter().zip(expected_boundaries) {
            offset += chunk.data.len();
            assert_eq!(offset, want, "chunk boundary offset mismatch");
        }
        assert_eq!(chunks[emitted].data.len(), 5043, "finish tail");
    }

    #[test]
    fn chunk_weibull_sizes_within_min_max() {
        // Level 0 never hash-splits, and 1 MiB is an exact multiple of MAX,
        // so every chunk is a full 16384-byte forced split with no tail.
        let mut chunker = WeibullChunker::with_level(0);
        let payload: Vec<u8> = (0..1_048_576u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        chunker.push(&payload);
        let chunks = chunker.finish();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(
                c.data.len() >= WeibullChunker::MIN && c.data.len() <= WeibullChunker::MAX,
                "chunk size {} outside [{}, {}]",
                c.data.len(),
                WeibullChunker::MIN,
                WeibullChunker::MAX
            );
        }
    }

    #[test]
    fn chunk_weibull_deterministic() {
        let payload: Vec<u8> = (0..1_048_576u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();

        let mut c1 = WeibullChunker::new();
        c1.push(&payload);
        let chunks1 = c1.finish();

        let mut c2 = WeibullChunker::new();
        c2.push(&payload);
        let chunks2 = c2.finish();

        assert_eq!(chunks1.len(), chunks2.len());
        for (a, b) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.data, b.data);
        }
    }

    #[test]
    fn chunk_weibull_empty_input() {
        let chunker = WeibullChunker::new();
        let chunks = chunker.finish();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_weibull_small_input_buffered() {
        let mut chunker = WeibullChunker::new();
        chunker.push(&[0xAB; 10]);
        assert_eq!(chunker.remaining().len(), 10);
        let chunks = chunker.finish();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data.len(), 10);
    }

    #[test]
    fn chunk_weibull_split_push_matches_single_push() {
        // A splitting level (8 → expected ~768-byte chunks) and ≥64 KiB total
        // so the test exercises hash-split boundaries, not one merged tail.
        let payload_a: Vec<u8> = (0..40_960u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        let payload_b: Vec<u8> = (0..40_960u32)
            .map(|i| (i.wrapping_mul(4_050_328) >> 8) as u8)
            .collect();

        let mut chunker1 = WeibullChunker::with_level(8);
        chunker1.push(&payload_a);
        chunker1.push(&payload_b);
        let chunks1 = chunker1.finish();

        let mut chunker2 = WeibullChunker::with_level(8);
        let mut combined = payload_a.clone();
        combined.extend_from_slice(&payload_b);
        chunker2.push(&combined);
        let chunks2 = chunker2.finish();

        assert!(chunks1.len() >= 2, "payload must reach a hash split");
        assert_eq!(chunks1.len(), chunks2.len());
        for (a, b) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.data, b.data);
        }
    }

    #[test]
    fn chunk_weibull_max_plus_10_tail() {
        // At level 0 nothing hash-splits, so push() forces full-MAX splits and
        // the 10 leftover bytes ride in the buffer as the finish() tail.
        let mut chunker = WeibullChunker::with_level(0);
        let payload = vec![0u8; WeibullChunker::MAX + 10];
        chunker.push(&payload);
        let emitted = chunker.push_emitted_len();
        assert!(emitted >= 1);
        let chunks = chunker.finish();
        assert_eq!(chunks.len(), emitted + 1);
        for c in &chunks[..emitted] {
            assert!(
                (WeibullChunker::MIN..=WeibullChunker::MAX).contains(&c.data.len()),
                "push-emitted chunk size {} outside [{}, {}]",
                c.data.len(),
                WeibullChunker::MIN,
                WeibullChunker::MAX
            );
        }
        assert_eq!(chunks[emitted].data.len(), 10);
    }

    #[test]
    fn chunk_weibull_nominal_target_near_l() {
        // new() picks the level whose expected chunk size is 2^level = L, so
        // the average chunk size on a splitting payload lands near L.
        let payload: Vec<u8> = (0..4_194_304u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        let mut chunker = WeibullChunker::new();
        chunker.push(&payload);
        let emitted = chunker.push_emitted_len();
        let chunks = chunker.finish();
        assert!(emitted >= 32);
        let total: usize = chunks[..emitted].iter().map(|c| c.data.len()).sum();
        let average = total / emitted;
        assert!(
            (WeibullChunker::L / 2..=WeibullChunker::L * 2).contains(&average),
            "average push-emitted chunk {average} not near L = {}",
            WeibullChunker::L
        );
    }

    #[test]
    fn chunk_weibull_nonzero_level_splits() {
        // Level 8 → threshold 2^24 (expected ~768-byte chunks), so a 64 KiB
        // mixed payload splits many times, exercising the hash path.
        let payload: Vec<u8> = (0..65_536u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();

        let mut first = WeibullChunker::with_level(8);
        first.push(&payload);
        let emitted = first.push_emitted_len();
        let chunks = first.finish();

        assert!(emitted >= 4, "level 8 on 64 KiB must split, got {emitted}");
        for c in &chunks[..emitted] {
            assert!(
                (WeibullChunker::MIN..=WeibullChunker::MAX).contains(&c.data.len()),
                "push-emitted chunk size {} outside [{}, {}]",
                c.data.len(),
                WeibullChunker::MIN,
                WeibullChunker::MAX
            );
        }
        assert!(
            chunks.len() <= emitted + 1,
            "only the finish tail may be extra"
        );

        let mut second = WeibullChunker::with_level(8);
        second.push(&payload);
        let chunks2 = second.finish();
        assert_eq!(chunks.len(), chunks2.len());
        for (a, b) in chunks.iter().zip(chunks2.iter()) {
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.data, b.data);
        }
    }

    #[test]
    fn chunk_weibull_hostile_no_hash_split_every_chunk_in_range() {
        // Level 0 never hash-splits, so zeros always fill to MAX. The finish
        // remainder lands in-range here only by arithmetic luck:
        // 100000 − 6×16384 = 1696, which is ≥ MIN. The MAX+10 test covers a
        // sub-MIN tail explicitly.
        let mut chunker = WeibullChunker::with_level(0);
        let payload = vec![0u8; 100_000];
        chunker.push(&payload);
        let chunks = chunker.finish();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(
                c.data.len() >= WeibullChunker::MIN && c.data.len() <= WeibullChunker::MAX,
                "hostile chunk size {} outside [{}, {}]",
                c.data.len(),
                WeibullChunker::MIN,
                WeibullChunker::MAX
            );
        }
    }

    #[test]
    fn chunk_node_pnod_roundtrip() {
        let node = ProllyNode {
            flags: NodeFlags::INTKEY | NodeFlags::BLOBKEY | NodeFlags::COUNTS,
            counts: [100, 200],
            items: vec![
                (b"key1".to_vec(), b"val1".to_vec()),
                (b"key2".to_vec(), b"val2".to_vec()),
            ],
        };
        let encoded = node.encode();
        let decoded = ProllyNode::decode(&encoded).unwrap();
        assert_eq!(decoded.flags.0, node.flags.0);
        assert_eq!(decoded.counts, node.counts);
        assert_eq!(decoded.items, node.items);
        assert_eq!(encoded, decoded.encode());
    }

    #[test]
    fn chunk_node_rejects_bad_magic() {
        let buf = [0xDEu8, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = ProllyNode::decode(&buf);
        assert!(matches!(result, Err(StoreError::BadNodeMagic)));
    }

    #[test]
    fn chunk_node_rejects_trailing_bytes() {
        let node = ProllyNode {
            flags: NodeFlags(0),
            counts: [0, 0],
            items: vec![],
        };
        let mut encoded = node.encode();
        encoded.push(0xFF);
        assert!(ProllyNode::decode(&encoded).is_err());
    }

    #[test]
    fn chunk_node_rejects_count_too_large() {
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&NODE_MAGIC.to_be_bytes());
        buf[6..8].copy_from_slice(&(MAX_ITEMS as u16 + 1).to_be_bytes());
        assert!(ProllyNode::decode(&buf).is_err());
    }

    #[test]
    fn chunk_builder_rejects_unordered() {
        let mut b = Builder::new();
        b.push(b"b".to_vec(), b"1".to_vec()).unwrap();
        let result = b.push(b"a".to_vec(), b"2".to_vec());
        assert!(matches!(result, Err(StoreError::UnorderedInsert)));
    }

    #[test]
    fn chunk_builder_rejects_oversized() {
        let mut b = Builder::new();
        for i in 0..MAX_ITEMS {
            let key = (i as u64).to_be_bytes().to_vec();
            b.push(key, vec![0]).unwrap();
        }
        let result = b.push(vec![0xFF; 8], vec![0]);
        assert!(matches!(result, Err(StoreError::UnorderedInsert)));
    }

    #[test]
    #[should_panic(expected = "prolly key too long")]
    fn chunk_node_encode_rejects_huge_key() {
        let node = ProllyNode {
            flags: NodeFlags(0),
            counts: [0, 0],
            items: vec![(vec![0u8; u16::MAX as usize + 1], vec![0u8])],
        };
        let _ = node.encode();
    }

    #[test]
    fn chunk_cursor_seek_plus_two_advances() {
        let node1 = ProllyNode {
            flags: NodeFlags(0),
            counts: [0, 0],
            items: vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ],
        };
        let node2 = ProllyNode {
            flags: NodeFlags(0),
            counts: [0, 0],
            items: vec![
                (b"c".to_vec(), b"3".to_vec()),
                (b"d".to_vec(), b"4".to_vec()),
            ],
        };
        let mut cursor = Cursor::new(vec![node1, node2]);
        let first = cursor.seek(b"b").unwrap();
        assert_eq!(first, (&b"b"[..], &b"2"[..]));
        let second = cursor.advance().unwrap();
        assert_eq!(second, (&b"c"[..], &b"3"[..]));
        let third = cursor.advance().unwrap();
        assert_eq!(third, (&b"d"[..], &b"4"[..]));
    }

    #[test]
    fn mutmap_put_get_delete_snapshot() {
        let mut m = MutMap::new();
        assert_eq!(m.get(b"x"), None);
        m.put(b"b".to_vec(), b"2".to_vec());
        m.put(b"a".to_vec(), b"1".to_vec());
        m.put(b"c".to_vec(), b"3".to_vec());
        assert_eq!(m.get(b"a"), Some(&b"1"[..]));
        assert_eq!(m.get(b"b"), Some(&b"2"[..]));
        assert_eq!(m.get(b"c"), Some(&b"3"[..]));
        let snap = m.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].0, b"a".to_vec());
        assert_eq!(snap[1].0, b"b".to_vec());
        assert_eq!(snap[2].0, b"c".to_vec());
        m.put(b"b".to_vec(), b"22".to_vec());
        assert_eq!(m.get(b"b"), Some(&b"22"[..]));
        assert!(m.delete(b"a"));
        assert_eq!(m.get(b"a"), None);
        assert!(!m.delete(b"missing"));
        assert_eq!(m.snapshot().len(), 2);
    }

    #[test]
    fn split_decision_levels() {
        // threshold = 1 << (32 - level): split iff hash < threshold
        // level 0: threshold = 0, no hash is below → never split
        assert!(!split_decision(0, 0));
        assert!(!split_decision(u32::MAX, 0));
        // level 1: threshold = 2^31, lower half of hashes split
        assert!(split_decision(0, 1));
        assert!(split_decision(2u32.pow(31) - 1, 1));
        assert!(!split_decision(2u32.pow(31), 1));
        assert!(!split_decision(u32::MAX, 1));
        // level 31: threshold = 2, only hashes 0 and 1 split
        assert!(split_decision(0, 31));
        assert!(split_decision(1, 31));
        assert!(!split_decision(2, 31));
        // level 32: threshold = 1, only hash 0 splits
        assert!(split_decision(0, 32));
        assert!(!split_decision(1, 32));
        assert!(!split_decision(u32::MAX, 32));
    }
}
