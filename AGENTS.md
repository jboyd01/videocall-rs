# Code Review Guidelines

This is a Rust-based real-time video conferencing platform. Participants connect
via WebTransport or WebSocket from phones, Chromebooks, satellite links, and
desktop browsers worldwide. The relay (actix-api/) forwards media per-receiver;
the client (videocall-client/, dioxus-ui/) decodes and renders in the browser
via wasm32. Latency, scale (20+ participants), and transport diversity are
first-class constraints.

## Mandatory checks

1. **Mutation sensitivity**: Tests must fail when the production code they guard is reverted. A test that re-implements production logic inline (instead of calling the production function) is NOT testing the production code — flag it.

2. **Lifecycle paths**: All state changes in encoder/connection/session/transport code must be traced through: cold start, reconnect, re-election, fatal restart, graceful disconnect, tab-background/resume. A value that means one thing on cold start may mean something different mid-session after a partial reset.

3. **Design intent preservation**: When constants/intervals are reused across camera+screen or WebTransport+WebSocket, check whether the existing values DIFFER between those contexts. If they do, the difference is deliberate — unifying them without justification is a regression.

4. **Both transports**: Changes to shared logic must work for both WebTransport and WebSocket. A fix for one must not regress the other.

5. **Signal semantics (relay code)**: For any trigger keyed on "congestion"/"drop"/"full"/"backpressure", verify the signal reads the ACTUAL queue/buffer where the condition surfaces — not a proxy that correlates in some cases (e.g., actix mailbox Full is a burst absorber, NOT per-receiver downlink backpressure).

## Output format

Report ONLY problems. Be concise — one line per finding with file:line reference. No praise, no summaries, no politeness. If zero problems found, say "No issues found." and nothing else.
