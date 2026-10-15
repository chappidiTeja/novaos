//! TCP: connections, reliable delivery, flow control and congestion control.
//!
//! Follows RFC 9293 (segment processing and the state machine), RFC 6298 (retransmission
//! timer), RFC 5681/6582 (slow start, congestion avoidance, fast retransmit and recovery
//! in the NewReno style) and RFC 6928 (initial window of ten segments). Not implemented:
//! window scaling, selective acknowledgements, timestamps, urgent data, keep-alive.
//!
//! The module touches no hardware. The stack feeds it received segments and the time (in
//! milliseconds) and takes the segments it wants sent from `out`.

use crate::wire::{transport_checksum, Ip, PROTO_TCP};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

pub const TCP_HEADER: usize = 20;
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

/// 1500 (MTU) - 20 (IP) - 20 (TCP).
pub const MSS: u16 = 1460;
const DEFAULT_PEER_MSS: u16 = 536;
pub const SEND_CAP: usize = 128 * 1024;
pub const RECV_CAP: usize = 65535;
const INITIAL_CWND_SEGMENTS: u32 = 10;
const INITIAL_RTO_MS: u32 = 1000;
const MIN_RTO_MS: u32 = 200;
const MAX_RTO_MS: u32 = 60_000;
const SYN_RETRIES: u32 = 6;
const DATA_RETRIES: u32 = 12;
const TIME_WAIT_MS: u64 = 30_000;
const ORPHAN_FIN_WAIT_MS: u64 = 60_000;
const MAX_CONNECTIONS: usize = 256;
const MAX_OOO_SEGMENTS: usize = 32;
pub const EPHEMERAL_FIRST: u16 = 49152;

// ---- sequence number arithmetic (modulo 2^32) ------------------------------------------------

pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
pub fn seq_le(a: u32, b: u32) -> bool {
    a == b || seq_lt(a, b)
}
pub fn seq_gt(a: u32, b: u32) -> bool {
    seq_lt(b, a)
}
pub fn seq_ge(a: u32, b: u32) -> bool {
    seq_le(b, a)
}
/// lo <= x < hi
fn seq_in(lo: u32, x: u32, hi: u32) -> bool {
    x.wrapping_sub(lo) < hi.wrapping_sub(lo)
}

// ---- segments ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Segment<'a> {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: &'a [u8],
}

impl Segment<'_> {
    /// Sequence space the segment uses: data plus one each for SYN and FIN.
    pub fn seq_len(&self) -> u32 {
        self.payload.len() as u32 + (self.flags & SYN != 0) as u32 + (self.flags & FIN != 0) as u32
    }
}

/// Parse and verify (the checksum is mandatory for TCP) a segment from the IP payload.
pub fn parse(src: Ip, dst: Ip, b: &[u8]) -> Option<Segment<'_>> {
    if b.len() < TCP_HEADER {
        return None;
    }
    let offset = ((b[12] >> 4) as usize) * 4;
    if offset < TCP_HEADER || offset > b.len() {
        return None;
    }
    if transport_checksum(src, dst, PROTO_TCP, b) != 0 {
        return None;
    }
    // Options: only the MSS matters; the rest is skipped, but must be well formed.
    let mut mss = None;
    let opts = &b[TCP_HEADER..offset];
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => break,
            1 => i += 1,
            kind => {
                let len = *opts.get(i + 1)? as usize;
                if len < 2 || i + len > opts.len() {
                    return None;
                }
                if kind == 2 && len == 4 {
                    mss = Some(u16::from_be_bytes([opts[i + 2], opts[i + 3]]));
                }
                i += len;
            }
        }
    }
    Some(Segment {
        sport: u16::from_be_bytes([b[0], b[1]]),
        dport: u16::from_be_bytes([b[2], b[3]]),
        seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ack: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        flags: b[13] & 0x3F,
        window: u16::from_be_bytes([b[14], b[15]]),
        mss,
        payload: &b[offset..],
    })
}

pub fn build(src: Ip, dst: Ip, s: &Segment<'_>) -> Vec<u8> {
    let opt_len = if s.mss.is_some() { 4 } else { 0 };
    let header = TCP_HEADER + opt_len;
    let mut p = Vec::with_capacity(header + s.payload.len());
    p.extend_from_slice(&s.sport.to_be_bytes());
    p.extend_from_slice(&s.dport.to_be_bytes());
    p.extend_from_slice(&s.seq.to_be_bytes());
    p.extend_from_slice(&s.ack.to_be_bytes());
    p.push(((header / 4) as u8) << 4);
    p.push(s.flags);
    p.extend_from_slice(&s.window.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]); // checksum, urgent pointer
    if let Some(m) = s.mss {
        p.extend_from_slice(&[2, 4]);
        p.extend_from_slice(&m.to_be_bytes());
    }
    p.extend_from_slice(s.payload);
    let c = transport_checksum(src, dst, PROTO_TCP, &p);
    p[16..18].copy_from_slice(&c.to_be_bytes());
    p
}

// ---- connection state ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Nothing to read / no room to write right now.
    WouldBlock,
    /// The connection does not exist (any more).
    NoSuchConn,
    /// The connection is not (yet) established.
    NotConnected,
    /// The other side reset the connection.
    Reset,
    /// Too many retransmissions without an answer.
    TimedOut,
    /// Nobody listens on that port.
    Refused,
    /// We have already shut down our sending side.
    BrokenPipe,
    PortInUse,
    NoPorts,
    TooMany,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// A program holds the handle.
    App,
    /// The program closed it; the stack finishes the shutdown and then frees it.
    Orphan,
    /// Accepted by the stack for a listener, not yet handed to a program.
    Listener(u16),
}

struct Tcb {
    state: State,
    owner: Owner,
    local_port: u16,
    remote_ip: Ip,
    remote_port: u16,
    error: Option<TcpError>,

