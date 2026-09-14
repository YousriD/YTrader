//! Shared inventory helpers for indicator strategies.
//!
//! P1-2 rule: no strategy pyramids by default. Every strategy in this
//! crate (and `strategy-sma` via its `with_inventory` builder) applies
//! the same two guards: skip when a position is already open unless the
//! agent opted in, and never emit more than `max_units`.

/// True when the agent already holds a position and pyramiding is off.
/// Callers must still update their crossover state before returning
/// `None`, so a suppressed signal doesn't fire stale later.
pub fn pyramid_blocked(open_units: f64, allow_pyramid: bool) -> bool {
    open_units != 0.0 && !allow_pyramid
}

/// Clamp emitted size into `[1.0, max_units]`. The 1.0 floor is a unit
/// count, not a dollar guarantee — the broker's margin check still
/// rejects what the account can't cover.
pub fn clamp_units(wanted: f64, max_units: f64) -> f64 {
    wanted.clamp(1.0, max_units.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_blocks_only_open_and_unopted() {
        assert!(pyramid_blocked(10.0, false));
        assert!(pyramid_blocked(-10.0, false));
        assert!(!pyramid_blocked(10.0, true));
        assert!(!pyramid_blocked(0.0, false));
    }

    #[test]
    fn clamp_respects_floor_and_cap() {
        assert_eq!(clamp_units(500.0, 100.0), 100.0);
        assert_eq!(clamp_units(0.2, 100.0), 1.0);
        assert_eq!(clamp_units(50.0, 100.0), 50.0);
    }
}
