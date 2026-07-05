/* Hand-rolled C HTTP server for a 2-endpoint TechEmpower-style benchmark.
 * Same architecture as the unsafe-Rust sibling project: raw BSD sockets +
 * kqueue, one thread per core, SO_REUSEPORT fan-out, hand-written SQLite
 * queries, zero-allocation response building, flat fd-indexed connection
 * table, and best-effort core affinity pinning. */

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include <mach/mach.h>
#include <mach/thread_policy.h>

#include <sqlite3.h>

#define PORT 8080
#define WORLD_ROWS 10000
#define DB_PATH "/tmp/techempower_world.db"
#define CONN_BUF 8192

/* ---------- tiny xorshift PRNG ---------- */

typedef struct { uint64_t state; } xorshift_t;

static void xorshift_init(xorshift_t *r, uint64_t seed) {
    r->state = seed ^ 0x9E3779B97F4A7C15ULL;
}

static uint64_t xorshift_next(xorshift_t *r) {
    uint64_t x = r->state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    r->state = x;
    return x;
}

/* ---------- SQLite: seed + per-thread prepared-statement handle ---------- */

static void seed_database(void) {
    sqlite3 *db;
    sqlite3_open(DB_PATH, &db);
    sqlite3_exec(db, "PRAGMA journal_mode=WAL;", NULL, NULL, NULL);
    sqlite3_exec(db,
        "CREATE TABLE IF NOT EXISTS World (id INTEGER PRIMARY KEY, randomNumber INTEGER NOT NULL);",
        NULL, NULL, NULL);

    sqlite3_stmt *count_stmt;
    sqlite3_prepare_v2(db, "SELECT COUNT(*) FROM World;", -1, &count_stmt, NULL);
    sqlite3_step(count_stmt);
    int count = sqlite3_column_int(count_stmt, 0);
    sqlite3_finalize(count_stmt);

    if (count < WORLD_ROWS) {
        sqlite3_exec(db, "DELETE FROM World;", NULL, NULL, NULL);
        sqlite3_exec(db, "BEGIN;", NULL, NULL, NULL);
        xorshift_t rng;
        xorshift_init(&rng, 0xC0FFEE);
        sqlite3_stmt *ins;
        sqlite3_prepare_v2(db, "INSERT INTO World (id, randomNumber) VALUES (?1, ?2);", -1, &ins, NULL);
        for (int id = 1; id <= WORLD_ROWS; id++) {
            sqlite3_bind_int(ins, 1, id);
            sqlite3_bind_int(ins, 2, (int)(xorshift_next(&rng) % 10000) + 1);
            sqlite3_step(ins);
            sqlite3_reset(ins);
        }
        sqlite3_finalize(ins);
        sqlite3_exec(db, "COMMIT;", NULL, NULL, NULL);
    }
    sqlite3_close(db);
}

typedef struct {
    sqlite3 *db;
    sqlite3_stmt *lookup;
} WorldDb;

static void world_db_open(WorldDb *w) {
    sqlite3_open(DB_PATH, &w->db);
    sqlite3_exec(w->db, "PRAGMA query_only=ON;", NULL, NULL, NULL);
    sqlite3_exec(w->db, "PRAGMA mmap_size=268435456;", NULL, NULL, NULL);
    sqlite3_prepare_v2(w->db, "SELECT id, randomNumber FROM World WHERE id = ?1;", -1, &w->lookup, NULL);
}

static void world_db_fetch_random(WorldDb *w, xorshift_t *rng, int *out_id, int *out_rn) {
    int id = (int)(xorshift_next(rng) % WORLD_ROWS) + 1;
    sqlite3_bind_int(w->lookup, 1, id);
    int rc = sqlite3_step(w->lookup);
    if (rc == SQLITE_ROW) {
        *out_id = sqlite3_column_int(w->lookup, 0);
        *out_rn = sqlite3_column_int(w->lookup, 1);
    } else {
        *out_id = id;
        *out_rn = 0;
    }
    sqlite3_reset(w->lookup);
}

/* ---------- shared clock: one atomic, refreshed once/sec, no locks to read ---------- */

static _Atomic uint64_t g_epoch_secs = 0;

static uint64_t now_secs(void) {
    return (uint64_t)time(NULL);
}

static void *date_updater_thread(void *arg) {
    (void)arg;
    for (;;) {
        usleep(250000);
        atomic_store_explicit(&g_epoch_secs, now_secs(), memory_order_relaxed);
    }
    return NULL;
}

