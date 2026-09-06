# beam — Agent Guidance

<!-- AGENTS-CORE:BEGIN — generated from frecar/dotfiles agent-harness/policy/AGENTS-CORE-public.md. Do NOT edit inline; run agent-harness/policy/sync-agents-core.sh. -->
## Cross-agent core rules

These rules bind **every** agent working in this repo — Claude, Codex, OpenCode — regardless of tool. They are the shared contract; your tool-specific file (`CLAUDE.md` / `AGENTS.md` / your config) adds only tool mechanics on top. This block is machine-synced — do not edit it inline.

### Worktrees
- Create worktrees **only** under `/tmp/wt-<branch-slug>-<instance>/<repo>` (branch with `/`→`-`; `<instance>` = `$$`+epoch or a harness dispatch id — never omit it). **Never** derive the path from the issue number, agent name, or branch alone: each is constant across concurrent dispatches, so two agents on one issue land in the same directory and silently interleave edits. **Never** nest a worktree inside the main clone directory — that pollutes the workspace.
- Base off fresh `origin/main`: `git fetch origin` immediately before `git worktree add "/tmp/wt-<slug>-<instance>/<repo>" origin/main -b <branch>`.
- Name the worktree by the branch, not the issue. **Tear it down** on completion or hand-off: `git worktree remove <path>` + `rm -rf` the parent.
- **Leave every worktree RECOVERABLE-FROM-REMOTE before you stop — for ANY reason** (task done, context exhausted, timeout, error, hand-off). "On completion" is not enough: most stranded worktrees come from agents that stopped *without* completing. Before your last turn, your work must be in one of these states, and you must say which:
  - **pushed + PR open** → the normal done state; remove the worktree after the PR merges.
  - **pushed, no PR yet** → say so explicitly and name the branch. Commits are safe on the remote, so the worktree is disposable — but nothing tracks the work, and it is invisible to every `gh pr list` and backlog query.
  - **nothing pushed** → push it (a draft PR is fine) **or** state in one line that you are discarding it and why. Never leave uncommitted or unpushed work as your final state: automated cleanup can evict the worktree and that is the only copy.
  - **throwaway** (a worktree you made for a quick check) → remove it in the same breath you finish the check.
- **Removing someone else's worktree: check state first, never blanket-clean.** Safe to remove only when the work is recoverable from the remote — its PR is merged, **or** the branch is pushed with no unpushed commits and a clean tree. Verify with `git -C <wt> status --porcelain` (must be empty) and `git -C <wt> log --oneline origin/<branch>..HEAD` (must be empty). A worktree holding the only copy of something is *never* debris, however old it looks. Find stranded work by walking `git worktree list` against `git ls-remote` — a pushed branch with no PR appears in no other query.
- **Cross-clone edit leak (most damaging):** Only Edit/Write/`sed -i` paths **inside your worktree**. Verify with `git -C <worktree> status` — your edits must appear there, never in the main clone. Shell-outs and non-Claude agents bypass any edit hooks; this written rule is the only guard for all agents.
- **Worktrees isolate the directory, not the branch ref.** Two agents in separate worktrees can still commit to the same branch. If you detect a foreign commit on your branch: escape to a fresh distinctly-named branch, preserve the foreign commit, never force-push-war.
- **`refs/stash` is shared across every worktree of a clone, not scoped to yours.** Never `git stash` in an agent worktree — a push/pop can silently discard another agent's WIP. Save/restore via a scratch diff instead: `git diff > /tmp/<slug>.patch` → `git apply`.
- **git records when a commit landed, never who wrote it.** No AI-attribution trailer is added, so author == committer on every agent commit. "I don't remember writing this" is not evidence someone else did — provenance doubt alone is never grounds to revert, reset, or force-push; verify with diff/log, not memory.
- **A vanished or silent worktree is NOT a dead agent.** A quiet output file, missing `/tmp/wt-*` dir, or a failed process-grep are NOT done-signals — the worktree reaper can evict a live worktree mid-run. Done = the **completion notification only**. Never take over another agent's worktree based on mtime or absence.
- **Deploy from a clean on-main clone.** A detached-HEAD, dirty, or behind-main worktree is fine for a PR merge but may be rejected by the deploy-currency guard. Always deploy from a clean main clone, not a worktree.
- **Pre-commit shared-cache stash collision.** Concurrent agents sharing the pre-commit cache collide on the stash. Always push with a **clean working tree** — no staged or uncommitted changes at push time.
- **Resuming an aged worktree (hand-off / idle / post-crash reattach):** Run `git -C <worktree> fetch origin` then `git rebase origin/main` before continuing. Its base is stale; building on it un-synced causes conflicts and duplicates already-merged work. If the rebase is non-trivial or the branch is badly diverged, open a fresh worktree off `origin/main` and cherry-pick instead.

