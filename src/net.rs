//! Networking in the kernel: owns the network card and the protocol stack (the
//! `novanet` crate), runs the network thread, and offers UDP sockets, ping and
//! name lookup to the system-call layer.
//!
//! Everything that touches the stack goes through `with_stack`, which first
//! drains the card, then runs the caller's closure, then sends whatever the
//! stack queued. The network thread calls it once per timer tick so DHCP, ARP
//! retries and lease renewals happen even when no program is asking.

use crate::ipc::waitq::WaitQueue;
use crate::pci;
use crate::sync::SpinLock;
use crate::virtio_net::{self, VirtioNet};
use crate::{println, sched, timer};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use novanet::dns;
use novanet::wire::Ip;
use novanet::tcp::{Tcp, TcpError};
use novanet::{Config, NetError, Stack};

pub const EIO: i64 = -5;
pub const EAGAIN: i64 = -11;
pub const EMSGSIZE: i64 = -90;
pub const EADDRINUSE: i64 = -98;
pub const EADDRNOTAVAIL: i64 = -99;
pub const ENETDOWN: i64 = -100;
pub const ENETUNREACH: i64 = -101;
pub const ETIMEDOUT: i64 = -110;
const ENOENT: i64 = -2;
const EINVAL: i64 = -22;
const EDESTADDRREQ: i64 = -89;

struct NetState {
    dev: VirtioNet,
    stack: Stack,
    reported: bool,
}

static NET: SpinLock<Option<NetState>> = SpinLock::new(None);
static ACTIVITY: AtomicU64 = AtomicU64::new(0);
static EVENT: WaitQueue = WaitQueue::new();
static PING_SEQ: AtomicU16 = AtomicU16::new(1);
static UP: AtomicBool = AtomicBool::new(false);

pub fn available() -> bool {
    UP.load(Ordering::Relaxed)
}

/// Milliseconds on the best clock we have.
pub fn now_ms() -> u64 {
    if timer::tsc_khz() != 0 {
        timer::now_us() / 1000
    } else {
        timer::ticks() * 1000 / crate::TIMER_HZ as u64
    }
}

pub fn map_error(e: NetError) -> i64 {
    match e {
        NetError::NoConfig => ENETDOWN,
        NetError::NoRoute => ENETUNREACH,
        NetError::PortInUse => EADDRINUSE,
        NetError::TooBig => EMSGSIZE,
        NetError::NoPorts => EADDRNOTAVAIL,
        NetError::NotBound => EINVAL,
    }
}

