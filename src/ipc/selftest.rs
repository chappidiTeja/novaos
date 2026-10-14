//! Boot-time tests for the IPC primitives, run between kernel threads.
//! Panics on any failure.

use super::channel::{channel, Receiver, Sender};
use super::console;
use super::mutex::{Mutex, Semaphore};
use super::pipe::{pipe, BrokenPipe, PipeReader, PipeWriter, PIPE_CAPACITY};
use super::waitq::WaitQueue;
use crate::sync::SpinLock;
use crate::{println, sched, timer};
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub fn self_test() {
    mutex_contention();
    bounded_buffer();
    timeouts();
    pipe_between_threads();
    pipe_close_wakes_sleepers();
    channel_fan_in();
    channel_close_wakes_sleepers();
    console_wakeup();

    // Everything above ran on threads that have to be reaped before the thread
    // count means anything.
    sched::sleep_ticks(10);
    println!("[ok] ipc self-test");
}

/// Four threads bump a shared counter, yielding while holding the lock. Without
/// mutual exclusion updates would be lost.
fn mutex_contention() {
    static COUNTER: Mutex<u64> = Mutex::new(0);
    static FINISHED: Semaphore = Semaphore::new(0);

    fn worker() {
        for i in 0..2000 {
            let mut guard = COUNTER.lock();
            let value = *guard;
            if i % 100 == 0 {
                sched::yield_now(); // invite other threads in while the lock is held
            }
            *guard = value + 1;
        }
        FINISHED.up();
    }

    for _ in 0..4 {
        sched::spawn("mutex-worker", worker);
    }
    for _ in 0..4 {
        assert!(FINISHED.down_timeout(500), "a mutex worker never finished");
    }
    assert_eq!(*COUNTER.lock(), 8000, "mutual exclusion failed");
    println!("[ok] mutex: 4 threads, 8000 increments, none lost");
}

/// Classic producer/consumer: one producer, two consumers, a buffer of 8.
fn bounded_buffer() {
    static QUEUE: Mutex<VecDeque<u32>> = Mutex::new(VecDeque::new());
    static EMPTY_SLOTS: Semaphore = Semaphore::new(8);
    static FILLED_SLOTS: Semaphore = Semaphore::new(0);
    static FINISHED: Semaphore = Semaphore::new(0);
    static SUM: AtomicU64 = AtomicU64::new(0);
    static MAX_LEN: AtomicU64 = AtomicU64::new(0);

    fn producer() {
        for i in 1..=500u32 {
            EMPTY_SLOTS.down();
            let mut queue = QUEUE.lock();
            queue.push_back(i);
            MAX_LEN.fetch_max(queue.len() as u64, Ordering::Relaxed);
            drop(queue);
            FILLED_SLOTS.up();
        }
        FINISHED.up();
    }

    fn consumer() {
        for _ in 0..250 {
            FILLED_SLOTS.down();
            let value = QUEUE.lock().pop_front().expect("semaphore promised an item");
            EMPTY_SLOTS.up();
            SUM.fetch_add(value as u64, Ordering::Relaxed);
        }
        FINISHED.up();
    }

    sched::spawn("producer", producer);
    sched::spawn("consumer-a", consumer);
    sched::spawn("consumer-b", consumer);
    for _ in 0..3 {
        assert!(FINISHED.down_timeout(500), "producer/consumer stalled");
    }
    assert_eq!(SUM.load(Ordering::Relaxed), 500 * 501 / 2, "items lost or duplicated");
    assert!(MAX_LEN.load(Ordering::Relaxed) <= 8, "buffer bound exceeded");
    *QUEUE.lock() = VecDeque::new(); // release the buffer's memory
    println!("[ok] semaphore: 500 items through a buffer of 8, none lost");
}

fn timeouts() {
    static QUEUE: WaitQueue = WaitQueue::new();
    static FLAG: AtomicBool = AtomicBool::new(false);

    // Nothing ever signals: must give up at the deadline.
    let start = timer::ticks();
    let met = QUEUE.wait_until_deadline(start + 10, || FLAG.load(Ordering::Acquire));
    let waited = timer::ticks() - start;
    assert!(!met, "wait reported success without a signal");
    assert!((10..30).contains(&waited), "timeout took {} ticks", waited);

    // Signalled after 5 ticks: must return early, long before the deadline.
    fn signaller() {
        sched::sleep_ticks(5);
        FLAG.store(true, Ordering::Release);
        QUEUE.wake_all();
    }
    sched::spawn("signaller", signaller);
    let start = timer::ticks();
    let met = QUEUE.wait_until_deadline(start + 100, || FLAG.load(Ordering::Acquire));
    let waited = timer::ticks() - start;
    assert!(met, "wait missed the wake-up");
    assert!(waited < 30, "woken after {} ticks, expected about 5", waited);

    // A semaphore that is never raised times out too.
    static NEVER: Semaphore = Semaphore::new(0);
    assert!(!NEVER.down_timeout(5));
    println!("[ok] timeouts: expired on schedule, woke early when signalled");
}