### Multi-agent coordination (several agents run concurrently as the same GitHub user)
- Before starting an issue, sweep for an existing branch touching it (`git ls-remote --heads origin "*<issue>*"`). If one exists, another agent has it — back off.
- Claim atomically: assign yourself, flip `status:ready`→`status:in-progress`, push an agent-name-prefixed branch, and post a claim comment. Then **wait ~60s and re-read** — if another agent's claim landed in the gap, back off and undo yours.
- One issue per agent. Never edit, review, or push to another agent's claimed issue / PR / branch. Stamp your agent identity on branches and comments so ownership is visible.
- **Branch names are `<agent>/<kebab-slug>` and contain a `/` — never embed a raw branch name in a file path, container name, or metric label.** Sanitize first (`${branch##*/}`, or `tr '/' '-'`), the same class of hazard as the worktree-path rules above. Trigger incident: a scratchpad log path derived from `push-$branch.log` embedded the slash, producing an invalid path; a failed `>` redirect **aborts the whole zsh command line** rather than erroring loudly, so `npm ci` and `git push` silently never ran while the loop printed misleading return codes — two hot-path pushes looked attempted-and-failed when they were never attempted at all. Cross-ref the scratchpad unique-naming rule above (issue number + a literal token, never a raw branch name) for the same reason.

### Engineering autonomy and PM boundary
- Engineers own the ordinary end-to-end delivery loop: `claim -> inspect -> implement -> focused/full tests as appropriate -> commit -> normal pre-push -> push -> PR -> fix exact-head CI -> green review outcome`. Those offline development steps are autonomous and do not require PM approval between them; fix directly in-scope failures without waiting for another permission comment.
- PM owns prioritization, backlog/dependency state, and outcome/risk review; PM-only sessions do not implement code. An explicitly required causal design checkpoint may be appropriate once for a named high-risk shared-infrastructure/performance boundary. Once accepted, it releases the normal delivery loop; it is not a standing or repeated approval gate.
- An issue-specific checkpoint constrains only its named boundary. Continue normal development unless an instruction explicitly and justifiably holds a later live/merge action. Stop and escalate only for a material scope/architecture expansion, secrets/security/legal/policy judgment, destructive/live action, an active incident/machine/review blocker, or a genuinely unresolvable blocker — not for routine edits, commits, pushes, PRs, or in-scope CI fixes.
- **A PM session specifically does NOT**: author or edit implementation code or config, create implementation branches/worktrees/commits, claim or self-assign an engineering issue as implementer, merge implementation PRs, deploy, restart services, rotate secrets, or mutate production. Findings become scoped issues with evidence, acceptance criteria and an engineer handoff. "Ensure X works", "follow up", "unblock engineering", urgency, or a high `priority:` label does **not** implicitly authorize a PM to implement the fix.

### Agent authority — default to doing the thing
- **You operate with full authority over this project.** Routine work does not need a confirmation round-trip — do it.
- **The narrow set that genuinely needs the operator, and nothing else:** minting a *new* external credential that has no creation API (you can still store the resulting value yourself; only the minting is external); physical-world actions; and genuine human/policy judgment — irreversible disclosure, money, legal.
- **Never manufacture an operator-blocker.** Before writing "needs operator", ask: *can I do this with the access I already have?* Check whether an existing credential already carries the needed scope before declaring a new one is required, and specify the **minimum** truly-external dependency. Over-specifying invents work for the operator.