fn fmt_ip(ip: Ip) -> String {
    alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// Find the card and start the network thread. False if there is none.
pub fn init() -> bool {
    let devices = pci::scan();
    match virtio_net::probe(&devices) {
        None => {
            println!("[net] no network card");
            false
        }
        Some(Err(reason)) => {
            println!("[net] network card unusable: {}", reason);
            false
        }
        Some(Ok(dev)) => {
            let mac = dev.mac();
            let seed = (unsafe { core::arch::x86_64::_rdtsc() } & 0xFFFF_FFFF) as u32;
            let mut stack = Stack::new(mac, seed);
            stack.dhcp_start(now_ms());
            println!(
                "[ok] net: virtio-net {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
                if dev.link_up() { "up" } else { "down" }
            );
            let line = dev.irq_line();
            NIC_IO.store(dev.io_base(), Ordering::Relaxed);
            *NET.lock() = Some(NetState { dev, stack, reported: false });
            if (5..=11).contains(&line) && matches!(line, 5 | 9 | 10 | 11) {
                crate::pic::unmask(line);
                println!("[ok] net: interrupt line {}", line);
            } else {
                println!("[net] interrupt line {} not usable: polling every tick", line);
            }
            UP.store(true, Ordering::Relaxed);
            sched::spawn("net", net_thread);
            true
        }
    }
}

/// Set by the card's interrupt handler; the network thread sleeps until it is (or 1 tick).
static IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static IRQ_WAIT: WaitQueue = WaitQueue::new();
/// The card's I/O base, for the interrupt handler (which must not take the stack's lock).
static NIC_IO: AtomicU16 = AtomicU16::new(0);
/// An interrupt arrived on the line the network card uses. Acknowledge every device that may
/// share the line (the disk does): reading a virtio device's ISR register is what makes it lower
/// the line, and a level-triggered line that stays up fires again at once, for ever.
pub fn interrupt() {
    for io in [NIC_IO.load(Ordering::Relaxed), crate::virtio_blk::IO_BASE.load(Ordering::Relaxed)] {
        if io != 0 {
            let _ = unsafe { crate::port::inb(io + 19) };
        }
    }
    IRQ_PENDING.store(true, Ordering::SeqCst);
    IRQ_WAIT.wake_all();
}

fn net_thread() {
    loop {
        with_stack(|_, _| ());
        IRQ_WAIT.wait_until_deadline(timer::ticks() + 1, || IRQ_PENDING.swap(false, Ordering::SeqCst));
    }
}

/// Run `f` on the stack with the card serviced before and after. `None` if there is no network.
pub fn with_stack<R>(f: impl FnOnce(&mut Stack, u64) -> R) -> Option<R> {
    let (result, announce, activity) = {
        let mut guard = NET.lock();
        let st = guard.as_mut()?;
        let now = now_ms();
        let NetState { dev, stack, reported } = st;
        let work_before = stack.work();
        let received = dev.poll_rx(|frame| stack.receive(frame, now));
        stack.poll(now);
        let result = f(stack, now);
        while dev.tx_space() {
            match stack.next_frame() {
                Some(frame) => {
                    dev.send(&frame);
                }
                None => break,
            }
        }
        let mut announce = None;
        if !*reported {
            if let Some(cfg) = stack.config() {
                *reported = true;
                announce = Some(cfg);
            }
        }
        (result, announce, received > 0 || announce.is_some() || stack.work() != work_before)
    };
    if activity {
        ACTIVITY.fetch_add(1, Ordering::SeqCst);
        EVENT.wake_all();
    }
    if let Some(cfg) = announce {
        println!(
            "[ok] net: address {} mask {} gateway {} dns {}",
            fmt_ip(cfg.ip),
            fmt_ip(cfg.mask),
            cfg.gateway.map_or(String::from("none"), fmt_ip),
            cfg.dns.map_or(String::from("none"), fmt_ip)
        );
    }
    Some(result)
}

pub fn config() -> Option<Config> {
    with_stack(|s, _| s.config()).flatten()
}

/// Wait for network activity (a frame arriving, a state change), but never past
/// `deadline` (in ticks) and never longer than two ticks, so callers re-check.
fn wait_activity(seen: u64, deadline: Option<u64>) {
    let soonest = timer::ticks() + 2;
    let until = deadline.map_or(soonest, |d| d.min(soonest));
    EVENT.wait_until_deadline(until, || ACTIVITY.load(Ordering::SeqCst) != seen);
}

fn deadline_of(timeout_ticks: Option<u64>) -> Option<u64> {
    timeout_ticks.map(|t| timer::ticks().saturating_add(t))
}

fn expired(deadline: Option<u64>) -> bool {
    deadline.map_or(false, |d| timer::ticks() >= d)
}

// ---- UDP sockets ---------------------------------------------------------------------

pub struct UdpSocket {
    /// 0 = not bound yet.
    port: AtomicU32,
    peer: SpinLock<Option<(Ip, u16)>>,
}

impl UdpSocket {
    pub fn new() -> UdpSocket {
        UdpSocket { port: AtomicU32::new(0), peer: SpinLock::new(None) }
    }

    pub fn local_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst) as u16
    }

    pub fn bind(&self, port: u16) -> Result<(), i64> {
        if self.local_port() != 0 {
            return Err(EINVAL);
        }
        let got = with_stack(|s, _| s.udp_bind(port)).ok_or(ENETDOWN)?.map_err(map_error)?;
        self.port.store(got as u32, Ordering::SeqCst);
        Ok(())
    }

    fn ensure_bound(&self) -> Result<u16, i64> {
        if self.local_port() == 0 {
            self.bind(0)?;
        }
        Ok(self.local_port())
    }

    /// Remember a default peer (and filter incoming datagrams to it).
    pub fn connect(&self, ip: Ip, port: u16) -> Result<(), i64> {
        self.ensure_bound()?;
        *self.peer.lock() = Some((ip, port));
        Ok(())
    }

    pub fn send_to(&self, data: &[u8], to: Option<(Ip, u16)>) -> Result<usize, i64> {
        let (ip, port) = match to.or(*self.peer.lock()) {
            Some(addr) => addr,
            None => return Err(EDESTADDRREQ),
        };
        if port == 0 {
            return Err(EINVAL);
        }
        let local = self.ensure_bound()?;
        with_stack(|s, now| s.udp_send(local, ip, port, data, now)).ok_or(ENETDOWN)?.map_err(map_error)?;
        Ok(data.len())
    }

    /// Next datagram, truncated to `max` bytes. `timeout_ticks`: Some(0) only looks,
    /// None waits as long as it takes.
    pub fn recv_from(&self, max: usize, timeout_ticks: Option<u64>) -> Result<(Vec<u8>, Ip, u16), i64> {
        let port = self.ensure_bound()?;
        let deadline = deadline_of(timeout_ticks);
        loop {
            let seen = ACTIVITY.load(Ordering::SeqCst);
            let got = with_stack(|s, _| s.udp_recv(port)).ok_or(ENETDOWN)?;
            if let Some(d) = got {
                let peer = *self.peer.lock();
                if peer.map_or(true, |p| p == (d.src_ip, d.src_port)) {
                    let mut data = d.data;
                    data.truncate(max);
                    return Ok((data, d.src_ip, d.src_port));
                }
                continue; // a connected socket ignores strangers
            }
            if expired(deadline) {
                return Err(if timeout_ticks == Some(0) { EAGAIN } else { ETIMEDOUT });
            }
            wait_activity(seen, deadline);
        }
    }

    pub fn ready(&self) -> bool {
        let port = self.local_port();
        port != 0 && with_stack(|s, _| s.udp_pending(port)).unwrap_or(0) > 0
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let port = self.local_port();
        if port != 0 {
            with_stack(|s, _| s.udp_unbind(port));
        }
    }
}

