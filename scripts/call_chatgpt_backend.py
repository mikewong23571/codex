#!/usr/bin/env python3
"""
Call the ChatGPT Codex backend using an existing Codex ChatGPT login `auth.json`.

This is primarily intended for debugging/auth validation.

Notes (as observed on 2025-12-17):
  - This uses *ChatGPT subscription login* tokens (OAuth), not an OpenAI API key.
  - The request uses:
      - `Authorization: Bearer <access_token>`
      - `ChatGPT-Account-ID: <account_id>` (workspace/account identifier; stored in auth.json)
    Without `ChatGPT-Account-ID`, many ChatGPT backend endpoints will reject the request.
  - Endpoint quirks:
      - Streaming Responses: `https://chatgpt.com/backend-api/codex/responses`
      - Models list: `https://chatgpt.com/backend-api/models` (NOT `/backend-api/codex/models`)
    This script tries both model endpoints to handle backend differences.
  - Model slug quirks:
      - `/backend-api/models` returns slugs like `gpt-5-2`
      - `/backend-api/codex/responses` expects `gpt-5.2`
    This script normalizes `gpt-5-*` slugs from hyphen form to dot form before calling `/codex/responses`.

Examples:
  - List models:
      python3 scripts/call_chatgpt_backend.py --list-models

  - Run a simple prompt (defaults to a known-working model):
      python3 scripts/call_chatgpt_backend.py --prompt "hello"

  - Run a prompt with a specific model:
      python3 scripts/call_chatgpt_backend.py --model gpt-5.2 --prompt "hello"
"""

from __future__ import annotations

import argparse
import http.client
import json
import ssl
import sys
import time
import urllib.parse
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Optional


DEFAULT_AUTH_JSON = Path("/home/mikewong/.codex-mgr/accounts/whale-styleofwong/auth.json")
DEFAULT_CODEX_BASE_URL = "https://chatgpt.com/backend-api/codex"
DEFAULT_CLIENT_VERSION = "99.99.99"
DEFAULT_MODEL_FALLBACK = "gpt-5.2"
DEFAULT_ORIGINATOR = "codex_cli_rs"
DEFAULT_VERSION_HEADER = "0.0.0"

# Matches `codex-rs/core/src/auth.rs` (CLIENT_ID).
OAUTH_CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"
OAUTH_REFRESH_URL = "https://auth.openai.com/oauth/token"


@dataclass(frozen=True)
class AuthTokens:
    access_token: str
    refresh_token: str
    account_id: Optional[str]


def _redact_error_body(raw: bytes, limit: int = 2000) -> str:
    try:
        text = raw.decode("utf-8", "replace")
    except Exception:
        return "<non-text response body>"
    text = text.replace("\r", "")
    if len(text) > limit:
        return text[:limit] + "\n...<truncated>..."
    return text


def _http_json(
    method: str,
    url: str,
    headers: dict[str, str],
    body: Optional[dict[str, Any]] = None,
    timeout_seconds: int = 60,
    retries: int = 3,
) -> tuple[int, dict[str, str], bytes]:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https":
        raise ValueError(f"Only https URLs are supported, got: {url}")
    if not parts.hostname:
        raise ValueError(f"Invalid URL (missing host): {url}")

    path = parts.path or "/"
    if parts.query:
        path = f"{path}?{parts.query}"

    payload: Optional[bytes] = None
    if body is not None:
        payload = json.dumps(body, separators=(",", ":")).encode("utf-8")
        headers = dict(headers)
        headers.setdefault("Content-Type", "application/json")

    last_error: Optional[BaseException] = None
    for attempt in range(max(1, retries)):
        conn = http.client.HTTPSConnection(
            parts.hostname,
            parts.port or 443,
            timeout=timeout_seconds,
            context=ssl.create_default_context(),
        )
        try:
            conn.request(method, path, body=payload, headers=headers)
            resp = conn.getresponse()
            status = resp.status
            resp_headers = {k.lower(): v for (k, v) in resp.getheaders()}
            data = resp.read()
            return status, resp_headers, data
        except (ssl.SSLError, ConnectionResetError, TimeoutError, OSError) as err:
            last_error = err
            if attempt + 1 >= max(1, retries):
                raise
            time.sleep(0.3 * (2**attempt))
        finally:
            conn.close()

    raise RuntimeError(f"request failed: {last_error}") from last_error


