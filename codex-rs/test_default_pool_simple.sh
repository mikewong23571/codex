#!/bin/bash
set -e

# Setup Env
export CODEX_MGR_STATE_ROOT=/tmp/def_state
export CODEX_MGR_ACCOUNTS_ROOT=/tmp/def_accounts
export CODEX_MGR_SHARED_ROOT=/tmp/def_shared
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

cat <<EOF > $CODEX_MGR_STATE_ROOT/config.toml
[gateway]
listen = "127.0.0.1:34567"
upstream_base_url = "http://127.0.0.1:8890"
redis_url = "redis://127.0.0.1:6379"

[pools]
EOF

$CODEX_MGR_BIN serve &
SERVER_PID=$!
sleep 2

cleanup() {
  kill $SERVER_PID || true
  pkill -f mock_upstream_def.py
}
trap cleanup EXIT

# Get Token for DEFAULT pool
TOKEN_JSON=$($CODEX_MGR_BIN gateway issue --pool default --json)
TOKEN=$(echo $TOKEN_JSON | grep -oP '"token":\s*"\K[^"]+')
echo "Default Token: $TOKEN"

# Mock Upstream
cat <<EOF > mock_upstream_def.py
from http.server import HTTPServer, BaseHTTPRequestHandler
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")
if __name__ == '__main__':
    server = HTTPServer(('localhost', 8890), Handler)
    server.serve_forever()
EOF
nohup python3 mock_upstream_def.py > mock_def.log 2>&1 &
MOCK_PID=$!
sleep 2

# NO Accounts -> Should Fail (500)
CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:34567/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")
if [ "$CODE" == "200" ]; then
  echo "FAIL: Succeeded with no accounts"
  exit 1
fi
echo "Passed: 500 when no accounts"

# Create Account
create_account u1

# Should Succeed (200)
CODE=$(curl --max-time 5 -s -o /dev/null -w "%{http_code}" -X POST http://127.0.0.1:34567/backend-api/codex/completions -H "Authorization: Bearer $TOKEN")
if [ "$CODE" == "200" ]; then
  echo "PASS: Default pool routed to new account"
else
  echo "FAIL: Code $CODE"
  exit 1
fi