static void spawn_date_updater(void) {
    atomic_store_explicit(&g_epoch_secs, now_secs(), memory_order_relaxed);
    pthread_t t;
    pthread_create(&t, NULL, date_updater_thread, NULL);
    pthread_detach(t);
}

/* civil_from_days (Howard Hinnant's algorithm) */
static void format_http_date(uint64_t epoch_secs, char out[29]) {
    int64_t secs = (int64_t)epoch_secs;
    int64_t days = secs / 86400;
    int64_t tod = secs % 86400;
    int64_t h = tod / 3600, m = (tod % 3600) / 60, s = tod % 60;

    int64_t z = days + 719468;
    int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    uint64_t doe = (uint64_t)(z - era * 146097);
    uint64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t y = (int64_t)yoe + era * 400;
    uint64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    uint64_t mp = (5 * doy + 2) / 153;
    int64_t d = (int64_t)(doy - (153 * mp + 2) / 5 + 1);
    int64_t mo = (mp < 10) ? (int64_t)mp + 3 : (int64_t)mp - 9;
    if (mo <= 2) y += 1;

    int64_t dow = ((days % 7) + 7 + 4) % 7; /* 1970-01-01 was a Thursday */
    static const char *DOWS[7] = {"Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"};
    static const char *MONTHS[12] = {"Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                      "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"};

    char tmp[30];
    snprintf(tmp, sizeof(tmp), "%s, %02lld %s %04lld %02lld:%02lld:%02lld GMT",
              DOWS[dow], (long long)d, MONTHS[mo - 1], (long long)y,
              (long long)h, (long long)m, (long long)s);
    memcpy(out, tmp, 29);
}

/* ---------- best-effort core affinity (Mach API; a scheduling *hint*, not a
 * hard pin like Linux's sched_setaffinity) ---------- */

static void pin_to_affinity_tag(int tag) {
    thread_affinity_policy_data_t policy;
    policy.affinity_tag = tag;
    thread_policy_set(pthread_mach_thread_np(pthread_self()),
                       THREAD_AFFINITY_POLICY,
                       (thread_policy_t)&policy,
                       THREAD_AFFINITY_POLICY_COUNT);
}

/* ---------- raw socket / kqueue plumbing ---------- */

static void set_nonblocking(int fd) {
    int flags = fcntl(fd, F_GETFL, 0);
    fcntl(fd, F_SETFL, flags | O_NONBLOCK);
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
}

static int make_listener(void) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one));
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(PORT);
    addr.sin_addr.s_addr = htonl(INADDR_ANY);

    bind(fd, (struct sockaddr *)&addr, sizeof(addr));
    listen(fd, 1024);
    set_nonblocking(fd);
    return fd;
}

static void kqueue_add_read(int kq, int fd) {
    struct kevent ev;
    EV_SET(&ev, fd, EVFILT_READ, EV_ADD | EV_ENABLE, 0, 0, NULL);
    kevent(kq, &ev, 1, NULL, 0, NULL);
}

/* ---------- per-connection request buffer, flat fd-indexed table ---------- */

typedef struct {
    int active;
    size_t len;
    char buf[CONN_BUF];
} Conn;

typedef struct {
    Conn *slots;
    size_t cap;
} ConnTable;

static void conn_table_ensure(ConnTable *t, int fd) {
    size_t idx = (size_t)fd;
    if (idx >= t->cap) {
        size_t new_cap = idx + 1;
        t->slots = realloc(t->slots, new_cap * sizeof(Conn));
        memset(t->slots + t->cap, 0, (new_cap - t->cap) * sizeof(Conn));
        t->cap = new_cap;
    }
}

typedef enum { ROUTE_JSON, ROUTE_DB, ROUTE_NOT_FOUND } Route;

/* Scans buf[0..len) for one full HTTP request terminated by CRLFCRLF.
 * Returns bytes consumed (>0) if a full request is present, else 0. */
