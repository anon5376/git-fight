//! Mix integers into a portable `u64` hash. Not a cryptographic hash.

pub fn mix(h: u64, v: u64) -> u64 {
    let mut x = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= v.rotate_left(17);
    x = x.wrapping_add(v);
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_stable() {
        assert_eq!(mix(1, 2), mix(1, 2));
        assert_ne!(mix(1, 2), mix(2, 1));
    }
}
