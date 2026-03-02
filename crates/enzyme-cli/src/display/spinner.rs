/// Braille helix spinner frames.
pub const HELIX_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A simple tick-based spinner using braille dot characters.
pub struct HelixSpinner {
    tick: usize,
}

impl HelixSpinner {
    pub fn new() -> Self {
        Self { tick: 0 }
    }

    /// Advance the spinner and return the current frame.
    pub fn tick(&mut self) -> &'static str {
        let frame = HELIX_FRAMES[self.tick % HELIX_FRAMES.len()];
        self.tick += 1;
        frame
    }
}
