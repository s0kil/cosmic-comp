// SPDX-License-Identifier: GPL-3.0-only

use std::sync::atomic::{AtomicUsize, Ordering};

/// Bit zero tracks the current pending flip's undelivered event; the remaining
/// bits count events for flips replaced by a successful blocking commit.
/// Check this and complete frames under the compositor lock. A suspended worker
/// can also consume an event without a compositor; the atomic update then stops
/// a later blocking commit from counting that already-consumed event again.
#[derive(Debug, Default)]
pub(super) struct SupersededFlips(AtomicUsize);

impl SupersededFlips {
    pub(super) fn submitted(&self, pending: bool) {
        if pending {
            self.0.fetch_or(1, Ordering::AcqRel);
        } else {
            self.0.fetch_and(!1, Ordering::AcqRel);
        }
    }

    pub(super) fn committed(&self, replaced_pending: bool) {
        if replaced_pending {
            let _ = self
                .0
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    (state & 1 != 0).then_some((state & !1) + 2)
                });
        }
    }

    pub(super) fn consume(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(if state >= 2 { state - 2 } else { state & !1 })
            })
            .is_ok_and(|previous| previous >= 2)
    }

    pub(super) fn may_complete(&self, pending: bool) -> bool {
        // Consume first, including while suspended or a render timer is queued.
        !self.consume() && pending
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum QueueState<T> {
    #[default]
    Idle,
    Queued(T),
    WaitingForVBlank {
        redraw_needed: bool,
    },
    WaitingForEstimatedVBlank {
        token: T,
        force: bool,
    },
    WaitingForEstimatedVBlankAndQueued {
        estimated_vblank: T,
        queued_render: T,
        force: bool,
    },
}

impl<T: Copy> QueueState<T> {
    pub(super) fn expects_vblank(&self) -> bool {
        matches!(self, Self::WaitingForVBlank { .. })
    }

    /// Only abandon a wait when its presentation owner really disappeared. A
    /// superseded event must not reset the wait for a newer pending frame.
    pub(super) fn recover_missing_completion(&mut self, pending: bool) -> bool {
        if self.expects_vblank() && !pending {
            *self = Self::Idle;
            true
        } else {
            false
        }
    }

    /// Retire a real KMS owner, even when a lifecycle operation changed the
    /// scheduler state. Existing timers survive unless KMS submitted a queued
    /// successor; then transfer their redraw intent to that pending frame.
    pub(super) fn completed(&mut self, next_pending: bool) -> (bool, [Option<T>; 2]) {
        if !next_pending && !self.expects_vblank() {
            return (false, [None, None]);
        }
        let (redraw, timers) = match *self {
            Self::Idle => (false, [None, None]),
            Self::WaitingForVBlank { redraw_needed } => (redraw_needed, [None, None]),
            Self::Queued(token) => (true, [Some(token), None]),
            Self::WaitingForEstimatedVBlank { token, force } => (force, [Some(token), None]),
            Self::WaitingForEstimatedVBlankAndQueued {
                estimated_vblank,
                queued_render,
                ..
            } => (true, [Some(estimated_vblank), Some(queued_render)]),
        };
        *self = if next_pending {
            Self::WaitingForVBlank {
                redraw_needed: redraw,
            }
        } else {
            Self::Idle
        };
        (redraw, timers)
    }

    /// Install a render timer and return a superseded render timer to remove.
    pub(super) fn queue_render(&mut self, token: T, replace: bool) -> Option<T> {
        let (next, removed) = match *self {
            Self::Idle => (Self::Queued(token), None),
            Self::WaitingForEstimatedVBlank {
                token: estimated_vblank,
                force,
            } => (
                Self::WaitingForEstimatedVBlankAndQueued {
                    estimated_vblank,
                    queued_render: token,
                    force,
                },
                None,
            ),
            Self::Queued(old) if replace => (Self::Queued(token), Some(old)),
            Self::WaitingForEstimatedVBlankAndQueued {
                estimated_vblank,
                queued_render,
                force,
            } if replace => (
                Self::WaitingForEstimatedVBlankAndQueued {
                    estimated_vblank,
                    queued_render: token,
                    force,
                },
                Some(queued_render),
            ),
            _ => unreachable!("render timer installed in invalid queue state"),
        };
        *self = next;
        removed
    }

    /// The render callback ran without submitting. Keep the earlier deadline,
    /// but merge its updated request to re-evaluate cursor-only content.
    pub(super) fn reuse_estimated_vblank(&mut self, force: bool) -> bool {
        match *self {
            Self::Queued(_) => false,
            Self::WaitingForEstimatedVBlank {
                token,
                force: previous,
            }
            | Self::WaitingForEstimatedVBlankAndQueued {
                estimated_vblank: token,
                force: previous,
                ..
            } => {
                *self = Self::WaitingForEstimatedVBlank {
                    token,
                    force: previous || force,
                };
                true
            }
            _ => unreachable!("empty render in invalid queue state"),
        }
    }

    /// Return the live force intent when the estimated deadline expires. An
    /// already queued render supplies the re-evaluation without another timer.
    pub(super) fn estimated_vblank(&mut self) -> Option<bool> {
        match std::mem::replace(self, Self::Idle) {
            Self::WaitingForEstimatedVBlank { force, .. } => Some(force),
            Self::WaitingForEstimatedVBlankAndQueued { queued_render, .. } => {
                *self = Self::Queued(queued_render);
                None
            }
            _ => unreachable!("estimated timer fired in invalid queue state"),
        }
    }

    /// A real flip replaces any estimated deadline, never another real flip.
    pub(super) fn submitted(&mut self) -> Option<T> {
        match std::mem::replace(
            self,
            Self::WaitingForVBlank {
                redraw_needed: false,
            },
        ) {
            Self::Queued(_) => None,
            Self::WaitingForEstimatedVBlank { token, .. }
            | Self::WaitingForEstimatedVBlankAndQueued {
                estimated_vblank: token,
                ..
            } => Some(token),
            _ => unreachable!("frame submitted in invalid queue state"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum CommitResult {
        Presented,
        Empty,
        Failed,
    }

    /// Model only KMS ownership; use the production queue transitions and event
    /// tracker so these tests exercise both orders of timer/DRM dispatch.
    struct Output {
        queue: QueueState<u32>,
        pending: Option<u32>,
        superseded: SupersededFlips,
    }

    impl Output {
        fn new() -> Self {
            Self {
                queue: QueueState::Idle,
                pending: None,
                superseded: SupersededFlips::default(),
            }
        }

        fn submit(&mut self, id: u32) {
            assert!(self.pending.is_none());
            self.queue.queue_render(id, false);
            self.queue.submitted();
            self.pending = Some(id);
            self.superseded.submitted(true);
        }

        fn blocking_commit(&mut self, result: CommitResult) {
            // Mirrors commit_frame: empty/error leaves its pending owner alive.
            if matches!(result, CommitResult::Presented) {
                self.superseded.committed(self.pending.take().is_some());
            }
        }

        fn vblank(&mut self) -> Option<u32> {
            if self.superseded.may_complete(self.pending.is_some())
                && let Some(id) = self.pending.take()
            {
                self.queue.completed(false);
                return Some(id);
            }
            self.queue
                .recover_missing_completion(self.pending.is_some());
            None
        }
    }

    #[test]
    fn forced_commit_consumes_old_event_and_recovers_wait() {
        let mut output = Output::new();
        output.submit(1);
        output.blocking_commit(CommitResult::Presented);
        assert_eq!(output.vblank(), None);
        assert_eq!(output.queue, QueueState::Idle);
        output.submit(2);
        assert_eq!(output.vblank(), Some(2));
    }

    #[test]
    fn forced_commit_old_event_cannot_retire_new_pending_frame() {
        let mut output = Output::new();
        output.submit(1);
        output.blocking_commit(CommitResult::Presented);
        // A lifecycle transition may schedule a new frame before the old event.
        output.queue = QueueState::Idle;
        output.submit(2);
        assert_eq!(output.vblank(), None);
        assert!(output.queue.expects_vblank());
        assert_eq!(output.pending, Some(2));
        assert_eq!(output.vblank(), Some(2));
    }

    #[test]
    fn event_ignored_during_suspend_is_not_counted_again_by_forced_commit() {
        let mut output = Output::new();
        output.submit(1);
        // The worker temporarily has no compositor; the underlying manager
        // still holds its pending frame until the subsequent blocking commit.
        assert!(!output.superseded.consume());
        output.queue = QueueState::Idle;
        output.blocking_commit(CommitResult::Presented);
        output.submit(2);
        assert_eq!(output.vblank(), Some(2));
    }

    #[test]
    fn dropped_compositor_event_cannot_retire_first_frame_after_resume() {
        let mut output = Output::new();
        output.submit(1);
        // Suspension removes the old compositor, but does not retract its
        // queued kernel event. Resume installs an independent pending owner.
        output.superseded.committed(true);
        output.pending = None;
        output.queue = QueueState::Idle;
        output.submit(2);
        assert_eq!(output.vblank(), None);
        assert_eq!(output.pending, Some(2));
        assert_eq!(output.vblank(), Some(2));
    }

    #[test]
    fn repeated_forced_commits_only_invalidate_actual_pending_owners() {
        let mut output = Output::new();
        output.submit(1);
        output.blocking_commit(CommitResult::Presented);
        output.blocking_commit(CommitResult::Presented);
        output.queue = QueueState::Idle;
        output.submit(2);
        output.blocking_commit(CommitResult::Presented);
        output.queue = QueueState::Idle;
        output.submit(3);
        assert_eq!(output.vblank(), None);
        assert_eq!(output.vblank(), None);
        assert_eq!(output.vblank(), Some(3));
    }

    #[test]
    fn empty_and_failed_forced_commits_preserve_pending_frame() {
        for result in [CommitResult::Empty, CommitResult::Failed] {
            let mut output = Output::new();
            output.submit(1);
            output.blocking_commit(result);
            assert_eq!(output.vblank(), Some(1));
            output.submit(2);
            assert_eq!(output.vblank(), Some(2));
        }
    }

    #[test]
    fn failed_dpms_clear_does_not_swallow_real_completion_before_queued_redraw() {
        let mut output = Output::new();
        output.submit(1);
        output.blocking_commit(CommitResult::Failed);
        // DPMS reset the scheduler, but clear failed and KMS still owns frame 1.
        output.queue = QueueState::Idle;
        output.queue.queue_render(2, false);
        assert_eq!(output.vblank(), Some(1));
        assert_eq!(output.queue, QueueState::Queued(2));
        assert!(output.pending.is_none());
        output.queue.submitted();
        output.pending = Some(2);
        output.superseded.submitted(true);
        assert_eq!(output.vblank(), Some(2));
    }

    #[test]
    fn real_completion_preserves_estimated_deadline_and_queued_render() {
        let mut output = Output::new();
        output.submit(1);
        output.queue = QueueState::WaitingForEstimatedVBlank {
            token: 2,
            force: true,
        };
        output.queue.queue_render(3, false);
        assert_eq!(output.vblank(), Some(1));
        assert_eq!(output.queue.estimated_vblank(), None);
        assert_eq!(output.queue, QueueState::Queued(3));
    }

    #[test]
    fn real_successor_inherits_repaint_intent_without_concurrent_render_timer() {
        let mut queue = QueueState::WaitingForEstimatedVBlank {
            token: 1,
            force: false,
        };
        queue.queue_render(2, false);
        assert_eq!(queue.completed(true), (true, [Some(1), Some(2)]));
        assert_eq!(
            queue,
            QueueState::WaitingForVBlank {
                redraw_needed: true
            }
        );
        assert_eq!(queue.completed(false), (true, [None, None]));
        assert_eq!(queue, QueueState::Idle);
    }

    #[test]
    fn stale_completion_preserves_all_queued_timer_states() {
        for queue in [
            QueueState::Idle,
            QueueState::Queued(7),
            QueueState::WaitingForEstimatedVBlank {
                token: 8,
                force: true,
            },
            QueueState::WaitingForEstimatedVBlankAndQueued {
                estimated_vblank: 8,
                queued_render: 7,
                force: true,
            },
        ] {
            let mut output = Output::new();
            // Force the old flip while the worker has a pending owner, then
            // dispatch the event after the lifecycle has installed new timers.
            output.submit(1);
            output.blocking_commit(CommitResult::Presented);
            let before = format!("{queue:?}");
            output.queue = queue;
            assert_eq!(output.vblank(), None);
            assert_eq!(format!("{:?}", output.queue), before);
            assert!(!output.superseded.consume());
        }
    }

    #[test]
    fn missing_owner_recovers_but_existing_owner_and_timers_do_not() {
        let mut waiting = QueueState::<u32>::WaitingForVBlank {
            redraw_needed: true,
        };
        assert!(!waiting.recover_missing_completion(true));
        assert!(waiting.recover_missing_completion(false));
        assert_eq!(waiting, QueueState::Idle);
        let mut queued = QueueState::Queued(7);
        assert!(!queued.recover_missing_completion(false));
        assert_eq!(queued, QueueState::Queued(7));
    }

    #[test]
    fn untracked_stale_event_does_not_remove_resume_render_or_estimated_timer() {
        let mut output = Output::new();
        output.queue = QueueState::WaitingForEstimatedVBlank {
            token: 1,
            force: true,
        };
        output.queue.queue_render(2, false);
        assert_eq!(output.vblank(), None);
        assert_eq!(output.queue.estimated_vblank(), None);
        assert_eq!(output.queue, QueueState::Queued(2));
        assert_eq!(output.queue.submitted(), None);
    }

    #[test]
    fn reused_estimated_timer_retains_upgraded_force_and_original_deadline() {
        let mut queue = QueueState::WaitingForEstimatedVBlank {
            token: 1,
            force: false,
        };
        queue.queue_render(2, false);
        assert!(queue.reuse_estimated_vblank(true));
        assert_eq!(
            queue,
            QueueState::WaitingForEstimatedVBlank {
                token: 1,
                force: true
            }
        );
        queue.queue_render(3, false);
        assert!(queue.reuse_estimated_vblank(false));
        assert_eq!(queue.estimated_vblank(), Some(true));
        assert_eq!(queue, QueueState::Idle);
    }

    #[test]
    fn estimated_timer_before_render_preserves_already_queued_recheck() {
        let mut queue = QueueState::WaitingForEstimatedVBlank {
            token: 1,
            force: true,
        };
        queue.queue_render(2, false);
        assert_eq!(queue.estimated_vblank(), None);
        assert_eq!(queue, QueueState::Queued(2));
        // A genuinely empty recheck may now schedule an unforced next deadline.
        assert!(!queue.reuse_estimated_vblank(false));
        queue = QueueState::WaitingForEstimatedVBlank {
            token: 3,
            force: false,
        };
        assert_eq!(queue.estimated_vblank(), Some(false));
    }

    #[test]
    fn successful_render_cancels_estimated_timer_and_retry_replaces_only_render() {
        let mut queue = QueueState::WaitingForEstimatedVBlank {
            token: 1,
            force: true,
        };
        queue.queue_render(2, false);
        assert_eq!(queue.queue_render(3, true), Some(2));
        assert_eq!(queue.submitted(), Some(1));
        assert_eq!(
            queue,
            QueueState::WaitingForVBlank {
                redraw_needed: false
            }
        );
    }
}
