#!/bin/bash
# The network end to end. Boots NovaOS with a network card on QEMU's user-mode network and
# types real commands (ifconfig, ping, nslookup, fetch, httpd, nettest ...) while servers and
# clients written separately in Python run on this machine (scripts/net_servers.py,
# scripts/net_clients.py). Afterwards it checks what those servers saw, and that the disk is
# still consistent.
#
# usage: net_test.sh [novaos.iso] [disk.img] [mkfs]   (QEMU_ACCEL="-enable-kvm -cpu host" for KVM)

ISO=${1:-novaos.iso}
DISK=${2:-build/disk.img}
MKFS=${3:-tools/mkfs/target/x86_64-unknown-linux-gnu/release/mkfs}
here=$(cd "$(dirname "$0")" && pwd)
. "$here/shelllib.sh"

ECHO_PORT=${ECHO_PORT:-17007}
DNS_PORT=${DNS_PORT:-15353}
TCP_ECHO=${TCP_ECHO:-17008}
TCP_SINK=${TCP_SINK:-17009}
HTTP_PORT=${HTTP_PORT:-18000}
FWD_HTTPD=${FWD_HTTPD:-18080}     # host port forwarded to the guest's port 80
FWD_HTTPD_ROOT=${FWD_HTTPD_ROOT:-18081}   # ... to the guest's port 81
FWD_SERVE=${FWD_SERVE:-17200}     # ... to the guest's port 7200
work=$(mktemp -d)
trap 'stop_os; kill $SERVERS_PID 2>/dev/null; rm -rf "$dir" "$work"' EXIT

python3 "$here/net_servers.py" $ECHO_PORT $DNS_PORT $TCP_ECHO $TCP_SINK --http-port $HTTP_PORT \
    --log "$work/servers.log" --ready "$work/ready" &
SERVERS_PID=$!
for _ in $(seq 1 50); do [ -f "$work/ready" ] && break; sleep 0.1; done
[ -f "$work/ready" ] || { echo "NET TEST FAILED: the host servers did not start"; exit 1; }
big_crc=$(python3 "$here/net_servers.py" --big-crc)

boot_os "$ISO" "$DISK" \
    -netdev "user,id=net0,hostfwd=tcp::$FWD_HTTPD-:80,hostfwd=tcp::$FWD_HTTPD_ROOT-:81,hostfwd=tcp::$FWD_SERVE-:7200" \
    -device virtio-net-pci,netdev=net0,disable-modern=on
wait_for_prompts 1 120 || { echo "NET TEST FAILED: the shell never started"; tail -8 "$log"; exit 1; }
boot_seconds=$(awk -v a="$boot_started" -v b="$(date +%s.%N)" 'BEGIN { printf "%.1f", b - a }')
echo "boot to shell prompt: ${boot_seconds} s"

# DHCP finishes by itself shortly after boot.
for _ in $(seq 1 100); do grep -aq "net: address 10.0.2.15" "$log" && break; sleep 0.1; done
grep -aq "\[ok\] net: virtio-net 52:54:00:12:34:56, link up" "$log" || fail "the card was not found"
grep -aq "\[ok\] net: address 10.0.2.15 mask 255.255.255.0 gateway 10.0.2.2 dns 10.0.2.3" "$log" || fail "DHCP did not configure the interface"

run "ifconfig" \
    "eth0: link up" \
    "~^  mac      52:54:00:12:34:56$" \
    "  address  10.0.2.15" \
    "  netmask  255.255.255.0" \
    "  gateway  10.0.2.2" \
    "  dns      10.0.2.3" \
    "  (configured by DHCP)" \
    "~^  frames   received [1-9][0-9]*  sent [1-9][0-9]* "

# ---- ping: the gateway answers, a silent address does not, arguments are checked.
run "ping -c 3 10.0.2.2" \
    "PING 10.0.2.2 (10.0.2.2): 32 bytes of data" \
    "~^reply from 10\.0\.2\.2: bytes=32 seq=1 time=[0-9]+\.[0-9]{3} ms$" \
    "~^reply from 10\.0\.2\.2: bytes=32 seq=3 time=" \
    "--- 10.0.2.2 ping statistics ---" \
    "3 sent, 3 received, 0% loss" \
    "~^rtt min/avg/max = [0-9]+\.[0-9]{3}/[0-9]+\.[0-9]{3}/[0-9]+\.[0-9]{3} ms$"
