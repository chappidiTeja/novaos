#!/usr/bin/env python3
"""Clients that test servers running ON NovaOS, reached through QEMU's port forwarding.

  net_clients.py httpd-count              print how many connections the www test makes
  net_clients.py httpd PORT               test the guest's web server (docroot /www)
  net_clients.py httpd-root PORT          test it serving the whole disk (large file integrity)
  net_clients.py tcp-serve PORT           test `nettest tcp-serve`: half-close, megabyte echo, resets

Exit status 0 only if every check passed. Written independently of the OS's own code.
"""

import socket
import sys
import threading
import time
import zlib

failures = 0


def check(ok, what):
    global failures
    if ok:
        print(f"  ok: {what}", flush=True)
    else:
        failures += 1
        print(f"  FAIL: {what}", flush=True)


def connect(port, timeout=15):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.settimeout(timeout)
    return s


def read_all(s):
    data = b""
    while True:
        chunk = s.recv(65536)
        if not chunk:
            return data
        data += chunk


def parse_response(raw):
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = int(lines[0].split(" ")[1])
    headers = {}
    for line in lines[1:]:
        k, _, v = line.partition(":")
        headers[k.strip().lower()] = v.strip()
    return status, headers, body


def request(port, raw_request, expect_body=True):
    s = connect(port)
    s.sendall(raw_request)
    data = read_all(s)
    s.close()
    return parse_response(data)


def get(port, path, method="GET", version="HTTP/1.0"):
    return request(port, f"{method} {path} {version}\r\nHost: test\r\n\r\n".encode())


# ---- the web server on the guest ----------------------------------------------------------------

HTTPD_REQUESTS = 41


def test_httpd(port):
    st, h, body = get(port, "/")
    check(st == 200 and b"Hello from NovaOS" in body, "GET / serves index.html")
    check(h.get("content-type", "").startswith("text/html") and int(h["content-length"]) == len(body), "with the right type and length")
    check(h.get("server") == "NovaOS-httpd/0.1" and h.get("connection") == "close", "and the server headers")
    st, h, body = get(port, "/about.html")
    check(st == 200 and b"TCP/IP stack" in body, "GET /about.html")
    st, h, body = get(port, "/style.css")
    check(st == 200 and h["content-type"] == "text/css" and b"font-family" in body, "css has its own content type")
    st, h, body = get(port, "/hello.txt")
    check(st == 200 and body == b"Plain text served by NovaOS.\n" and h["content-type"].startswith("text/plain"), "a plain text file, exactly")
    st, h, body = get(port, "/nope.html")
    check(st == 404 and b"404" in body, "a missing file is a 404")
    st, h, body = get(port, "/../etc/motd")
    check(st == 400, "a path with .. is refused")
    st, h, body = get(port, "/a//b")
    check(st == 400, "so is an empty path segment")
    st, h, body = get(port, "/", method="POST")
    check(st == 405, "POST is not allowed")
    st, h, body = get(port, "/index.html", method="HEAD")
    st2, h2, body2 = get(port, "/index.html")
    check(st == 200 and body == b"" and h["content-length"] == h2["content-length"] == str(len(body2)), "HEAD sends the headers of a GET and no body")
    st, h, body = get(port, "/index.html?x=1&y=2")
    check(st == 200 and b"Hello from NovaOS" in body, "a query string is ignored")
    st, h, body = get(port, "/%2e%2e/etc/motd")
    check(st == 404, "an encoded .. is just a name that does not exist")
    st, h, body = request(port, b"FOO\r\n\r\n")
    check(st == 400, "garbage is a 400")
    st, h, body = get(port, "/", version="HTTP/2.0")
    check(st == 400, "an unknown protocol version is a 400")
    st, h, body = get(port, "/docs")
    check(st == 301 and h.get("location") == "/docs/", "a directory without a slash is redirected")
    st, h, body = get(port, "/docs/")
    check(st == 200 and b"Docs index" in body, "and the directory serves its index.html")
    st, h, body = get(port, "/docs/readme.txt")
    check(st == 200 and body == b"readme in a subdirectory\n", "a file in a subdirectory")
    # Many connections in a row: nothing leaks, nothing gets stuck.
    ok = True
    for i in range(20):
        st, h, body = get(port, "/hello.txt")
        ok &= st == 200 and body == b"Plain text served by NovaOS.\n"
    check(ok, "20 requests in a row, all answered")
    # Requests that are not nice, last, because they take time or end the connection oddly.
    st, h, body = request(port, b"GET / HTTP/1.0\r\n" + b"X-Padding: " + b"A" * 9000 + b"\r\n\r\n")
    check(st == 431, "headers over 8 KB are refused")
    s = connect(port)
    s.sendall(b"GET / HTTP/1.0\r\n")  # never finished
    t0 = time.time()
    st, h, body = parse_response(read_all(s))
    s.close()
    check(st == 408 and 4 < time.time() - t0 < 9, "a request that never ends gets a 408 after about 5 s")
    s = connect(port)
    s.close()
    time.sleep(0.5)  # a client that vanishes must not take the server down
    st, h, body = get(port, "/hello.txt")
    check(st == 200, "the server survives a client that connects and leaves")


def test_httpd_root(port):
    st, h, body = get(port, "/test/pattern.bin")
    expected = bytes((i ^ (i >> 8) ^ (i >> 16)) & 255 for i in range(1572864))
    check(st == 200 and len(body) == len(expected), f"the 1.5 MB test file arrives whole ({len(body)} bytes)")
    check(zlib.crc32(body) == zlib.crc32(expected), "with the right checksum")
    check(h.get("content-type") == "application/octet-stream", "as binary")
    st, h, body = get(port, "/etc/motd")
    check(st == 200 and len(body) > 0, "files outside /www are served when asked to serve the whole disk")
    st, h, body = get(port, "/bin/")
    check(st == 404, "a directory without an index is a 404")


# ---- the TCP server in nettest on the guest ---------------------------------------------------------

def test_tcp_serve(port):
    # 1. small message, half-close, read everything
    s = connect(port)
    s.sendall(b"hello")
    s.shutdown(socket.SHUT_WR)
    data = read_all(s)
    check(data == b"hello" + b"done 5\n", "echo and trailer after the client half-closes")
    s.close()

    # 2. a megabyte both ways at the same time
    payload = bytes((i * 7 + (i >> 9)) & 255 for i in range(1 << 20))
    s = connect(port, timeout=60)
    result = {}

    def sender():
        s.sendall(payload)
        s.shutdown(socket.SHUT_WR)

    t = threading.Thread(target=sender)
    t0 = time.time()
    t.start()
    got = read_all(s)
    t.join()
    secs = time.time() - t0
    s.close()
    check(got == payload + b"done 1048576\n", f"1 MB echoed byte for byte ({len(payload) / 1024 / max(secs, 0.001):.0f} KB/s each way)")

    # 3. connect and leave without a word
    s = connect(port)
    s.close()
    time.sleep(0.3)

    # 4. send some data, then abort with a reset
    s = connect(port)
    s.sendall(b"x" * 100)
    time.sleep(0.3)
    import struct
    s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    s.close()
    time.sleep(0.5)


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "httpd-count":
        print(HTTPD_REQUESTS)
        return 0
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    if mode == "httpd":
        test_httpd(port)
    elif mode == "httpd-root":
        test_httpd_root(port)
    elif mode == "tcp-serve":
        test_tcp_serve(port)
    else:
        print(__doc__)
        return 2
    print("CLIENT TESTS PASSED" if failures == 0 else f"CLIENT TESTS FAILED: {failures}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
