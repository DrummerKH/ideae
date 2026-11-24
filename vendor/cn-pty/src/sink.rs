//! Where a pty's output goes. A terminal's engine is one [`Output`]; a
//! [`Sink`] with its [`Detached`] end is another, for a process that relays
//! the bytes elsewhere instead of drawing them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The output a source may queue before [`Output::wait_for_room`] holds
/// it back.
const QUEUE_BYTES: usize = 1 << 20;

/// What a pty writes its program's output to.
pub trait Output: Clone + Send + Sync + 'static {
    /// Deliver output; false once the receiver is closing. Never blocks.
    fn write(&self, bytes: &[u8]) -> bool;
    /// Blocks while too much output waits, which holds the program back
    /// through the pty; false once the receiver is closing.
    fn wait_for_room(&self) -> bool;
    /// The program can take input again after a short write.
    fn writable(&self);
    /// The source reached end of file.
    fn finished(&self);
}

#[derive(Default)]
struct Mailbox {
    reads: VecDeque<Vec<u8>>,
    read_bytes: usize,
    writable: bool,
    finished: bool,
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    room: Condvar,
    closing: AtomicBool,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An [`Output`] whose bytes a [`Detached`] takes.
#[derive(Clone, Default)]
pub struct Sink {
    shared: Arc<Shared>,
}

/// The receiving end of a [`Sink`].
pub struct Detached {
    shared: Arc<Shared>,
}

/// What a [`Detached`] took since it was last asked.
#[derive(Debug, Default)]
pub struct SinkBatch {
    /// Output in the order it came.
    pub output: Vec<Vec<u8>>,
    /// The source can take input again after a short write.
    pub writable: bool,
    /// The source reached end of file.
    pub finished: bool,
}

impl Sink {
    /// A sink and its receiving end.
    pub fn detached() -> (Sink, Detached) {
        let shared = Arc::new(Shared::default());
        (Sink { shared: shared.clone() }, Detached { shared })
    }
}

impl Output for Sink {
    fn write(&self, bytes: &[u8]) -> bool {
        let mut mail = self.shared.lock();
        if self.shared.closing.load(Ordering::Acquire) {
            return false;
        }
        mail.reads.push_back(bytes.to_vec());
        mail.read_bytes += bytes.len();
        self.shared.wake.notify_one();
        true
    }

    fn wait_for_room(&self) -> bool {
        let mut mail = self.shared.lock();
        while mail.read_bytes >= QUEUE_BYTES && !self.shared.closing.load(Ordering::Acquire) {
            mail = self.shared.room.wait(mail).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        !self.shared.closing.load(Ordering::Acquire)
    }

    fn writable(&self) {
        self.shared.lock().writable = true;
        self.shared.wake.notify_one();
    }

    fn finished(&self) {
        self.shared.lock().finished = true;
        self.shared.wake.notify_one();
    }
}

impl Detached {
    /// Waits up to `timeout` for anything new, then takes it all.
    pub fn next(&self, timeout: Duration) -> SinkBatch {
        let mut mail = self.shared.lock();
        if mail.reads.is_empty() && !mail.writable && !mail.finished {
            mail = self.shared.wake.wait_timeout(mail, timeout).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        let batch = SinkBatch {
            output: mail.reads.drain(..).collect(),
            writable: std::mem::take(&mut mail.writable),
            finished: mail.finished,
        };
        mail.read_bytes = 0;
        self.shared.room.notify_all();
        batch
    }

    /// Stops the source's reader.
    pub fn close(&self) {
        let _mail = self.shared.lock();
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        self.shared.room.notify_all();
    }
}
