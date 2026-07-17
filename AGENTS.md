# beam — Agent Guidance

<!-- AGENTS-CORE:BEGIN — generated from frecar/dotfiles code/AGENTS-CORE-public.md. Do NOT edit inline; run code/sync-agents-core.sh. -->
## Cross-agent core rules

These rules bind **every** agent working in this repo — Claude, Codex, OpenCode — regardless of tool. They are the shared contract; your tool-specific file (`CLAUDE.md` / `AGENTS.md` / your config) adds only tool mechanics on top. This block is machine-synced — do not edit it inline.

### Worktrees
- Create worktrees **only** under `/tmp/wt-<branch-slug>/<repo>` (branch with `/`→`-`). **Never** nest a worktree inside the main clone directory — that pollutes the workspace.
- Base off fresh `origin/main`: `git fetch origin` immediately before `git worktree add "/tmp/wt-<slug>/<repo>" origin/main -b <branch>`.
- Name the worktree by the branch, not the issue. **Tear it down** on completion or hand-off: `git worktree remove <path>` + `rm -rf` the parent.
- **Cross-clone edit leak (most damaging):** Only Edit/Write/`sed -i` paths **inside your worktree**. Verify with `git -C <worktree> status` — your edits must appear there, never in the main clone. Shell-outs and non-Claude agents bypass any edit hooks; this written rule is the only guard for all agents.
- **Worktrees isolate the directory, not the branch ref.** Two agents in separate worktrees can still commit to the same branch. If you detect a foreign commit on your branch: escape to a fresh distinctly-named branch, preserve the foreign commit, never force-push-war.
- **A vanished or silent worktree is NOT a dead agent.** A quiet output file, missing `/tmp/wt-*` dir, or a failed process-grep are NOT done-signals — the worktree reaper can evict a live worktree mid-run. Done = the **completion notification only**. Never take over another agent's worktree based on mtime or absence.
- **Deploy from a clean on-main clone.** A detached-HEAD, dirty, or behind-main worktree is fine for a PR merge but may be rejected by the deploy-currency guard. Always deploy from a clean main clone, not a worktree.
- **Pre-commit shared-cache stash collision.** Concurrent agents sharing the pre-commit cache collide on the stash. Always push with a **clean working tree** — no staged or uncommitted changes at push time.
- **Resuming an aged worktree (hand-off / idle / post-crash reattach):** Run `git -C <worktree> fetch origin` then `git rebase origin/main` before continuing. Its base is stale; building on it un-synced causes conflicts and duplicates already-merged work. If the rebase is non-trivial or the branch is badly diverged, open a fresh worktree off `origin/main` and cherry-pick instead.

### Multi-agent coordination (several agents run concurrently as the same GitHub user)
- Before starting an issue, sweep for an existing branch touching it (`git ls-remote --heads origin "*<issue>*"`). If one exists, another agent has it — back off.
- Claim atomically: assign yourself, flip `status:ready`→`status:in-progress`, push an agent-name-prefixed branch, and post a claim comment. Then **wait ~60s and re-read** — if another agent's claim landed in the gap, back off and undo yours.
- One issue per agent. Never edit, review, or push to another agent's claimed issue / PR / branch. Stamp your agent identity on branches and comments so ownership is visible.

### Merge discipline
- `main` is protected on every repo: **never** `git push` to it, **never** raw `gh pr merge` or web-merge.
- Merge **only** via the project's gated merge wrapper, which refuses unless every required check has concluded `success`.
- **Never merge red.** A failing/missing required check or a change-requesting review is a signal to FIX, not to merge. Branch off latest `origin/main` → worktree-isolate → wait for CI green → merge via the gate.

### Commits & conventions
- **Never** add `Co-Authored-By` or any AI-attribution line — commits are the operator's own work.
- **Never** fix production by ad-hoc SSH. Fix in the repo, commit, deploy. Ad-hoc SSH creates drift that the next deploy overwrites.
- Docker-first — run tooling in containers, not on the host. Keep secrets in environment variables or a secrets manager, never committed to the repo.
- Do not hard-code external LLM API endpoints (OpenAI, Anthropic, etc.) in source. Route model calls through the endpoint configured via environment variable.
- **Ad-hoc Python via a shell tool:** do NOT backslash-escape quotes inside a heredoc f-string (`peak[\"run\"]` → `SyntaxError: unexpected character after line continuation character`). Prefer (a) writing the script to a file and running it, (b) single-quoted dict keys inside a double-quoted f-string (`f"{d['k']}"`), or (c) `%`/`.format()`.
- **Never write scratch into `$HOME` root.** Temporary files, one-off scripts, dumps, and logs go in the session scratch dir or a repo-local gitignored path — never `~/`. If your cwd is `$HOME`, that is a bug: change directory first.

