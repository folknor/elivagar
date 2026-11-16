/// Deterministic xorshift64 stream for seeded property suites. Use a nonzero seed.
pub(crate) fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}
