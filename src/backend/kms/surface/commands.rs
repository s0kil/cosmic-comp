// SPDX-License-Identifier: GPL-3.0-only

use super::ThreadCommand;
use calloop::channel::Sender;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::SendError,
};

/// Avoid enqueueing another redraw while one is still waiting to be dispatched.
/// Other commands form batching boundaries so a redraw after a power/configuration
/// change cannot be swallowed by a request queued before that change.
#[derive(Debug)]
pub(super) struct CommandSender {
    sender: Sender<ThreadCommand>,
    redraw_pending: Arc<AtomicBool>,
}

impl CommandSender {
    pub(super) fn new(sender: Sender<ThreadCommand>, redraw_pending: Arc<AtomicBool>) -> Self {
        Self {
            sender,
            redraw_pending,
        }
    }

    pub(super) fn send(&self, command: ThreadCommand) -> Result<(), Box<SendError<ThreadCommand>>> {
        let redraw = matches!(command, ThreadCommand::ScheduleRender);
        if redraw {
            if self.redraw_pending.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
        } else {
            self.redraw_pending.store(false, Ordering::Release);
        }

        let result = self.sender.send(command);
        if result.is_err() && redraw {
            self.redraw_pending.store(false, Ordering::Release);
        }
        result.map_err(Box::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calloop::channel::{Channel, channel};
    use cosmic_comp_config::output::comp::AdaptiveSync;
    use std::sync::mpsc::TryRecvError;

    fn setup() -> (CommandSender, Channel<ThreadCommand>, Arc<AtomicBool>) {
        let (sender, receiver) = channel();
        let pending = Arc::new(AtomicBool::new(false));
        (
            CommandSender::new(sender, pending.clone()),
            receiver,
            pending,
        )
    }

    #[test]
    fn redraw_bursts_queue_one_notification_and_rearm_on_dispatch() {
        let (sender, receiver, pending) = setup();
        for _ in 0..1000 {
            sender.send(ThreadCommand::ScheduleRender).unwrap();
        }
        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::ScheduleRender)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        // The worker clears this before acting on the redraw, so requests that
        // arrive during rendering can still schedule the next frame.
        pending.swap(false, Ordering::AcqRel);
        sender.send(ThreadCommand::ScheduleRender).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::ScheduleRender)
        ));
    }

    #[test]
    fn redraws_after_control_commands_keep_their_fifo_position() {
        let (sender, receiver, pending) = setup();
        sender.send(ThreadCommand::ScheduleRender).unwrap();
        sender.send(ThreadCommand::DpmsOff).unwrap();
        sender
            .send(ThreadCommand::UseAdaptiveSync(AdaptiveSync::Enabled))
            .unwrap();
        sender.send(ThreadCommand::ScheduleRender).unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::ScheduleRender)
        ));
        // An older notification can clear the flag for a newer batch. This may
        // admit one more notification but must never lose the newer redraw.
        pending.swap(false, Ordering::AcqRel);
        sender.send(ThreadCommand::ScheduleRender).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(ThreadCommand::DpmsOff)));
        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::UseAdaptiveSync(AdaptiveSync::Enabled))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::ScheduleRender)
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(ThreadCommand::ScheduleRender)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn disconnected_receiver_does_not_leave_a_pending_redraw() {
        let (sender, receiver, _) = setup();
        drop(receiver);
        assert!(sender.send(ThreadCommand::ScheduleRender).is_err());
        assert!(sender.send(ThreadCommand::ScheduleRender).is_err());
    }
}