fn pattern(index: usize) -> u8 {
    (index % 251) as u8
}

/// 20 000 bytes through a 4 KiB pipe force the writer and reader to take turns.
fn pipe_between_threads() {
    static WRITER: SpinLock<Option<PipeWriter>> = SpinLock::new(None);

    fn writer() {
        let writer = WRITER.lock().take().expect("writer end missing");
        let mut sent = 0;
        while sent < 20_000 {
            let n = 300.min(20_000 - sent);
            let chunk: alloc::vec::Vec<u8> = (sent..sent + n).map(pattern).collect();
            assert_eq!(writer.write(&chunk), Ok(n));
            sent += n;
        }
        // Dropping the write end is what gives the reader its end-of-file.
    }

    let (reader, write_end) = pipe();
    *WRITER.lock() = Some(write_end);
    sched::spawn("pipe-writer", writer);

    let mut buf = [0u8; 700];
    let mut received = 0;
    loop {
        let n = reader.read(&mut buf);
        if n == 0 {
            break;
        }
        for (i, &byte) in buf[..n].iter().enumerate() {
            assert_eq!(byte, pattern(received + i), "pipe corrupted data at {}", received + i);
        }
        received += n;
    }
    assert_eq!(received, 20_000, "pipe lost data");
    drop(reader);

    // With the reader gone, writing must fail instead of blocking forever.
    let (reader, writer) = pipe();
    drop(reader);
    assert_eq!(writer.write(b"nobody listens"), Err(BrokenPipe));
    println!("[ok] pipe: 20000 bytes in order, EOF on close, error when unread");
}

/// Closing one end of a pipe must wake a thread that is *already asleep* on the
/// other end. (The transfer test above never has anyone asleep at close time.)
fn pipe_close_wakes_sleepers() {
    static READER: SpinLock<Option<PipeReader>> = SpinLock::new(None);
    static WRITER: SpinLock<Option<PipeWriter>> = SpinLock::new(None);
    static DONE: Semaphore = Semaphore::new(0);
    static RESULT: AtomicUsize = AtomicUsize::new(usize::MAX);

    fn reader() {
        let reader = READER.lock().take().expect("reader end missing");
        let mut buf = [0u8; 16];
        let first = reader.read(&mut buf); // sleeps until "abc" arrives
        let second = reader.read(&mut buf); // sleeps again, then must see end of file
        RESULT.store(first * 100 + second, Ordering::Release);
        DONE.up();
    }

    let (read_end, write_end) = pipe();
    *READER.lock() = Some(read_end);
    sched::spawn("eof-reader", reader);
    sched::sleep_ticks(5); // the reader is asleep in its first read
    assert_eq!(write_end.write(b"abc"), Ok(3));
    sched::sleep_ticks(5); // and asleep again, waiting for more
    drop(write_end);
    assert!(DONE.down_timeout(50), "reader slept through the last writer closing");
    assert_eq!(RESULT.load(Ordering::Acquire), 300, "reader saw the wrong data or no EOF");

    fn writer() {
        let writer = WRITER.lock().take().expect("writer end missing");
        let result = writer.write(&[7u8; 6000]); // 4096 fit, then it must sleep
        RESULT.store(result.unwrap_or(1), Ordering::Release);
        DONE.up();
    }

    let (read_end, write_end) = pipe();
    *WRITER.lock() = Some(write_end);
    sched::spawn("full-writer", writer);
    sched::sleep_ticks(5); // the writer filled the pipe and is asleep
    drop(read_end);
    assert!(DONE.down_timeout(50), "writer slept through the last reader closing");
    assert_eq!(
        RESULT.load(Ordering::Acquire),
        PIPE_CAPACITY,
        "writer should report the bytes that did fit"
    );
    println!("[ok] pipe: closing an end wakes a sleeping reader or writer");
}

