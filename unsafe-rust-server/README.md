# TechEmpower-style benchmark: unsafe Rust vs. C vs. safe Rust vs. Python/ASGI

A fun exercise inspired by the TechEmpower Web Framework Benchmarks: how much
throughput separates a hand-rolled, unsafe, raw-socket Rust server from the
same architecture in hand-rolled C, from a "normal" safe-Rust web API, and
from a lean Python async stack?

Four projects are compared:

| | `unsafe-rust-server` (this repo) | `c-server` (this repo) | `rust_pure_api` | `uvicorn` |
|---|---|---|---|---|
| Language | Rust, `unsafe` | C | Rust, safe | Python 3.10 |
| Style | Hand-rolled, no HTTP/web framework | Hand-rolled, identical architecture to `unsafe-rust-server` | Idiomatic, `actix-web` + `diesel` | Raw ASGI app (no framework), `gunicorn` + `uvicorn` workers |
| Networking | Raw BSD sockets + `kqueue`, `SO_REUSEPORT`, 1 thread/core | Same | `actix-web` (`tokio` under the hood) | `uvloop` + `httptools`, 1 `gunicorn` worker process/core |
| Data store | SQLite, hand-written FFI, one connection per worker thread | SQLite via the real `sqlite3.h`, one connection per worker thread | Postgres via `diesel` + `r2d2` pool (default size 10) | Postgres via `asyncpg` pool (default size 10/worker) |
| Tuning | `lto=fat`, `codegen-units=1`, `panic=abort`, `-C target-cpu=native` | `clang -O3 -flto -march=native` | Default `cargo build --release`, untouched | Default `pip install`, untouched app code |
| Endpoint tested | `GET /db` — random row by id, 10,000-row table | `GET /db` — same table, same dataset | `GET /v1/users/{id}` — random row by id, 10,000-row table | `GET /auth/login?id=..&password=..` — random row by id, 10,000-row table |

`c-server` was written from scratch to mirror `unsafe-rust-server`'s
architecture line-for-line after its optimization pass: same kqueue event
loop, same zero-allocation response building, same flat fd-indexed
connection table, same best-effort Mach affinity pinning, same SQLite schema
and dataset (it points at the same `/tmp/techempower_world.db`). The goal was
to see whether hand-written C could beat an equivalently-optimized unsafe
Rust implementation doing the exact same thing. See "C vs. unsafe Rust" below
for the result.

Neither `rust_pure_api` nor `uvicorn` has a dependency-free "static, no DB"
route, so there's no counterpart to the `/json` endpoint in those two — only
the single-row DB lookup is compared apples-to-apples across all four.

## Machine

- MacBook Pro, Intel Core i9-9880H (8 physical / 16 logical cores), macOS 14.5
- Load generator: `wrk 4.2.0`, run on the **same machine** as the server (client
  and server compete for the same cores — see caveats below)

## Results

`wrk -t4 -c128 -d15s --latency`, random id 1–10,000 per request, 10,000-row
table for every implementation.

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `unsafe-rust-server` `/json` (no DB), v1 | **114,762** | 1.07ms | 1.22ms | 1.97ms |
| `unsafe-rust-server` `/db` (SQLite), v1 | **48,638** | 2.48ms | 2.78ms | 5.13ms |
| `uvicorn` `/auth/login` (Postgres, asyncpg) | **16,440** | 6.91ms | 18.72ms | 71.08ms |
| `rust_pure_api` `/v1/users/{id}` (Postgres, diesel) | **12,319** | 7.25ms | 26.65ms | 40.68ms |

v1 is the first pass at `unsafe-rust-server`; see "Optimization pass" below for
a second, more tuned version and why its `/json` number barely moved despite
real server-side improvements, and "C vs. unsafe Rust" further below for a
from-scratch C rewrite of the same architecture.

At the DB endpoint, the hand-rolled unsafe server pushed roughly **4x** the
throughput of the Python/asyncpg stack and **~7x** the Rust/diesel stack — but
see below, this is not an apples-to-apples "unsafe beats safe beats Python"
result; each stack differs in more than one variable at once.