// ---- TCP sockets ---------------------------------------------------------------------------

pub const EPIPE: i64 = -32;
pub const ENOBUFS: i64 = -105;
pub const ECONNRESET: i64 = -104;
pub const ENOTCONN: i64 = -107;
pub const ECONNREFUSED: i64 = -111;
pub const EISCONN: i64 = -106;

pub fn map_tcp_error(e: TcpError) -> i64 {
    match e {
        TcpError::WouldBlock => EAGAIN,
        TcpError::NoSuchConn => -9,
        TcpError::NotConnected => ENOTCONN,
        TcpError::Reset => ECONNRESET,
        TcpError::TimedOut => ETIMEDOUT,
        TcpError::Refused => ECONNREFUSED,
        TcpError::BrokenPipe => EPIPE,
        TcpError::PortInUse => EADDRINUSE,
        TcpError::NoPorts => EADDRNOTAVAIL,
        TcpError::TooMany => ENOBUFS,
    }
}

/// Run `f` on the TCP layer (with the card serviced around it).
fn with_tcp<R>(f: impl FnOnce(&mut Tcp, u64) -> R) -> Option<R> {
    with_stack(|s, now| s.with_tcp(now, |t, _| f(t, now)))
}

#[derive(Clone, Copy)]
enum TcpPhase {
    /// Just created; `port` is where it was bound (0 = not bound).
    Fresh { port: u16 },
    Listening { port: u16 },
    Connected { id: u32 },
    /// A failed connect: the socket cannot be used again.
    Failed,
}

pub struct TcpSocket {
    phase: SpinLock<TcpPhase>,
}

