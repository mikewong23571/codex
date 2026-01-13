#!/bin/bash
set -e
set -x

export CODEX_MGR_STATE_ROOT=/tmp/hr_state
export CODEX_MGR_ACCOUNTS_ROOT=/tmp/hr_accounts
export CODEX_MGR_SHARED_ROOT=/tmp/hr_shared
export CODEX_MGR_BIN=./target/debug/codex-mgr
export RUST_LOG=info

rm -rf $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT
mkdir -p $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT

cat <<EOF > $CODEX_MGR_STATE_ROOT/config.toml
[gateway]
listen = "127.0.0.1:23456"
upstream_base_url = "http://127.0.0.1:8889"
redis_url = "redis://127.0.0.1:6379"

[pools]
EOF

TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
mkdir -p $CODEX_MGR_ACCOUNTS_ROOT/u1
cat <<EOF > $CODEX_MGR_ACCOUNTS_ROOT/u1/auth.json
{
  "tokens": {
    "access_token": "eyJhbGciOiJub25lIn0.eyJleHAiOjE5OTk5OTk5OTksInN1YiI6ImJhZCJ9.sig",
    "id_token": "eyJhbGciOiJub25lIn0.eyJleHAiOjE5OTk5OTk5OTksInN1YiI6ImJhZCJ9.sig",
    "refresh_token": "active",
    "account_id": "u1"
  },
  "last_refresh": "$TIMESTAMP"
}
EOF

# Start Server
$CODEX_MGR_BIN serve &
SERVER_PID=$!
sleep 2

# Helper to cleanup
cleanup() {
  kill $SERVER_PID || true
  rm -f mock_upstream_hr.py
}
trap cleanup EXIT

# Create Pool p1
$CODEX_MGR_BIN pools set p1 --labels u1
echo "Created p1. Waiting for reload..."
sleep 8

# Issue Token
TOKEN_JSON=$($CODEX_MGR_BIN gateway issue --pool p1 --json)
TOKEN=$(echo $TOKEN_JSON | grep -oP '"token":\s*"\K[^"]+')

# Mock Upstream
cat <<EOF > mock_upstream_hr.py
from http.server import HTTPServer, BaseHTTPRequestHandler
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")
if __name__ == '__main__':
    server = HTTPServer(('localhost', 8889), Handler)
    server.serve_forever()
EOF
nohup python3 mock_upstream_hr.py > mock_hr.log 2>&1 &

# Curl
echo "Curling..."
CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:23456/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")

if [ "$CODE" == "200" ]; then
  echo "PASS: Hot reload worked"
else
  echo "FAIL: Code $CODE"
  exit 1
fi