A second run against `rust_pure_api` at `-c10` (matching the r2d2 pool size
exactly) shows the pool, not "safe Rust" as a language, is a dominant
bottleneck at higher concurrency:

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `rust_pure_api` `/v1/users/{id}` at `-c10` | 10,814 | 0.58ms | 11.28ms | 17.27ms |
| `rust_pure_api` `/v1/users/{id}` at `-c128` | 12,319 | 7.25ms | 26.65ms | 40.68ms |

Throughput barely moves once concurrency exceeds the pool size — the extra
118 connections at `-c128` just queue for one of 10 DB connections, which is
where most of that 7ms p50 (vs 0.58ms at `-c10`) comes from.

`uvicorn`, by contrast, has 16 `gunicorn` worker **processes** each with their
own 10-connection `asyncpg` pool (up to 160 DB connections total), so it isn't
pool-starved the same way at `-c128` — its p50 is close to `rust_pure_api`'s
but its throughput is ~33% higher and its p99 tail is worse (more variance
across 16 independent Python processes and GC/event-loop scheduling).

## Optimization pass

After the first pass, four concrete headroom items were identified (see the
"is this the fastest unsafe Rust can go" discussion). Four were implemented;
one was intentionally skipped and one is impossible on this machine:

| # | Optimization | Done? |
|---|---|---|
| 1 | Zero-allocation responses: precompute `/json` once per thread and patch only the 29-byte Date field in place; build `/db` responses into a reused stack buffer with hand-written integer-to-ASCII instead of `to_string()`/`Vec` allocation | ✅ |
| 2 | Batch pipelined responses with `writev` | ⏭️ skipped (explicitly excluded) |
| 3 | Flat `Vec<Option<Conn>>` indexed directly by fd instead of `HashMap<RawFd, Conn>` — no hashing per event | ✅ |
| 4 | Best-effort core affinity via macOS's Mach `thread_policy_set(THREAD_AFFINITY_POLICY)` | ✅ (hint only — see caveat) |
| 5 | `io_uring` | ❌ not possible here — it's a **Linux kernel feature**, unavailable on macOS/Darwin at any layer. Doing this properly means either a Linux VM/container (which would confound the "same machine" comparison with virtualization overhead) or a genuinely separate Linux benchmark run. Not attempted. |

Item 4's caveat, confirmed in practice: macOS's thread affinity API is
documented as an *affinity set* hint for cache locality, not a hard pin like
Linux's `sched_setaffinity` — the kernel is free to ignore it, and Apple
doesn't guarantee any particular behavior (especially on Apple Silicon; this
machine is Intel, where the hint has historically been honored more often).
Treat it as "asked nicely," not "pinned."

### Results after the optimization pass

Same methodology: `wrk -t4 -c128 -d15s --latency`.

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `unsafe-rust-server` `/json`, v2 (optimized) | **118,834** | 1.01ms | 1.19ms | 2.22ms |
| `unsafe-rust-server` `/db`, v2 (optimized) | **52,474** | 2.26ms | 2.66ms | 7.23ms |

- `/db`: **48,638 → 52,474 req/s (+7.9%)** — a real, clearly-visible gain.
  Removing the per-request `Vec` allocations and hash-map lookup shaved
  measurable overhead off a request that's otherwise dominated by the SQLite
  round trip.
- `/json`: **114,762 → 118,834 req/s (+3.5%)** — real, but much smaller than
  expected given the hot path went from "several heap allocations and a hash
  lookup per request" to "zero allocations and an array index."

The `/json` number is almost certainly **understated** by client-side
contention, not by the optimization being ineffective. Repeating the `/json`
benchmark at `-t2 -c64`, `-t3 -c96`, and `-t6 -c192` all landed in the same
116,500–118,800 req/s band regardless of client concurrency — the same
plateau behavior seen in the v1 benchmarking. `wrk` and the server share the
same 8 physical cores, and generating/measuring ~118k HTTP requests per
second is itself real CPU work; at this rate `wrk` looks like it's the
bottleneck, not the server. `/db` shows a cleaner signal because its
per-request cost is high enough (SQLite round trip) that the request rate,
and therefore `wrk`'s own CPU load, is roughly 2x lower — leaving more
headroom for the server-side improvement to actually show up in the number.
Confirming the true server-side ceiling would need a two-machine setup (a
separate load-generation host), which wasn't done here.

## C vs. unsafe Rust