run "ping -c 2 -s 1000 10.0.2.2" "~^reply from 10\.0\.2\.2: bytes=1000 seq=2 " "2 sent, 2 received, 0% loss"
run "ping -c 1 10.0.2.77" "1 sent, 0 received, 100% loss" "seq=1: Connection timed out"
run "ping" "usage: ping [-c count] [-s size] [-i ticks] host"
run "ping -s 5000 10.0.2.2" "ping: size must be at most 1400"
run "ping -c" "usage: ping [-c count] [-s size] [-i ticks] host"
run "ping bad..name" "~^ping: cannot resolve bad\.\.name: Invalid argument$"

# A flood: 300 pings 10 ms apart is far more traffic than the card's 64 receive and 32
# transmit buffers hold, so buffers that are not handed back to the device show up here.
CMD_TIMEOUT=90 run "ping -c 300 -i 1 -s 1 10.0.2.2" "300 sent, 300 received, 0% loss"
run "ifconfig" "~  frames   received [0-9]+  sent [0-9]+  dropped [0-9]+ \(in\) 0 \(out\)$"

# ---- nslookup against the host's DNS server (port given explicitly), and the failure cases.
run "nslookup alpha.test 10.0.2.2:$DNS_PORT" "alpha.test has address 192.0.2.10"
run "nslookup BETA.test 10.0.2.2:$DNS_PORT" "BETA.test has address 192.0.2.20"
run "nslookup nope.test 10.0.2.2:$DNS_PORT" "nslookup: nope.test: no such host"
run "nslookup 8.8.8.8" "8.8.8.8 has address 8.8.8.8"
run "nslookup 1.2.3" "nslookup: 1.2.3: Invalid argument"
run "nslookup" "usage: nslookup name [server[:port]]"
run "nslookup alpha.test 10.0.2.2:99999" "nslookup: '10.0.2.2:99999' is not an address"

# ---- the socket system calls: UDP, then TCP, by themselves and against the host's servers.
CMD_TIMEOUT=90 run "nettest local" "~^NETTEST PASSED \([0-9]+ checks\)$"
CMD_TIMEOUT=90 run "nettest ext 10.0.2.2 $ECHO_PORT 10.0.2.2 $DNS_PORT" "~^NETTEST PASSED \([0-9]+ checks\)$"
echo "$LAST_OUTPUT" | grep -a "^FAIL" && fail "nettest reported failures"
run "nettest" "usage: nettest local | ext HOST ECHO_PORT DNS_IP DNS_PORT | tcp-local | tcp-ext HOST ECHO_PORT SINK_PORT"

CMD_TIMEOUT=240 run "nettest tcp-local" "~^NETTEST PASSED \([0-9]+ checks\)$" "~^tcp loopback echo: [0-9]+ KB in [0-9]+ ms$"
echo "$LAST_OUTPUT" | grep -a "^FAIL" && fail "nettest tcp-local reported failures"
CMD_TIMEOUT=300 run "nettest tcp-ext 10.0.2.2 $TCP_ECHO $TCP_SINK" "~^NETTEST PASSED \([0-9]+ checks\)$" \
    "~^tcp echo with the host: " "~^tcp upload to the host: "
echo "$LAST_OUTPUT" | grep -a "^FAIL" && fail "nettest tcp-ext reported failures"
echo "$LAST_OUTPUT" | grep -a "^tcp "

# ---- fetch, against a web server on the host.
H="http://10.0.2.2:$HTTP_PORT"
run "fetch $H/small" "small file from the host"
run "fetch -i $H/small" "HTTP/1.0 200 OK" "~^Content-Length: 25$" "small file from the host"
run "fetch $H/missing" "fetch: server answered 404" "nothing here"
run "fetch $H/redir" "fetch: moved to /small (use -L to follow)"
run "fetch -L $H/redir" "small file from the host"
run "fetch -L $H/redir-loop" "fetch: too many redirects"
run "fetch $H/empty | wc" "0 0 0"
run "fetch $H/no-length" "no content length, ends when the connection closes"
run "fetch $H/slow" "~xxxxxxxxxx"
CMD_TIMEOUT=120 run "fetch -o /tmp/big $H/big"
run "cksum /tmp/big" "${big_crc%% *} ${big_crc##* } /tmp/big"
run "rm /tmp/big"
run "fetch http://10.0.2.2:1/x" "fetch: cannot connect to 10.0.2.2:1: Connection refused"
run "fetch ftp://x/y" "fetch: 'ftp://x/y' is not an http:// address"
run "fetch" "usage: fetch [-i] [-L] [-o file] http://host[:port]/path"

