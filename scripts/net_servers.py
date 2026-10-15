#!/usr/bin/env python3
"""Servers the network tests talk to, running on the build machine.

The emulated machine reaches the build machine as 10.0.2.2 (QEMU's user-mode network).
Written independently of the OS's own network code, so the two check each other:

  * a UDP echo server: sends every datagram straight back
  * a tiny DNS server: alpha.test -> 192.0.2.10, beta.test -> 192.0.2.20, anything else
    NXDOMAIN (name case is ignored, as in real DNS)
  * a TCP echo server and a TCP "discard and count" server (used by later stages)

usage: net_servers.py ECHO_PORT DNS_PORT [TCP_ECHO_PORT [TCP_SINK_PORT]] --log FILE --ready FILE

Every request is appended to the log file as one line, so the test script can check
that the OS really sent what it claims. The ready file is created once all servers listen.
"""

import argparse
import sys
import socket
import struct
import threading

ANSWERS = {"alpha.test": "192.0.2.10", "beta.test": "192.0.2.20"}

log_lock = threading.Lock()
log_file = None


def log(line):
    with log_lock:
        log_file.write(line + "\n")
        log_file.flush()


def udp_echo(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    while True:
        data, addr = s.recvfrom(4096)
        log(f"echo {len(data)}")
        s.sendto(data, addr)


def parse_question(packet):
    """(name, offset after the question) or None if the packet is not a query."""
    if len(packet) < 12:
        return None
    flags, qd = struct.unpack(">HH", packet[2:6])
    if flags & 0x8000 or qd != 1:
        return None
    labels, pos = [], 12
    while True:
        if pos >= len(packet):
            return None
        n = packet[pos]
        if n == 0:
            pos += 1
            break
        if n & 0xC0:
            return None
        labels.append(packet[pos + 1:pos + 1 + n].decode("ascii", "replace"))
        pos += 1 + n
    if pos + 4 > len(packet):
        return None
    return ".".join(labels), pos + 4


def dns_server(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    while True:
        packet, addr = s.recvfrom(1500)
        parsed = parse_question(packet)
        if parsed is None:
            log("dns malformed")
            continue
        name, end = parsed
        question = packet[12:end]
        qid = packet[:2]
        ip = ANSWERS.get(name.lower())
        log(f"dns {name} -> {ip or 'NXDOMAIN'}")
        if ip is None:
            reply = qid + struct.pack(">HHHHH", 0x8183, 1, 0, 0, 0) + question
        else:
            reply = (qid + struct.pack(">HHHHH", 0x8180, 1, 1, 0, 0) + question +
                     b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 60, 4) + socket.inet_aton(ip))
        s.sendto(reply, addr)


def tcp_serve(port, handler):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(16)
    while True:
        conn, addr = s.accept()
        threading.Thread(target=handler, args=(conn,), daemon=True).start()


def tcp_echo_handler(conn):
    # No Nagle: the echo must go back at once, or it waits for the emulated network's delayed ACK.
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    total = 0
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            total += len(data)
            conn.sendall(data)
    except OSError:
        pass
    finally:
        log(f"tcp-echo closed after {total}")
        conn.close()


def tcp_sink_handler(conn):
    """Count the bytes, answer with the count and a checksum when the client shuts down its side."""
    total, checksum = 0, 0
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            total += len(data)
            checksum = (checksum + sum(data)) & 0xFFFFFFFF
        conn.sendall(f"{total} {checksum}\n".encode())
    except OSError:
        pass
    finally:
        log(f"tcp-sink {total} {checksum}")
        conn.close()




# ---- an HTTP server for the `fetch` tests -------------------------------------------------------

def big_body():
    """3 MiB with a pattern that is not periodic at any small size."""
    return bytes(((i * 2654435761) >> 13) & 255 for i in range(3 * 1024 * 1024))


def http_server(port):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    big = big_body()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, fmt, *args):
            log("http " + (fmt % args))

        def send_body(self, status, body, ctype="text/plain", headers=()):
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            for k, v in headers:
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            p = self.path
            if p == "/small":
                self.send_body(200, b"small file from the host\n")
            elif p == "/big":
                self.send_body(200, big, "application/octet-stream")
            elif p == "/redir":
                self.send_body(302, b"", headers=[("Location", "/small")])
            elif p == "/redir-loop":
                self.send_body(302, b"", headers=[("Location", "/redir-loop")])
            elif p == "/empty":
                self.send_body(200, b"")
            elif p == "/missing":
                self.send_body(404, b"nothing here\n")
            elif p == "/slow":
                import time
                self.send_response(200)
                self.send_header("Content-Length", "10")
                self.end_headers()
                for _ in range(10):
                    self.wfile.write(b"x")
                    self.wfile.flush()
                    time.sleep(0.1)
            elif p == "/no-length":
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"no content length, ends when the connection closes\n")
            else:
                self.send_body(404, b"unknown\n")

    s = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    s.daemon_threads = True
    s.serve_forever()


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--big-crc":
        import zlib
        b = big_body()
        print(f"{zlib.crc32(b):08x} {len(b)}")
        return
    global log_file
    p = argparse.ArgumentParser()
    p.add_argument("echo_port", type=int)
    p.add_argument("dns_port", type=int)
    p.add_argument("tcp_echo_port", type=int, nargs="?")
    p.add_argument("tcp_sink_port", type=int, nargs="?")
    p.add_argument("--http-port", type=int)
    p.add_argument("--log", required=True)
    p.add_argument("--ready", required=True)
    a = p.parse_args()
    log_file = open(a.log, "a")
    threads = [threading.Thread(target=udp_echo, args=(a.echo_port,), daemon=True),
               threading.Thread(target=dns_server, args=(a.dns_port,), daemon=True)]
    if a.tcp_echo_port:
        threads.append(threading.Thread(target=tcp_serve, args=(a.tcp_echo_port, tcp_echo_handler), daemon=True))
    if a.http_port:
        threads.append(threading.Thread(target=http_server, args=(a.http_port,), daemon=True))
    if a.tcp_sink_port:
        threads.append(threading.Thread(target=tcp_serve, args=(a.tcp_sink_port, tcp_sink_handler), daemon=True))
    for t in threads:
        t.start()
    # Give the sockets a moment to bind (a bind failure would end its thread silently).
    import time
    time.sleep(0.3)
    if not all(t.is_alive() for t in threads):
        print("a server failed to start (port in use?)")
        raise SystemExit(1)
    open(a.ready, "w").write("ready\n")
    for t in threads:
        t.join()


if __name__ == "__main__":
    main()