impl TcpSocket {
    pub fn new() -> TcpSocket {
        TcpSocket { phase: SpinLock::new(TcpPhase::Fresh { port: 0 }) }
    }

    fn connected(id: u32) -> TcpSocket {
        TcpSocket { phase: SpinLock::new(TcpPhase::Connected { id }) }
    }

    fn phase(&self) -> TcpPhase {
        *self.phase.lock()
    }

    pub fn bind(&self, port: u16) -> Result<(), i64> {
        let mut p = self.phase.lock();
        match *p {
            TcpPhase::Fresh { port: 0 } => {
                *p = TcpPhase::Fresh { port };
                Ok(())
            }
            _ => Err(EINVAL),
        }
    }

    pub fn listen(&self, backlog: usize) -> Result<u16, i64> {
        let TcpPhase::Fresh { port } = self.phase() else { return Err(EINVAL) };
        if port == 0 {
            return Err(EINVAL); // a server must say which port it serves
        }
        with_tcp(|t, _| t.listen(port, backlog)).ok_or(ENETDOWN)?.map_err(map_tcp_error)?;
        *self.phase.lock() = TcpPhase::Listening { port };
        Ok(port)
    }

    /// Wait for a connection to arrive; returns it with the peer's address.
    pub fn accept(&self, timeout_ticks: Option<u64>) -> Result<(TcpSocket, Ip, u16), i64> {
        let TcpPhase::Listening { port } = self.phase() else { return Err(EINVAL) };
        let deadline = deadline_of(timeout_ticks);
        loop {
            let seen = ACTIVITY.load(Ordering::SeqCst);
            let got = with_tcp(|t, _| t.accept(port).map(|id| (id, t.peer(id)))).ok_or(ENETDOWN)?;
            if let Some((id, Some((ip, peer_port)))) = got {
                return Ok((TcpSocket::connected(id), ip, peer_port));
            }
            if expired(deadline) {
                return Err(if timeout_ticks == Some(0) { EAGAIN } else { ETIMEDOUT });
            }
            wait_activity(seen, deadline);
        }
    }

    pub fn connect(&self, ip: Ip, port: u16, timeout_ticks: Option<u64>) -> Result<(), i64> {
        match self.phase() {
            TcpPhase::Fresh { .. } => {}
            TcpPhase::Connected { .. } => return Err(EISCONN),
            _ => return Err(EINVAL),
        }
        let cfg = config().ok_or(ENETDOWN)?;
        let id = match with_tcp(|t, now| t.connect(cfg.ip, ip, port, now)).ok_or(ENETDOWN)? {
            Ok(id) => id,
            Err(e) => return Err(map_tcp_error(e)),
        };
        *self.phase.lock() = TcpPhase::Connected { id };
        let deadline = deadline_of(timeout_ticks);
        loop {
            let seen = ACTIVITY.load(Ordering::SeqCst);
            match with_tcp(|t, _| t.connect_result(id)).ok_or(ENETDOWN)? {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => {
                    with_tcp(|t, now| t.close(id, now));
                    *self.phase.lock() = TcpPhase::Failed;
                    return Err(map_tcp_error(e));
                }
            }
            if expired(deadline) {
                with_tcp(|t, now| t.abort(id, now));
                *self.phase.lock() = TcpPhase::Failed;
                return Err(if timeout_ticks == Some(0) { EAGAIN } else { ETIMEDOUT });
            }
            wait_activity(seen, deadline);
        }
    }

    fn id(&self) -> Result<u32, i64> {
        match self.phase() {
            TcpPhase::Connected { id } => Ok(id),
            _ => Err(ENOTCONN),
        }
    }

    /// Write all of `data` (waiting for room if the send buffer is full).
    pub fn send(&self, data: &[u8]) -> Result<usize, i64> {
        let id = self.id()?;
        let mut done = 0;
        while done < data.len() {
            let seen = ACTIVITY.load(Ordering::SeqCst);
            match with_tcp(|t, now| t.send(id, &data[done..], now)).ok_or(ENETDOWN)? {
                Ok(n) => done += n,
                Err(TcpError::WouldBlock) => wait_activity(seen, None),
                Err(e) => {
                    // Bytes already queued count as written; the error shows on the next call.
                    return if done > 0 { Ok(done) } else { Err(map_tcp_error(e)) };
                }
            }
        }
        Ok(done)
    }