# ---- nc against the host's TCP echo server.
run "echo hello over tcp | nc 10.0.2.2 $TCP_ECHO" "hello over tcp"
run "nc 10.0.2.2" "usage: nc host port | nc -l port"

# ---- httpd on the guest, with a Python client on the host (through port forwarding).
count=$(python3 "$here/net_clients.py" httpd-count)
begin "httpd -p 80 -n $count"
wait_cmd_line "httpd: serving /www on port 80" 20 || fail "httpd never started"
python3 "$here/net_clients.py" httpd $FWD_HTTPD > "$work/httpd_client.log" 2>&1 || fail "the httpd client tests failed"
grep -a "FAIL" "$work/httpd_client.log"
echo "httpd client: $(grep -ac "  ok:" "$work/httpd_client.log") checks ok"
grep -aq "CLIENT TESTS PASSED" "$work/httpd_client.log" || { fail "httpd client did not finish"; tail -5 "$work/httpd_client.log"; }
finish 60
echo "$LAST_OUTPUT" | grep -aqE "^request 10\.0\.2\.2:[0-9]+ -> 200 \(" || fail "httpd logged no successful request"
echo "$LAST_OUTPUT" | grep -aq -- "-> 404 " || fail "httpd did not log the 404"
echo "$LAST_OUTPUT" | grep -aq -- "-> 408 " || fail "httpd did not log the timeout"
echo "$LAST_OUTPUT" | grep -aq -- "client went away" || fail "httpd did not log the vanished client"

begin "httpd -p 81 -d / -n 3"
wait_cmd_line "httpd: serving  on port 81" 20 || fail "the second httpd never started"
python3 "$here/net_clients.py" httpd-root $FWD_HTTPD_ROOT > "$work/httpd_root.log" 2>&1 || fail "the large-file client tests failed"
grep -a "FAIL" "$work/httpd_root.log"
echo "httpd large file client: $(grep -ac "  ok:" "$work/httpd_root.log") checks ok"
finish 120

# ---- a TCP server on the guest, with a Python client on the host.
begin "nettest tcp-serve 7200 4"
wait_cmd_line "tcp-serve: listening on 7200" 20 || fail "tcp-serve never started"
python3 "$here/net_clients.py" tcp-serve $FWD_SERVE > "$work/serve_client.log" 2>&1 || fail "the tcp-serve client tests failed"
grep -a "FAIL" "$work/serve_client.log"
echo "tcp-serve client: $(grep -ac "  ok:" "$work/serve_client.log") checks ok"
grep -a "KB/s" "$work/serve_client.log"
CMD_TIMEOUT=60 finish 60

# ---- what the servers on this side actually received.
echo_count=$(grep -ac "^echo " "$work/servers.log")
[ "$echo_count" -ge 28 ] || fail "the echo server saw only $echo_count datagrams (expected at least 28)"
grep -aq "^echo 1472$" "$work/servers.log" || fail "the biggest datagram never arrived"
for line in "dns alpha.test -> 192.0.2.10" "dns BETA.test -> 192.0.2.20" "dns nope.test -> NXDOMAIN" "dns Alpha.TEST -> 192.0.2.10"; do
    grep -aqxF "$line" "$work/servers.log" || fail "the DNS server never logged: $line"
done
grep -aq "^dns malformed" "$work/servers.log" && fail "the OS sent a malformed DNS query"
grep -aq "^tcp-sink 8388608 " "$work/servers.log" || fail "the TCP sink did not receive the whole upload"
grep -aq "^http .*GET /big" "$work/servers.log" || fail "the HTTP server never saw the big download"

grep -aq "KERNEL PANIC" "$log" && fail "kernel panic"
stop_os
if [ -x "$MKFS" ]; then
    "$MKFS" --check "$dir/disk.img" || fail "the disk image is inconsistent after the session"
fi
if [ "$failures" -eq 0 ]; then
    echo "NET TEST PASSED"
else
    echo "NET TEST FAILED: $failures problem(s)"
    exit 1
fi
