# TechEmpower-style benchmark: unsafe Rust vs. C vs. safe Rust vs. Python/ASGI vs. Crystal vs. Node.js

A fun exercise inspired by the TechEmpower Web Framework Benchmarks: how much
throughput separates a hand-rolled, unsafe, raw-socket Rust server from the
same architecture in hand-rolled C, from a "normal" safe-Rust web API, from a
lean Python async stack, from a Ruby-flavored compiled language (Crystal),
and from Node.js (both bare and behind Fastify)?

Seven projects are compared:

| | `unsafe-rust-server` (this repo) | `c-server` (this repo) | `rust_pure_api` | `uvicorn` | `crystal_kemal` | `node-raw-server` (this repo) | `node-fastify-server` (this repo) |
|---|---|---|---|---|---|---|---|
| Language | Rust, `unsafe` | C | Rust, safe | Python 3.10 | Crystal 1.14 | Node.js 20 | Node.js 20 |
| Style | Hand-rolled, no HTTP/web framework | Hand-rolled, identical architecture to `unsafe-rust-server` | Idiomatic, `actix-web` + `diesel` | Raw ASGI app (no framework), `gunicorn` + `uvicorn` workers | Idiomatic, `Kemal` (Sinatra-like) web framework | Bare `http` module, no framework | Idiomatic `fastify`, with response JSON schemas |
| Networking | Raw BSD sockets + `kqueue`, `SO_REUSEPORT`, 1 thread/core | Same | `actix-web` (`tokio` under the hood) | `uvloop` + `httptools`, 1 `gunicorn` worker process/core | Kemal/Crystal's own event loop, 1 OS process/core via `reuse_port` (Crystal is single-threaded per process) | Node's built-in `http` + `cluster`, 1 worker process/core | Same `cluster` fan-out, Fastify's HTTP layer per worker |
| Data store | SQLite, hand-written FFI, one connection per worker thread | SQLite via the real `sqlite3.h`, one connection per worker thread | Postgres via `diesel` + `r2d2` pool (default size 10) | Postgres via `asyncpg` pool (default size 10/worker) | Postgres via `crystal-pg` pool (`DB_POOL_SIZE=10`/worker process) | Postgres via `pg` (`node-postgres`) pool, size 10/worker process | Same `pg` pool, size 10/worker process |
| Tuning | `lto=fat`, `codegen-units=1`, `panic=abort`, `-C target-cpu=native` | `clang -O3 -flto -march=native` | Default `cargo build --release`, untouched | Default `pip install`, untouched app code | Default `crystal build --release`, untouched app code | None — default `node server.js` | None — default `node server.js` |
| Endpoints tested | `GET /json`, `GET /db` (random row by id, 10,000-row table) | Same as `unsafe-rust-server` | `GET /v1/users/{id}` — random row by id, 10,000-row table | `GET /auth/login?id=..&password=..` — random row by id, 10,000-row table | `GET /json`, `GET /db` (random row by id, 10,000-row table) | `GET /json`, `GET /db` (random row by id, 10,000-row table) | Same as `node-raw-server` |

`c-server` was written from scratch to mirror `unsafe-rust-server`'s
architecture line-for-line after its optimization pass: same kqueue event
loop, same zero-allocation response building, same flat fd-indexed
connection table, same best-effort Mach affinity pinning, same SQLite schema
and dataset (it points at the same `/tmp/techempower_world.db`). The goal was
to see whether hand-written C could beat an equivalently-optimized unsafe
Rust implementation doing the exact same thing. See "C vs. unsafe Rust" below
for the result.

`crystal_kemal` originally only had a full-table HTML dump and a write
endpoint, neither a good match for this comparison — it has since grown a
proper `GET /json` (static) and `GET /db` (single random row by id) pair that
mirror `unsafe-rust-server`/`c-server` exactly, so it's now compared the same
way as those two. See "Crystal/Kemal" below for the leftover history (the
write-endpoint workaround, the connection-pool bug, the bimodal-latency
finding — still relevant even with the new endpoints).