    /// Up to `max` bytes; an empty result is the end of the stream.
    pub fn recv(&self, max: usize, timeout_ticks: Option<u64>) -> Result<Vec<u8>, i64> {
        let id = self.id()?;
        let deadline = deadline_of(timeout_ticks);
        let mut buf = alloc::vec![0u8; max];
        loop {
            let seen = ACTIVITY.load(Ordering::SeqCst);
            match with_tcp(|t, now| t.recv(id, &mut buf, now)).ok_or(ENETDOWN)? {
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(TcpError::WouldBlock) => {}
                Err(e) => return Err(map_tcp_error(e)),
            }
            if expired(deadline) {
                return Err(if timeout_ticks == Some(0) { EAGAIN } else { ETIMEDOUT });
            }
            wait_activity(seen, deadline);
        }
    }

    pub fn shutdown_write(&self) -> Result<(), i64> {
        let id = self.id()?;
        with_tcp(|t, now| t.shutdown_write(id, now)).ok_or(ENETDOWN)?.map_err(map_tcp_error)
    }

    pub fn ready(&self) -> bool {
        match self.phase() {
            TcpPhase::Fresh { .. } | TcpPhase::Failed => true,
            TcpPhase::Listening { port } => with_tcp(|t, _| t.accept_ready(port)).unwrap_or(true),
            TcpPhase::Connected { id } => with_tcp(|t, _| t.readable(id)).unwrap_or(true),
        }
    }

    pub fn local_addr(&self) -> Result<(Ip, u16), i64> {
        let ip = config().map(|c| c.ip).unwrap_or([0; 4]);
        match self.phase() {
            TcpPhase::Fresh { port } | TcpPhase::Listening { port } => Ok((ip, port)),
            TcpPhase::Connected { id } => {
                let port = with_tcp(|t, _| t.local_port(id)).flatten().ok_or(ENOTCONN)?;
                Ok((ip, port))
            }
            TcpPhase::Failed => Err(EINVAL),
        }
    }

    pub fn peer_addr(&self) -> Result<(Ip, u16), i64> {
        let id = self.id()?;
        with_tcp(|t, _| t.peer(id)).flatten().ok_or(ENOTCONN)
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        match self.phase() {
            TcpPhase::Listening { port } => {
                with_tcp(|t, now| t.unlisten(port, now));
            }
            TcpPhase::Connected { id } => {
                with_tcp(|t, now| t.close(id, now));
            }
            _ => {}
        }
    }
}

// ---- ping and name lookup -------------------------------------------------------------

/// Send one echo request and wait for its reply; returns the round trip in microseconds.
pub fn ping(dst: Ip, payload_len: usize, timeout_ticks: u64) -> Result<u64, i64> {
    if payload_len > 1400 {
        return Err(EINVAL);
    }
    let id = (sched::current_id() as u16) ^ 0x4E56;
    let seq = PING_SEQ.fetch_add(1, Ordering::Relaxed);
    let data: Vec<u8> = (0..payload_len).map(|i| b'a' + (i % 26) as u8).collect();
    let deadline = deadline_of(Some(timeout_ticks));
    let t0 = timer::now_us();
    with_stack(|s, now| s.ping_send(dst, id, seq, &data, now)).ok_or(ENETDOWN)?.map_err(map_error)?;
    loop {
        let seen = ACTIVITY.load(Ordering::SeqCst);
        let got = with_stack(|s, _| s.ping_take(id, seq)).ok_or(ENETDOWN)?;
        if got.is_some() {
            return Ok(timer::now_us().saturating_sub(t0));
        }
        if expired(deadline) {
            return Err(ETIMEDOUT);
        }
        // Spin (politely) for the first 20 ms so the round trip is measured in
        // microseconds, then fall back to waiting on the network thread.
        if timer::now_us().saturating_sub(t0) < 20_000 {
            sched::yield_now();
        } else {
            wait_activity(seen, deadline);
        }
    }
}

