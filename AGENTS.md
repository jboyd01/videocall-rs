# videocall-rs Codex Instructions

This is a Rust-based real-time video conferencing platform. Participants connect
via WebTransport or WebSocket from phones, Chromebooks, satellite links, and
desktop browsers worldwide. The relay (actix-api/) forwards media per-receiver;
the client (videocall-client/, dioxus-ui/) decodes and renders in the browser
via wasm32. Latency, scale (20+ participants), and transport diversity are
first-class constraints.

## Project Overview

- `videocall-client` - client library targeting `wasm32-unknown-unknown`
- `dioxus-ui` - Dioxus-based frontend and the sole UI; uses `videocall-client`
- `videocall-types` - shared protobuf types
- `videocall-codecs` - audio/video codec wrappers

## Build Commands

```bash
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client
cargo check --target wasm32-unknown-unknown -p videocall-client
```

## E2E Tests

Browser-based E2E tests live in `e2e/` and use Playwright against the Dioxus UI on port 3001. Auth is bypassed through JWT cookie injection.

Key files:

- `docker/docker-compose.e2e.yaml`
- `e2e/playwright.config.ts`
- `e2e/helpers/auth.ts`

See the `e2e-*` targets in the `Makefile` for available commands.

Choose the lowest test layer that proves the behavior:

- Use unit or backend integration tests when they can catch the bug.
- Use Dioxus browser integration tests for DOM, browser APIs, component behavior, and lightweight routing.
- Use Playwright E2E when behavior crosses UI, backend, and realtime boundaries, involves multiple participants, or depends on WebSocket/WebTransport behavior.

## Change Impact Policy

Every code change must be evaluated in the context of a real-time conferencing app running across diverse networks and devices.

- Consider the full lifecycle before changing connection, session, encoder, or transport code: initial connection, election, reconnection, re-election, graceful disconnect, tab background/resume, and crash or fatal restart recovery.
- Shared connection logic must be validated against both WebTransport and WebSocket paths.
- Thresholds, timeouts, and retry logic must account for high-latency links, packet loss, jitter, and mobile networks, not just localhost.
- Check fan-out and scaling costs. Events that fire per connection can cause O(n) storms during reconnect waves.
- Client-side fixes that rely on server behavior must verify that the server actually upholds those assumptions, and server-side fixes must verify the client-side behavior they depend on.

## Source Code Rules

- No symlinks or hardlinks for source files. Each crate or UI must own its files independently.
- WebSocket and WebTransport transport adapters have protocol-specific differences by design. Do not mechanically consolidate adapter I/O, keep-alive, or send-path code just because the high-level behavior is shared.
- Adaptive-quality thresholds, timing, tier, and tuning values should stay centralized in `videocall-aq/src/constants.rs` (re-exported as `videocall_client::adaptive_quality_constants::*` via the shim in `videocall-client/src/lib.rs`). Do not scatter magic numbers across encoders, PID/controller logic, or connection code.
- `WT_OUTBOUND_CHANNEL_CAPACITY_DEFAULT` in `actix-api/src/constants.rs` is the source of truth for WebTransport outbound queue depth. The Helm env override is redundant; raise the value only for exceptional workloads because deep queues buffer stale video for slow receivers.

## Runtime Config Files

- `dioxus-ui/scripts/config.js` is a committed fallback and is also rewritten by the E2E/dev container from environment variables. Do not casually stage it while the E2E stack is running; check whether changes are intentional source edits or generated env noise.
- When adding a field to the wasm `RuntimeConfig`, either give `dioxus-ui/scripts/config.js` a value that works against a vanilla `make e2e-up` stack or make the field optional with `#[serde(default)]`.

## Linter And Formatter Rules

All code changes must pass the project linters before the work is considered complete.

- Rust: run `cargo fmt` on changed crates. To match CI clippy behavior, run `make clippy-ci`; plain `cargo clippy` or `cargo clippy --all` misses test targets and crate-specific feature flags that CI checks.
- If adding a new crate with test code, add a `--tests` clippy step to both `.github/workflows/pr-check-rust-hcl.yaml` and the `clippy-ci` Makefile target. `scripts/check-clippy-ci-sync.sh` fails CI if the lists drift.
- TypeScript/JS in `e2e/`: run `cd e2e && npx prettier --write <files> && npx eslint <files> && npx tsc --noEmit`.
- Do not leave unused imports or variables.
- Respect local lint and formatter configuration.

## Verification Checklist

1. **Mutation sensitivity**: Tests must fail when the production code they guard is reverted. A test that re-implements production logic inline (instead of calling the production function) is NOT testing the production code — flag it.

2. **Lifecycle paths**: All state changes in encoder/connection/session/transport code must be traced through: cold start, reconnect, re-election, fatal restart, graceful disconnect, tab-background/resume. A value that means one thing on cold start may mean something different mid-session after a partial reset.

3. **Design intent preservation**: When constants/intervals are reused across camera+screen or WebTransport+WebSocket, check whether the existing values DIFFER between those contexts. If they do, the difference is deliberate — unifying them without justification is a regression.

4. **Both transports**: Changes to shared logic must work for both WebTransport and WebSocket. A fix for one must not regress the other.

5. **Signal semantics (relay code)**: For any trigger keyed on "congestion"/"drop"/"full"/"backpressure", verify the signal reads the ACTUAL queue/buffer where the condition surfaces — not a proxy that correlates in some cases (e.g., actix mailbox Full is a burst absorber, NOT per-receiver downlink backpressure).

6. **Execution path**: Changed code must actually execute under real runtime conditions. Trace init order, guard conditions, lifetimes, feature gates, failure paths, empty inputs, missing files, and command errors.

7. **Claim accuracy**: Every claim in a comment, doc, log message, test name, or PR description must be verified against the code.

8. **E2E coverage**: Before declaring an E2E or integration test deferred because a harness does not exist, grep `e2e/tests/` and relevant unit-test modules for an existing harness. A user-facing change is not done until its E2E spec exists and has been demonstrated green through the local docker E2E stack or a scoped CI dispatch. An untagged spec without `@bvt0` or `@bvt1` does not run in per-PR CI and must be validated another way.

Apply this checklist to Rust, TypeScript, CI workflows, shell, YAML, Helm, Dockerfiles, and config.

## Pre-Submission Review

There is currently no Codex hook or command that enforces the old Claude `/pre-submit` gate. Until one exists, do not describe `git push` or `gh pr create` as mechanically blocked by Codex.

Before pushing or creating a PR, run this manual pre-submission review unless the user explicitly says to skip it:

- Run `make clippy-ci`.
- Run `cargo fmt --all --check`.
- For substantive changes, explicitly ask Codex to spawn the personal custom agents `videocall-code-reviewer` and `videocall-performance-reviewer`, or run an equivalent fresh-context adversarial review.
- Route domain-specific changes to the right kind of review: backend/relay/transport, frontend/client transport, security, database/schema/wire format, E2E test sync, and UX/accessibility.
- Do not push if the gate finds blocking issues. Fix findings first, then rerun the gate.

Skip only for WIP commits, pure merge/rebase operations with no new code, or when the user explicitly says to skip.

Escalate further for changes spanning 5+ files of core transport/session/auth logic, security-adjacent changes, or schema/wire-format changes.

## Code Review Output Format

When the user asks for a code review, report only problems. Be concise: one line per finding with file:line reference. No praise, no summaries, no politeness. If zero problems are found, say `No issues found.` and nothing else.