    // sending
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_wnd: u32,
    snd_wl1: u32,
    snd_wl2: u32,
    /// Bytes from `snd_una` on that the program wrote and that are not yet acknowledged.
    send_buf: VecDeque<u8>,
    fin_queued: bool,
    fin_sent: bool,
    mss: u32,
    cwnd: u32,
    ssthresh: u32,
    dupacks: u32,
    in_recovery: bool,
    recover: u32,

    // retransmission timer
    srtt: Option<u32>,
    rttvar: u32,
    rto: u32,
    rtt_probe: Option<(u32, u64)>,
    rexmit_at: Option<u64>,
    retries: u32,

    // receiving
    irs: u32,
    rcv_nxt: u32,
    recv_buf: VecDeque<u8>,
    ooo: Vec<(u32, Vec<u8>)>,
    fin_ooo: Option<u32>,
    fin_received: bool,
    last_adv_wnd: u32,
    persist_shift: u32,

    deadline: Option<u64>,
}

pub struct Tx {
    pub dst: Ip,
    pub segment: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TcpStats {
    pub segments_in: u64,
    pub segments_out: u64,
    pub bad_segments: u64,
    pub retransmits: u64,
    pub fast_retransmits: u64,
    pub resets_sent: u64,
    pub connections_opened: u64,
    pub connections_accepted: u64,
}

struct Listener {
    backlog: usize,
    ready: VecDeque<u32>,
}

pub struct Tcp {
    conns: BTreeMap<u32, Tcb>,
    index: BTreeMap<(u16, Ip, u16), u32>,
    listeners: BTreeMap<u16, Listener>,
    next_id: u32,
    next_port: u16,
    rng: u32,
    pub ip: Ip,
    /// Test hook: the next connection starts at this sequence number.
    pub force_iss: Option<u32>,
    pub out: Vec<Tx>,
    pub stats: TcpStats,
}

impl Tcb {
    fn new(state: State, owner: Owner, local_port: u16, remote_ip: Ip, remote_port: u16, iss: u32) -> Tcb {
        Tcb {
            state,
            owner,
            local_port,
            remote_ip,
            remote_port,
            error: None,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_wnd: 0,
            snd_wl1: 0,
            snd_wl2: 0,
            send_buf: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            mss: DEFAULT_PEER_MSS as u32,
            cwnd: INITIAL_CWND_SEGMENTS * DEFAULT_PEER_MSS as u32,
            ssthresh: u32::MAX,
            dupacks: 0,
            in_recovery: false,
            recover: iss,
            srtt: None,
            rttvar: 0,
            rto: INITIAL_RTO_MS,
            rtt_probe: None,
            rexmit_at: None,
            retries: 0,
            irs: 0,
            rcv_nxt: 0,
            recv_buf: VecDeque::new(),
            ooo: Vec::new(),
            fin_ooo: None,
            fin_received: false,
            last_adv_wnd: RECV_CAP as u32,
            persist_shift: 0,
            deadline: None,
        }
    }

    fn ooo_bytes(&self) -> usize {
        self.ooo.iter().map(|(_, d)| d.len()).sum()
    }

    /// Room left in the receive buffer for in-order data (what we advertise).
    fn window(&self) -> u32 {
        // Out-of-order data does not shrink the window: a window that changes with every
        // out-of-order segment would stop the sender from recognising duplicate ACKs.
        RECV_CAP.saturating_sub(self.recv_buf.len()) as u32
    }

    /// Data bytes sent and not yet acknowledged (not counting a FIN).
    fn inflight_data(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una).wrapping_sub(self.fin_sent as u32)
    }

    fn synchronized(&self) -> bool {
        !matches!(self.state, State::SynSent | State::SynReceived | State::Closed)
    }
}

/// What a segment-processing step needs besides the connection itself.
struct Ctx<'a> {
    ip: Ip,
    now: u64,
    out: &'a mut Vec<Tx>,
    stats: &'a mut TcpStats,
}

impl Ctx<'_> {
    fn emit(&mut self, t: &Tcb, seq: u32, ack: u32, flags: u8, mss: Option<u16>, payload: &[u8]) {
        let seg = Segment {
            sport: t.local_port,
            dport: t.remote_port,
            seq,
            ack,
            flags,
            window: t.window().min(65535) as u16,
            mss,
            payload,
        };
        self.out.push(Tx { dst: t.remote_ip, segment: build(self.ip, t.remote_ip, &seg) });
        self.stats.segments_out += 1;
    }

    fn send_ack(&mut self, t: &mut Tcb) {
        t.last_adv_wnd = t.window();
        self.emit(t, t.snd_nxt, t.rcv_nxt, ACK, None, &[]);
    }
    fn send_syn(&mut self, t: &Tcb) {
        let flags = if t.state == State::SynSent { SYN } else { SYN | ACK };
        self.emit(t, t.iss, t.rcv_nxt, flags, Some(MSS), &[]);
    }

    fn arm(&mut self, t: &mut Tcb) {
        t.rexmit_at = Some(self.now + t.rto as u64);
    }
}

fn update_rtt(t: &mut Tcb, sample_ms: u32) {
    match t.srtt {
        None => {
            t.srtt = Some(sample_ms);
            t.rttvar = sample_ms / 2;
        }
        Some(srtt) => {
            let delta = srtt.abs_diff(sample_ms);
            t.rttvar = (3 * t.rttvar + delta) / 4;
            t.srtt = Some((7 * srtt + sample_ms) / 8);
        }
    }
    let rto = t.srtt.unwrap() + (4 * t.rttvar).max(10);
    t.rto = rto.clamp(MIN_RTO_MS, MAX_RTO_MS);
}

impl Tcp {
    pub fn new(seed: u32) -> Tcp {
        Tcp {
            conns: BTreeMap::new(),
            index: BTreeMap::new(),
            listeners: BTreeMap::new(),
            next_id: 1,
            next_port: EPHEMERAL_FIRST + (seed % 1000) as u16,
            rng: if seed == 0 { 0x2545_F491 } else { seed },
            force_iss: None,
            ip: [0; 4],
            out: Vec::new(),
            stats: TcpStats::default(),
        }
    }

