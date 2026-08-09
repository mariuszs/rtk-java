# LSP Strategy — Symbol-Level Navigation

Companion to `.claude/rules/search-strategy.md`. That file ranks tools for **pattern**
searches; this one covers **symbol** questions, where `rust-analyzer` answers exactly and
Grep only approximates.

Fork-only file — not present upstream.

## Requirement

The `LSP` tool needs the `rust-analyzer-lsp` plugin enabled (`.claude/settings.json`) and
the rustup component on PATH:

```bash
rustup component add rust-analyzer   # binary lands in ~/.cargo/bin
rust-analyzer --version
```

If the tool is unavailable, fall back to Grep. Never block on it.

## Symbols → LSP, patterns → Grep

| Question | Tool |
|----------|------|
| Where is `X` defined? | `LSP goToDefinition` |
| Who calls `X`? | `LSP findReferences` — real call sites, not text hits |
| What's in this 1000+ line file? | `LSP documentSymbol` — **not** `Read` |
| Blast radius before changing a filter signature | `LSP incomingCalls` / `outgoingCalls` |
| Which trait impls exist? | `LSP goToImplementation` |
| `LazyLock` regexes, `.unwrap()` outside tests, fixtures | **Grep** — LSP cannot pattern-match |

Rule of thumb: **named symbol → LSP; text pattern → Grep.**

## Why it pays here

`src/core/tracking.rs` is ~1850 lines; `documentSymbol` maps every struct, impl and test
in one call, where `Read` would bill the whole file. Same for `src/main.rs` routing and
the larger `src/cmds/**/*_cmd.rs` filters.

`findReferences` matters most before touching shared infrastructure — `tracking.rs`,
`utils.rs`, `guard.rs` — where a Grep for the function name also hits doc comments,
`//!` headers and unrelated strings.

## Gotchas

- `line` and `character` are **1-based**, as shown in editors — not 0-based like raw LSP.
- `workspaceSymbol` needs a non-empty `query`; an empty one returns nothing.
- The first call after a restart may lag while rust-analyzer indexes the crate.

## Anti-Patterns

❌ **Don't** use `findReferences` to count regex patterns — that is Grep's job
❌ **Don't** `Read` a large module just to locate a function — `documentSymbol` first
❌ **Don't** block or retry when the LSP server is missing — fall back to Grep