def _http_sse(
    url: str,
    headers: dict[str, str],
    body: dict[str, Any],
    timeout_seconds: int,
    max_seconds: int,
    retries: int = 3,
) -> int:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https" or not parts.hostname:
        raise ValueError(f"Invalid https URL: {url}")

    path = parts.path or "/"
    if parts.query:
        path = f"{path}?{parts.query}"

    payload = json.dumps(body, separators=(",", ":")).encode("utf-8")

    for attempt in range(max(1, retries)):
        conn = http.client.HTTPSConnection(
            parts.hostname,
            parts.port or 443,
            timeout=timeout_seconds,
            context=ssl.create_default_context(),
        )
        try:
            conn.request("POST", path, body=payload, headers=headers)
            resp = conn.getresponse()
            if resp.status != 200:
                err = resp.read()
                print(
                    f"HTTP {resp.status} {resp.reason}\n{_redact_error_body(err)}",
                    file=sys.stderr,
                )
                return 1

            # The ChatGPT backend sometimes omits or varies Content-Type; detect SSE by
            # peeking at the first line.
            try:
                first_raw = resp.readline()
            except TimeoutError:
                print("Socket timeout while reading response.", file=sys.stderr)
                return 1
            if not first_raw:
                print("Empty response body.", file=sys.stderr)
                return 1
            first_line = first_raw.decode("utf-8", "replace").strip()

            def iter_lines() -> Iterable[str]:
                yield first_line
                while True:
                    try:
                        raw = resp.readline()
                    except TimeoutError:
                        break
                    if not raw:
                        break
                    yield raw.decode("utf-8", "replace").strip()

            if first_line.startswith("{") or first_line.startswith("["):
                # Non-streaming JSON response.
                remainder = resp.read()
                raw = first_raw + remainder
                try:
                    obj = json.loads(raw)
                except Exception:
                    print(_redact_error_body(raw), file=sys.stderr)
                    return 1
                print(json.dumps(obj, indent=2, ensure_ascii=False))
                return 0

            start = time.time()
            buffered_event: Optional[str] = None
            out: list[str] = []
            for line in iter_lines():
                if max_seconds > 0 and (time.time() - start) > max_seconds:
                    print("Timed out while reading SSE stream.", file=sys.stderr)
                    break

                if not line:
                    continue

                if line.startswith("event:"):
                    buffered_event = line[len("event:") :].strip()
                    continue

                if not line.startswith("data:"):
                    continue

                data = line[len("data:") :].strip()
                if data == "[DONE]":
                    break

                try:
                    ev = json.loads(data)
                except json.JSONDecodeError:
                    continue

                if isinstance(ev, dict):
                    ev_type = ev.get("type") or buffered_event
                    if isinstance(ev_type, str) and ev_type.endswith("output_text.delta"):
                        delta = ev.get("delta")
                        if isinstance(delta, str) and delta:
                            sys.stdout.write(delta)
                            sys.stdout.flush()
                            out.append(delta)
                    elif ev_type in ("response.completed", "response.failed"):
                        break

            if out:
                sys.stdout.write("\n")
            return 0
        except (ssl.SSLError, ConnectionResetError, TimeoutError, OSError) as err:
            if attempt + 1 >= max(1, retries):
                raise
            time.sleep(0.3 * (2**attempt))
        finally:
            conn.close()

    return 1


