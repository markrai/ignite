#!/usr/bin/env bash
# Disposable Docker networking probe (host vs bridge). Not a physical-LAN Kindle proof.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

sudo apt-get update -qq
sudo apt-get install -y -qq docker.io curl ca-certificates python3 >/dev/null
sudo systemctl enable --now docker
sleep 1

echo "=== host interfaces ==="
ip -br addr
echo "=== docker version ==="
sudo docker version --format '{{.Server.Version}}'

echo "=== host-network container interfaces ==="
sudo docker run --rm --network host alpine:3.20 ip addr

echo "=== bridge-network container interfaces ==="
sudo docker run --rm alpine:3.20 ip addr

echo "=== NON_LAN prefix simulation ==="
python3 - <<'PY'
prefixes=("lo","docker","br-","veth","virbr","vnet","tailscale","tun","tap","wg")
for name in ["eth0","docker0","br-abc","veth123","lo","enp0s1"]:
    non = any(name.startswith(p) for p in prefixes)
    print(f"{name}: name_eligible={not non}")
PY

echo "=== host-network loopback HTTP ==="
sudo docker rm -f ignite-hn-probe >/dev/null 2>&1 || true
sudo docker run -d --name ignite-hn-probe --network host python:3.12-alpine \
  python -c "from http.server import HTTPServer,BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
  def do_GET(self):
    self.send_response(200); self.end_headers(); self.wfile.write(b'ok-hostnet')
  def log_message(self,*a): pass
HTTPServer(('127.0.0.1',18787),H).serve_forever()"
sleep 1
curl -fsS http://127.0.0.1:18787/
echo
sudo docker rm -f ignite-hn-probe >/dev/null

echo "=== bridge published-port HTTP ==="
sudo docker rm -f ignite-br-probe >/dev/null 2>&1 || true
sudo docker run -d --name ignite-br-probe -p 127.0.0.1:18788:8000 python:3.12-alpine \
  python -c "from http.server import HTTPServer,BaseHTTPRequestHandler
class H(BaseHTTPRequestHandler):
  def do_GET(self):
    self.send_response(200); self.end_headers(); self.wfile.write(b'ok-bridge')
  def log_message(self,*a): pass
HTTPServer(('0.0.0.0',8000),H).serve_forever()"
sleep 1
curl -fsS http://127.0.0.1:18788/
echo
echo "=== docker0 ==="
ip -br addr show docker0 || true
sudo docker rm -f ignite-br-probe >/dev/null
echo "PROBE_DONE"