static size_t parse_one(const char *buf, size_t len, Route *route) {
    if (len < 4) return 0;
    size_t end = 0;
    for (size_t i = 0; i + 3 < len; i++) {
        if (buf[i] == '\r' && buf[i + 1] == '\n' && buf[i + 2] == '\r' && buf[i + 3] == '\n') {
            end = i + 4;
            break;
        }
    }
    if (end == 0) return 0;

    size_t sp1 = 0;
    int found1 = 0;
    for (size_t j = 0; j < end; j++) {
        if (buf[j] == ' ') { sp1 = j; found1 = 1; break; }
    }
    if (!found1) return 0;

    size_t sp2 = 0;
    int found2 = 0;
    for (size_t k = sp1 + 1; k < end; k++) {
        if (buf[k] == ' ') { sp2 = k; found2 = 1; break; }
    }
    if (!found2) return 0;

    size_t path_len = sp2 - sp1 - 1;
    const char *path = buf + sp1 + 1;
    if (path_len == 5 && memcmp(path, "/json", 5) == 0) *route = ROUTE_JSON;
    else if (path_len == 3 && memcmp(path, "/db", 3) == 0) *route = ROUTE_DB;
    else *route = ROUTE_NOT_FOUND;
    return end;
}

/* ---------- response building: zero heap allocation on the hot path ---------- */

static const char NOT_FOUND[] = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
static const char DB_HEADER_PREFIX[] =
    "HTTP/1.1 200 OK\r\nServer: c-unsafe\r\nContent-Type: application/json\r\nContent-Length: ";

static size_t write_uint(char *buf, unsigned n) {
    if (n == 0) { buf[0] = '0'; return 1; }
    char tmp[10];
    int i = 0;
    while (n > 0) { tmp[i++] = (char)('0' + (n % 10)); n /= 10; }
    for (int j = 0; j < i; j++) buf[j] = tmp[i - 1 - j];
    return (size_t)i;
}

typedef struct {
    char buf[160];
    size_t len;
    size_t date_offset;
    uint64_t cached_epoch;
} JsonTemplate;

static void json_template_init(JsonTemplate *t) {
    static const char *body = "{\"message\":\"Hello, World!\"}";
    size_t body_len = strlen(body);
    static const char *prefix =
        "HTTP/1.1 200 OK\r\nServer: c-unsafe\r\nContent-Type: application/json\r\nContent-Length: ";
    size_t prefix_len = strlen(prefix);

    size_t pos = 0;
    memcpy(t->buf + pos, prefix, prefix_len);
    pos += prefix_len;
    pos += write_uint(t->buf + pos, (unsigned)body_len);
    memcpy(t->buf + pos, "\r\nDate: ", 8);
    pos += 8;
    t->date_offset = pos;
    memset(t->buf + pos, ' ', 29);
    pos += 29;
    memcpy(t->buf + pos, "\r\n\r\n", 4);
    pos += 4;
    memcpy(t->buf + pos, body, body_len);
    pos += body_len;

    t->len = pos;
    t->cached_epoch = 0;
}

static void json_template_refresh(JsonTemplate *t, uint64_t epoch, const char date_bytes[29]) {
    if (epoch != t->cached_epoch) {
        memcpy(t->buf + t->date_offset, date_bytes, 29);
        t->cached_epoch = epoch;
    }
}

static size_t build_db_response(char *scratch, int id, int random_number, const char date[29]) {
    char body[48];
    size_t bp = 0;
    memcpy(body + bp, "{\"id\":", 6);
    bp += 6;
    bp += write_uint(body + bp, (unsigned)id);
    memcpy(body + bp, ",\"randomNumber\":", 16);
    bp += 16;
    bp += write_uint(body + bp, (unsigned)random_number);
    body[bp++] = '}';

    size_t pos = 0;
    size_t prefix_len = sizeof(DB_HEADER_PREFIX) - 1;
    memcpy(scratch + pos, DB_HEADER_PREFIX, prefix_len);
    pos += prefix_len;
    pos += write_uint(scratch + pos, (unsigned)bp);
    memcpy(scratch + pos, "\r\nDate: ", 8);
    pos += 8;
    memcpy(scratch + pos, date, 29);
    pos += 29;
    memcpy(scratch + pos, "\r\n\r\n", 4);
    pos += 4;
    memcpy(scratch + pos, body, bp);
    pos += bp;
    return pos;
}

/* ---------- worker thread ---------- */

static int write_all(int fd, const char *data, size_t len) {
    size_t sent = 0;
    while (sent < len) {
        ssize_t n = write(fd, data + sent, len - sent);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) continue;
            return 0;
        }
        sent += (size_t)n;
    }
    return 1;
}