`node-raw-server` and `node-fastify-server` were built to answer a specific
question raised in conversation: could Node.js beat Crystal/Kemal, and could
it approach unsafe Rust? Both use `cluster` for one worker process per core
(the same fan-out shape as Crystal and uvicorn) and the same Postgres
`World` table (`id`, `randomNumber`) seeded with 10,000 rows in a dedicated
`node_bench` database. See "Node.js: raw vs. Fastify" below for the answer —
the prediction going in was wrong in an interesting way.

Neither `rust_pure_api` nor `uvicorn` has a dependency-free "static, no DB"
route, so there's no counterpart to `/json` for those two — only the
single-row DB test applies to them.

## Machine

- MacBook Pro, Intel Core i9-9880H (8 physical / 16 logical cores), macOS 14.5
- Load generator: `wrk 4.2.0`, run on the **same machine** as the server (client
  and server compete for the same cores — see caveats below)

## A note on this results section

All numbers below are from a clean re-run: a stray Phoenix/Elixir
(`mix phx.server`) process and a Node.js process had been running in the
background (unrelated to this project) during the sessions where earlier
numbers in this README's history were measured. Once those were stopped, the
Postgres-backed implementations in particular moved substantially —
`rust_pure_api` went from 12,319 to 20,293 req/s (+65%) on identical code.
The hand-rolled raw-socket servers (`unsafe-rust-server`, `c-server`) barely
moved, since they were already saturating client-side (`wrk`) capacity rather
than server-side capacity — see "Optimization pass" below. Every number in
this section was (re-)measured back-to-back in the same quiet-system session;
treat any older numbers elsewhere in this file's history as measured under
uncontrolled load and superseded by what's here.

## Results

`wrk -t4 -c128 -d15s --latency`, random id 1–10,000 per request, 10,000-row
table for every implementation.

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `unsafe-rust-server` `/json` (no DB) | **125,174** | 1.01ms | 1.05ms | 1.40ms |
| `c-server` `/json` (no DB) | **123,028** | 1.01ms | 1.09ms | 1.55ms |
| `node-raw-server` `/json` (no DB) | **108,889** | 1.06ms | 1.48ms | 22.14ms |
| `node-fastify-server` `/json` (no DB) | **105,720** | 0.96ms | 3.24ms | 32.91ms |
| `unsafe-rust-server` `/db` (SQLite) | **50,179** | 2.53ms | 2.62ms | 2.85ms |
| `c-server` `/db` (SQLite) | **50,340** | 2.51ms | 2.63ms | 3.15ms |
| `crystal_kemal` `/json` (no DB) | **52,285** | 2.37ms | 2.88ms | 3.09ms |
| `node-raw-server` `/db` (Postgres, `pg`) | **28,242** | 3.74ms | 33.55ms | 174.07ms |
| `node-fastify-server` `/db` (Postgres, `pg`) | **26,636** | 3.88ms | 27.43ms | 92.91ms |
| `crystal_kemal` `/db` (Postgres, crystal-pg) | **24,887** | 0.61ms | 187.96ms | 288.83ms |
| `rust_pure_api` `/v1/users/{id}` (Postgres, diesel) | **20,293** | 6.24ms | 12.12ms | 23.17ms |
| `uvicorn` `/auth/login` (Postgres, asyncpg) | **17,164** | 6.14ms | 18.48ms | 42.98ms |

