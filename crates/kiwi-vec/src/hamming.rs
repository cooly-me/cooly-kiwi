//! Hamming distance between 384-bit codes: XOR and population count over six 64-bit
//! words. On aarch64 the count runs on NEON (`cnt` per byte, then a horizontal add);
//! elsewhere it uses `count_ones`, which compiles to `popcnt` where available.

use crate::CODE_BYTES;

/// Portable distance: six `u64` XORs and `count_ones`.
pub fn distance_portable(a: &[u8], b: &[u8]) -> u32 {
    let mut total = 0u32;
    for (x, y) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        let mut xa = [0u8; 8];
        let mut xb = [0u8; 8];
        xa.copy_from_slice(x);
        xb.copy_from_slice(y);
        total += (u64::from_le_bytes(xa) ^ u64::from_le_bytes(xb)).count_ones();
    }
    total
}

/// Distance between two codes of [`CODE_BYTES`] bytes. Shorter inputs compare only
/// their common prefix.
#[inline]
pub fn distance(a: &[u8], b: &[u8]) -> u32 {
    if a.len() < CODE_BYTES || b.len() < CODE_BYTES {
        return distance_portable(a, b);
    }
    #[cfg(target_arch = "aarch64")]
    {
        neon::distance(a, b)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        distance_portable(a, b)
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::{uint8x16_t, vaddlvq_u8, vcntq_u8, veorq_u8, vld1q_u8};

    use crate::CODE_BYTES;

    /// NEON distance over exactly [`CODE_BYTES`] (48) bytes: three 16-byte lanes.
    #[inline]
    pub fn distance(a: &[u8], b: &[u8]) -> u32 {
        debug_assert!(a.len() >= CODE_BYTES && b.len() >= CODE_BYTES);
        let mut total = 0u32;
        for lane in 0..CODE_BYTES / 16 {
            let offset = lane * 16;
            // SAFETY: NEON is part of the aarch64 baseline, and both slices hold at least
            // CODE_BYTES bytes (checked by the caller), so every 16-byte load at
            // `offset < CODE_BYTES` is in bounds.
            let count = unsafe {
                let x: uint8x16_t = vld1q_u8(a.as_ptr().add(offset));
                let y: uint8x16_t = vld1q_u8(b.as_ptr().add(offset));
                vaddlvq_u8(vcntq_u8(veorq_u8(x, y)))
            };
            total += u32::from(count);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_matches_portable() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..1000 {
            let a: Vec<u8> = (0..CODE_BYTES).map(|_| next() as u8).collect();
            let b: Vec<u8> = (0..CODE_BYTES).map(|_| next() as u8).collect();
            assert_eq!(distance(&a, &b), distance_portable(&a, &b));
        }
        let zeros = [0u8; CODE_BYTES];
        let ones = [0xffu8; CODE_BYTES];
        assert_eq!(distance(&zeros, &ones), 384);
        assert_eq!(distance(&ones, &ones), 0);
    }
}