def load_chatgpt_tokens(auth_json_path: Path) -> AuthTokens:
    obj = json.loads(auth_json_path.read_text())
    tokens = obj.get("tokens") or {}
    access_token = tokens.get("access_token")
    refresh_token = tokens.get("refresh_token")
    # In Codex CLI this is derived from the id_token claim:
    # `https://api.openai.com/auth.chatgpt_account_id`
    # and sent as the `ChatGPT-Account-ID` header (workspace/account identifier).
    account_id = tokens.get("account_id")

    if not isinstance(access_token, str) or not access_token:
        raise ValueError("auth.json missing tokens.access_token")
    if not isinstance(refresh_token, str) or not refresh_token:
        raise ValueError("auth.json missing tokens.refresh_token")
    if account_id is not None and not isinstance(account_id, str):
        raise ValueError("auth.json tokens.account_id must be a string when present")

    return AuthTokens(access_token=access_token, refresh_token=refresh_token, account_id=account_id)


def refresh_chatgpt_access_token(refresh_token: str, timeout_seconds: int = 60) -> str:
    payload = {
        "client_id": OAUTH_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    }
    headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": f"{DEFAULT_ORIGINATOR}-python/{DEFAULT_VERSION_HEADER}",
        "originator": DEFAULT_ORIGINATOR,
    }
    status, _headers, body = _http_json(
        "POST", OAUTH_REFRESH_URL, headers=headers, body=payload, timeout_seconds=timeout_seconds
    )
    if status != 200:
        raise RuntimeError(f"Token refresh failed (HTTP {status}): {_redact_error_body(body)}")
    obj = json.loads(body)
    access_token = obj.get("access_token")
    if not isinstance(access_token, str) or not access_token:
        raise RuntimeError("Token refresh response did not include access_token")
    return access_token


def base_headers(tokens: AuthTokens) -> dict[str, str]:
    headers = {
        "Authorization": f"Bearer {tokens.access_token}",
        "originator": DEFAULT_ORIGINATOR,
        "version": DEFAULT_VERSION_HEADER,
        "User-Agent": f"{DEFAULT_ORIGINATOR}-python/{DEFAULT_VERSION_HEADER}",
    }
    if tokens.account_id:
        headers["ChatGPT-Account-ID"] = tokens.account_id
    return headers


def _candidate_models_urls(codex_base_url: str) -> list[str]:
    codex_base_url = codex_base_url.rstrip("/")
    urls = [f"{codex_base_url}/models?client_version={DEFAULT_CLIENT_VERSION}"]

    # The ChatGPT backend may serve `/models` at `/backend-api/models` (no `/codex`),
    # while serving `/responses` at `/backend-api/codex/responses`.
    if codex_base_url.endswith("/codex"):
        backend_api_base = codex_base_url[: -len("/codex")]
        if backend_api_base:
            urls.append(f"{backend_api_base}/models?client_version={DEFAULT_CLIENT_VERSION}")

    return urls


def list_models(base_url: str, tokens: AuthTokens) -> list[dict[str, Any]]:
    headers = base_headers(tokens)
    headers["Accept"] = "application/json"

    last_error: Optional[str] = None
    for url in _candidate_models_urls(base_url):
        status, _resp_headers, raw = _http_json("GET", url, headers=headers)
        if status == 404:
            last_error = f"/models returned 404 at {url}"
            continue
        if status != 200:
            raise RuntimeError(f"/models failed (HTTP {status}) at {url}: {_redact_error_body(raw)}")

        obj = json.loads(raw)
        models = obj.get("models")
        if not isinstance(models, list):
            raise RuntimeError(f"Unexpected /models response shape from {url} (missing models list)")
        return [m for m in models if isinstance(m, dict)]

    raise RuntimeError(last_error or "/models failed (no candidate URLs)")


def pick_model(models: Iterable[dict[str, Any]]) -> str:
    for m in models:
        slug = m.get("slug")
        if isinstance(slug, str) and slug:
            return slug
        # Some payloads use `model` instead of `slug`.
        model = m.get("model")
        if isinstance(model, str) and model:
            return model
    raise RuntimeError("No model slug found in /models response")