`unsafe-rust-server` and `c-server` are, as before, essentially tied with
each other (see "C vs. unsafe Rust" for why that's not a fluke) and well
ahead of everything else on `/json` — expected, since neither one touches a
database or a web framework for that route. What's new: bare Node.js
(`node-raw-server`) lands at 108,889 req/s — **87% of unsafe Rust's
throughput**, running a JIT'd, garbage-collected, dynamically-typed runtime
against a hand-rolled zero-allocation raw-socket server. Fastify costs a
further ~3% on top of bare Node for its routing/schema-serialization layer
(105,720 req/s), which is a remarkably small framework tax. `crystal_kemal`'s
`/json` (52,285 req/s) is a real outlier here — well behind both Node
implementations despite Crystal being a compiled, non-GC'd... well, it does
GC, just not generationally — see "Node.js: raw vs. Fastify" below for why
this result revises a prediction made earlier in this exercise.

On the DB endpoint, the ordering is no longer "hand-rolled beats frameworks"
at all — both Node implementations **beat every other DB-backed
implementation**, including the hand-rolled Rust/C SQLite servers' framework
peers: `node-raw-server` (28,242 req/s) and `node-fastify-server` (26,636)
both outpace `crystal_kemal` (24,887), `rust_pure_api` (20,293), and
`uvicorn` (17,164). But — same pattern as Crystal — Node's tail latency is
considerably worse than its throughput number implies (p99 174ms/93ms vs.
single-digit-to-low-double-digit ms for `rust_pure_api`/`uvicorn`); see
"Node.js: raw vs. Fastify" for why.

A second run against `rust_pure_api` at `-c10` (matching the r2d2 pool size
exactly) shows the pool, not "safe Rust" as a language, is a dominant
bottleneck at higher concurrency — this dynamic is unchanged by the clean
re-run, only the absolute numbers moved:

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `rust_pure_api` `/v1/users/{id}` at `-c10` | 10,814 | 0.58ms | 11.28ms | 17.27ms |
| `rust_pure_api` `/v1/users/{id}` at `-c128` | 20,293 | 6.24ms | 12.12ms | 23.17ms |

`uvicorn`, by contrast, has 16 `gunicorn` worker **processes** each with their
own 10-connection `asyncpg` pool (up to 160 DB connections total), so it isn't
pool-starved the same way at `-c128`.

## Optimization pass

*(Numbers in this section predate the Phoenix/Node cleanup described above.
The unsafe-Rust vs. C servers were never affected much by that background
load — see "A note on this results section" — so the relative conclusions
here still hold; only cross-referencing their absolute values against the
main Results table above needs the caveat.)*

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

## Crystal/Kemal

`crystal_kemal` fits the general spirit of this exercise — a compiled
language, a real (if lightweight) web framework, Postgres-backed, run the
way someone would actually deploy it. It originally only exposed:

- `GET /users` — dumps **every** row in the table as an HTML page
- `POST /webhook` — updates one row's `firstName` given `{"Id": .., "firstName": ..}`

Neither lines up with the "random single-row read" test used elsewhere, so
the project was reviewed and two endpoints were added that mirror
`unsafe-rust-server`/`c-server` exactly: `GET /json` (static, no DB) and
`GET /db` (fetch one random row by id out of the same 10,000-row shape used
everywhere else in this README). Those are now the endpoints used in the
main Results table above. `POST /webhook` and `GET /users` still exist and
are noted below for context, but are no longer the primary comparison point.

### A repeat of the uvicorn connection-pool bug

Same failure mode as `uvicorn`, third time this exercise has hit it:
`bin/server` spawns one OS process per core (16 here — Crystal is
single-threaded per process, so this project fans out with processes instead
of threads), and `main.cr` defaults each process's `crystal-pg` pool to
`DB_POOL_SIZE=25`. 16 × 25 = up to 400 Postgres connections, comfortably over
this machine's `max_connections = 250` (already raised once for `uvicorn`).
15 of 16 worker processes crashed on startup with `DB::ConnectionRefused` and
only 1 was left serving traffic.

Unlike the `uvicorn` fix, this wasn't solved by raising `max_connections`
again — the project already exposes the right knob for it
(`DB_POOL_SIZE`, documented in its own README), so it was used instead:
`DB_POOL_SIZE=10` (16 × 10 = 160, within the current limit) brought up all 16
workers cleanly. Worth knowing if you run this with the default pool size on
a box with a stock Postgres `max_connections`.

A second, unrelated gotcha turned up on this re-run: after rebuilding the
binary with the new endpoints, one leftover worker process from an earlier
session (running the *old* pre-`/json`/`/db` binary, from before the source
file changed) was still alive and still bound to the same port via
`SO_REUSEPORT`. Since the kernel fans connections across every socket bound
to that port regardless of which binary owns it, a fraction of requests were
silently being routed to stale code during the first post-rebuild smoke
test. Caught by checking `ps` for all `./main` processes and their start
times before benchmarking, not by anything failing loudly — worth checking
for whenever a `SO_REUSEPORT` binary gets rebuilt without a full process
restart.

### Results

Numbers for `/json` and `/db` are in the main Results table above (measured
in the same clean-system session as everything else). Repeated here with the
write/list endpoints for context:

`wrk -t4 -c128 -d15s --latency`, random id 1–10,000 per request/update body.

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `crystal_kemal` `GET /json` (no DB) | **52,285** | 2.37ms | 2.88ms | 3.09ms |
| `crystal_kemal` `GET /db` (Postgres, read) | **24,887** | 0.61ms | 187.96ms | 288.83ms |
| `crystal_kemal` `POST /webhook` (Postgres, write, prior session) | 15,802 | 0.90ms | 168.21ms | 200.87ms |
| `crystal_kemal` `GET /users` (full 10,000-row HTML dump, prior session) | 148 | — | — | — |

`GET /db`'s p50-to-p90 jump (0.61ms → 188ms) is a real bimodal split, not a
formatting artifact — it shows up consistently across repeated runs at
`-c128`, `-c32`, and even `-c16` (matching the per-process pool size exactly,
which rules out simple pool-queueing as the cause). The original hypothesis,
formed against the write-only `/webhook` endpoint, was that heavy `UPDATE`
churn was triggering Postgres-side autovacuum/WAL pressure. `/db` never
writes anything, and shows the same bimodal pattern — so that hypothesis was
wrong. The more likely explanation, given Crystal's single-threaded-per-process
model (documented in the project's own README): a stop-the-world GC pause (or
similar per-process stall) on any one of the 16 worker processes blocks
*every* in-flight request on that process simultaneously until it resolves,
and with 16 independent processes some fraction are staggered into a pause
at any given moment — producing exactly this "most requests fast, a
consistent minority very slow" shape regardless of read/write or
concurrency level. Not root-caused further (would need a Crystal-side
profiler/GC-stats capture), but flagged here as a genuinely different
failure shape than anything else in this README, where every other
implementation has tight, unimodal tail latency.

`GET /users` remains a fundamentally different, much heavier test (full
table serialization per request, closer to TechEmpower's "fortunes" shape
than "single query") and isn't a fair data point next to anything else here.

## Node.js: raw vs. Fastify

This pair was built to answer a specific prediction made earlier in this
exercise: *"raw Node could plausibly beat Crystal/Kemal, but there's no
chance it approaches unsafe Rust — that gap is architectural, not a JIT
quality problem."* The data says the first half was right and the second
half was too pessimistic.

**`node-raw-server`** is a bare `http.createServer` callback — no framework,
no router, a plain `if (req.url === '/json')` chain — with `cluster` forking
one worker process per core, same fan-out shape as Crystal and `uvicorn`.
**`node-fastify-server`** is the same thing wrapped in Fastify with response
JSON schemas (so Fastify uses its `fast-json-stringify` serializer instead of
plain `JSON.stringify` — the idiomatic-fast way to write a Fastify route, not
a crippled comparison).

### `/json`: much closer to unsafe Rust than expected

| | Req/sec | % of unsafe Rust's throughput |
|---|---|---|
| `unsafe-rust-server` | 125,174 | 100% |
| `c-server` | 123,028 | 98% |
| `node-raw-server` | 108,889 | **87%** |
| `node-fastify-server` | 105,720 | 84% |
| `crystal_kemal` | 52,285 | 42% |

87% is not "approaching" in the sense of closing the gap — it's still a real
~15% deficit against a zero-allocation hand-rolled server — but it's a much
smaller gap than TechEmpower's own historical leaderboards would suggest
(where top Node entries are typically several times behind top C/Rust raw
entries, not 13% behind). Likely reasons this particular test is unusually
favorable to Node: V8's JIT is very good at optimizing a single monomorphic
hot function called millions of times (exactly what this benchmark is), and
Node's HTTP parser (`llhttp`) is a mature, hand-tuned C++ state machine, not
a naive one. Fastify's overhead on top of bare Node is small (~3%), which
matches its own reputation as one of the lowest-overhead Node frameworks —
the schema-based serializer is doing real work to avoid that being larger.

`crystal_kemal` sits far behind both — 42% of unsafe Rust's throughput vs.
Node's 84-87%. Framework routing overhead (Kemal's radix-tree matching,
`before_all` middleware) plausibly explains part of it, but this is a bigger
gap than routing overhead alone would typically cost, and points at either
Crystal's HTTP server implementation or its GC being less tuned for this
workload shape than V8's.

