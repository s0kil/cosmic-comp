// SPDX-License-Identifier: GPL-3.0-only

use smithay::reexports::wayland_server::DisplayHandle;

/// Coalesces latency-sensitive input and frame events into one early flush.
///
/// The normal end-of-iteration flush is still required: housekeeping can queue
/// more events, and the socket may temporarily be unable to accept all bytes.
#[derive(Debug, Default)]
pub(crate) struct InputEventFlush {
    pending: bool,
}

impl InputEventFlush {
    pub(crate) fn request(&mut self) {
        self.pending = true;
    }

    pub(crate) fn flush(&mut self, display: &mut DisplayHandle) {
        if std::mem::take(&mut self.pending) {
            // Match the normal event-loop flush's error handling. Client
            // disconnections must not terminate the compositor's event path.
            let _ = display.flush_clients();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;
    use std::{
        io::{ErrorKind, Read, Write},
        os::unix::net::UnixStream,
        sync::Arc,
    };

    fn connection() -> (Display<()>, UnixStream) {
        let display = Display::new().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        display
            .handle()
            .insert_client(server, Arc::new(()))
            .unwrap();
        client.set_nonblocking(true).unwrap();
        (display, client)
    }

    // A real wl_display.sync request queues callback.done and delete_id events
    // without requiring a compositor, GPU, input injection or a visible window.
    fn queue_callback(display: &mut Display<()>, client: &mut UnixStream, id: u32) {
        let request = [1_u32, 12 << 16, id];
        for word in request {
            client.write_all(&word.to_ne_bytes()).unwrap();
        }
        display.dispatch_clients(&mut ()).unwrap();
    }

    fn assert_no_events(client: &mut UnixStream) {
        let mut byte = [0];
        assert_eq!(
            client.read(&mut byte).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    }

    fn read_callbacks(client: &mut UnixStream, expected: &[u32]) {
        for id in expected {
            let mut response = [0; 24];
            client.read_exact(&mut response).unwrap();
            let words: Vec<_> = response
                .chunks_exact(4)
                .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                .collect();
            assert_eq!(&words[..2], &[*id, 12 << 16]); // callback.done
            assert_eq!(&words[3..], &[1, (12 << 16) | 1, *id]); // display.delete_id
        }
        assert_no_events(client);
    }

    #[test]
    fn unrequested_iterations_leave_events_for_the_regular_flush() {
        let (mut display, mut client) = connection();
        let mut input = InputEventFlush::default();
        queue_callback(&mut display, &mut client, 2);

        input.flush(&mut display.handle());
        assert_no_events(&mut client);

        display.flush_clients().unwrap();
        read_callbacks(&mut client, &[2]);
    }

    #[test]
    fn requested_batch_is_delivered_before_later_housekeeping_events() {
        let (mut display, mut client) = connection();
        let mut input = InputEventFlush::default();
        for id in [2, 3] {
            queue_callback(&mut display, &mut client, id);
            input.request();
        }
        assert_no_events(&mut client);

        input.flush(&mut display.handle());
        read_callbacks(&mut client, &[2, 3]);

        // Housekeeping after the early flush still needs the regular flush.
        queue_callback(&mut display, &mut client, 4);
        input.flush(&mut display.handle());
        assert_no_events(&mut client);
        display.flush_clients().unwrap();
        read_callbacks(&mut client, &[4]);

        // Another dispatch batch must be able to request an early flush again.
        queue_callback(&mut display, &mut client, 5);
        input.request();
        input.flush(&mut display.handle());
        read_callbacks(&mut client, &[5]);
    }
}