def normalize_model_for_codex_responses(model: str) -> str:
    # The models list may use `gpt-5-2`, while `/backend-api/codex/responses` expects `gpt-5.2`.
    # Normalize only the `gpt-5-*` prefix, preserving suffixes like `-pro`, `-thinking`, etc.
    #
    # Examples:
    #   - gpt-5-2        -> gpt-5.2
    #   - gpt-5-2-pro    -> gpt-5.2-pro
    #   - gpt-5-1        -> gpt-5.1
    #   - gpt-4o         -> gpt-4o  (unchanged)
    if model.startswith("gpt-5-") and len(model) > len("gpt-5-") and model[len("gpt-5-")].isdigit():
        return model.replace("gpt-5-", "gpt-5.", 1)
    return model


def call_responses(
    base_url: str,
    tokens: AuthTokens,
    model: str,
    prompt: str,
    instructions: str,
    timeout_seconds: int,
    max_stream_seconds: int,
) -> int:
    conversation_id = str(uuid.uuid4())
    headers = base_headers(tokens)
    headers.update(
        {
            "Accept": "text/event-stream",
            "Content-Type": "application/json",
            "conversation_id": conversation_id,
            "session_id": conversation_id,
        }
    )

    body: dict[str, Any] = {
        "model": model,
        "instructions": instructions,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": prompt}],
            }
        ],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": False,
        "reasoning": None,
        "store": False,
        "stream": True,
        "include": [],
        "prompt_cache_key": conversation_id,
        "text": None,
    }

    url = f"{base_url.rstrip('/')}/responses"
    return _http_sse(
        url,
        headers=headers,
        body=body,
        timeout_seconds=timeout_seconds,
        max_seconds=max_stream_seconds,
    )


