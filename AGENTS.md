# Repository guidance

Relay is a small local durable handoff core, not a full agent platform.

## Preserve the boundary

- Keep durable queue state, submission idempotency, claim ownership, generation fencing, and bounded results in the Rust library
- Keep the CLI thin and machine-readable: JSON responses, no alternate state machine
- Keep requirements, payload semantics, model calls, GitHub integration, development workflow, and evidence policy outside the core
- Let the trusted host own workspace cleanup and process lifecycle
- Do not add network listeners, dynamic plugins, distributed services, or automatic claim expiry without an explicit requirement
- Never infer that an old execution stopped merely because a timeout elapsed; manual requeue requires trusted-host confirmation
- Do not claim exactly-once external effects: generation fencing protects database transitions only

## Work in small verified increments

- Read `README.md` and `docs/architecture.md` before changing behavior
- Prefer the existing Rust / SQLite stack; explain new dependencies
- Use immediate transactions for state changes that check and update shared queue state
- Preserve byte-size limits and UTF-8 validation at every public entry point
- Add focused tests for changed behavior, especially concurrency, restart, idempotency, and stale ownership
- Run relevant tests first; before delivery run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`
- State which checks passed, failed, or could not run; do not turn a small change into repeated full-host scans or inherited process bureaucracy
- Update the concise architecture notes when a responsibility boundary changes; add an ADR only for a consequential design decision
- Use English for code and identifiers; concise Chinese is preferred for user-facing project explanations
