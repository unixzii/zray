//! The opening and closing animation's clock.

use std::time::{Duration, Instant};

use super::Outcome;

/// Opening: how long the prompt line takes to slide into place.
pub(super) const SLIDE: Duration = Duration::from_millis(150);
/// Opening: how long the list and tab bar then take to fade in.
pub(super) const FADE: Duration = Duration::from_millis(180);
/// Redraw interval during the opening and closing transitions.
pub(super) const FRAME: Duration = Duration::from_millis(12);

/// Where the opening transition stands. Closing runs it backwards from
/// wherever it is, so closing right after opening costs no more time than
/// the opening got to take.
pub(super) struct Transition {
    /// Time into the opening: the slide first, then the fade.
    pub(super) elapsed: Duration,
    pub(super) last_tick: Instant,
    /// Set once the user is done: what to report when the closing ends.
    pub(super) outcome: Option<Outcome>,
    /// Without the terminal's colors there is nothing to mix, so the fade
    /// is skipped and the list appears as the slide ends.
    pub(super) fade: Duration,
}

impl Transition {
    pub(super) fn new(can_fade: bool) -> Self {
        Transition {
            elapsed: Duration::ZERO,
            last_tick: Instant::now(),
            outcome: None,
            fade: if can_fade { FADE } else { Duration::ZERO },
        }
    }

    pub(super) fn advance(&mut self) {
        let now = Instant::now();
        let step = now - self.last_tick;
        self.last_tick = now;
        self.elapsed = if self.outcome.is_some() {
            self.elapsed.saturating_sub(step)
        } else {
            (self.elapsed + step).min(SLIDE + self.fade)
        };
    }

    pub(super) fn closing(&self) -> bool {
        self.outcome.is_some()
    }

    /// Still moving, in either direction.
    pub(super) fn running(&self) -> bool {
        self.closing() || self.elapsed < SLIDE + self.fade
    }

    pub(super) fn closed(&self) -> bool {
        self.closing() && self.elapsed.is_zero()
    }

    /// 0 with the prompt line where the shell drew it, 1 with it in place.
    /// Eased so it decelerates into place.
    pub(super) fn slide(&self) -> f32 {
        let t = (self.elapsed.as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        1.0 - (1.0 - t).powi(2)
    }

    /// Opacity of everything but the prompt line. Linear in time, with no
    /// easing: an eased fade spends most of its change in a few frames,
    /// which at this length reads as a jump.
    pub(super) fn alpha(&self) -> f32 {
        match self.elapsed.checked_sub(SLIDE) {
            None => 0.0,
            Some(_) if self.fade.is_zero() => 1.0,
            Some(into_fade) => (into_fade.as_secs_f32() / self.fade.as_secs_f32()).min(1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_slides_then_fades_and_reverses() {
        let mut t = Transition::new(true);
        assert_eq!((t.slide(), t.alpha()), (0.0, 0.0));
        t.elapsed = SLIDE / 2;
        assert!(t.slide() > 0.5 && t.slide() < 1.0, "eases out");
        assert_eq!(t.alpha(), 0.0);
        t.elapsed = SLIDE + FADE / 2;
        assert_eq!(t.slide(), 1.0);
        assert!((t.alpha() - 0.5).abs() < 0.01);
        t.elapsed = SLIDE + FADE;
        assert!(!t.running());

        t.outcome = Some(Outcome::Cancel);
        assert!(t.running() && !t.closed());
        t.elapsed = Duration::ZERO;
        assert!(t.closed());
    }

    #[test]
    fn without_colors_the_list_appears_when_the_slide_ends() {
        let mut t = Transition::new(false);
        t.elapsed = SLIDE - Duration::from_millis(1);
        assert_eq!(t.alpha(), 0.0);
        t.elapsed = SLIDE;
        assert_eq!(t.alpha(), 1.0);
        assert!(!t.running());
    }
}
