//! Input helpers that are the same wherever the pointer ends up.
//!
//! The protocol speaks in pixels of finger travel, while every real pointer API
//! speaks in wheel detents. Turning one into the other is fiddly enough — and
//! its feel is user-visible enough — that both backends should be doing it the
//! same way rather than each rounding to taste.

/// Pixels of finger travel that make one wheel detent.
///
/// Chosen so a comfortable two-finger swipe covers a few lines rather than a
/// page. Shared so the gesture feels identical on every host.
pub const SCROLL_PIXELS_PER_DETENT: f64 = 24.0;

/// High-resolution wheel units in one detent. Both `REL_WHEEL_HI_RES` on Linux
/// and `WHEEL_DELTA` on Windows use 120, which is not a coincidence: they both
/// inherited it from the original Windows mouse wheel.
pub const WHEEL_UNITS_PER_DETENT: i32 = 120;

/// Turns a stream of pixel deltas into whole wheel detents.
///
/// Sub-detent movement is remembered rather than discarded, so a slow drag
/// still scrolls instead of doing nothing at all; and when the gesture ends, a
/// leftover big enough to have been deliberate is flushed as one detent, so a
/// short flick is not silently swallowed.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollAccumulator {
    horizontal: f64,
    vertical: f64,
}

impl ScrollAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one delta and returns `(horizontal, vertical)` whole detents.
    ///
    /// Sign is passed through untouched: positive vertical means the same thing
    /// here as `REL_WHEEL` on Linux and `MOUSEEVENTF_WHEEL` on Windows, both of
    /// which take positive as "wheel forward". Flipping it in one backend and
    /// not the other is exactly the bug this shared type exists to prevent.
    pub fn push(&mut self, dx: f64, dy: f64, finish: bool) -> (i32, i32) {
        if !dx.is_finite() || !dy.is_finite() {
            return (0, 0);
        }
        self.horizontal += dx;
        self.vertical += dy;
        (
            take_detents(&mut self.horizontal, finish),
            take_detents(&mut self.vertical, finish),
        )
    }
}

fn take_detents(remainder: &mut f64, finish: bool) -> i32 {
    if finish && remainder.abs() > SCROLL_PIXELS_PER_DETENT / 8.0 {
        let direction = if *remainder > 0.0 { 1 } else { -1 };
        *remainder = 0.0;
        return direction;
    }
    let detents = (*remainder / SCROLL_PIXELS_PER_DETENT).trunc();
    if detents == 0.0 {
        return 0;
    }
    *remainder -= detents * SCROLL_PIXELS_PER_DETENT;
    detents as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_scroll_pixels_into_whole_detents() {
        let mut scroll = ScrollAccumulator::new();
        assert_eq!(scroll.push(0.0, 0.0, false), (0, 0));
        assert_eq!(scroll.push(0.0, 10.0, false), (0, 0));
        assert_eq!(scroll.push(0.0, 20.0, false), (0, 1));
        assert!(scroll.vertical.abs() < SCROLL_PIXELS_PER_DETENT);
    }

    #[test]
    fn flushes_a_partial_detent_when_the_gesture_finishes() {
        let mut scroll = ScrollAccumulator::new();
        scroll.push(0.0, 6.0, false);
        assert_eq!(scroll.push(0.0, 0.0, true), (0, 1));
        assert_eq!(scroll.vertical, 0.0);

        let mut negative = ScrollAccumulator::new();
        negative.push(0.0, -6.0, false);
        assert_eq!(negative.push(0.0, 0.0, true), (0, -1));
    }

    #[test]
    fn a_finish_with_nothing_pending_scrolls_nothing() {
        // Every gesture ends with a finish event, so this is the common case:
        // it must not invent a detent out of rounding noise.
        let mut scroll = ScrollAccumulator::new();
        scroll.push(0.0, 1.0, false);
        assert_eq!(scroll.push(0.0, 0.0, true), (0, 0));
    }

    #[test]
    fn the_two_axes_accumulate_independently() {
        let mut scroll = ScrollAccumulator::new();
        assert_eq!(scroll.push(30.0, 10.0, false), (1, 0));
        assert_eq!(scroll.push(0.0, 20.0, false), (0, 1));
    }

    #[test]
    fn a_fast_swipe_yields_several_detents_at_once() {
        let mut scroll = ScrollAccumulator::new();
        assert_eq!(scroll.push(0.0, 100.0, false), (0, 4));
    }

    #[test]
    fn non_finite_deltas_are_ignored_rather_than_poisoning_the_remainder() {
        let mut scroll = ScrollAccumulator::new();
        scroll.push(0.0, 20.0, false);
        assert_eq!(scroll.push(f64::NAN, f64::INFINITY, false), (0, 0));
        // The good 20 px is still there and still counts.
        assert_eq!(scroll.push(0.0, 10.0, false), (0, 1));
    }
}