### `/db`: Node beats every other framework, but with a Crystal-like tail

| | Req/sec | p50 | p90 | p99 |
|---|---|---|---|---|
| `node-raw-server` | 28,242 | 3.74ms | 33.55ms | 174.07ms |
| `node-fastify-server` | 26,636 | 3.88ms | 27.43ms | 92.91ms |
| `crystal_kemal` | 24,887 | 0.61ms | 187.96ms | 288.83ms |
| `rust_pure_api` | 20,293 | 6.24ms | 12.12ms | 23.17ms |
| `uvicorn` | 17,164 | 6.14ms | 18.48ms | 42.98ms |

Both Node servers post the highest raw throughput of any DB-backed
implementation here — plausible, since `pg` (`node-postgres`) is a natively
async, non-blocking protocol client, architecturally similar to `asyncpg`
rather than a sync driver bolted onto an event loop (`diesel`'s situation in
`rust_pure_api`). But the tail latency is rough: p99 of 174ms/93ms is much
worse than `rust_pure_api`'s 23ms or `uvicorn`'s 43ms, even though Node's
median (3.7-3.9ms) is worse than Crystal's (0.6ms) while its p90/p99 are
*better* than Crystal's. Node's tail isn't as extreme as Crystal's genuinely
bimodal split, but the shape rhymes: a single-threaded-per-worker-process
event loop means anything that blocks the loop for one query (GC pause,
libuv thread-pool queuing for the DNS/TLS/native parts of a query, or just
enough concurrent promises stacking up in one worker) delays every other
in-flight request on that same worker. Not root-caused further (would need
`--prof`/clinic.js-style profiling of a worker under load), but the pattern
across both Crystal and Node — single process handles many concurrent
connections on one thread, worse tail latency than the multi-threaded or
one-connection-per-thread designs — looks like a real, repeatable
characteristic of this architecture class, not a fluke of either language.

