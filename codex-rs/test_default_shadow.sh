#!/bin/bash
set -e

# Setup Env
export CODEX_MGR_STATE_ROOT=/tmp/sha_state
export CODEX_MGR_ACCOUNTS_ROOT=/tmp/sha_accounts
export CODEX_MGR_SHARED_ROOT=/tmp/sha_shared
export CODEX_MGR_BIN=./target/debug/codex-mgr
export RUST_LOG=info

rm -rf $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT
mkdir -p $CODEX_MGR_STATE_ROOT $CODEX_MGR_ACCOUNTS_ROOT $CODEX_MGR_SHARED_ROOT

cat <<EOF > $CODEX_MGR_STATE_ROOT/config.toml
[gateway]
listen = "127.0.0.1:45678"
upstream_base_url = "http://127.0.0.1:8891"
redis_url = "redis://127.0.0.1:6379"

[pools.default]
labels = ["non-existent-account"]
EOF

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

# Start Server
$CODEX_MGR_BIN serve &
SERVER_PID=$!
sleep 2

cleanup() {
  kill $SERVER_PID || true
  pkill -f mock_upstream_sha.py
}
trap cleanup EXIT

# Create Account u1
create_account u1

# Issue Token for default
TOKEN_JSON=$($CODEX_MGR_BIN gateway issue --pool default --json)
TOKEN=$(echo $TOKEN_JSON | grep -oP '"token":\s*"\K[^"]+')

# Mock Upstream
cat <<EOF > mock_upstream_sha.py
from http.server import HTTPServer, BaseHTTPRequestHandler
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")
if __name__ == '__main__':
    server = HTTPServer(('localhost', 8891), Handler)
    server.serve_forever()
EOF
nohup python3 mock_upstream_sha.py > mock_sha.log 2>&1 &
sleep 2

# We defined [pools.default] in config to point to "non-existent-account".
# But dynamically we have u1.
# If internal "default" logic wins, it should see u1 and succeed.
# If config "default" logic wins, it should fail (account not found or empty candidates).

CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:45678/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")
if [ "$CODE" == "200" ]; then
  echo "PASS: Dynamic default shadowed the config default"
else
  echo "FAIL: Code $CODE - Logic fell back to config or failed?"
  exit 1
fi