**Short answer: no, the C rewrite did not beat the optimized unsafe-Rust
server.** Across repeated back-to-back runs on this machine, the two are
close enough to call a rough tie, with unsafe Rust holding a small, fairly
consistent edge.

The first head-to-head (C tested alone, compared against the Rust numbers
from the optimization pass measured earlier in the session) showed C
noticeably behind on `/db` — 48,782 vs. 52,474 req/s. That looked like a real
gap worth explaining, so before writing it up, both servers were re-tested
**immediately back-to-back** under identical system conditions (same running
processes, same thermal state) rather than trusting numbers measured minutes
apart. That changed the picture substantially:

| | C | Rust |
|---|---|---|
| `/db`, run 1 | 48,782 | 49,849 |
| `/db`, run 2 | 49,692 | 49,565 |
| `/db`, run 3 (final, 15s) | 46,761 | 50,061 |
| `/json`, run 1 | 116,873 | 123,812 |
| `/json`, run 2 | 122,850 | — |
| `/json`, run 3 (final, 15s) | 117,145 | 123,441 |

Rust comes out ahead in every single trial once measured back-to-back, by
roughly 3-7%, even though the two programs are doing textually the same
thing: same kqueue event loop shape, same zero-allocation response
construction, same flat connection table, same SQLite prepared-statement
reuse, same dataset. That consistency (small but one-directional across 6+
runs) suggests it isn't pure noise — but it's also nowhere near the kind of
gap that would suggest C has some fundamental edge being left on the table by
Rust, or vice versa. The likely explanation is mundane: `rustc`/LLVM and
`clang`/LLVM made slightly different inlining/register-allocation decisions
for this specific ~400-line hot path, and the Rust build profile
(`codegen-units=1`, `panic=abort`) removes unwinding tables and forces
whole-crate codegen as a single unit, which has no direct C equivalent to
match exactly.

The bigger takeaway from this exercise: once you're both doing raw syscalls,
manual HTTP parsing, and zero-allocation response building, **the
language stops mattering much** — C's famous low-level edge doesn't show up
here because unsafe Rust, at this level, generates comparably tight code
through the same LLVM backend. The earlier, much larger gaps in this
README (4-7x) came entirely from *architecture* — raw sockets vs. a web
framework, in-process SQLite vs. Postgres-over-a-socket, a dedicated
connection per thread vs. a shared pool — not from which of these two
languages was used to write the raw-socket version.

## Why this isn't a clean "unsafe > safe Rust > Python" verdict

Several architectural differences are stacked on top of the language/safety
variable:

- **SQLite in-process vs. Postgres over a TCP/Unix socket.** Postgres pays
  IPC round-trip cost per query that SQLite (linked directly into the
  process) doesn't. This alone likely accounts for a large chunk of the gap
  between the unsafe server's `/db` and either Postgres-backed stack.
- **Connection strategy.** The unsafe server gives each of its 16 worker
  threads a dedicated, never-contended SQLite connection. `rust_pure_api`
  pools only 10 Postgres connections total (shared across all `actix-web`
  workers); `uvicorn` pools 10 *per worker process* (16 workers = up to 160
  connections) — a very different scaling shape from either Rust app.
- **Sync-DB-on-async-runtime tax.** `diesel` is synchronous, so
  `rust_pure_api` hops every query through `web::block` (a `tokio`
  blocking-thread-pool dispatch); `uvicorn`'s `asyncpg` is natively async, so
  it doesn't pay that particular tax — one reason it edges out `rust_pure_api`
  despite being the interpreted, GIL-bound language of the three.
- **Framework/process layers.** `actix-web` and ASGI/`gunicorn` both give you
  routing, process management, and (de)serialization out of the box; the
  unsafe server hand-parses bytes and has none of that.
- **Tuning asymmetry, deliberately.** The unsafe server is built with LTO,
  `codegen-units=1`, and `target-cpu=native`; both other stacks are run with
  their defaults, matching how each was actually built/tried. That gap is
  intentional — this compares "the fastest I could reasonably build" against
  "what you get by default" in each ecosystem — but it does mean part of the
  Rust-vs-Rust delta is compiler flags, not `unsafe` itself.
- **Client contention.** `wrk` and the server share the same cores on a single
  laptop; a two-machine setup would isolate server-side throughput more
  cleanly, and would matter more for `uvicorn`'s 16 OS processes than for the
  two Rust binaries.

