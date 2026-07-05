# TechEmpower-style benchmark: hand-rolled unsafe Rust vs. a conservative safe-Rust API

A fun exercise inspired by the TechEmpower Web Framework Benchmarks: how much
throughput is left on the table between a "just write it the normal way" Rust
web API and a hand-rolled, unsafe, raw-socket server built specifically to go
fast?

Two projects are compared:

| | `unsafe-rust-server` (this repo) | `rust_pure_api` |
|---|---|---|
| Style | Hand-rolled, unsafe, no HTTP/web framework | Idiomatic, safe, `actix-web` + `diesel` |
| Networking | Raw BSD sockets + `kqueue`, `SO_REUSEPORT`, 1 thread/core | `actix-web` (`tokio` under the hood) |
| Data store | SQLite, hand-written FFI, one connection per worker thread | Postgres via `diesel` + `r2d2` pool (**default pool size: 10**) |
| Tuning | `lto=fat`, `codegen-units=1`, `panic=abort`, `-C target-cpu=native` | Default `cargo build --release`, untouched |
| Endpoint tested | `GET /db` — random row lookup, 10,000-row table | `GET /v1/users/{id}` — random row lookup, 10,000-row table |

`rust_pure_api` has no dependency-free "static, no DB" route, so there's no
counterpart to this repo's `/json` endpoint — only the single-row DB lookup is
compared apples-to-apples.

## Machine

- MacBook Pro, Intel Core i9-9880H (8 physical / 16 logical cores), macOS 14.5
- Load generator: `wrk 4.2.0`, run on the **same machine** as the server (client
  and server compete for the same 8 physical cores — see caveats below)

## Results

`wrk -t4 -c128 -d15s --latency`, random id 1–10,000 per request.

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `unsafe-rust-server` `/json` (no DB) | **114,762** | 1.07ms | 1.22ms | 1.97ms |
| `unsafe-rust-server` `/db` (SQLite) | **48,638** | 2.48ms | 2.78ms | 5.13ms |
| `rust_pure_api` `/v1/users/{id}` (Postgres) | **12,319** | 7.25ms | 26.65ms | 40.68ms |

At the DB endpoint, the hand-rolled unsafe server pushed roughly **4x** the
throughput of the conservative actix+diesel+Postgres app, with much tighter
tail latency.

A second run against `rust_pure_api` at `-c10` (matching the r2d2 pool size
exactly) shows the pool, not "safe Rust" as a language, is the dominant
bottleneck at higher concurrency:

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `rust_pure_api` `/v1/users/{id}` at `-c10` | 10,814 | 0.58ms | 11.28ms | 17.27ms |
| `rust_pure_api` `/v1/users/{id}` at `-c128` | 12,319 | 7.25ms | 26.65ms | 40.68ms |

Throughput barely moves once concurrency exceeds the pool size — the extra
118 connections at `-c128` just queue for one of 10 DB connections, which is
where most of that 7ms p50 (vs 0.58ms at `-c10`) comes from.

## Why this isn't a clean "safe vs unsafe" verdict

The gap above is real but it's not isolating the *language safety* variable —
several architectural differences are stacked on top of it:

- **SQLite in-process vs. Postgres over a TCP socket.** Postgres pays IPC/TCP
  round-trip cost per query that SQLite (linked directly into the process)
  doesn't.
- **Connection strategy.** The unsafe server gives each of its 16 worker
  threads a dedicated, never-contended SQLite connection. `rust_pure_api` pools
  only 10 Postgres connections behind `r2d2`'s default config, shared across
  all `actix-web` workers — an explicit bottleneck at any concurrency above 10.
- **Sync-DB-on-async-runtime tax.** `diesel` is synchronous, so `rust_pure_api`
  hops every query through `web::block` (a `tokio` blocking-thread-pool
  dispatch) — extra scheduling overhead the unsafe server's thread-per-core
  loop never pays.
- **Framework layers.** `actix-web` gives you routing, extractors, middleware,
  and JSON (de)serialization out of the box; the unsafe server hand-parses
  bytes and has none of that — it's not carrying the same feature set.
- **Tuning asymmetry, deliberately.** The unsafe server is built with LTO,
  `codegen-units=1`, and `target-cpu=native`; `rust_pure_api` is built with
  plain `cargo build --release`, matching how it was actually used/tried. That
  gap is intentional — this compares "the fastest I could reasonably build" to
  "what you get by default" — but it does mean part of the delta is compiler
  flags, not `unsafe`.
- **Client contention.** `wrk` and the server share the same 8 physical cores
  on a single laptop; a two-machine setup would isolate server-side throughput
  more cleanly.

The honest takeaway: `unsafe` Rust *can* buy a large multiple over a default,
un-tuned safe-Rust web stack — but most of that multiple here comes from
architecture (raw sockets, no pooling bottleneck, in-process data store, zero
framework overhead), not from `unsafe` blocks specifically. A safe Rust stack
with a properly sized connection pool, `sqlx` instead of `web::block`-wrapped
`diesel`, and LTO turned on would likely close a good chunk of this gap
without touching `unsafe` at all.

## Reproducing

**`unsafe-rust-server`:**
```
RUSTFLAGS="-C target-cpu=native" cargo build --release
./target/release/unsafe-rust-server &
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/db
```

**`rust_pure_api`** (requires Postgres running locally):
```
createdb rust_pure_api
psql -d rust_pure_api -f migrations/2019-30-09-174926_create_users/up.sql
psql -d rust_pure_api -f migrations/2019-10-11-214632_create_sessions/up.sql
echo "DATABASE_URL=postgres://$(whoami)@localhost/rust_pure_api" > .env
cargo build --release
./target/release/rust_pure_api &
wrk -t4 -c128 -d15s --latency -s random_user_id.lua http://127.0.0.1:8088
```
where `random_user_id.lua` is:
```lua
math.randomseed(os.time())
request = function()
   local id = math.random(1, 10000)
   return wrk.format("GET", "/v1/users/" .. id)
end
```

Note: as committed, `rust_pure_api`'s migration creates unquoted (lowercase-
folded) Postgres columns while `src/schema.rs` expects mixed-case column names
(`Id`, `playerNumber`, `firstName`, `lastName`, `isBanned`) — every DB query
500s with `column users.Id does not exist` until those columns are renamed to
match, e.g.:
```sql
ALTER TABLE users RENAME COLUMN id TO "Id";
ALTER TABLE users RENAME COLUMN playernumber TO "playerNumber";
ALTER TABLE users RENAME COLUMN firstname TO "firstName";
ALTER TABLE users RENAME COLUMN lastname TO "lastName";
ALTER TABLE users RENAME COLUMN isbanned TO "isBanned";
```
