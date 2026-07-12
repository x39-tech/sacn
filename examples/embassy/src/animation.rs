/// The number of DMX slots in a full sACN universe.
pub const SLOTS: usize = 512;

/// The animated patterns the firmware can transmit. The onboard button cycles
/// through them in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pattern {
    /// A short pattern of high-to-low slots chase across the universe.
    Chase,
    /// A sawtooth gradient that scrolls along the universe.
    Ramp,
    /// All slots flashing fully on and off together.
    Strobe,
    /// All slots held at a steady half level.
    Solid,
}

impl Pattern {
    /// The next pattern in the cycle.
    pub fn next(self) -> Self {
        match self {
            Pattern::Chase => Pattern::Ramp,
            Pattern::Ramp => Pattern::Strobe,
            Pattern::Strobe => Pattern::Solid,
            Pattern::Solid => Pattern::Chase,
        }
    }

    /// A short human-readable name, for logging.
    pub fn name(self) -> &'static str {
        match self {
            Pattern::Chase => "chase",
            Pattern::Ramp => "ramp",
            Pattern::Strobe => "strobe",
            Pattern::Solid => "solid",
        }
    }
}

/// A stateful animation that renders successive frames of DMX levels.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    /// The pattern currently being rendered.
    pub pattern: Pattern,
    /// A monotonically increasing frame counter driving the animation.
    frame: usize,
}

impl Default for Animation {
    fn default() -> Self {
        Self::new()
    }
}

impl Animation {
    /// A fresh animation starting on the [`Pattern::Chase`] pattern.
    pub const fn new() -> Self {
        Self {
            pattern: Pattern::Chase,
            frame: 0,
        }
    }

    /// Advances the animation by one frame.
    pub fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Switches to the next pattern and restarts its animation.
    pub fn next_pattern(&mut self) {
        self.pattern = self.pattern.next();
        self.frame = 0;
    }

    /// Renders the current frame into `out`, one byte per DMX slot.
    pub fn render(&self, out: &mut [u8; SLOTS]) {
        match self.pattern {
            Pattern::Chase => {
                let head = self.frame % SLOTS;
                for (i, level) in out.iter_mut().enumerate() {
                    // A bright head that decays over the eight preceding slots.
                    let distance = i.abs_diff(head);
                    *level = if distance < 8 {
                        255u16.saturating_sub(distance as u16 * 32) as u8
                    } else {
                        0
                    };
                }
            }
            Pattern::Ramp => {
                let shift = self.frame;
                for (i, level) in out.iter_mut().enumerate() {
                    *level = (i + shift) as u8;
                }
            }
            Pattern::Strobe => {
                // Toggle every 8 frames so the flash is visible at ~33 fps.
                let on = (self.frame / 8).is_multiple_of(2);
                out.fill(if on { 255 } else { 0 });
            }
            Pattern::Solid => {
                out.fill(128);
            }
        }
    }
}
