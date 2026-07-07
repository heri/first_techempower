# TechEmpower-style benchmark: 7 implementations, 1 laptop

A fun exercise: implement the same two endpoints — a static JSON response and a single random-row database lookup — across several languages and frameworks, and see what actually moves the numbers.

Repository layout:

| Directory | Implementation |
|---|---|
| `unsafe-rust-server/` | Rust, hand-rolled, `unsafe` raw sockets + `kqueue`, no framework |
| `c-server/` | C, same hand-rolled architecture as `unsafe-rust-server` |
| `node-raw-server/` | Node.js, bare `http` module, no framework |
| `node-fastify-server/` | Node.js, `fastify` |
| `crystal_kemal/` | Crystal, `Kemal` web framework |
| `rust_pure_api/` | Rust (safe), `actix-web` + `diesel` |
| `uvicorn/` | Python, raw ASGI app on `uvicorn`/`gunicorn` |

## Benchmarks

`wrk -t4 -c128 -d15s`, MacBook Pro (Intel i9-9880H, 8 physical/16 logical
cores, macOS), load generator running on the same machine as the server.
Static JSON needs no database; the DB column is a single row fetched by a
random id out of a 10,000-row table (SQLite for the two hand-rolled servers,
Postgres for everything else).

| Project | Language / Framework | Static JSON (req/s) | DB single-row lookup (req/s) |
|---|---|---|---|
| `unsafe-rust-server` | Rust, `unsafe`, hand-rolled | 125,174 | 50,179 |
| `c-server` | C, hand-rolled | 123,028 | 50,340 |
| `node-raw-server` | Node.js, bare `http` | 108,889 | 28,242 |
| `node-fastify-server` | Node.js, Fastify | 105,720 | 26,636 |
| `crystal_kemal` | Crystal, Kemal | 52,285 | 24,887 |
| `rust_pure_api` | Rust (safe), actix-web + diesel | — | 20,293 |
| `uvicorn` | Python, raw ASGI + asyncpg | — | 17,164 |

## Why the numbers differ

The gap is architecture, not raw language speed: `unsafe-rust-server` and
`c-server` hand-roll their own socket/event loop, do zero heap allocation per
request, and talk to an in-process SQLite database — which is why they lead
by a wide margin and why C and Rust land within a few percent of each other
despite being different languages. Everything else pays for at least one of:
a general-purpose web framework's routing/serialization overhead, a network
round trip to Postgres instead of an in-process database, or a
connection-pool design that can bottleneck under load (a single
under-sized pool was the single biggest swing factor of any change made
during this exercise — bigger than any language difference measured). Node.js
and Crystal are also architecturally similar to each other — one OS process
per CPU core, no parallelism within a process — and share a corresponding
weakness: strong average throughput but a noticeably worse latency tail than
the thread-based competitors, because a single stall (garbage collection or
otherwise) on any one process blocks every request in flight on that process
at once.

## Recommendation: unsafe Rust or C for a dev team?

**Unsafe Rust**, given the choice. Performance is a wash — across repeated
back-to-back runs, C and unsafe Rust land within a few percent of each other,
well inside run-to-run noise, so raw speed doesn't favor either one. What
does favor Rust:

- **`unsafe` is scoped and greppable.** You can find every unsafe block in a
  Rust codebase and concentrate review and testing there; the rest of the
  program still gets borrow-checking, no null-pointer dereferences, no data
  races, by default. In C, every line has the same lack of a safety net —
  there's no way to mark most of a codebase "safe" and narrow scrutiny to
  the rest.
- **Tooling.** Cargo gives reproducible builds and dependency management for
  free; Miri and Clippy catch undefined behavior and common mistakes even
  inside unsafe blocks. C's equivalent (ASan/UBSan/Valgrind, vendored
  dependencies, a hand-assembled build system) has to be put together and
  maintained separately.
- **Hiring and onboarding.** It's easier to find and grow engineers who can
  safely extend a codebase with a handful of well-isolated unsafe blocks
  than to find engineers who can be trusted with a fully unchecked C
  codebase at the same performance tier.

The case for sticking with C: an existing large C codebase, a target where
Rust support is immature (some embedded/legacy platforms), or a team with
deep C expertise and no Rust experience where migration cost would outweigh
the safety benefit. For a greenfield high-performance service, this
benchmark's own numbers remove "C is faster" as a reason to choose it over
Rust.
