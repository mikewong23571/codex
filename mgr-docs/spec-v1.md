Goal: build a standalone, non-invasive client (`codex-mgr`) that manages multiple **ChatGPT-login** accounts for Codex on a single machine, sharing all state except authentication, and selecting an account automatically based on **rate limit windows** when starting a new Codex session.

Scope: guarantee happy-path behavior for `login(label)`, `accounts list`, `accounts del`, `run --label`, `run --auto` (when auth + usage are available).

## 1) Terminology

- **shared root**: a directory holding all Codex state that should be shared across accounts (history, prompts, sessions, logs, config, etc.).
- **accounts root**: a directory containing one subdirectory per account label.
- **account home**: per-account `CODEX_HOME` directory; stores only auth (and symlinks to shared state).

Default locations:

- `state_root = ~/.codex-mgr/` (contains `config.toml`, `state.json`)
- `shared_root = ~/.codex-mgr/shared/`
- `accounts_root = ~/.codex-mgr/accounts/`

## 2) Storage layout

### 2.1 Shared non-auth state

The manager keeps these paths in `shared_root` and symlinks them into every account home:

- `config.toml`
- `managed_config.toml`
- `history.jsonl`
- `prompts/`
- `log/`
- `sessions/`
- `archived_sessions/`
- `models_cache.json`
- `.credentials.json` (MCP OAuth fallback store)
- `version.json`

The list lives in `ensure_shared_layout(...)` in `codex-rs/mgr/src/main.rs`.

### 2.2 Per-account auth (`CODEX_HOME`)

Each account label gets its own `CODEX_HOME`:

- `CODEX_HOME = <accounts_root>/<label>/`
- `auth.json` is the only intentionally per-account file.

Everything else in the account home should be symlinks into `shared_root`.

### 2.3 Symlink repair

Upstream Codex sometimes writes via *write temp + rename*, which can replace a symlink with a regular file/dir. `ensure_shared_layout` repairs:

- files: copy materialized file back into `shared_root` (last writer wins) and re-create the symlink
- directories: move materialized dir back into `shared_root` **only if** the shared target is empty/non-existent; otherwise it fails fast to avoid silent merges

## 3) Commands

### 3.1 `codex-mgr login --label <label> [--codex-path <path>]`

Purpose: add a new account (ChatGPT login).

Behavior:

1. Validate label (no path traversal, max length, allowed chars).
2. Create `<accounts_root>/<label>/` and materialize symlinks to shared state.
3. Spawn upstream `codex login` with env `CODEX_HOME=<accounts_root>/<label>/`.
4. Validate `auth.json` exists and includes a refresh token.

### 3.2 `codex-mgr accounts list [--json]`

Purpose: list all known labels and the cached rate-limit snapshot, with aligned columns in the text view.

### 3.3 `codex-mgr accounts del <label>`

Purpose: remove only login data (e.g. expired subscription).

Behavior:

- Deletes `<accounts_root>/<label>/auth.json`.
- Does **not** delete shared files (history/prompts/logs/sessions/config/MCP credentials).
- Removes cached state for this label (usage cache + label list).

### 3.4 `codex-mgr run [--auto|--label <label>] [--refresh] [--no-cache] -- <codex args...>`

Purpose: run upstream `codex` with a chosen account.

Behavior:

- Disallow upstream `codex login`; require `codex-mgr login --label ...`.
- If `--auto`, choose an account using rate-limit windows (see below).
- If `--label`, pin that account for the whole session (no switching).
- Disallow upstream `codex logout` for `--auto` (must use `accounts del` or pin a label then `logout`).

#### Auto selection algorithm

Selection is lexicographic:

1. Prefer the account with the highest **weekly** remaining percent (if present).
2. Tie-break by the highest **5h** remaining percent (if present).
3. Tie-break by label name (stable ordering).

#### Fetching usage (performance)

- Cached usage TTL: **15 minutes**.
- When cache is stale/missing, `codex-mgr` fetches usage concurrently (concurrency = **5**) to reduce wall-clock latency for many accounts.
- `--refresh` refreshes the token **before** querying usage for accounts that require a network query (cache hits do not refresh).

#### Trust config pre-seeding (to reduce rename races)

When running in a new working directory, upstream Codex may update `CODEX_HOME/config.toml` to add `[projects."<cwd>"] trust_level="trusted"`. `codex-mgr` pre-seeds this entry in the **shared** `config.toml` for the current working directory (best-effort) to reduce conflicting writes when multiple Codex sessions start in new folders.

## 4) Shim

The repo includes a shim script `scripts/codex-shim`:

- Requires `CODEX_SHIM_REAL_CODEX` to be set to the path of the real upstream `codex` binary.
- Delegates to `codex-mgr run --auto -- <args>` so all upstream commands work except auth operations.

## 5) Maintenance notes (keeping the shared-path list correct)

The shared-path list is derived from scanning upstream Codex source for `CODEX_HOME`-relative paths (e.g. `cfg.codex_home.join("...")`) and related constants.

Suggested scans:

- `rg -n 'codex_home\\.join\\(\"' codex-rs/core/src`
- `rg -n 'SESSIONS_SUBDIR|ARCHIVED_SESSIONS_SUBDIR|history\\.jsonl|prompts|models_cache\\.json|managed_config\\.toml|version\\.json|\\.credentials\\.json' codex-rs`

If upstream introduces new `CODEX_HOME`-scoped state that must be shared (or must be per-account), update:

- `ensure_shared_layout(...)` list in `codex-rs/mgr/src/main.rs`
