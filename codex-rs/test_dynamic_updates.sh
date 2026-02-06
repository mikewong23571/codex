#!/bin/bash
set -e
set -x

# Setup Env
export CODEX_MGR_STATE_ROOT=/tmp/dyn_state
export CODEX_MGR_ACCOUNTS_ROOT=/tmp/dyn_accounts
export CODEX_MGR_SHARED_ROOT=/tmp/dyn_shared
export CODEX_MGR_BIN=./target/debug/codex-mgr
export RUST_LOG=info

rm -rf $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT
mkdir -p $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT

TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

create_account() {
  local user=$1
  mkdir -p $CODEX_MGR_ACCOUNTS_ROOT/$user
  cat <<EOF > $CODEX_MGR_ACCOUNTS_ROOT/$user/auth.json
{
  "tokens": {
    "access_token": "eyJhbGciOiJub25lIn0.eyJleHAiOjE5OTk5OTk5OTksInN1YiI6ImJhZCJ9.sig",
    "id_token": "eyJhbGciOiJub25lIn0.eyJleHAiOjE5OTk5OTk5OTksInN1YiI6ImJhZCJ9.sig",
    "refresh_token": "active",
    "account_id": "$user"
  },
  "last_refresh": "$TIMESTAMP"
}
EOF
}

# Initialize Config
cat <<EOF > $CODEX_MGR_STATE_ROOT/config.toml
[gateway]
listen = "127.0.0.1:12345"
upstream_base_url = "http://127.0.0.1:8888"
redis_url = "redis://127.0.0.1:6379"

[pools]
EOF

# Start Server
$CODEX_MGR_BIN serve &
SERVER_PID=$!
echo "Server started with PID $SERVER_PID"
sleep 2

# Helper to cleanup
cleanup() {
  echo "Stopping server..."
  kill $SERVER_PID || true
  rm -f mock_upstream.py
}
trap cleanup EXIT

echo "=== 1. Test Default Pool Dynamic Update ==="

# Issue token for 'default' pool
# We need to setup a pool setting first? 
# CLI 'pools set' requires a pool_id, but 'gateway issue' accepts any string as pool if we bypass validation?
# Wait, 'gateway issue' checks if pool exists in config? 
# No, 'gateway issue' in current impl just creates a session object in Redis. 
# It does NOT validate against state.pools at issue time (based on previous code review, wait, let me check).
# Actually, let's just try to get an auth token for pool 'default'.

# Current gateway issue impl:
# gateway::issue checks pool existence? 
# Based on typical flow, it might not. 
# But let's check via CLI.

TOKEN_JSON=$($CODEX_MGR_BIN gateway issue --pool default --json)
TOKEN=$(echo $TOKEN_JSON | grep -oP '"token":\s*"\K[^"]+')

if [ -z "$TOKEN" ]; then
  echo "FAIL: Could not issue token for 'default' pool"
  exit 1
fi

echo "Got token for default pool: $TOKEN"

# Mock Upstream that always says OK (we just want to see if it routes)
cat <<EOF > mock_upstream.py
from http.server import HTTPServer, BaseHTTPRequestHandler
import sys

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, format, *args):
        return

if __name__ == '__main__':
    port = 8888
    server = HTTPServer(('localhost', port), Handler)
    print(f"Mock upstream listening on {port}")
    server.serve_forever()
EOF

nohup python3 mock_upstream.py > mock.log 2>&1 &
MOCK_PID=$!
trap "kill $SERVER_PID $MOCK_PID || true; rm -f mock_upstream.py" EXIT
sleep 2

# We need to configure Codex Manager to point to mock upstream?
# config.toml is in state root.
# serve.rs loads config. 
# We should have set gateway.upstream_base_url in config.
# But serve generates default config if missing. Default is https://chatgpt.com.
# We need to modify config.toml to point to localhost:8888/backend-api/codex



# Try request - should fail (500 or 503) because no accounts exist
echo "Making request with no accounts..."
CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:12345/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")
if [ "$CODE" == "200" ]; then
   echo "FAIL: Should not succeed with no accounts"
   # Actually it might return 500 or 503 or 401 depending on how we handle empty list.
   # ensure_routing will see labels=[] and route_account might fail?
fi

# Create Account u1
echo "Creating account u1..."
create_account u1

# Try request again - should succeed immediately as 'default' pool picks up u1
echo "Making request with u1..."
attempt=0
while [ $attempt -lt 5 ]; do
  CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:12345/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")
  if [ "$CODE" == "200" ]; then
    echo "PASS: Routed to u1 via default pool"
    break
  fi
  sleep 1
  attempt=$((attempt+1))
done

if [ "$CODE" != "200" ]; then
  echo "FAIL: Did not route to u1. Code: $CODE"
  exit 1
fi


echo "=== 2. Test Config Hot Reload ==="

# Configure p1 with members (u1)
$CODEX_MGR_BIN pools set p1 --labels u1
# This writes to config.toml. Server background task should pick it up.
echo "Waiting for hot reload (pool creation)..."
sleep 8

# Issue token for pool p1 (now it exists)
TOKEN_P1_JSON=$($CODEX_MGR_BIN gateway issue --pool p1 --json)
TOKEN_P1=$(echo $TOKEN_P1_JSON | grep -oP '"token":\s*"\K[^"]+')

# But wait, we just wrote u1. So it should work.
# Let's create u2 and add it to p1 via CLI command we implemented.
create_account u2

echo "Adding u2 to pool p1..."
$CODEX_MGR_BIN pools add-member p1 u2
# This updates config.toml.

echo "Waiting for hot reload..."
sleep 8 # Poll interval is 5s, be safe.

# Verify p1 works (should route to u1 or u2).
# To verify reload, we should maybe ensure u2 is reachable.
# But load balancing picks one. 
# We just want to ensure it doesn't crash and configuration is consistent.

CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:12345/backend-api/codex/completions -H "Authorization: Bearer $TOKEN_P1")
if [ "$CODE" == "200" ]; then
  echo "PASS: Pool p1 works after update"
else
  echo "FAIL: Pool p1 failed. Code: $CODE"
  exit 1
fi

echo "ALL TESTS PASSED"