### Estate policy (binds you outside any single repo)
- **Deploys are AUTOMATIC — do not hand-deploy after a merge.** Merged code reaches production by itself. Run a manual deploy only for an active incident needing immediate rollout, and verify afterwards: CI green, error tracker clean, container health.
- **Security findings are never closed by renewing an ignore.** An `ignoreUntil`-style suppression date is a forcing function, not a lifecycle: remove the dependency, upgrade to a patched version, or document a compensating control. Maintenance is a cycle, not chance discovery, and covers vendor advisories and config/supply-chain risk, not just CVE feeds.
- **Scratchpad names must be unique from the moment of creation — and re-readable later.** A session's scratch directory is shared by every agent in that session, so two agents choosing the same obvious name silently overwrite each other — no error, the loser's bytes are simply gone. Bake the issue number *and* a short literal token you choose once into any file you will read back (`scratchpad/pr-body-1234-k3f.md`, never `pr_body.md`). **Do not use `$$` for this.** Each shell tool call runs in a NEW shell, so `$$` differs between the call that writes the file and the call that reads it — measured at four distinct PIDs inside one session. `$$` therefore buys uniqueness at the cost of ever finding the file again: `git commit -F .../msg-$$.txt` fails with `could not read log file`, and a mutation script written under `$$` silently never runs, which reads exactly like a mutation that survived. If you have lost the token, recover by globbing (`ls scratchpad/pr-body-1234-*.md`), never by regenerating `$$`. After publishing via `--body-file`, **re-fetch the live object** and confirm it matches. Renaming at cleanup is too late.
- **Where a durable fact belongs:** can a fresh clone rediscover it by reading the code? Then it goes in the repo's committed `AGENTS.md`. If not, it goes in the harness's own memory layer. Never both.
- **Prefer editing an existing file over creating a new one**, and use your harness's plan mode for a non-trivial feature before writing code.
- **Tooling conventions:** `gh` for every GitHub operation rather than the web UI; `make` targets are uniform across repos (`dev`, `test`, `lint`, `build`). Consultation protocols are harness-specific — do not claim you consulted one your harness does not have.

