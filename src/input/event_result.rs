// SPDX-License-Identifier: GPL-3.0-only

/// Whether an input event needs the backend's fallback redraw request.
/// Input delivery and explicit redraw requests are independent of this result.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InputEventResult {
    needs_redraw: bool,
}

impl Default for InputEventResult {
    fn default() -> Self {
        Self { needs_redraw: true }
    }
}

impl InputEventResult {
    pub(crate) fn needs_redraw(self) -> bool {
        self.needs_redraw
    }

    pub(crate) fn locked_pointer(
        &mut self,
        cursor_revealed: bool,
        magnifying: bool,
        grabbed: bool,
    ) {
        // A locked pointer does not move the cursor. Keep the conservative
        // redraw for visible cursor changes and grabs with arbitrary handlers.
        self.needs_redraw = cursor_revealed || magnifying || grabbed;
    }
}

#[cfg(test)]
mod tests {
    use super::InputEventResult;

    #[test]
    fn only_unchanged_ungrabbed_locked_motion_skips_redraw() {
        for (revealed, magnifying, grabbed, expected) in [
            (false, false, false, false),
            (true, false, false, true),
            (false, true, false, true),
            (false, false, true, true),
            (true, true, false, true),
            (true, false, true, true),
            (false, true, true, true),
            (true, true, true, true),
        ] {
            let mut result = InputEventResult::default();
            result.locked_pointer(revealed, magnifying, grabbed);
            assert_eq!(result.needs_redraw(), expected);
        }
    }

    #[test]
    fn each_event_starts_with_conservative_redraw() {
        let mut locked = InputEventResult::default();
        locked.locked_pointer(false, false, false);
        assert!(!locked.needs_redraw());
        assert!(InputEventResult::default().needs_redraw());
    }
}