    fn rand(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    fn next_iss(&mut self) -> u32 {
        match self.force_iss.take() {
            Some(v) => v,
            None => self.rand(),
        }
    }

    /// Panics if the tables are inconsistent. For tests and self-tests: this is how a bug in
    /// the state machine shows up as a failure at the moment it happens.
    pub fn check_invariants(&self) {
        assert_eq!(self.index.len(), self.conns.len(), "index and connection table disagree");
        assert!(self.conns.len() <= MAX_CONNECTIONS);
        for (&id, t) in &self.conns {
            assert_eq!(
                self.index.get(&(t.local_port, t.remote_ip, t.remote_port)),
                Some(&id),
                "connection {id} is not indexed under its own address"
            );
            let flight = t.snd_nxt.wrapping_sub(t.snd_una) as usize;
            assert!(flight <= t.send_buf.len() + 2, "connection {id}: {flight} bytes in flight but only {} queued", t.send_buf.len());
            assert!(t.send_buf.len() <= SEND_CAP, "send buffer over capacity");
            assert!(t.recv_buf.len() <= RECV_CAP, "receive buffer over capacity");
            assert!(t.ooo.len() <= MAX_OOO_SEGMENTS, "too many out-of-order segments");
            assert!(t.cwnd >= 1 && t.mss >= 64 && t.mss <= MSS as u32, "cwnd {} mss {}", t.cwnd, t.mss);
            assert!(t.rto >= MIN_RTO_MS.min(INITIAL_RTO_MS) && t.rto <= MAX_RTO_MS, "rto {}", t.rto);
            if t.state == State::Closed {
                assert!(t.rexmit_at.is_none(), "a closed connection still has a timer");
            }
            if matches!(t.state, State::SynSent | State::SynReceived) {
                assert!(t.send_buf.is_empty() || t.state == State::SynReceived);
            }
            if let Owner::Listener(p) = t.owner {
                assert!(self.listeners.contains_key(&p), "connection {id} belongs to a listener that is gone");
            }
        }
        for (port, l) in &self.listeners {
            for id in &l.ready {
                let t = self.conns.get(id).expect("an accept queue entry without a connection");
                assert_eq!(t.owner, Owner::Listener(*port));
            }
        }
    }

    fn ephemeral_port(&mut self, remote_ip: Ip, remote_port: u16) -> Option<u16> {
        for _ in 0..(65536 - EPHEMERAL_FIRST as u32) {
            let p = self.next_port;
            self.next_port = if p == 65535 { EPHEMERAL_FIRST } else { p + 1 };
            if !self.listeners.contains_key(&p) && !self.index.contains_key(&(p, remote_ip, remote_port)) {
                return Some(p);
            }
        }
        None
    }

    // ---- API used by the stack ------------------------------------------------------------------

    pub fn listen(&mut self, port: u16, backlog: usize) -> Result<(), TcpError> {
        if port == 0 || self.listeners.contains_key(&port) {
            return Err(TcpError::PortInUse);
        }
        self.listeners.insert(port, Listener { backlog: backlog.clamp(1, 128), ready: VecDeque::new() });
        Ok(())
    }

    pub fn is_listening(&self, port: u16) -> bool {
        self.listeners.contains_key(&port)
    }

    /// An established connection waiting on listener `port`, if any.
    pub fn accept(&mut self, port: u16) -> Option<u32> {
        let id = self.listeners.get_mut(&port)?.ready.pop_front()?;
        if let Some(t) = self.conns.get_mut(&id) {
            t.owner = Owner::App;
        }
        self.stats.connections_accepted += 1;
        Some(id)
    }

    pub fn accept_ready(&self, port: u16) -> bool {
        self.listeners.get(&port).map_or(false, |l| !l.ready.is_empty())
    }

    /// Stop listening; connections nobody accepted yet are reset.
    pub fn unlisten(&mut self, port: u16, now: u64) {
        let Some(l) = self.listeners.remove(&port) else { return };
        let doomed: Vec<u32> = self
            .conns
            .iter()
            .filter(|(_, t)| t.owner == Owner::Listener(port))
            .map(|(&id, _)| id)
            .collect();
        let _ = l;
        for id in doomed {
            self.abort(id, now);
        }
    }

    pub fn connect(&mut self, ip: Ip, remote_ip: Ip, remote_port: u16, now: u64) -> Result<u32, TcpError> {
        self.ip = ip;
        if self.conns.len() >= MAX_CONNECTIONS {
            return Err(TcpError::TooMany);
        }
        let local_port = self.ephemeral_port(remote_ip, remote_port).ok_or(TcpError::NoPorts)?;
        let iss = self.next_iss();
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut t = Tcb::new(State::SynSent, Owner::App, local_port, remote_ip, remote_port, iss);
        t.snd_nxt = iss.wrapping_add(1);
        // The SYN's round trip is the first RTT sample (dropped if the SYN has to be resent).
        t.rtt_probe = Some((t.snd_nxt, now));
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        ctx.send_syn(&t);
        ctx.arm(&mut t);
        self.index.insert((local_port, remote_ip, remote_port), id);
        self.conns.insert(id, t);
        self.stats.connections_opened += 1;
        Ok(id)
    }

    pub fn state(&self, id: u32) -> Option<State> {
        self.conns.get(&id).map(|t| t.state)
    }

    pub fn local_port(&self, id: u32) -> Option<u16> {
        self.conns.get(&id).map(|t| t.local_port)
    }

    pub fn peer(&self, id: u32) -> Option<(Ip, u16)> {
        self.conns.get(&id).map(|t| (t.remote_ip, t.remote_port))
    }

    /// Has the three-way handshake finished (or failed)? Err carries why a connect failed.
    pub fn connect_result(&self, id: u32) -> Result<bool, TcpError> {
        let t = self.conns.get(&id).ok_or(TcpError::NoSuchConn)?;
        if let Some(e) = t.error {
            return Err(e);
        }
        Ok(!matches!(t.state, State::SynSent | State::SynReceived))
    }

    pub fn readable(&self, id: u32) -> bool {
        self.conns.get(&id).map_or(true, |t| !t.recv_buf.is_empty() || t.fin_received || t.error.is_some() || t.state == State::Closed)
    }

    pub fn writable(&self, id: u32) -> bool {
        self.conns.get(&id).map_or(true, |t| {
            t.error.is_some()
                || (matches!(t.state, State::Established | State::CloseWait) && !t.fin_queued && t.send_buf.len() < SEND_CAP)
                || t.state == State::Closed
        })
    }

    /// Bytes waiting to be read.
    pub fn available(&self, id: u32) -> usize {
        self.conns.get(&id).map_or(0, |t| t.recv_buf.len())
    }

    /// Bytes written but not yet acknowledged by the other side.
    pub fn unsent_or_unacked(&self, id: u32) -> usize {
        self.conns.get(&id).map_or(0, |t| t.send_buf.len())
    }

    pub fn send(&mut self, id: u32, data: &[u8], now: u64) -> Result<usize, TcpError> {
        let ip = self.ip;
        let t = self.conns.get_mut(&id).ok_or(TcpError::NoSuchConn)?;
        if let Some(e) = t.error {
            return Err(e);
        }
        match t.state {
            State::Established | State::CloseWait => {}
            State::SynSent | State::SynReceived => return Err(TcpError::NotConnected),
            _ => return Err(TcpError::BrokenPipe),
        }
        if t.fin_queued {
            return Err(TcpError::BrokenPipe);
        }
        let room = SEND_CAP - t.send_buf.len();
        if room == 0 {
            return Err(TcpError::WouldBlock);
        }
        let n = data.len().min(room);
        t.send_buf.extend(&data[..n]);
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        output(t, &mut ctx);
        Ok(n)
    }

    /// Ok(0) is end of file (the other side closed and everything was read).
    pub fn recv(&mut self, id: u32, buf: &mut [u8], now: u64) -> Result<usize, TcpError> {
        let ip = self.ip;
        let t = self.conns.get_mut(&id).ok_or(TcpError::NoSuchConn)?;
        if !t.recv_buf.is_empty() && !buf.is_empty() {
            let n = buf.len().min(t.recv_buf.len());
            for (i, b) in t.recv_buf.drain(..n).enumerate() {
                buf[i] = b;
            }
            // Tell the sender when a closed window has opened up (otherwise it only learns
            // by probing, which is slow).
            let now_wnd = t.window();
            if t.synchronized() && (now_wnd >= t.last_adv_wnd + t.mss.max(1) * 2 || (t.last_adv_wnd < t.mss && now_wnd >= t.mss)) {
                let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
                ctx.send_ack(t);
            }
            return Ok(n);
        }
        if let Some(e) = t.error.take() {
            return Err(e);
        }
        if t.fin_received || t.state == State::Closed {
            return Ok(0);
        }
        match t.state {
            State::SynSent | State::SynReceived => Err(TcpError::NotConnected),
            _ => Err(TcpError::WouldBlock),
        }
    }

    /// Graceful close from the program's side; the stack frees the connection when done.
    pub fn close(&mut self, id: u32, now: u64) {
        let ip = self.ip;
        let Some(t) = self.conns.get_mut(&id) else { return };
        // Unread data would be lost by a graceful close: reset instead (RFC 1122 4.2.2.13).
        let unread = !t.recv_buf.is_empty();
        if t.state == State::SynSent || t.state == State::Closed || (unread && t.synchronized()) {
            let state = t.state;
            if unread && state != State::SynSent && state != State::Closed {
                self.abort(id, now);
            } else {
                self.remove(id);
            }
            return;
        }
        t.owner = Owner::Orphan;
        t.fin_queued = true;
        if t.deadline.is_none() {
            t.deadline = Some(now + ORPHAN_FIN_WAIT_MS * 2);
        }
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        output(t, &mut ctx);
    }

    /// Stop sending (a FIN goes out after the queued data); reading stays possible.
    pub fn shutdown_write(&mut self, id: u32, now: u64) -> Result<(), TcpError> {
        let ip = self.ip;
        let t = self.conns.get_mut(&id).ok_or(TcpError::NoSuchConn)?;
        match t.state {
            State::Established | State::CloseWait => {}
            State::SynSent | State::SynReceived => return Err(TcpError::NotConnected),
            _ => return Ok(()),
        }
        t.fin_queued = true;
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        output(t, &mut ctx);
        Ok(())
    }

    /// Drop the connection at once, telling the other side with a reset.
    pub fn abort(&mut self, id: u32, now: u64) {
        let ip = self.ip;
        if let Some(t) = self.conns.get(&id) {
            if t.synchronized() || t.state == State::SynReceived {
                let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
                ctx.emit(t, t.snd_nxt, 0, RST, None, &[]);
                self.stats.resets_sent += 1;
            }
        }
        self.remove(id);
    }

    /// Reset everything (the machine's address changed).
    pub fn abort_all(&mut self, now: u64) {
        let ids: Vec<u32> = self.conns.keys().copied().collect();
        for id in ids {
            self.abort(id, now);
        }
        self.listeners.clear();
    }

    fn remove(&mut self, id: u32) {
        if let Some(t) = self.conns.remove(&id) {
            self.index.remove(&(t.local_port, t.remote_ip, t.remote_port));
            if let Owner::Listener(port) = t.owner {
                if let Some(l) = self.listeners.get_mut(&port) {
                    l.ready.retain(|&c| c != id);
                }
            }
        }
    }

    // ---- timers -----------------------------------------------------------------------------------

    pub fn poll(&mut self, now: u64) {
        let ip = self.ip;
        let ids: Vec<u32> = self.conns.keys().copied().collect();
        let mut dead = Vec::new();
        for id in ids {
            let t = self.conns.get_mut(&id).unwrap();
            let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
            timers(t, &mut ctx);
            let gone = match t.state {
                State::Closed => matches!(t.owner, Owner::Orphan | Owner::Listener(_)),
                _ => false,
            };
            if gone {
                dead.push(id);
            }
        }
        for id in dead {
            self.remove(id);
        }
    }

    // ---- input ------------------------------------------------------------------------------------

    /// A TCP segment (the IP payload) arrived from `src` for our address `dst`.
    pub fn input(&mut self, src: Ip, dst: Ip, bytes: &[u8], now: u64) {
        self.ip = dst;
        let Some(seg) = parse(src, dst, bytes) else {
            self.stats.bad_segments += 1;
            return;
        };
        self.stats.segments_in += 1;
        let key = (seg.dport, src, seg.sport);
        if let Some(&id) = self.index.get(&key) {
            self.deliver(id, &seg, now);
            return;
        }
        if seg.flags & RST != 0 {
            return;
        }
        if seg.flags & SYN != 0 && seg.flags & ACK == 0 {
            if self.listeners.contains_key(&seg.dport) {
                self.passive_open(src, &seg, now);
                return;
            }
        }
        self.reset_for(src, &seg);
    }

    /// Answer a segment that belongs to no connection (RFC 9293 3.10.7.1).
    fn reset_for(&mut self, src: Ip, seg: &Segment<'_>) {
        let (seq, ack, flags) = if seg.flags & ACK != 0 {
            (seg.ack, 0, RST)
        } else {
            (0, seg.seq.wrapping_add(seg.seq_len()), RST | ACK)
        };
        let reply = Segment { sport: seg.dport, dport: seg.sport, seq, ack, flags, window: 0, mss: None, payload: &[] };
        self.out.push(Tx { dst: src, segment: build(self.ip, src, &reply) });
        self.stats.segments_out += 1;
        self.stats.resets_sent += 1;
    }

    fn passive_open(&mut self, src: Ip, seg: &Segment<'_>, now: u64) {
        let port = seg.dport;
        let pending = self.conns.values().filter(|t| t.owner == Owner::Listener(port)).count();
        let backlog = self.listeners.get(&port).map_or(0, |l| l.backlog);
        if pending >= backlog || self.conns.len() >= MAX_CONNECTIONS {
            return; // full: stay silent and let the client's SYN retransmission try again
        }
        let iss = self.next_iss();
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let mut t = Tcb::new(State::SynReceived, Owner::Listener(port), port, src, seg.sport, iss);
        t.snd_nxt = iss.wrapping_add(1);
        t.irs = seg.seq;
        t.rcv_nxt = seg.seq.wrapping_add(1);
        t.snd_wnd = seg.window as u32;
        t.snd_wl1 = seg.seq;
        t.mss = seg.mss.unwrap_or(DEFAULT_PEER_MSS).clamp(64, MSS) as u32;
        t.cwnd = INITIAL_CWND_SEGMENTS * t.mss;
        let ip = self.ip;
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        ctx.send_syn(&t);
        ctx.arm(&mut t);
        self.index.insert((port, src, seg.sport), id);
        self.conns.insert(id, t);
    }

    fn deliver(&mut self, id: u32, seg: &Segment<'_>, now: u64) {
        let ip = self.ip;
        let Some(t) = self.conns.get_mut(&id) else { return };
        let was = t.state;
        let mut ctx = Ctx { ip, now, out: &mut self.out, stats: &mut self.stats };
        segment_arrives(t, seg, &mut ctx);
        let (state, owner, port) = (t.state, t.owner, t.local_port);
        // A passive connection that just completed its handshake becomes available to accept().
        if was == State::SynReceived && state != State::SynReceived && state != State::Closed {
            if let Owner::Listener(lport) = owner {
                if let Some(l) = self.listeners.get_mut(&lport) {
                    l.ready.push_back(id);
                }
            }
        }
        let _ = port;
        if state == State::Closed && matches!(owner, Owner::Orphan | Owner::Listener(_)) {
            self.remove(id);
        }
    }
}

// ---- the per-connection machinery ---------------------------------------------------------------

fn enter_closed(t: &mut Tcb, error: Option<TcpError>) {
    t.state = State::Closed;
    t.rexmit_at = None;
    t.deadline = None;
    t.rtt_probe = None;
    if t.error.is_none() {
        t.error = error;
    }
    if error.is_some() {
        t.recv_buf.clear();
        t.send_buf.clear();
        t.snd_una = t.snd_nxt;
        t.ooo.clear();
    }
}

/// Send what the windows allow: new data, then a FIN once all data is out.
fn output(t: &mut Tcb, ctx: &mut Ctx<'_>) {
    if !t.synchronized() {
        return;
    }
    loop {
        let flight = t.snd_nxt.wrapping_sub(t.snd_una);
        let window = t.snd_wnd.min(t.cwnd);
        let inflight_data = t.inflight_data();
        let unsent = t.send_buf.len() as u32 - inflight_data.min(t.send_buf.len() as u32);
        if unsent > 0 {
            let room = window.saturating_sub(flight);
            // Zero-window probe: if the peer's window is shut and nothing is in flight, push
            // one byte so a lost window update cannot deadlock the connection.
            let probe = t.snd_wnd == 0 && flight == 0;
            let len = if probe { 1 } else { unsent.min(t.mss).min(room) };
            if len == 0 {
                break;
            }
            let start = inflight_data as usize;
            let payload: Vec<u8> = t.send_buf.range(start..start + len as usize).copied().collect();
            let last = len == unsent;
            let flags = ACK | if last { PSH } else { 0 };
            let seq = t.snd_nxt;
            ctx.emit(t, seq, t.rcv_nxt, flags, None, &payload);
            t.last_adv_wnd = t.window();
            if t.rtt_probe.is_none() {
                t.rtt_probe = Some((seq.wrapping_add(len), ctx.now));
            }
            t.snd_nxt = t.snd_nxt.wrapping_add(len);
            if t.rexmit_at.is_none() {
                ctx.arm(t);
            }
            continue;
        }
        break;
    }
    // FIN goes after the last data byte.
    let all_data_sent = t.send_buf.len() as u32 == t.inflight_data();
    if t.fin_queued && !t.fin_sent && all_data_sent {
        let seq = t.snd_nxt;
        ctx.emit(t, seq, t.rcv_nxt, FIN | ACK, None, &[]);
        t.fin_sent = true;
        t.snd_nxt = t.snd_nxt.wrapping_add(1);
        t.state = match t.state {
            State::CloseWait => State::LastAck,
            _ => State::FinWait1,
        };
        if t.rexmit_at.is_none() {
            ctx.arm(t);
        }
    }
}

/// Resend the oldest unacknowledged segment.
fn retransmit_oldest(t: &mut Tcb, ctx: &mut Ctx<'_>) {
    ctx.stats.retransmits += 1;
    match t.state {
        State::SynSent | State::SynReceived => ctx.send_syn(t),
        _ => {
            let data_in_flight = t.inflight_data();
            if data_in_flight > 0 {
                let len = data_in_flight.min(t.mss) as usize;
                let payload: Vec<u8> = t.send_buf.iter().take(len).copied().collect();
                ctx.emit(t, t.snd_una, t.rcv_nxt, ACK, None, &payload);
            } else if t.fin_sent {
                ctx.emit(t, t.snd_una, t.rcv_nxt, FIN | ACK, None, &[]);
            }
        }
    }
}

fn timers(t: &mut Tcb, ctx: &mut Ctx<'_>) {
    if let Some(d) = t.deadline {
        if ctx.now >= d {
            match t.state {
                State::TimeWait => enter_closed(t, None),
                _ if t.owner == Owner::Orphan => enter_closed(t, Some(TcpError::TimedOut)),
                _ => t.deadline = None,
            }
            return;
        }
    }
    let Some(at) = t.rexmit_at else { return };
    if ctx.now < at {
        return;
    }
    let syn_phase = matches!(t.state, State::SynSent | State::SynReceived);
    let flight = t.snd_nxt.wrapping_sub(t.snd_una);
    if !syn_phase && t.snd_wnd == 0 && flight <= 1 && t.send_buf.len() as u32 > 0 {
        // Persist timer: the window is shut. Probe again; this is not a failure.
        let interval = ((t.rto as u64) << t.persist_shift).min(MAX_RTO_MS as u64);
        t.persist_shift = (t.persist_shift + 1).min(6);
        let len = 1.min(t.send_buf.len());
        let payload: Vec<u8> = t.send_buf.iter().take(len).copied().collect();
        let seq = t.snd_una;
        ctx.emit(t, seq, t.rcv_nxt, ACK, None, &payload);
        t.rexmit_at = Some(ctx.now + interval);
        return;
    }
    t.retries += 1;
    let limit = if syn_phase { SYN_RETRIES } else { DATA_RETRIES };
    if t.retries > limit {
        enter_closed(t, Some(TcpError::TimedOut));
        return;
    }
    // Timeout: collapse the congestion window, back off the timer, resend the oldest segment.
    let flight_bytes = flight.max(2 * t.mss);
    t.ssthresh = (flight_bytes / 2).max(2 * t.mss);
    t.cwnd = t.mss;
    t.dupacks = 0;
    t.in_recovery = false;
    t.rtt_probe = None; // Karn: a retransmitted segment gives no round-trip sample
    t.rto = (t.rto * 2).min(MAX_RTO_MS);
    retransmit_oldest(t, ctx);
    ctx.arm(t);
}

/// The big one: RFC 9293 3.10.7.
fn segment_arrives(t: &mut Tcb, seg: &Segment<'_>, ctx: &mut Ctx<'_>) {
    match t.state {
        State::Closed => return,
        State::SynSent => return syn_sent_arrives(t, seg, ctx),
        _ => {}
    }

    // A repeated SYN while we wait for the final ACK of the handshake: our SYN-ACK was lost.
    if t.state == State::SynReceived && seg.flags & (SYN | ACK | RST) == SYN && seg.seq == t.irs {
        ctx.send_syn(t);
        return;
    }

    // 1. Is any of it inside the window we are willing to accept?
    let wnd = t.window();
    let seg_len = seg.seq_len();
    let acceptable = if seg_len == 0 {
        if wnd == 0 {
            seg.seq == t.rcv_nxt
        } else {
            seq_in(t.rcv_nxt, seg.seq, t.rcv_nxt.wrapping_add(wnd))
        }
    } else if wnd == 0 {
        false
    } else {
        let end = seg.seq.wrapping_add(seg_len).wrapping_sub(1);
        let hi = t.rcv_nxt.wrapping_add(wnd);
        seq_in(t.rcv_nxt, seg.seq, hi) || seq_in(t.rcv_nxt, end, hi) || (seq_lt(seg.seq, t.rcv_nxt) && seq_ge(end, t.rcv_nxt))
    };
    if !acceptable {
        if seg.flags & RST == 0 && t.state != State::TimeWait {
            ctx.send_ack(t);
        } else if t.state == State::TimeWait && seg.flags & RST == 0 {
            ctx.send_ack(t);
        }
        return;
    }

    // 2. Reset.
    if seg.flags & RST != 0 {
        if seg.seq == t.rcv_nxt {
            let err = if t.state == State::SynReceived { None } else { Some(TcpError::Reset) };
            enter_closed(t, err);
        } else {
            ctx.send_ack(t); // in the window but not exact: could be forged, challenge it
        }
        return;
    }

    // 3. A SYN inside the window is an error in an established connection.
    if seg.flags & SYN != 0 {
        if t.state != State::SynReceived || seg.seq != t.irs {
            ctx.send_ack(t);
        }
        return;
    }

    // 4. Acknowledgement.
    if seg.flags & ACK == 0 {
        return;
    }
    if t.state == State::SynReceived {
        if seq_lt(t.snd_una, seg.ack) && seq_le(seg.ack, t.snd_nxt) {
            t.state = State::Established;
            t.snd_una = seg.ack;
            t.snd_wnd = seg.window as u32;
            t.snd_wl1 = seg.seq;
            t.snd_wl2 = seg.ack;
            t.retries = 0;
            t.rexmit_at = None;
        } else {
            let reset = Segment { sport: t.local_port, dport: t.remote_port, seq: seg.ack, ack: 0, flags: RST, window: 0, mss: None, payload: &[] };
            ctx.out.push(Tx { dst: t.remote_ip, segment: build(ctx.ip, t.remote_ip, &reset) });
            ctx.stats.segments_out += 1;
            ctx.stats.resets_sent += 1;
            return;
        }
    } else {
        if !process_ack(t, seg, ctx) {
            return;
        }
    }

    // 5. Data and FIN.
    let may_receive = matches!(t.state, State::Established | State::FinWait1 | State::FinWait2);
    let mut need_ack = false;
    if may_receive && (!seg.payload.is_empty() || seg.flags & FIN != 0) {
        need_ack = receive_data(t, seg);
    } else if matches!(t.state, State::CloseWait | State::Closing | State::LastAck | State::TimeWait)
        && (!seg.payload.is_empty() || seg.flags & FIN != 0)
    {
        // Data after the other side's FIN is not valid: just acknowledge what we have.
        need_ack = true;
    }

    // State changes caused by a FIN that is now in sequence.
    if t.fin_received && matches!(t.state, State::Established | State::FinWait1 | State::FinWait2) {
        match t.state {
            State::Established => t.state = State::CloseWait,
            State::FinWait1 => {
                // Our FIN is not acknowledged yet (otherwise process_ack made us FinWait2).
                t.state = State::Closing;
            }
            State::FinWait2 => {
                t.state = State::TimeWait;
                t.deadline = Some(ctx.now + TIME_WAIT_MS);
                t.rexmit_at = None;
            }
            _ => {}
        }
    }
    if t.state == State::TimeWait && seg.flags & FIN != 0 {
        t.deadline = Some(ctx.now + TIME_WAIT_MS);
        need_ack = true;
    }

    if need_ack {
        ctx.send_ack(t);
    }
    output(t, ctx);
}

fn syn_sent_arrives(t: &mut Tcb, seg: &Segment<'_>, ctx: &mut Ctx<'_>) {
    let ack_ok = seg.flags & ACK != 0 && seq_lt(t.iss, seg.ack) && seq_le(seg.ack, t.snd_nxt);
    if seg.flags & ACK != 0 && !ack_ok {
        if seg.flags & RST == 0 {
            let reset = Segment { sport: t.local_port, dport: t.remote_port, seq: seg.ack, ack: 0, flags: RST, window: 0, mss: None, payload: &[] };
            ctx.out.push(Tx { dst: t.remote_ip, segment: build(ctx.ip, t.remote_ip, &reset) });
            ctx.stats.segments_out += 1;
            ctx.stats.resets_sent += 1;
        }
        return;
    }
    if seg.flags & RST != 0 {
        if ack_ok {
            enter_closed(t, Some(TcpError::Refused));
        }
        return;
    }
    if seg.flags & SYN == 0 {
        return;
    }
    t.irs = seg.seq;
    t.rcv_nxt = seg.seq.wrapping_add(1);
    t.mss = seg.mss.unwrap_or(DEFAULT_PEER_MSS).clamp(64, MSS) as u32;
    t.cwnd = INITIAL_CWND_SEGMENTS * t.mss;
    t.snd_wnd = seg.window as u32;
    t.snd_wl1 = seg.seq;
    if ack_ok {
        t.snd_una = seg.ack;
        t.snd_wl2 = seg.ack;
        t.state = State::Established;
        t.retries = 0;
        t.rexmit_at = None;
        if let Some((_, at)) = t.rtt_probe.take() {
            update_rtt(t, ctx.now.saturating_sub(at).min(u32::MAX as u64) as u32);
        }
        ctx.send_ack(t);
        output(t, ctx);
    } else {
        // Simultaneous open: both sides sent a SYN.
        t.state = State::SynReceived;
        ctx.send_syn(t);
        ctx.arm(t);
    }
}

/// Process the acknowledgement field. False means the segment must not be processed further.
fn process_ack(t: &mut Tcb, seg: &Segment<'_>, ctx: &mut Ctx<'_>) -> bool {
    if seq_gt(seg.ack, t.snd_nxt) {
        // Acknowledges something we have not sent.
        ctx.send_ack(t);
        return false;
    }
    let flight_before = t.snd_nxt.wrapping_sub(t.snd_una);
    if seq_gt(seg.ack, t.snd_una) {
        let acked = seg.ack.wrapping_sub(t.snd_una);
        let fin_acked = t.fin_sent && seg.ack == t.snd_nxt;
        let data_acked = (acked - fin_acked as u32) as usize;
        let drop_n = data_acked.min(t.send_buf.len());
        t.send_buf.drain(..drop_n);
        t.snd_una = seg.ack;
        t.retries = 0;
        t.dupacks = 0;
        if let Some((end, at)) = t.rtt_probe {
            if seq_ge(seg.ack, end) {
                update_rtt(t, ctx.now.saturating_sub(at).min(u32::MAX as u64) as u32);
                t.rtt_probe = None;
            }
        }
        // Congestion control.
        if t.in_recovery {
            if seq_ge(seg.ack, t.recover) {
                t.cwnd = t.ssthresh.max(t.mss);
                t.in_recovery = false;
            } else {
                // Partial acknowledgement (NewReno): the next hole is lost as well.
                t.cwnd = t.cwnd.saturating_sub(acked).saturating_add(t.mss).max(t.mss);
                ctx.stats.fast_retransmits += 1;
                retransmit_oldest(t, ctx);
            }
        } else if t.cwnd < t.ssthresh {
            t.cwnd = t.cwnd.saturating_add(acked.min(t.mss));
        } else {
            t.cwnd = t.cwnd.saturating_add((t.mss * t.mss / t.cwnd.max(1)).max(1));
        }
        // Timer: restart while data is outstanding, stop otherwise.
        if t.snd_una == t.snd_nxt {
            t.rexmit_at = None;
        } else {
            ctx.arm(t);
        }
        if fin_acked {
            match t.state {
                State::FinWait1 => {
                    t.state = if t.fin_received { State::TimeWait } else { State::FinWait2 };
                    if t.state == State::TimeWait {
                        t.deadline = Some(ctx.now + TIME_WAIT_MS);
                    }
                }
                State::Closing => {
                    t.state = State::TimeWait;
                    t.deadline = Some(ctx.now + TIME_WAIT_MS);
                }
                State::LastAck => {
                    enter_closed(t, None);
                    return false;
                }
                _ => {}
            }
            if t.state == State::FinWait2 && t.owner == Owner::Orphan {
                t.deadline = Some(ctx.now + ORPHAN_FIN_WAIT_MS);
            }
        }
    } else if seg.ack == t.snd_una
        && seg.payload.is_empty()
        && seg.flags & (SYN | FIN) == 0
        && flight_before > 0
        && seg.window as u32 == t.snd_wnd
    {
        // A duplicate acknowledgement: the receiver got something out of order.
        t.dupacks += 1;
        if t.dupacks == 3 && !t.in_recovery {
            t.ssthresh = (flight_before / 2).max(2 * t.mss);
            t.cwnd = t.ssthresh + 3 * t.mss;
            t.in_recovery = true;
            t.recover = t.snd_nxt;
            t.rtt_probe = None;
            ctx.stats.fast_retransmits += 1;
            retransmit_oldest(t, ctx);
            ctx.arm(t);
        } else if t.in_recovery {
            t.cwnd = t.cwnd.saturating_add(t.mss);
        }
    }
    // Window update (RFC 9293: only from a segment that is not older than the last update).
    if seq_lt(t.snd_wl1, seg.seq) || (t.snd_wl1 == seg.seq && seq_le(t.snd_wl2, seg.ack)) {
        let was_shut = t.snd_wnd == 0;
        t.snd_wnd = seg.window as u32;
        if t.snd_wnd > 0 {
            t.persist_shift = 0;
            // The probe byte (and anything else sent into a shut window) was thrown away by the
            // receiver: send it again now instead of waiting for the retransmission timer.
            if was_shut && t.snd_nxt != t.snd_una && t.synchronized() {
                retransmit_oldest(t, ctx);
                ctx.arm(t);
            }
        }
        t.snd_wl1 = seg.seq;
        t.snd_wl2 = seg.ack;
    }
    true
}

/// Take in the data (and FIN) of a segment. True if an acknowledgement should go out.
fn receive_data(t: &mut Tcb, seg: &Segment<'_>) -> bool {
    let mut seq = seg.seq;
    let mut data = seg.payload;
    // Cut off what we already have.
    if seq_lt(seq, t.rcv_nxt) {
        let skip = t.rcv_nxt.wrapping_sub(seq) as usize;
        if skip >= data.len() {
            data = &[];
        } else {
            data = &data[skip..];
        }
        seq = t.rcv_nxt;
    }
    let fin_at = seg.seq.wrapping_add(seg.payload.len() as u32);
    let free = t.window() as usize;
    let mut accepted_all = true;
    if data.len() > free {
        data = &data[..free];
        accepted_all = false;
    }
    let mut need_ack = !data.is_empty() || seg.flags & FIN != 0;

    if seq == t.rcv_nxt {
        if !data.is_empty() {
            t.recv_buf.extend(data);
            t.rcv_nxt = t.rcv_nxt.wrapping_add(data.len() as u32);
        }
        merge_out_of_order(t);
        if seg.flags & FIN != 0 && accepted_all {
            if fin_at == t.rcv_nxt {
                t.rcv_nxt = t.rcv_nxt.wrapping_add(1);
                t.fin_received = true;
            } else {
                t.fin_ooo = Some(fin_at);
            }
        }
        if let Some(f) = t.fin_ooo {
            if f == t.rcv_nxt && !t.fin_received {
                t.rcv_nxt = t.rcv_nxt.wrapping_add(1);
                t.fin_received = true;
            }
        }
    } else if seq_gt(seq, t.rcv_nxt) {
        // Ahead of what we expect: keep it for when the gap is filled, and say what we are
        // missing (a duplicate acknowledgement) so the sender can retransmit quickly.
        if !data.is_empty() && t.ooo.len() < MAX_OOO_SEGMENTS && t.ooo_bytes() + data.len() <= RECV_CAP && t.ooo.iter().all(|(s, d)| !(*s == seq && d.len() >= data.len())) {
            t.ooo.push((seq, data.to_vec()));
        }
        if seg.flags & FIN != 0 && accepted_all {
            t.fin_ooo = Some(fin_at);
        }
        need_ack = true;
    }
    need_ack
}

fn merge_out_of_order(t: &mut Tcb) {
    loop {
        let mut progressed = false;
        let mut i = 0;
        while i < t.ooo.len() {
            let (start, ref data) = t.ooo[i];
            let end = start.wrapping_add(data.len() as u32);
            if seq_le(end, t.rcv_nxt) {
                t.ooo.swap_remove(i); // completely old
                progressed = true;
                continue;
            }
            if seq_le(start, t.rcv_nxt) {
                let skip = t.rcv_nxt.wrapping_sub(start) as usize;
                let room = RECV_CAP.saturating_sub(t.recv_buf.len());
                let take = (data.len() - skip).min(room);
                t.recv_buf.extend(&data[skip..skip + take]);
                t.rcv_nxt = t.rcv_nxt.wrapping_add(take as u32);
                t.ooo.swap_remove(i);
                progressed = true;
                continue;
            }
            i += 1;
        }
        if !progressed {
            break;
        }
    }
}

#[cfg(test)]
#[path = "tcp_tests.rs"]
mod tests;