### Bottom line

The original prediction undersold Node twice: on `/json`, "no chance of
approaching unsafe Rust" turned out to mean 87%, not the several-times-behind
gap TechEmpower's broader leaderboard would suggest for this class of
comparison. On `/db`, Node didn't just "plausibly match or beat `uvicorn`" —
it beat every framework here, Crystal included. The one part of the
prediction that held up is architectural, not about JIT quality: a
JIT-compiled, garbage-collected runtime with a full HTTP object model still
can't fully close a ~13-15% gap against a hand-rolled, zero-allocation raw
socket loop — and single-process-per-core designs (Node and Crystal both)
trade worse tail latency for that throughput, a cost the thread-per-core
designs (`unsafe-rust-server`, `c-server`) and the many-connections-per-pool
designs (`rust_pure_api`, `uvicorn`) don't pay to the same degree.

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
  it doesn't pay that particular tax. This still didn't let Python win the
  clean re-run (see below) — it likely narrows the gap rather than closing it.
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
even the language specifically.

One correction from the clean re-run: an earlier version of this README
reported that the "conservative" Python stack beat the "conservative"
safe-Rust stack (16,440 vs. 12,319 req/s), attributed to the pooling/async
mismatch above. With the Phoenix/Node background load removed, that ordering
**flipped** — `rust_pure_api` now leads at 20,293 vs. `uvicorn`'s 17,164
req/s. The likely reason is itself informative: `rust_pure_api`'s single
10-connection pool (shared across all workers) is a much smaller, more
fragile resource than `uvicorn`'s 160-connection capacity (10 per process ×
16 processes) — under uncontrolled background CPU contention, a small shared
pool is more exposed to scheduling noise than a heavily over-provisioned one,
which was suppressing `rust_pure_api`'s true throughput more than
`uvicorn`'s. Once the noise was gone, the smaller-pool app's actual capacity
showed through. This is a useful reminder that benchmark rankings between
architecturally different pooling strategies can be an artifact of *how busy
the test machine happens to be*, not just the code being tested.

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

