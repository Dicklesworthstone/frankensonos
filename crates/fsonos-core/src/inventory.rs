//! Device inventory: turning raw discovery results into a [`HouseholdState`].
//!
//! SSDP (and the direct-seed fallback) yields device-description URLs; this
//! module fetches and parses them, classifies S1 vs S2, and reconciles the
//! result into the authoritative model. The network fetch is wired in
//! FND-DEPS; the parsing/classification is pure.

use fsonos_types::Generation;

/// Classify a Sonos model string into its software generation. The model list
/// is extended as hardware is confirmed on real households.
#[must_use]
pub fn generation_for_model(model: &str) -> Generation {
    let m = model.to_ascii_lowercase();
    // S1-only hardware (never upgraded to S2).
    if m.contains("zp80")
        || m.contains("zp90")
        || m.contains("zp100")
        || m.contains("zp120")
        || m.contains("bridge")
        || m.contains("play:5") && m.contains("gen 1")
        || m.contains("s5")
    {
        return Generation::S1;
    }
    // Everything else modern defaults to S2; confirmed per-household at runtime.
    Generation::S2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_bridge_as_s1() {
        assert_eq!(generation_for_model("Sonos Bridge"), Generation::S1);
    }

    #[test]
    fn classifies_one_as_s2() {
        assert_eq!(generation_for_model("Sonos One"), Generation::S2);
    }
}