/* Returns 1 if the connection should be closed. */
static int handle_readable(int fd, ConnTable *conns, WorldDb *world, xorshift_t *rng,
                            JsonTemplate *json_tpl, const char date_bytes[29], char db_scratch[256]) {
    if ((size_t)fd >= conns->cap || !conns->slots[fd].active) return 1;
    Conn *conn = &conns->slots[fd];

    size_t space = CONN_BUF - conn->len;
    if (space == 0) return 1;

    ssize_t n = read(fd, conn->buf + conn->len, space);
    if (n == 0) return 1;
    if (n < 0) return !(errno == EAGAIN || errno == EWOULDBLOCK);
    conn->len += (size_t)n;

    for (;;) {
        Route route;
        size_t consumed = parse_one(conn->buf, conn->len, &route);
        if (consumed == 0) break;

        int ok;
        if (route == ROUTE_JSON) {
            ok = write_all(fd, json_tpl->buf, json_tpl->len);
        } else if (route == ROUTE_DB) {
            int id, rn;
            world_db_fetch_random(world, rng, &id, &rn);
            size_t len = build_db_response(db_scratch, id, rn, date_bytes);
            ok = write_all(fd, db_scratch, len);
        } else {
            ok = write_all(fd, NOT_FOUND, sizeof(NOT_FOUND) - 1);
        }
        if (!ok) return 1;

        size_t remaining = conn->len - consumed;
        if (remaining > 0) memmove(conn->buf, conn->buf + consumed, remaining);
        conn->len = remaining;
        if (remaining == 0) break;
    }
    return 0;
}

typedef struct {
    int idx;
} WorkerArg;

static void *worker_loop(void *arg) {
    int idx = ((WorkerArg *)arg)->idx;
    pin_to_affinity_tag(idx);

    int listen_fd = make_listener();
    int kq = kqueue();
    kqueue_add_read(kq, listen_fd);

    WorldDb world;
    world_db_open(&world);

    xorshift_t rng;
    xorshift_init(&rng, (uint64_t)getpid() ^ (uint64_t)idx ^ 0xA5A5A5A5ULL);

    ConnTable conns;
    memset(&conns, 0, sizeof(conns));

    JsonTemplate json_tpl;
    json_template_init(&json_tpl);
    uint64_t date_epoch = 0;
    char date_bytes[29] = "Thu, 01 Jan 1970 00:00:00 GMT";
    char db_scratch[256];

    struct kevent events[256];

    for (;;) {
        int n = kevent(kq, NULL, 0, events, 256, NULL);
        if (n < 0) {
            if (errno == EINTR) continue;
            break;
        }

        uint64_t epoch = atomic_load_explicit(&g_epoch_secs, memory_order_relaxed);
        if (epoch != date_epoch) {
            format_http_date(epoch, date_bytes);
            date_epoch = epoch;
        }
        json_template_refresh(&json_tpl, epoch, date_bytes);

        for (int i = 0; i < n; i++) {
            int fd = (int)events[i].ident;

            if (fd == listen_fd) {
                for (;;) {
                    int cfd = accept(listen_fd, NULL, NULL);
                    if (cfd < 0) break;
                    set_nonblocking(cfd);
                    kqueue_add_read(kq, cfd);
                    conn_table_ensure(&conns, cfd);
                    conns.slots[cfd].active = 1;
                    conns.slots[cfd].len = 0;
                }
                continue;
            }

            int close_conn = handle_readable(fd, &conns, &world, &rng, &json_tpl, date_bytes, db_scratch);
            if (close_conn) {
                if ((size_t)fd < conns.cap) conns.slots[fd].active = 0;
                close(fd);
            }
        }
    }
    return NULL;
}

int main(void) {
    printf("seeding sqlite World table at %s (%d rows)...\n", DB_PATH, WORLD_ROWS);
    seed_database();

    spawn_date_updater();

    long n = sysconf(_SC_NPROCESSORS_ONLN);
    if (n <= 0) n = 4;
    printf("listening on 0.0.0.0:%d with %ld worker threads (SO_REUSEPORT)\n", PORT, n);

    pthread_t *threads = malloc(sizeof(pthread_t) * (size_t)n);
    WorkerArg *args = malloc(sizeof(WorkerArg) * (size_t)n);
    for (long i = 0; i < n; i++) {
        args[i].idx = (int)i;
        pthread_create(&threads[i], NULL, worker_loop, &args[i]);
    }
    for (long i = 0; i < n; i++) pthread_join(threads[i], NULL);
    return 0;
}