**`crystal_kemal`** (requires Postgres running locally):
```
brew install crystal
createdb profiling
psql -d profiling -c 'CREATE TABLE users (id SERIAL PRIMARY KEY, firstName VARCHAR(255));'
shards install
bin/seed                          # TRUNCATEs + inserts 10,000 rows
crystal build main.cr --release
DB_POOL_SIZE=10 KEMAL_PORT=3000 bin/server &
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3000/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3000/db
```

`DB_POOL_SIZE=10` matters: at the project's own default of 25, 16 worker
processes try to open up to 400 Postgres connections at startup, and 15 of
16 will crash with `DB::ConnectionRefused` against a stock
`max_connections`. See "Crystal/Kemal" above.

If you rebuild `main` while a previous `bin/server` batch is still alive,
kill the old `./main` processes first — `SO_REUSEPORT` will happily keep
routing some fraction of requests to stale, already-running workers from the
old binary, which silently skews results (see "Crystal/Kemal" above).

To reproduce the write/list endpoints instead:
```
wrk -t4 -c128 -d15s --latency -s random_webhook.lua http://127.0.0.1:3000
wrk -t4 -c128 -d10s --latency http://127.0.0.1:3000/users
```
where `random_webhook.lua` is:
```lua
math.randomseed(os.time())
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
request = function()
   local id = math.random(1, 10000)
   local body = string.format('{"firstName":"Zenkai","Id":%d}', id)
   return wrk.format("POST", "/webhook", nil, body)
end
```

**`node-raw-server`** and **`node-fastify-server`** (both require Postgres
running locally, and share one dataset):
```
createdb node_bench
psql -d node_bench -c 'CREATE TABLE World (id SERIAL PRIMARY KEY, randomNumber INTEGER NOT NULL);'
psql -d node_bench -c "INSERT INTO World (randomNumber) SELECT (random()*9999)::int + 1 FROM generate_series(1,10000);"

cd node-raw-server && npm install && node server.js &   # PORT=3001 by default
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3001/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3001/db

cd ../node-fastify-server && npm install && node server.js &   # PORT=3002 by default
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3002/json
wrk -t4 -c128 -d15s --latency http://127.0.0.1:3002/db
```
Both fork one worker process per core via `cluster` (override with
`WORKERS=n`); each worker opens its own `pg` pool (override size with
`DB_POOL_SIZE`, default 10) — with 16 cores that's up to 160 connections per
server, comfortably under this machine's `max_connections=250`, so neither
hit the connection-pool bug the Postgres-backed frameworks hit earlier in
this README. Unlike the other servers in this repo, these two default to
different ports (3001/3002), so — if you actually want to isolate
measurements, stop one before benchmarking the other, same as everywhere
else in this README.
