#!/usr/bin/env python3
"""Injected bugs for the network stack and its kernel/system-call layer.

  python3 scripts/mutations_net.py --check        only verify the edits still apply
  python3 scripts/mutations_net.py                run them all (about an hour)
  python3 scripts/mutations_net.py arp udp        run the ones whose names contain these words

Needs a built project; work happens in a copy (../novaos-mut by default).
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mutlib import Mutation, main


def M(name, edits, target, expect=None):
    # Host unit tests finish in seconds; one that runs for minutes is hung by the bug.
    return Mutation(name, edits, target, expect, timeout=150 if target == "test-net-stack" else 900)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORK = os.environ.get("MUT_WORK", os.path.join(os.path.dirname(ROOT), "novaos-mut"))

W = "net/src/wire.rs"
S = "net/src/stack.rs"
D = "net/src/dhcp.rs"
N = "net/src/dns.rs"
NS = "test-net-stack"

MUTATIONS = [
    # ---- wire formats (host unit tests)
    M("checksum: carries folded only once", [(W, "while s >> 16 != 0 {", "if s >> 16 != 0 {")], NS, "checksum"),
    M("pseudo-header without the protocol", [(W, "s += proto as u64;", "s += 0;")], NS, "udp"),
    M("ipv4: header checksum not verified", [(W, "if checksum(&b[..header_len]) != 0 {", "if false {")], NS, "ipv4"),
    M("ipv4: more-fragments flag ignored", [(W, "if frag & 0x3FFF != 0 {", "if frag & 0x1FFF != 0 {")], NS, "ipv4"),
    M("ipv4: Ethernet padding counted as data", [(W, "payload: &b[header_len..total],", "payload: &b[header_len..],")], NS, "ipv4"),
    M("udp: checksum not verified", [(W, "if sent != 0 && transport_checksum(src, dst, PROTO_UDP, seg) != 0 {", "if false {")], NS, "udp"),
    M("ethernet: frames not padded to 60", [(W, "if f.len() < ETH_MIN_FRAME {", "if false {")], NS, "ethernet"),
    M("arp: hardware type not checked", [(W, "if htype != 1 || ptype != ETHERTYPE_IPV4 || b[4] != 6 || b[5] != 4 {", "if false {")], NS, "arp"),
    # ---- stack logic (host unit tests)
    M("arp: learns from every broadcast", [(S, "if for_us || known {", "if true {")], NS, "arp_cache_is_not_poisoned"),
    M("arp: entries never expire", [(S, ".find(|e| e.ip == ip && e.expires > now)", ".find(|e| e.ip == ip)")], NS, "arp_cache_expires"),
    M("arp: five tries instead of four", [(S, "const ARP_TRIES: u8 = 4;", "const ARP_TRIES: u8 = 5;")], NS, "unanswered_arp"),
    M("icmp: answers broadcast pings", [(S, "ICMP_ECHO_REQUEST if unicast_to_us && m.code == 0 => {", "ICMP_ECHO_REQUEST if m.code == 0 => {")], NS, "broadcast_ping"),
    M("icmp: no port-unreachable", [(S, "} else if unicast_to_us {", "} else if false {")], NS, "closed_port"),
    M("ip: accepts packets for other hosts", [(S, "if !unicast_to_us && !broadcast && !dhcp_waiting {", "if false {")], NS, "corrupted_and_misaddressed"),
    M("ethernet: accepts frames for other MACs", [(S, "if eth.dst != self.mac && eth.dst != BROADCAST_MAC {", "if false {")], NS, "corrupted_and_misaddressed"),
    M("routing: off-link traffic not sent to the gateway", [(S, "cfg.gateway.ok_or(NetError::NoRoute)", "Ok(dst)")], NS, "off_link"),
    M("udp: receive queue twice as long", [(S, "if q.len() >= UDP_QUEUE_MAX {", "if q.len() >= UDP_QUEUE_MAX * 2 {")], NS, "udp_queue_overflow"),
    M("udp: busy ephemeral port handed out again", [(S, "if !self.udp.contains_key(&p) {", "if true {")], NS, "udp_ports"),
    M("dhcp: xid not checked", [(S, "Dhcp::Discovering { xid, .. } if r.xid == xid && r.msg == MsgType::Offer => {", "Dhcp::Discovering { xid, .. } if r.msg == MsgType::Offer => {")], NS, "dhcp_ignores_wrong_xid"),
    M("dhcp: client address not checked", [(S, "if r.chaddr != self.mac {", "if false {")], NS, "dhcp_ignores_wrong_xid"),
    M("dhcp: renews at the end of the lease", [(S, "renew_at: now + lease_ms / 2,", "renew_at: now + lease_ms,")], NS, "dhcp_lease"),
    M("dhcp: retransmit backoff capped too early", [(S, "1000u64 << tries.min(4)", "1000u64 << tries.min(2)")], NS, "dhcp_retransmits"),
    M("dhcp: magic cookie not checked", [(D, "if b[HEADER..HEADER + 4] != COOKIE {", "if false {")], NS, "dhcp_parser_rejects"),
    M("dns: response id not checked", [(N, "if u16::from_be_bytes([b[0], b[1]]) != id {", "if false {")], NS, "dns_rejects_bad_responses"),
    M("dns: name compression not understood", [(N, "if len & 0xC0 == 0xC0 {", "if false {")], NS, "dns"),
    # ---- TCP (host unit tests)
    M("tcp: sequence numbers compared without wrap-around", [("net/src/tcp.rs", "(a.wrapping_sub(b) as i32) < 0", "a < b")], NS, "wrap"),
    M("tcp: out-of-order data shrinks the window", [("net/src/tcp.rs", "RECV_CAP.saturating_sub(self.recv_buf.len()) as u32", "RECV_CAP.saturating_sub(self.recv_buf.len() + self.ooo_bytes()) as u32")], NS, "fast_retransmit"),
    M("tcp: fast retransmit after two duplicate ACKs", [("net/src/tcp.rs", "if t.dupacks == 3 && !t.in_recovery {", "if t.dupacks == 2 && !t.in_recovery {")], NS, "reordering"),
    M("tcp: minimum retransmission timeout 50 ms", [("net/src/tcp.rs", "const MIN_RTO_MS: u32 = 200;", "const MIN_RTO_MS: u32 = 50;")], NS, "last_segment"),
    M("tcp: retransmitted segments give round-trip samples", [("net/src/tcp.rs", "t.rtt_probe = None; // Karn: a retransmitted segment gives no round-trip sample", "")], NS, "round_trip_estimate"),
    M("tcp: no exponential backoff", [("net/src/tcp.rs", "t.rto = (t.rto * 2).min(MAX_RTO_MS);\n    retransmit_oldest(t, ctx);", "retransmit_oldest(t, ctx);")], NS, "syn_retransmissions"),
    M("tcp: timeout does not lower ssthresh", [("net/src/tcp.rs", "t.ssthresh = (flight_bytes / 2).max(2 * t.mss);", "")], NS, "round_trip_estimate"),
    M("tcp: window updates ignored", [("net/src/tcp.rs", "if seq_lt(t.snd_wl1, seg.seq) || (t.snd_wl1 == seg.seq && seq_le(t.snd_wl2, seg.ack)) {", "if false {")], NS, "tcp::tests"),
    M("tcp: TIME-WAIT lasts 3 s", [("net/src/tcp.rs", "const TIME_WAIT_MS: u64 = 30_000;", "const TIME_WAIT_MS: u64 = 3_000;")], NS, "closing_in_both"),
    M("tcp: any reset in the window is obeyed", [("net/src/tcp.rs", "if seg.seq == t.rcv_nxt {\n            let err =", "if true {\n            let err =")], NS, "resets_are_only"),
    M("tcp: segments outside the window are processed", [("net/src/tcp.rs", "    if !acceptable {", "    if false {")], NS, "outside_the_window"),
    M("tcp: accept backlog ignored", [("net/src/tcp.rs", "if pending >= backlog || self.conns.len() >= MAX_CONNECTIONS {", "if self.conns.len() >= MAX_CONNECTIONS {")], NS, "backlog"),
    M("tcp: close with unread data is graceful", [("net/src/tcp.rs", "|| (unread && t.synchronized())", "")], NS, "unread"),
    M("tcp: no zero-window probe", [("net/src/tcp.rs", "let probe = t.snd_wnd == 0 && flight == 0;", "let probe = false;")], NS, "window_update"),
    M("tcp: out-of-order data never merged", [("net/src/tcp.rs", "        merge_out_of_order(t);\n        if seg.flags & FIN != 0 && accepted_all {", "        if seg.flags & FIN != 0 && accepted_all {")], NS, "tcp::tests"),
    M("tcp: acknowledged data stays in the send buffer", [("net/src/tcp.rs", "t.send_buf.drain(..drop_n);", "")], NS, "tcp::tests"),
    M("tcp: checksum not verified", [("net/src/tcp.rs", "if transport_checksum(src, dst, PROTO_TCP, b) != 0 {", "if false {")], NS, "segment_parser"),
    M("tcp: peer MSS taken at face value", [("net/src/tcp.rs", "t.mss = seg.mss.unwrap_or(DEFAULT_PEER_MSS).clamp(64, MSS) as u32;\n        t.cwnd = INITIAL_CWND_SEGMENTS * t.mss;\n        let ip = self.ip;", "t.mss = seg.mss.unwrap_or(DEFAULT_PEER_MSS) as u32;\n        t.cwnd = INITIAL_CWND_SEGMENTS * t.mss;\n        let ip = self.ip;")], NS, "hostile_mss"),
    M("tcp: a FIN is never acknowledged-for (states stuck)", [("net/src/tcp.rs", "let fin_acked = t.fin_sent && seg.ack == t.snd_nxt;", "let fin_acked = false;")], NS, "tcp::tests"),
    # ---- TCP in the kernel and the programs (end to end)
    M("tcp send: returns after the first partial write", [("src/net.rs", "                Ok(n) => done += n,\n                Err(TcpError::WouldBlock) => wait_activity(seen, None),", "                Ok(n) => {\n                    done += n;\n                    break;\n                }\n                Err(TcpError::WouldBlock) => wait_activity(seen, None),")], "test-net", "tcp"),
    M("tcp accept: bad address pointer not checked first", [("src/syscall_net.rs", "    if addr_out != 0 {\n        if let Err(e) = walk_user(addr_out, 8, true, |_, _| {}) {\n            return e;\n        }\n    }\n    let (sock, ip, port)", "    let (sock, ip, port)")], "test-net", "bad address pointer"),
    M("tcp socket: connection not closed when the socket goes away", [("src/net.rs", "                with_tcp(|t, now| t.close(id, now));\n            }\n            _ => {}", "            }\n            _ => {}")], "test-net", "tcp"),
    M("httpd: path traversal allowed", [("user/httpd/src/main.rs", "&& !t.contains(\"..\")", "")], "test-net", "../etc/motd"),
    M("httpd: closes with unread data (no lingering close)", [("user/httpd/src/main.rs", "    shutdown(conn, SHUT_WR);\n    let mut sink", "    let mut sink")], "test-net", "431"),
    # ---- the kernel's driver and system calls (end to end through the shell)
    M("card: receive buffers never given back", [("src/virtio_net.rs", "self.rx.push_avail(id);", "")], "test-net", "300 sent"),
    M("card: transmit buffers never reclaimed", [("src/virtio_net.rs", "self.tx_free.push(id);", "{}")], "test-net", "300 sent"),
    M("recvfrom: user buffer checked after the datagram is taken", [("src/syscall_net.rs", "if let Err(e) = walk_user(ptr, len, true, |_, _| {}) {", "if false {")], "test-net", "datagram survived"),
    M("socket: port not released on close", [("src/net.rs", "with_stack(|s, _| s.udp_unbind(port));", "let _ = port;")], "test-net", "closing"),
    M("sendto: payload cut to 100 bytes", [("src/syscall_net.rs", "let data = match copy_from_user(ptr, len) {", "let data = match copy_from_user(ptr, len.min(100)) {")], "test-net", "echo has the same length"),
    M("recvfrom: timeout reported as would-block", [("src/net.rs", "continue; // a connected socket ignores strangers\n            }\n            if expired(deadline) {\n                return Err(if timeout_ticks == Some(0) { EAGAIN } else { ETIMEDOUT });", "continue; // a connected socket ignores strangers\n            }\n            if expired(deadline) {\n                return Err(EAGAIN);")], "test-net", "waiting 5 ticks"),
    M("clock: runs twice as fast", [("src/timer.rs", "let khz = (end - start) / MEASURE_MS;", "let khz = (end - start) / (MEASURE_MS * 2);")], "test-net", "microsecond clock"),
    M("ping: round trip always reads zero", [("src/net.rs", "return Ok(timer::now_us().saturating_sub(t0));", "return Ok(0);")], "test", "implausible round trip"),
]

if __name__ == "__main__":
    sys.exit(main(ROOT, WORK, MUTATIONS))