/// "a.b.c.d" -> address; strict (four decimal numbers 0-255, nothing else).
pub fn parse_ip(s: &str) -> Option<Ip> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse::<u16>().ok().filter(|&v| v <= 255)? as u8;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Look up an IPv4 address. A dotted address is returned as is. `server` overrides
/// the resolver from DHCP (address and port; used by tests).
pub fn resolve(name: &str, server: Option<(Ip, u16)>, timeout_ticks: u64) -> Result<Ip, i64> {
    if let Some(ip) = parse_ip(name) {
        return Ok(ip);
    }
    // A host name never ends in a purely numeric label ("1.2.3" is a malformed address,
    // not a name worth asking a server about).
    let last = name.strip_suffix('.').unwrap_or(name).rsplit('.').next().unwrap_or("");
    if last.bytes().all(|b| b.is_ascii_digit()) || dns::build_query(0, name).is_err() {
        return Err(EINVAL);
    }
    let sock = UdpSocket::new();
    let port = sock.ensure_bound()?;
    let id = (timer::now_us() as u16) ^ port;
    let deadline = deadline_of(Some(timeout_ticks));
    let mut next_send = 0u64;
    loop {
        let seen = ACTIVITY.load(Ordering::SeqCst);
        if timer::ticks() >= next_send {
            next_send = timer::ticks() + 100;
            let sent = with_stack(|s, now| match server {
                Some((sv, sv_port)) => match dns::build_query(id, name) {
                    Ok(q) => s.udp_send(port, sv, sv_port, &q, now),
                    Err(_) => Err(NetError::TooBig),
                },
                None => s.dns_query(port, id, name, now),
            })
            .ok_or(ENETDOWN)?;
            match sent {
                Ok(()) | Err(NetError::NoConfig) => {} // no address yet: keep waiting for DHCP
                Err(e) => return Err(map_error(e)),
            }
        }
        let want = server.or_else(|| config().and_then(|c| c.dns).map(|ip| (ip, dns::PORT)));
        if let Ok((data, from, from_port)) = sock.recv_from(1500, Some(0)) {
            // Only believe the server we asked.
            if Some((from, from_port)) == want {
                match dns::parse_response(id, &data) {
                    Ok(ip) => return Ok(ip),
                    Err(dns::DnsError::NoSuchName) => return Err(ENOENT),
                    Err(dns::DnsError::WrongId) | Err(dns::DnsError::Malformed) => {}
                    Err(_) => return Err(EIO),
                }
            }
            continue;
        }
        if expired(deadline) {
            return Err(ETIMEDOUT);
        }
        wait_activity(seen, deadline);
    }
}

// ---- what `netinfo` reports ------------------------------------------------------------

pub const NETINFO_BYTES: usize = 64;

/// Layout (little endian): 0 flags (bit0 link up, bit1 has address, bit2 DHCP bound),
/// 4 mac[6], 12 ip, 16 mask, 20 gateway, 24 dns, 32 rx_frames, 40 tx_frames,
/// 48 rx_dropped, 56 tx_dropped.
pub fn netinfo() -> Option<[u8; NETINFO_BYTES]> {
    let mut guard = NET.lock();
    let st = guard.as_mut()?;
    let mut raw = [0u8; NETINFO_BYTES];
    let cfg = st.stack.config();
    let flags = (st.dev.link_up() as u32) | (cfg.is_some() as u32) << 1 | (st.stack.dhcp_bound() as u32) << 2;
    raw[0..4].copy_from_slice(&flags.to_le_bytes());
    raw[4..10].copy_from_slice(&st.stack.mac);
    if let Some(c) = cfg {
        raw[12..16].copy_from_slice(&c.ip);
        raw[16..20].copy_from_slice(&c.mask);
        raw[20..24].copy_from_slice(&c.gateway.unwrap_or([0; 4]));
        raw[24..28].copy_from_slice(&c.dns.unwrap_or([0; 4]));
    }
    raw[32..40].copy_from_slice(&st.stack.stats.rx_frames.to_le_bytes());
    raw[40..48].copy_from_slice(&st.stack.stats.tx_frames.to_le_bytes());
    raw[48..56].copy_from_slice(&st.stack.stats.rx_dropped.to_le_bytes());
    raw[56..64].copy_from_slice(&st.dev.tx_dropped.to_le_bytes());
    Some(raw)
}