The honest takeaway: `unsafe` Rust *can* buy a large multiple over both a
default safe-Rust web stack and a lean Python/ASGI stack — but most of that
multiple here comes from architecture (raw sockets, an in-process data store,
no pooling bottleneck, zero framework overhead), not from `unsafe` blocks or
even the language specifically. Notably, the "conservative" Python stack beat
the "conservative" safe-Rust stack here, entirely because of a pooling/async
mismatch in the Rust app, not because Python is faster than Rust.

## Bugs found and fixed while setting this up

Both other projects had pre-existing issues that had nothing to do with this
benchmark — worth fixing upstream in each repo:

- **`rust_pure_api`**: the migration creates unquoted (lowercase-folded)
  Postgres columns while `src/schema.rs` expects mixed-case column names
  (`Id`, `playerNumber`, `firstName`, `lastName`, `isBanned`). Every DB query
  500s with `column users.Id does not exist` until those columns are renamed
  to match (see Reproducing steps below). This means the app likely never
  successfully queried a real Postgres instance before.
- **`uvicorn`**: `requirements.txt` pins `asyncpg==0.21.0`, whose C extension
  uses CPython internals (`_PyGen_Send`) removed from modern Python — it
  fails to build at all on Python 3.10+/current Xcode clang. Installing an
  unpinned, current `asyncpg` (0.31.0) fixed this with no app-code changes.
- **`uvicorn`**: with `workers = multiprocessing.cpu_count()` (16 on this
  machine) and `asyncpg`'s default pool `min_size=10`, the app tries to open
  up to 160 Postgres connections at startup — comfortably over this machine's
  default `max_connections = 100`. Workers that lost the race got a `None`
  pool and 500'd on every request, and Postgres itself became briefly
  unreachable ("sorry, too many clients already"). Fixed locally by raising
  `max_connections` to 250 in `postgresql.conf` and restarting the service —
  not an app change, but worth knowing if you run this with many workers.

## Reproducing

**`unsafe-rust-server`:**
```
RUSTFLAGS="-C target-cpu=native" cargo build --release
./target/release/unsafe-rust-server &
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/db
```

**`c-server`** (shares the same `/tmp/techempower_world.db` — run
`unsafe-rust-server` first at least once so it exists and is seeded, or let
`c-server` seed it itself on first run):
```
cd ../c-server
clang -O3 -flto -march=native -o server main.c -lsqlite3
./server &
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:8080/db
```
Only one of `unsafe-rust-server`/`c-server` can run at a time — both bind
`0.0.0.0:8080`.

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
— rename them to match before it'll run against a real DB:
```sql
ALTER TABLE users RENAME COLUMN id TO "Id";
ALTER TABLE users RENAME COLUMN playernumber TO "playerNumber";
ALTER TABLE users RENAME COLUMN firstname TO "firstName";
ALTER TABLE users RENAME COLUMN lastname TO "lastName";
ALTER TABLE users RENAME COLUMN isbanned TO "isBanned";
```

**`uvicorn`** (requires Postgres running locally):
```
python3 -m venv .venv && source .venv/bin/activate
pip install asyncpg gunicorn orjson markupsafe "uvicorn[standard]"
createdb uvicorn
psql -d uvicorn -c 'CREATE TABLE users (id TEXT PRIMARY KEY, password TEXT NOT NULL, "firstName" TEXT NOT NULL, "lastName" TEXT NOT NULL);'
psql -d uvicorn -c "INSERT INTO users (id, password, \"firstName\", \"lastName\") SELECT g::text, 'pass', 'First'||g::text, 'Last'||g::text FROM generate_series(1, 10000) AS g;"
PGUSER=$(whoami) PGPASS="" gunicorn -c uvicorn_conf.py app:main &
wrk -t4 -c128 -d15s --latency -s random_auth_login.lua http://127.0.0.1:8080
```
where `random_auth_login.lua` is:
```lua
math.randomseed(os.time())
request = function()
   local id = math.random(1, 10000)
   return wrk.format("GET", "/auth/login?id=" .. id .. "&password=pass")
end
```

If you hit build errors installing `requirements.txt` as pinned, or 500s /
"too many clients already" once running, see the Bugs section above — install
an unpinned `asyncpg` and raise Postgres's `max_connections` past
`workers * pool_size`.