def load_codex_instructions_for_model(model: str) -> str:
    repo_root = Path(__file__).resolve().parent.parent
    core_dir = repo_root / "codex-rs" / "core"

    # In the real Codex CLI, base/system instructions are compiled into the Rust binary via
    # `include_str!()` and selected per-model-family (see
    # `codex-rs/core/src/openai_models/model_family.rs`).
    #
    # This debug script is intentionally simpler: it reads the same prompt markdown files from
    # the repo checkout (when available) so you can inspect/override what gets sent as the
    # `instructions` field in the request body.
    #
    # If this script is copied elsewhere (without the repo), it falls back to a minimal
    # instructions string.
    prompt_model = model
    if model.startswith("gpt-5-2"):
        prompt_model = model.replace("gpt-5-2", "gpt-5.2", 1)
    elif model.startswith("gpt-5-1"):
        prompt_model = model.replace("gpt-5-1", "gpt-5.1", 1)

    candidates: list[Path] = []
    if prompt_model.startswith("gpt-5.1-codex-max") or prompt_model.startswith("exp-codex"):
        candidates.append(core_dir / "gpt-5.1-codex-max_prompt.md")
    if prompt_model.startswith("gpt-5.2"):
        candidates.append(core_dir / "gpt_5_2_prompt.md")
    if prompt_model.startswith("gpt-5.1"):
        candidates.append(core_dir / "gpt_5_1_prompt.md")
    if prompt_model.startswith("gpt-5-codex") or prompt_model.startswith(
        "gpt-5.1-codex"
    ) or prompt_model.startswith(
        "codex-"
    ):
        candidates.append(core_dir / "gpt_5_codex_prompt.md")
    candidates.append(core_dir / "prompt.md")

    for path in candidates:
        try:
            return path.read_text(encoding="utf-8")
        except FileNotFoundError:
            continue

    return "You are a helpful assistant."


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--auth",
        type=Path,
        default=DEFAULT_AUTH_JSON,
        help=f"Path to auth.json (default: {DEFAULT_AUTH_JSON})",
    )
    parser.add_argument(
        "--base-url",
        default=DEFAULT_CODEX_BASE_URL,
        help=f"ChatGPT Codex base URL (default: {DEFAULT_CODEX_BASE_URL})",
    )
    parser.add_argument("--list-models", action="store_true", help="List available models and exit")
    parser.add_argument("--model", default=None, help="Model slug to use (optional)")
    parser.add_argument("--prompt", default=None, help="User prompt text to send")
    parser.add_argument("--instructions", default=None, help="Override the `instructions` string")
    parser.add_argument(
        "--instructions-file",
        type=Path,
        default=None,
        help="Read `instructions` from a file (overrides --instructions)",
    )
    parser.add_argument("--timeout", type=int, default=60, help="HTTP timeout seconds")
    parser.add_argument(
        "--max-stream-seconds",
        type=int,
        default=60,
        help="Maximum seconds to read SSE stream (0 = no limit)",
    )
    parser.add_argument(
        "--refresh-on-401",
        action="store_true",
        help="If request returns 401, attempt to refresh the access token and retry once (does not write auth.json).",
    )
    args = parser.parse_args()

    tokens = load_chatgpt_tokens(args.auth)

    normalized_model_arg = normalize_model_for_codex_responses(args.model) if args.model else None

    if args.list_models:
        try:
            models = list_models(args.base_url, tokens)
        except RuntimeError as err:
            print(str(err), file=sys.stderr)
            return 1
        for m in models[:50]:
            slug = m.get("slug") or m.get("model") or "<unknown>"
            name = m.get("display_name") or m.get("displayName") or ""
            suffix = f" — {name}" if isinstance(name, str) and name else ""
            print(f"{slug}{suffix}")
        if len(models) > 50:
            print(f"... ({len(models)} total)")
        return 0

    if not args.prompt:
        parser.error("--prompt is required unless --list-models is set")

    model = normalized_model_arg
    if not model:
        try:
            models = list_models(args.base_url, tokens)
            raw_model = pick_model(models)
            model = normalize_model_for_codex_responses(raw_model)
            if model != raw_model:
                print(
                    f"Using model from /models: {raw_model} (normalized to {model} for /codex/responses)",
                    file=sys.stderr,
                )
            else:
                print(f"Using model from /models: {model}", file=sys.stderr)
        except RuntimeError as err:
            model = DEFAULT_MODEL_FALLBACK
            print(
                f"Warning: failed to list models ({err}); falling back to `{model}`. "
                "Pass --model explicitly to override.",
                file=sys.stderr,
            )

    instructions: str
    if args.instructions_file:
        instructions = args.instructions_file.read_text(encoding="utf-8")
    elif args.instructions is not None:
        instructions = args.instructions
    else:
        instructions = load_codex_instructions_for_model(model)

    rc = call_responses(
        args.base_url,
        tokens,
        model=model,
        prompt=args.prompt,
        instructions=instructions,
        timeout_seconds=args.timeout,
        max_stream_seconds=args.max_stream_seconds,
    )
    if rc == 0:
        return 0

    if not args.refresh_on_401:
        return rc

    # If it failed, do a cheap 401 check by pinging /models; if unauthorized, refresh and retry.
    try:
        _ = list_models(args.base_url, tokens)
    except RuntimeError as err:
        if "HTTP 401" not in str(err):
            raise
        new_access = refresh_chatgpt_access_token(tokens.refresh_token, timeout_seconds=args.timeout)
        tokens = AuthTokens(
            access_token=new_access, refresh_token=tokens.refresh_token, account_id=tokens.account_id
        )
        return call_responses(
            args.base_url,
            tokens,
            model=model,
            prompt=args.prompt,
            instructions=instructions,
            timeout_seconds=args.timeout,
            max_stream_seconds=args.max_stream_seconds,
        )

    return rc


if __name__ == "__main__":
    raise SystemExit(main())