### Merge discipline
- `main` is protected on every repo: **never** `git push` to it, **never** raw `gh pr merge` or web-merge.
- Merge **only** via the project's gated merge wrapper, which refuses unless every required check has concluded `success`.
- **The gated merge wrapper lives in its own home repo, not necessarily this one** — if this checkout has no such wrapper, `cd` into the repo that owns it before invoking it, and always pass an explicit repo target rather than relying on cwd auto-detection (an implicit target can silently resolve to the wrong repo's identically-numbered PR).
- Prefer the wrapper's wait-for-green flag (merge-when-green in one command) over hand-rolling a poll loop; `gh pr checks <n> --watch` is a read-only status-poll fallback — it never merges anything.
- **Same-owner review blockers:** when the environment provides a machine-readable merge-blocker helper, use it for blocking agent/PM findings instead of prose-only comments. Resolve blockers only with the matching machine-readable receipt after reviewing the current head; do not merge around unresolved blockers except through the wrapper's explicit audited override.
- **Wait for review, then disposition it.** When a PR has a configured or requested human or automated review, do not merge while that review is pending. After the final push, read every review and review comment against the exact head. For every finding, either fix it and rerun the affected checks, or reply in the PR with an evidence-backed rationale for no change; record that disposition before merge. A `COMMENTED` review is not an empty review, and green CI is not a substitute for reading it. If the only review predates the final head, revalidate its findings against the final diff and say so in the PR.
- Before an autonomous merge, deploy, converge, or live probe, run the configured incident guard when the operator environment provides one and stop on HALT. Deliberate exceptions must be explicit and auditable in the same shell command as the guarded action.
- **Never merge red.** A failing/missing required check or a change-requesting review is a signal to FIX, not to merge. Branch off latest `origin/main` → worktree-isolate → wait for CI green → merge via the gate.
- **Verify the merge actually happened.** A gate run from the wrong cwd/repo is a silent no-merge — it can exit 0 while nothing merges. After the wrapper returns, confirm the PR reached `state == MERGED`: `gh pr view <n> --json state -q .state` (expect `MERGED`) or `gh api repos/<owner>/<repo>/pulls/<n> -q .merged` (expect `true`). Do not treat gate exit 0 alone as proof.

### Commits & conventions
- **Never** add `Co-Authored-By` or any AI-attribution line — commits are the operator's own work.
- **Never** fix production by ad-hoc SSH. Fix in the repo, commit, deploy. Ad-hoc SSH creates drift that the next deploy overwrites.
- **Manage estate state declaratively, through the project's IaC layer — not by hand.** DNS, containers/services, secrets, and network/cert config each have a config home — edit there and converge/deploy, never by hand-editing a vendor dashboard or console, hand-curling a mutation API, or an ad-hoc host edit, whenever an IaC path exists or can reasonably be added. Same principle as the ad-hoc-SSH rule above, generalized from hosts to all estate state. **Exception:** vendor state with no IaC path (e.g. an OAuth consent screen) — file an operator-action issue to track the manual step, don't silently skip it.
- Docker-first — run tooling in containers, not on the host. Keep secrets in environment variables or a secrets manager, never committed to the repo.
- Do not hard-code external LLM API endpoints (OpenAI, Anthropic, etc.) in source. Route model calls through the endpoint configured via environment variable.
- **Ad-hoc Python via a shell tool:** do NOT backslash-escape quotes inside a heredoc f-string (`peak[\"run\"]` → `SyntaxError: unexpected character after line continuation character`). Prefer (a) writing the script to a file and running it, (b) single-quoted dict keys inside a double-quoted f-string (`f"{d['k']}"`), or (c) `%`/`.format()`. **Never put backticks or `$(...)` inside a double-quoted `python3 -c "…"`** — zsh command-substitutes them *before* Python sees the string, silently splicing command output into your source, which then commits cleanly and passes lint. Prose containing shell metacharacters must go through a **quoted** heredoc (`<<'PYEOF'`) or a file, never `-c "…"`.
- **`until ! pgrep -f PATTERN` NEVER exits — it matches itself.** `pgrep -f` tests every process's full command line, and the waiting shell's own command line contains the literal pattern, so the loop spins forever after the job is long gone. Prefer your harness's completion signal or the task's output file. If you must check a process: `pgrep -x <binary>`, a PID captured at launch (`kill -0 "$pid"`), or a lock file — never a pattern that also appears in the waiting command. Bound every wait with a timeout so a mistake degrades into a late check, not an indefinite stall.
- **The interactive/Bash-tool shell here is zsh, not bash — it does NOT word-split unquoted `$vars`.** `flags="--a --b"; cmd $flags` passes ONE argument in zsh (bash would split it), so `gh issue create $flags` fails `unknown flag: --a --b` — a multi-step script can silently do nothing before anyone notices. Pass flags explicitly, or build an **array** (`args=(--label a --label b); cmd "${args[@]}"`), or force one split with `cmd ${=flags}`. Also: an unquoted empty `$x` expands to nothing (no empty-string arg), and unmatched globs **error** (`no matches found`) rather than passing through literally — quote literal globs, or `setopt NULL_GLOB` locally. Prefer `[[ … ]]` over `[ … ]`.
- **A hook that reformats files ABORTS the commit — verify the commit landed.** pre-commit rewrites a staged file (a formatter is the common case) and then *fails* the commit, but its output still ends in a wall of `Passed` lines, so it reads as success. The commit does not exist and the tree is left dirty, so the next `git push` ships the **base** commit. After every commit, confirm `git log --oneline -1` shows your message AND `git status --porcelain` is empty, before pushing. Re-stage the formatter's changes and commit again; never `--no-verify`.
- **No `vN` version suffixes on production identifiers** (classes, feature flags, components, functions, endpoints, UI labels, service names). Name *the current thing* — `UserDashboard`, not `UserDashboard v2`; a `checkout` flag, not `checkout_v2`. Version suffixes are iteration scaffolding: iterate behind a feature flag or branch, then ship under the canonical name (and retire the flag). **Exception — genuine compatibility-contract versions stay:** a suffix encoding a wire-format/schema/API contract a consumer depends on is not cruft and must not be renamed (crypto key-scheme tags where a v1 row must still decrypt, `/api/v1/...` REST paths a client is pinned to, Django `migrations/000N_*`, third-party API paths like `orders/v6`). The test: does the suffix encode a contract a consumer depends on (keep) or just "the 2nd attempt at building X" (rename)?
- **Never write scratch into `$HOME` root.** Temporary files, one-off scripts, dumps, and logs go in the session scratch dir or a repo-local gitignored path — never `~/`. If your cwd is `$HOME`, that is a bug: change directory first.

### GitHub issues
- Any non-trivial plan or task becomes a GH issue, before or as you start — the issue is the durable record. Apply **exactly one each** of `type:` (bug/feature/chore/docs/infra), `severity:` (critical/high/medium/low), `status:` (triage/ready/in-progress/blocked/burn-in), `priority:` (p0..p3) and `effort:` (s/m/l/xl) at file time — **five axes, all of them**. The backlog-hygiene check requires all five and flags a missing one on arrival; filing three produces an issue that is immediately non-conforming.
- Self-filed issue → `Closes #N` in the PR. **External-reporter** issue → `Refs #N` (never auto-close on merge; the reporter verifies first).
- **GitHub does not parse negation.** `Closes #N` / `Fixes #N` / `Resolves #N` anywhere in a commit or PR body closes #N on merge — even inside "this does **not** close #N". Never put a closing keyword next to an issue number you are not closing; write `Refs #N` or spell the number out ("issue N") instead.
- This is a public repo — never reference internal hostnames, IPs, private repos, or private deployment details in issues/PRs/comments.
- **Check `state` before editing an issue.** `gh issue view <N> --json state` first. A CLOSED issue is a resolved historical record: capture new work in a **new** issue or a comment, never by rewriting a closed issue's title/body — nobody reads a closed issue, so the work becomes invisible.
- **Parent/child uses the native sub-issue API**, not prose. A `Refs #N` / `Blocked by #N` line is documentation; it is invisible to board rollups and to every programmatic query. Both is fine, prose alone is not.
- **`priority:` is the ONLY ordering signal.** `priority:p0..p3` decides what to work on next. `severity:` describes **impact if the thing occurs** and must never be read as urgency. `severity:high` + `priority:p3` is a **valid, expected** combination meaning "high impact, deliberately deferred" — it is not an error and must not be auto-corrected. Set priority/effort **once, as labels**; the board's Priority/Effort fields are derived views — never hand-edit them.
- **Never write ad-hoc GraphQL against ProjectV2** (project/field/item IDs) — use the operator tooling, which resolves projects and fields by name and is quota-budgeted. Projects V2 field IDs **and** single-select option IDs are unique **per board**: a hardcoded or cross-board ID fails *silently*, leaving the field empty rather than erroring. Board routing is decided by repo, with a milestone override taking precedence — resolve it from the tooling, never from a hardcoded id.

### Quality gates
- Detail behind these rules — measured numbers, worked examples, the reasoning — is in `agent-harness/docs/estate-conventions.md`. It adds no rules; if it disagrees with this block, this block wins.
- **Every fact gets ONE authoritative home.** A second appearance must use the weakest mechanism its consumer supports: **refer** (a pointer) → **link** (symlink, or an `@import` where the harness supports one) → **generate** (a marker-delimited block, byte-identical-gated) — and generate ONLY where the consumer physically cannot follow a reference. A hand-maintained copy is banned even with a "keep in lockstep" note, and a test asserting two hand-written copies agree is the same ban in test form: it preserves N copies and merely makes their divergence a merge blocker.
- **A trailing pipe discards the exit status you care about.** `cmd | tail`, `cmd | grep …` report the LAST command's status, so a reported "exit code 0" on a piped invocation is evidence of nothing. Observed: `uv run pytest -q | tail -6` surfaced as exit 0 while the output itself said `1 failed, 3751 passed`, and the failure was a real defect in the change under test; separately a piped `git commit` masked a pre-commit abort. Never conclude a suite, build, push or commit succeeded from a piped run — run it unpiped and redirect to a file, or capture `${pipestatus[1]}` (zsh) / `${PIPESTATUS[0]}` (bash), or grep the output for the real verdict line.
- Pre-commit and pre-push run automatically; **never** `--no-verify`. Fix the failure instead.
- Wait for CI green before merging. The coverage floor is a fixed **95%** — never lower a gate to pass, and never RAISE it above 95 either — a climbing floor manufactures razor-thin reds on rounding artifacts rather than catching regressions.
- **A prober being down is not the probed thing being down.** An alert derived from a probe must distinguish "the target failed" from "the check could not run" — otherwise one dead host manufactures a wall of false critical alerts about healthy services and stalls unrelated work. Emit an explicit unknown/stale state and alert on it separately; "cannot verify" is never "verified broken".
- **Correlation over a handful of events is not a root cause — find the variable that CHANGED.** Before asserting cause from a small series, enumerate every candidate rather than the memorable one, check for a confound that makes the correlation near-tautological, and confirm n is what you think it is. A theory built on 3 events collapsed on all three counts: the sample was 4, the "cause" was downstream of the effect, and the one variable nobody checked was the real candidate.
- **Never sync or mirror another repo's state captured during an incident.** A snapshot taken while the source is degraded bakes the degradation in and outlives it. Re-take it after recovery, or gate the sync on the source being healthy.
- **A checker must be validated against the failure mode it claims to detect** — including the **absent/empty** case, not only the wrong-value case. Test any drift/audit tool against a deliberately injected instance of each mode it claims to cover; one that only compares non-empty values silently certifies its own blind spot. This is distinct from the coverage floor, which is line coverage of code under test, not failure-mode coverage of a checker.
- **A registry/catalog CI guard rejecting your push is the guard working, not a bug to route around.** Many guards enumerate a governed artifact class (a script, config unit, workflow step, collector) against a companion registry; a red run here almost always means "go add the entry," not "fight the check." If you are the one *writing* such a guard, derive membership from the authoritative source (a template, generator, or import closure) rather than hand-enumerating literal string occurrences — a literal match is blind to templated/generated instances of the same real artifact.

- **Adding or promoting a CI/test gate is a budgeted decision, not a free win.** Before you add one, state four things in the PR: the **failure class** it catches, its **measured** runtime, whether it is **required / advisory / scheduled**, and the **change classes** that should run it. Measure before you claim — the critical path is often image scan, browser smoke or a vulnerability scanner rather than the test suite, so "tests are slow" is a conclusion to earn from job timings, not an assumption. Prefer a **contract/failure-mode test** over tests written only to move a coverage number: the coverage floor and the never-merge-red rule are not negotiable, but neither is satisfied by a gate whose failures nobody can attribute. Prefer focused, change-aware lanes for Docker/image/browser work where that does not drop a required context. If a gate is expected to be temporary, write its **retirement condition** down with it — an incident-driven rule with no exit condition never gets one.
- **README files are agent-facing operational documentation — update them in the SAME PR** that changes commands, paths, the merge or deploy flow, package consumption, or onboarding. A README must never teach an ungated merge, a direct deploy, or point a reader at `CLAUDE.md` as the deeper project doc (those are `@AGENTS.md` shims, and some agents treat `@import` as literal text — the canonical agent guidance is `AGENTS.md`). Defer agent rules to `AGENTS.md` rather than restating them; a second copy drifts.

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