// ---- self-test ---------------------------------------------------------------------------

/// Boot-time checks of the whole path (card, DHCP, ARP, ICMP, sockets). Needs a card
/// on QEMU's user-mode network; skipped (and said so) when there is none.
pub fn self_test() {
    if !available() {
        println!("[ok] net self-test: skipped (no network card)");
        return;
    }
    // 1. DHCP finishes by itself.
    let start = timer::ticks();
    while config().is_none() {
        assert!(timer::ticks() - start < 800, "net self-test: DHCP did not finish in 8 s");
        sched::sleep_ticks(2);
    }
    let cfg = config().unwrap();
    assert_eq!(cfg.ip, [10, 0, 2, 15], "net self-test: QEMU's user network hands out 10.0.2.15");
    assert_eq!(cfg.gateway, Some([10, 0, 2, 2]));
    assert_eq!(cfg.mask, [255, 255, 255, 0]);
    assert!(cfg.dns.is_some());

    // 2. ARP + ICMP to the gateway, twice (the second needs no ARP).
    let first = ping([10, 0, 2, 2], 32, 300).expect("net self-test: ping to the gateway");
    let second = ping([10, 0, 2, 2], 56, 300).expect("net self-test: second ping");
    assert!(first > 0 && first < 1_000_000 && second > 0, "net self-test: implausible round trip");
    // Nobody answers on an unused address on the LAN: must time out, not hang or crash.
    assert_eq!(ping([10, 0, 2, 77], 8, 40), Err(ETIMEDOUT), "net self-test: ping to a silent address");

    // 3. UDP between two sockets of ours, through the loopback path.
    let a = UdpSocket::new();
    let b = UdpSocket::new();
    b.bind(7777).unwrap();
    a.connect(cfg.ip, 7777).unwrap();
    a.send_to(b"through the stack", None).unwrap();
    let (data, from, from_port) = b.recv_from(100, Some(100)).expect("net self-test: loopback datagram");
    assert_eq!((&data[..], from), (&b"through the stack"[..], cfg.ip));
    assert_eq!(from_port, a.local_port());
    assert_eq!(b.recv_from(100, Some(0)), Err(EAGAIN));
    assert_eq!(b.recv_from(100, Some(5)), Err(ETIMEDOUT));
    // A port can be taken only once, and is free again after the socket is gone.
    let c = UdpSocket::new();
    assert_eq!(c.bind(7777), Err(EADDRINUSE));
    drop(b);
    assert_eq!(c.bind(7777), Ok(()));
    drop(c);
    drop(a);

    // 4. Name lookup: literals need no network, bad names are refused.
    assert_eq!(resolve("10.1.2.3", None, 10), Ok([10, 1, 2, 3]));
    assert_eq!(resolve("1.2.3", Some(([10, 0, 2, 3], 53)), 1), Err(EINVAL));
    assert_eq!(resolve("bad name!", None, 10), Err(EINVAL));
    assert_eq!(parse_ip("255.255.255.255"), Some([255; 4]));
    assert_eq!(parse_ip("256.1.1.1"), None);
    assert_eq!(parse_ip("1.2.3.4.5"), None);
    assert_eq!(parse_ip("1..3.4"), None);
    assert_eq!(parse_ip("+1.2.3.4"), None);
    assert_eq!(parse_ip("1.2.3.0x4"), None);
    println!("[ok] net self-test: DHCP, ARP, ping, UDP sockets and name checks ({} us to the gateway)", second);
}