/// Same for channels: the last sender dropping wakes a sleeping receiver, and
/// the receiver dropping wakes a sender sleeping on a full channel.
fn channel_close_wakes_sleepers() {
    static RECEIVER: SpinLock<Option<Receiver<u32>>> = SpinLock::new(None);
    static SENDER: SpinLock<Option<Sender<u32>>> = SpinLock::new(None);
    static DONE: Semaphore = Semaphore::new(0);
    static RESULT: AtomicUsize = AtomicUsize::new(usize::MAX);

    fn recv_when_empty() {
        let receiver = RECEIVER.lock().take().expect("receiver missing");
        RESULT.store(receiver.recv().map_or(0, |_| 1), Ordering::Release);
        DONE.up();
    }

    let (sender, receiver_end) = channel::<u32>(1);
    *RECEIVER.lock() = Some(receiver_end);
    sched::spawn("idle-receiver", recv_when_empty);
    sched::sleep_ticks(5); // asleep on an empty channel
    drop(sender);
    assert!(DONE.down_timeout(50), "receiver slept through the last sender dropping");
    assert_eq!(RESULT.load(Ordering::Acquire), 0, "receiver should see the stream end");

    fn send_when_full() {
        let sender = SENDER.lock().take().expect("sender missing");
        sender.send(1).expect("first message fits");
        RESULT.store(sender.send(2).map_or(0, |_| 1), Ordering::Release); // channel full: sleeps
        DONE.up();
    }

    let (sender_end, receiver_end) = channel::<u32>(1);
    *SENDER.lock() = Some(sender_end);
    sched::spawn("blocked-sender", send_when_full);
    sched::sleep_ticks(5); // the sender is asleep on a full channel
    drop(receiver_end);
    assert!(DONE.down_timeout(50), "sender slept through the receiver dropping");
    assert_eq!(RESULT.load(Ordering::Acquire), 0, "send should fail once the receiver is gone");
    println!("[ok] channel: closing an end wakes a sleeping sender or receiver");
}

/// Three producers feed one consumer through a channel with room for 4.
fn channel_fan_in() {
    static SENDERS: [SpinLock<Option<Sender<u32>>>; 3] = [
        SpinLock::new(None),
        SpinLock::new(None),
        SpinLock::new(None),
    ];

    fn producer<const N: usize>() {
        let sender = SENDERS[N].lock().take().expect("sender missing");
        for i in 0..100 {
            sender.send(1000 * (N as u32 + 1) + i).expect("receiver vanished");
        }
    }

    let (sender, receiver) = channel::<u32>(4);
    *SENDERS[0].lock() = Some(sender.clone());
    *SENDERS[1].lock() = Some(sender.clone());
    *SENDERS[2].lock() = Some(sender);
    sched::spawn("producer0", producer::<0>);
    sched::spawn("producer1", producer::<1>);
    sched::spawn("producer2", producer::<2>);

    let mut count = 0;
    let mut sum = 0u64;
    let mut last_per_producer = [None::<u32>; 3];
    while let Some(message) = receiver.recv() {
        let producer = (message / 1000 - 1) as usize;
        if let Some(previous) = last_per_producer[producer] {
            assert!(message > previous, "messages from one sender arrived out of order");
        }
        last_per_producer[producer] = Some(message);
        count += 1;
        sum += message as u64;
    }
    // None means: empty, and every sender dropped.
    assert_eq!(count, 300, "messages lost");
    assert_eq!(sum, (1000 + 2000 + 3000) * 100 + 3 * (99 * 100 / 2), "messages corrupted");

    let (sender, receiver) = channel::<u32>(1);
    drop(receiver);
    assert_eq!(sender.send(7), Err(7), "send to a dead receiver must give the message back");
    println!("[ok] channel: 300 messages from 3 senders, ordered per sender");
}

/// A reader sleeping on console input is woken when bytes arrive.
fn console_wakeup() {
    fn typist() {
        sched::sleep_ticks(5);
        for &byte in b"xyz" {
            console::push_input(byte);
        }
    }

    sched::spawn("typist", typist);
    let mut got = alloc::vec::Vec::new();
    let mut buf = [0u8; 8];
    let start = timer::ticks();
    while got.len() < 3 {
        let n = console::read(&mut buf);
        got.extend_from_slice(&buf[..n]);
        assert!(timer::ticks() - start < 100, "console read never returned");
    }
    assert_eq!(got, b"xyz");
    assert!(timer::ticks() - start >= 5, "read returned before any input existed");
    println!("[ok] console: blocked reader woke when input arrived");
}
