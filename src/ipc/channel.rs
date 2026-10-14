//! Bounded multi-producer channels for passing typed messages between kernel
//! threads. `send` sleeps while the channel is full; `recv` sleeps while it is
//! empty. Dropping the last `Sender` ends the stream; dropping the `Receiver`
//! makes `send` fail and hand the message back.

use super::waitq::WaitQueue;
use crate::sync::SpinLock;
use alloc::collections::VecDeque;
use alloc::sync::Arc;

struct State<T> {
    items: VecDeque<T>,
    capacity: usize,
    senders: usize,
    receiver_alive: bool,
}

struct Shared<T> {
    state: SpinLock<State<T>>,
    not_empty: WaitQueue,
    not_full: WaitQueue,
}

pub struct Sender<T>(Arc<Shared<T>>);
pub struct Receiver<T>(Arc<Shared<T>>);

pub fn channel<T: Send>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "a channel needs room for at least one message");
    let shared = Arc::new(Shared {
        state: SpinLock::new(State {
            items: VecDeque::with_capacity(capacity),
            capacity,
            senders: 1,
            receiver_alive: true,
        }),
        not_empty: WaitQueue::new(),
        not_full: WaitQueue::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl<T: Send> Sender<T> {
    /// Queue `message`, sleeping while the channel is full. Gives the message
    /// back if the receiver is gone.
    pub fn send(&self, message: T) -> Result<(), T> {
        let shared = &*self.0;
        let mut slot = Some(message);
        let mut delivered = false;
        shared.not_full.wait_until(|| {
            let mut state = shared.state.lock();
            if !state.receiver_alive {
                return true;
            }
            if state.items.len() >= state.capacity {
                return false;
            }
            state.items.push_back(slot.take().expect("message already sent"));
            delivered = true;
            true
        });
        if delivered {
            shared.not_empty.wake_one();
            Ok(())
        } else {
            Err(slot.take().expect("message vanished"))
        }
    }
}

impl<T: Send> Receiver<T> {
    /// Next message, sleeping while the channel is empty. None once it is empty
    /// and every sender has been dropped.
    pub fn recv(&self) -> Option<T> {
        let shared = &*self.0;
        let mut message = None;
        shared.not_empty.wait_until(|| {
            let mut state = shared.state.lock();
            match state.items.pop_front() {
                Some(item) => {
                    message = Some(item);
                    true
                }
                None => state.senders == 0,
            }
        });
        if message.is_some() {
            shared.not_full.wake_one();
        }
        message
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.state.lock().senders += 1;
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let last = {
            let mut state = self.0.state.lock();
            state.senders -= 1;
            state.senders == 0
        };
        if last {
            self.0.not_empty.wake_all();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.0.state.lock().receiver_alive = false;
        self.0.not_full.wake_all();
    }
}
