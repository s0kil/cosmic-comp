// SPDX-License-Identifier: GPL-3.0-only

use crate::shell::Shell;
use smithay::{
    backend::renderer::PresentationMode,
    output::Output,
    reexports::wayland_protocols::wp::{
        presentation_time::server::wp_presentation_feedback,
        tearing_control::v1::server::wp_tearing_control_v1::PresentationHint,
    },
    utils::{Monotonic, Time},
    wayland::{
        compositor::with_states, seat::WaylandFocus,
        tearing_control::TearingControlSurfaceCachedState,
    },
};

/// A committed client hint only applies to its focused fullscreen surface.
pub(super) fn requested(shell: &Shell, output: &Output) -> bool {
    if !shell.allow_tearing
        || shell.session_lock.is_some()
        || shell.output_animations_going(output)
        || shell.zoom_state().is_some()
    {
        return false;
    }
    let Some((_, workspace)) = shell.workspaces.active(output) else {
        return false;
    };
    let seat = shell.seats.last_active();
    let Some(fullscreen) = workspace.get_fullscreen(seat) else {
        return false;
    };
    let Some(surface) = fullscreen.surface.wl_surface() else {
        return false;
    };
    let focused = seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus())
        .and_then(|focus| focus.wl_surface().map(|surface| surface.into_owned()));
    if focused.as_ref() != Some(surface.as_ref()) {
        return false;
    }
    with_states(&surface, |states| {
        *states
            .cached_state
            .get::<TearingControlSurfaceCachedState>()
            .current()
            .presentation_hint()
            == PresentationHint::Async
    })
}

pub(super) fn eligible(
    requested: bool,
    mirrored: bool,
    effects: bool,
    vrr: bool,
    capture: bool,
) -> bool {
    requested && !mirrored && !effects && !vrr && !capture
}

/// Only stop waiting for vblank after an async flip has actually succeeded.
pub(super) fn render_immediately(last: PresentationMode, requested: bool) -> bool {
    requested && last == PresentationMode::Async
}

pub(super) fn presentation_flags(
    mode: PresentationMode,
    hardware_clock: bool,
) -> wp_presentation_feedback::Kind {
    let mut flags = wp_presentation_feedback::Kind::HwCompletion;
    if hardware_clock && mode == PresentationMode::VSync {
        flags |= wp_presentation_feedback::Kind::HwClock;
    }
    if mode == PresentationMode::VSync {
        flags |= wp_presentation_feedback::Kind::Vsync;
    }
    flags
}

pub(super) struct CompletionTiming {
    pub presented: Time<Monotonic>,
    pub display_phase: Time<Monotonic>,
    pub hardware_clock: bool,
}

/// Async DRM events can carry the previous vblank's timestamp, not the flip's.
/// Keep that phase for display scheduling, but report software-observed completion.
pub(super) fn completion_timing(
    mode: PresentationMode,
    hardware_time: Option<Time<Monotonic>>,
    observed: Time<Monotonic>,
) -> CompletionTiming {
    let hardware_clock = mode == PresentationMode::VSync && hardware_time.is_some();
    CompletionTiming {
        presented: if hardware_clock {
            hardware_time.unwrap()
        } else {
            observed
        },
        display_phase: hardware_time.unwrap_or(observed),
        hardware_clock,
    }
}

pub(super) fn fullscreen_drives_vrr(
    vrr: bool,
    estimate: impl FnOnce() -> Option<std::time::Duration>,
) -> bool {
    vrr && estimate()
        .is_some_and(|time| time <= std::time::Duration::from_nanos(1_000_000_000 / 30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_describes_actual_submission() {
        for hardware_clock in [false, true] {
            let sync = presentation_flags(PresentationMode::VSync, hardware_clock);
            let asynchronous = presentation_flags(PresentationMode::Async, hardware_clock);
            assert!(sync.contains(wp_presentation_feedback::Kind::Vsync));
            assert!(!asynchronous.contains(wp_presentation_feedback::Kind::Vsync));
            assert!(!asynchronous.contains(wp_presentation_feedback::Kind::HwClock));
            assert!(asynchronous.contains(wp_presentation_feedback::Kind::HwCompletion));
            assert_eq!(
                sync.contains(wp_presentation_feedback::Kind::HwClock),
                hardware_clock
            );
        }
    }

    #[test]
    fn async_events_keep_phase_without_backdating_completion() {
        use std::time::Duration;
        let phase = Duration::from_secs(1000).into();
        for millis in [1_000_001, 1_000_003] {
            let observed = Duration::from_millis(millis).into();
            let timing = completion_timing(PresentationMode::Async, Some(phase), observed);
            assert_eq!(timing.presented, observed);
            assert_eq!(timing.display_phase, phase);
            assert!(!timing.hardware_clock);
        }
        let sync = completion_timing(
            PresentationMode::VSync,
            Some(phase),
            Duration::from_secs(1001).into(),
        );
        assert_eq!(sync.presented, phase);
        assert!(sync.hardware_clock);
        let observed = Duration::from_secs(1001).into();
        let fallback = completion_timing(PresentationMode::VSync, None, observed);
        assert_eq!(fallback.presented, observed);
        assert_eq!(fallback.display_phase, observed);
        assert!(!fallback.hardware_clock);
    }

    #[test]
    fn disabled_vrr_never_walks_the_surface_tree() {
        use std::time::Duration;
        assert!(!fullscreen_drives_vrr(false, || panic!("unused estimate")));
        assert!(fullscreen_drives_vrr(true, || Some(Duration::from_millis(
            10
        ))));
        assert!(!fullscreen_drives_vrr(true, || Some(
            Duration::from_millis(40)
        )));
        assert!(!fullscreen_drives_vrr(true, || None));
    }

    #[test]
    fn compatibility_gates_keep_synchronized_presentation() {
        assert!(eligible(true, false, false, false, false));
        for case in [
            (false, false, false, false, false),
            (true, true, false, false, false),
            (true, false, true, false, false),
            (true, false, false, true, false),
            (true, false, false, false, true),
        ] {
            assert!(!eligible(case.0, case.1, case.2, case.3, case.4));
        }
    }

    #[test]
    fn scheduling_follows_accepted_mode_and_current_policy() {
        assert!(!render_immediately(PresentationMode::VSync, true));
        assert!(render_immediately(PresentationMode::Async, true));
        assert!(!render_immediately(PresentationMode::Async, false));
    }
}