### GitHub issues
- Any non-trivial plan or task becomes a GH issue, before or as you start — the issue is the durable record. Apply **exactly one each** of `type:` (bug/feature/chore/docs/infra), `severity:` (critical/high/medium/low), `status:` (triage/ready/in-progress/blocked/burn-in) at file time.
- Self-filed issue → `Closes #N` in the PR. **External-reporter** issue → `Refs #N` (never auto-close on merge; the reporter verifies first).
- This is a public repo — never reference internal hostnames, IPs, private repos, or private deployment details in issues/PRs/comments.

### Quality gates
- Pre-commit and pre-push run automatically; **never** `--no-verify`. Fix the failure instead.
- Wait for CI green before merging. The coverage floor is a fixed **95%** — never lower a gate to pass.

> Detail and rationale live in this repo's own `AGENTS.md` below. This CORE is the non-negotiable shared minimum.
<!-- AGENTS-CORE:END -->


Canonical agent instructions for this repository per [agents.md](https://agents.md/) convention. Compatibility files point here.

## Build Commands
- Build everything (debug): `make build`
- Build everything (release): `make build-release`
- Build Rust only: `make build-rust`
- Build Web only: `make build-web`

## Test Commands
- Run all tests: `make test`
- Run Rust tests: `cargo test --workspace`
- Run Web tests: `cd web && npm test`
- Type check Web: `cd web && npx tsc --noEmit`

### Coverage (#45 ratchet protocol)
- Rust: `make coverage-rust` — runs `cargo llvm-cov --workspace --ignore-filename-regex 'tests?/' --fail-under-lines $(COVERAGE_FLOOR)`. The floor is the single `COVERAGE_FLOOR` variable in the `Makefile` (currently `87`); the CI Coverage job calls `make coverage-rust`, so there is exactly one number to bump. Tooling is `cargo-llvm-cov` (source-based LLVM coverage — handles async/tokio + FFI cleanly; chosen over `cargo-tarpaulin`). Most recent measured on main: 88.30% lines / 88.09% regions (2026-05-30).
- `make coverage` runs both Rust and web; `make coverage-report` writes a browsable HTML report to `target/llvm-cov/html/index.html` for finding the next gaps.
- Web: `cd web && npm run test:coverage` — vitest's v8 provider, threshold `lines: 25` in `vite.config.ts` (baseline 26.61% on 2026-05-17 measuring all `src/**/*.ts`, not just imported files).
- Ratchet rule: every PR that raises actual coverage bumps `COVERAGE_FLOOR` to `floor(actual - 1)`. Never lower. Target end-state: 85% lines on both sides (Rust is past it; web is the remaining gap).

## Lint and Format
- Run all lints: `make lint`
- Run Rust clippy: `cargo clippy --workspace -- -D warnings`
- Format Rust: `make fmt`
- Full pre-commit check: `make check`
- CI check: `make ci` (mirrors GitHub Actions; needs libclang+pkg-config+gstreamer-dev)
- Fast pre-push: `make pre-push` (fmt + tsc only; no link, no system deps)
- Install pre-push hook: `make install-hooks`

## LLM Endpoint Policy

Beam itself does not call LLMs at runtime. If a contributor adds model-assisted tooling (e.g. release notes, log summarization, doc generation), route it through the endpoint configured in the `LLM_ENDPOINT` environment variable — never hard-code a third-party API URL or runtime fallback in source.

`scripts/check_no_external_llm.py` enforces this policy via pre-commit and CI: it scans for hard-coded `api.openai.com`, `api.anthropic.com`, and similar third-party endpoints. Configure your own endpoint via env at build/run time.

`scripts/check-no-internal-refs.sh` also runs in pre-commit and CI. Keep source, docs, and examples standalone; use runtime configuration for private deployment values.

## Deployment

**CRITICAL: NEVER deploy by SSHing into hosts and running manual builds.** Hosts consume .deb packages via APT — they should not have build toolchains. Manual deploys cause path mismatches and wasted debugging time.

**Production deployment** (the ONLY correct way):
1. `make release VERSION=X.Y.Z` — runs CI, tags, pushes
2. Wait for GitHub Actions to build `.deb` and publish to APT repo
3. Hosts pick up the new package via their normal APT update cycle (e.g.
   an Ansible playbook or your deployment tool of choice calling
   `apt update && apt install beam=X.Y.Z` from your own deployment
   infrastructure — out of scope for this project)
4. Verify: check service health on each host

**Development only** (local testing on your own machine):
- Install from source: `sudo make install`
- Quick deploy after local build: `make build-release && sudo make deploy`
- Uninstall: `sudo make uninstall`

## Configuration
- Default port: `8444` (avoids conflict with other services on 8443)
- SPA Fallback: Enabled (unknown paths serve `index.html`)
- Performance:
  - Input: JSON over WebSocket text messages, coalesced mouse moves (RAF)
  - Visual: Local cursor rendering for zero-latency feel
  - Video: Ultra-low latency encoder tuning (`cbr-low-delay-hq`), WebCodecs hardware decode in browser
  - Transport: Binary WebSocket frames with 24-byte header (video/audio), no SDP/ICE/DTLS/SRTP overhead

## Project Structure
- `crates/agent`: Remote desktop agent — capture, encode, WebSocket streaming (Rust)
- `crates/server`: HTTPS server, auth, session management, binary frame relay (Rust)
- `crates/protocol`: Shared message types, binary frame header, config (Rust)
- `web/`: Frontend client (TypeScript/Vite)
- `config/`: Configuration files
- `scripts/`: Setup and installation scripts
- `systemd/`: Systemd service unit

## Versioning & Release

**CRITICAL**: Version bumps require updating THREE files in sync:
1. `Cargo.toml` — `[workspace.package]` version field (source of truth)
2. `web/package.json` — version field (must match exactly)
3. `Cargo.lock` — updated automatically when any `cargo` command runs (e.g., `make check`). No manual step needed if you follow the release workflow

**Before committing any version bump**: run `make version-check` — this MUST pass or CI will block the release.

### When to Bump

Follow strict semver:
- **Patch** (0.1.1 → 0.1.2): Bug fixes, performance improvements, internal refactors
- **Minor** (0.1.2 → 0.2.0): New features, new capabilities, backward-compatible changes
- **Major** (0.2.0 → 1.0.0): Breaking changes to config, API, or behavior that require user action

### Release Process

```bash
# 1. Bump version (updates Cargo.toml, web/package.json, Cargo.lock, package-lock.json)
make bump-version VERSION=X.Y.Z

# 2. Update CHANGELOG.md with the new version section

# 3. Commit everything
git add Cargo.toml Cargo.lock web/package.json web/package-lock.json CHANGELOG.md
git commit -m "release: vX.Y.Z"

# 4. Release (runs full CI, tags, pushes -- all automatic)
make release VERSION=X.Y.Z

# 5. Monitor GitHub Actions: https://github.com/frecar/beam/actions
#    Wait for the release workflow to complete (builds .deb, publishes to APT)

# 6. Deploy to production via your own deployment tooling
#    (out of scope for this repo — typically an Ansible play or other
#    config-management tool that runs `apt update && apt install beam=X.Y.Z`
#    on each target host)

# 7. Verify deployment on each host with TWO complementary checks:
#
#    a) Network HTTP gate (from anywhere that can reach the host):
#         scripts/post-deploy-smoke.sh https://<host>:8444 X.Y.Z
#       Run it against EACH upgraded host (its own address, not a shared
#       front-end), passing the version you just deployed. It polls
#       /api/health until status=ok, asserts the SERVED version matches the
#       one you deployed (catches "the upgrade didn't take effect / an old
#       process is still bound"), re-checks after a short settle window
#       (catches a crash-loop that answers once then dies), and asserts the
#       SPA root still renders.
#
#    b) Host-local capture-readiness gate (ON the host):
#         beam-doctor --capture
#       The HTTP gate proves the server is up and serving the right version,
#       but the server only relays frames — it never links GStreamer, and
#       capture/encode runs in a per-session beam-agent absent on an idle
#       host, so HTTP says nothing about whether a session would produce a
#       picture. `beam-doctor --capture` checks the host-local capture stack
#       (GStreamer + an H.264 encoder element + Xorg dummy driver) and exits
#       non-zero if it would black-screen.
#
#    Both exit non-zero (loudly) on failure, so a deployment tool should
#    treat a non-zero exit from EITHER as a failed rollout.
```

`make release` validates version sync, runs the full CI suite (fmt, clippy, tests, tsc, vite build), creates the git tag, and pushes both the commit and tag. CI then builds the `.deb` and publishes to the APT repo and GitHub Releases.

The post-deploy smoke is intentionally a live-server HTTP gate, not a full
remote-desktop session validation: it needs no display/GPU/X11 (beam-server
starts agents lazily per session), so it stays hermetic and non-flaky. The
capture stack is a separate, host-local concern — beam-server relays frames and
never links GStreamer, so a network probe cannot honestly assert capture
readiness (there is no per-session X server on an idle host). Run
`beam-doctor --capture` ON the host to gate that: it checks the capture stack
(GStreamer, an H.264 encoder element, the Xorg dummy driver) and exits non-zero
if a session would black-screen. Both gates are exercised in CI: the
`post-deploy-smoke` job boots a real beam-server and runs the HTTP smoke
end-to-end, and the `capture-readiness-smoke` job proves `beam-doctor --capture`
passes with the stack present and fails with the encoder hidden — so neither
gate can silently rot into a no-op. Counterpart: the release pipeline's package
smoke (`scripts/package-smoke.sh`) proves a freshly-built `.deb` boots in
isolation; the post-deploy smoke proves the upgrade actually took effect on a
running host.

**IMPORTANT**: Always use `make release` to tag and push -- never tag manually. This ensures CI passes before a tag is created.

**CRITICAL**: NEVER skip steps 5-7. The release is not done until your deployment tooling has rolled out to production and you've verified the service is healthy. Do NOT SSH into hosts and build/deploy manually — that bypasses the entire pipeline.

### APT Repository
- Hosted on `gh-pages` branch, served via `raw.githubusercontent.com` (avoids GitHub Pages CDN caching)
- Landing page: `https://frecar.github.io/beam/`
- APT source: `https://raw.githubusercontent.com/frecar/beam/gh-pages`
- GPG key: `https://raw.githubusercontent.com/frecar/beam/gh-pages/gpg/beam.gpg`

### Package Paths (must stay consistent across install.sh, Makefile, systemd, nfpm.yaml)
- `/usr/local/bin/beam-server` — signaling server binary
- `/usr/local/bin/beam-agent` — capture agent binary
- `/usr/local/bin/beam-doctor` — diagnostic tool
- `/usr/share/beam/web/dist/` — web client files
- `/etc/beam/beam.toml` — configuration (preserved on upgrade)
- `/etc/systemd/system/beam.service` — systemd unit
- `/etc/X11/beam-xorg.conf` — static Xorg config for dummy driver
- `/var/lib/beam/sessions/` — runtime session data

## Security Decisions

Recorded: 2026-02-17. These are settled decisions — do not re-debate without a clear reason.

### Rate Limiter Architecture
- Split into read-only `is_allowed()` + write `record_failure()` -- only failures increment counters
- Dual limiters: username (5 failures / 60s) + IP (20 failures / 60s)
- On success: clear username counter only, NOT the IP counter
- Rationale: one success from IP shouldn't reset brute-force protection against other usernames from the same IP
- Rejected: single combined limiter (too coarse), clearing both on success (creates bypass)
- Release endpoint (`/api/sessions/:id/release`) uses a **separate** `release_limiter` (10 failures / 60s per IP) — decoupled from login in v0.1.21. Failed release token guesses no longer affect login availability.
- IPv6 addresses normalized to /64 prefix before rate limiting (`normalize_ip_for_rate_limit`) — prevents per-address rotation bypass from a single /64 allocation. Fixed in v0.1.21.

### Admin Authorization
- Config-based `admin_users` list in `beam.toml`; empty list = admin panel disabled (returns 403)
- No JWT role claims — adds complexity without benefit at current scale
- No Linux group checks — blocking syscalls, breaks in containers
- Rationale: simple, auditable, no syscall risk

### File Paths
- Self-signed TLS cert: `/var/lib/beam/server-cert.pem`
- Agent logs: `/var/log/beam/agent-{id}.log`
- Agent runtime files (PulseAudio socket, Xorg lock, keyring): stay in `/tmp`
- Rationale: agent runs as non-root user; runtime files are ephemeral per-session; `/tmp` is appropriate

### `constant_time_eq` Bug Fix
- Original code: `(a.len() ^ b.len()) as u8` — XOR values >255 apart would truncate to 0 (i.e., compare as equal), breaking timing-safe comparison
- Fixed to: `if a.len() != b.len() { 1u8 } else { 0u8 }` — explicit length mismatch, no truncation
- This was a security bug: attacker could supply a token of sufficiently wrong length and bypass length check

### systemd Hardening
- Full directive set (v0.1.21): `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictSUIDSGID`, `LockPersonality`, `UMask=0077`, `TimeoutStopSec=30`
- `RestrictRealtime` is NOT set — beam-agent uses `cap_sys_nice` for real-time frame pacing; seccomp propagates to children, blocking `sched_setscheduler()`
- `RestrictNamespaces` is NOT set (removed v0.1.27) — seccomp propagates to children; ALL modern browsers (Chrome, Firefox, Epiphany) require user namespaces for sandboxing and fail with "input/output error" when blocked
- `CapabilityBoundingSet=CAP_SETUID CAP_SETGID CAP_SETPCAP CAP_AUDIT_WRITE CAP_SYS_NICE` -- minimal set for spawning agent processes as real users. `CAP_SYS_NICE` is required in the bounding set (not effective) because beam-agent has `cap_sys_nice=ep` file capabilities; the kernel refuses to exec binaries with file caps outside the bounding set (EPERM). Fixed in v0.1.23 after production breakage on dev-laptop.
- Note: `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome=yes` were relaxed in v0.1.14 due to Xorg/display access requirements -- do not blindly re-add them
- `RestrictAddressFamilies` is NOT set -- beam-server needs AF_INET, AF_INET6, and AF_UNIX. Adding this is safe but was deferred; add `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX` when convenient

### udev Rules
- Input device permissions: `MODE="0660" GROUP="input"` (was `0666` world-writable)
- Rationale: input devices should not be readable by arbitrary processes

### Config File Permissions
- `beam.toml` installed as `0640` (was `0644`)
- Rationale: config contains `jwt_secret`; world-readable config is a credential leak

### TLS Certificate Handling
- Self-signed cert persisted to `/var/lib/beam/server-cert.pem` + `server-key.pem` (key file mode 0600)
- On startup: reuse existing cert if files exist and parse successfully; regenerate if missing or corrupt
- Cert age check (v0.1.21): uses file mtime as proxy for expiry — regenerates self-signed cert if >365 days old, warns at startup if >300 days. Does NOT parse x509 `not_after` (avoids adding x509 parsing dependency for self-signed certs). User-provided certs are not age-checked — the user is responsible for rotation.
- Rejected: automatic cert rotation, ACME/Let's Encrypt integration (out of scope for a LAN/home lab tool)

### Admin Error Responses
- Admin endpoints return `"You do not have permission to access this resource"` (generic, no information leakage)
- Rationale (Faramir security review): detailed messages leak config file name, format, and key names to authenticated non-admin users
- Configuration guidance belongs in server startup logs and documentation, not API responses
- Empty `admin_users` list = admin panel disabled (returns 403)
- Startup log emitted when admin panel is disabled

### Frontend Accessibility (Login Flow)
- 429 rate-limit responses: redirect to login form with countdown timer, assertive ARIA alert, submit button disabled during lockout
- Countdown uses `aria-live="assertive"` for first announcement, then `aria-live="polite"` for tick updates (prevents screen reader spam)
- Focus management: login error returns focus to username input; loading state moves focus to cancel button; reconnect overlay focuses reconnect button
- Progressive warning on failed attempts: client-side counter (never server-side — that would be a brute-force oracle per Faramir security review)
- Shake animation on login card for visual feedback on errors
