/// Winding rule for the boolean engine. The two-op engine only ever fills
/// non-zero sub-regions, so `NonZero` is the sole variant retained from
/// i_overlay (EvenOdd / Positive / Negative pruned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillRule {
    #[default]
    NonZero,
}
