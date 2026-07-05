// Hand-rolled unsafe-Rust HTTP server for a 2-endpoint TechEmpower-style benchmark.
// No async runtime, no HTTP crate: raw BSD sockets + kqueue, one thread per core,
// SO_REUSEPORT fan-out, and a hand-written sqlite3 FFI for the "data store" endpoint.
//
// Optimized variant: zero per-request heap allocations, a flat fd-indexed
// connection table instead of a hash map, and best-effort core affinity pinning.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PORT: u16 = 8080;
const WORLD_ROWS: i32 = 10_000;
const DB_PATH: &str = "/tmp/techempower_world.db";
const CONN_BUF: usize = 8192;

// ---------- sqlite3 FFI (hand-written, no rusqlite) ----------

#[allow(non_camel_case_types)]
enum sqlite3 {}
#[allow(non_camel_case_types)]
enum sqlite3_stmt {}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;

#[link(name = "sqlite3")]
extern "C" {
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: *const c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_bind_int(stmt: *mut sqlite3_stmt, idx: c_int, val: c_int) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_int(stmt: *mut sqlite3_stmt, col: c_int) -> c_int;
    fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
}

unsafe fn sqlite_exec_raw(db: *mut sqlite3, sql: &str) {
    let csql = std::ffi::CString::new(sql).unwrap();
    let rc = sqlite3_exec(db, csql.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "sqlite3_exec failed for: {sql}");
}

/// Creates (if needed) and seeds the World table used by the /db endpoint.
fn seed_database() {
    unsafe {
        let mut db: *mut sqlite3 = std::ptr::null_mut();
        let path = std::ffi::CString::new(DB_PATH).unwrap();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);

        sqlite_exec_raw(db, "PRAGMA journal_mode=WAL;");
        sqlite_exec_raw(
            db,
            "CREATE TABLE IF NOT EXISTS World (id INTEGER PRIMARY KEY, randomNumber INTEGER NOT NULL);",
        );

        // Only seed once: check row count first.
        let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
        let count_sql = std::ffi::CString::new("SELECT COUNT(*) FROM World;").unwrap();
        sqlite3_prepare_v2(db, count_sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut());
        sqlite3_step(stmt);
        let count = sqlite3_column_int(stmt, 0);
        sqlite3_reset(stmt);

        if count < WORLD_ROWS {
            sqlite_exec_raw(db, "DELETE FROM World;");
            sqlite_exec_raw(db, "BEGIN;");
            let mut rng = XorShift::new(0xC0FFEE);
            let ins_sql = std::ffi::CString::new(
                "INSERT INTO World (id, randomNumber) VALUES (?1, ?2);",
            )
            .unwrap();
            let mut ins_stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            sqlite3_prepare_v2(db, ins_sql.as_ptr(), -1, &mut ins_stmt, std::ptr::null_mut());
            for id in 1..=WORLD_ROWS {
                sqlite3_bind_int(ins_stmt, 1, id);
                sqlite3_bind_int(ins_stmt, 2, (rng.next() % 10_000) as c_int + 1);
                sqlite3_step(ins_stmt);
                sqlite3_reset(ins_stmt);
            }
            sqlite_exec_raw(db, "COMMIT;");
        }
        sqlite3_close(db);
    }
}

/// Per-thread handle: one connection + one prepared statement, reused across requests.
struct WorldDb {
    _db: *mut sqlite3, // kept alive for the lifetime of `lookup`; connection outlives the process
    lookup: *mut sqlite3_stmt,
}

impl WorldDb {
    fn open() -> Self {
        unsafe {
            let mut db: *mut sqlite3 = std::ptr::null_mut();
            let path = std::ffi::CString::new(DB_PATH).unwrap();
            assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
            sqlite_exec_raw(db, "PRAGMA query_only=ON;");
            sqlite_exec_raw(db, "PRAGMA mmap_size=268435456;");

            let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
            let sql = std::ffi::CString::new("SELECT id, randomNumber FROM World WHERE id = ?1;").unwrap();
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut());
            WorldDb { _db: db, lookup: stmt }
        }
    }

    /// Returns (id, randomNumber) for a random row.
    fn fetch_random(&self, rng: &mut XorShift) -> (i32, i32) {
        let id = (rng.next() % WORLD_ROWS as u64) as c_int + 1;
        unsafe {
            sqlite3_bind_int(self.lookup, 1, id);
            let rc = sqlite3_step(self.lookup);
            let result = if rc == SQLITE_ROW {
                (sqlite3_column_int(self.lookup, 0), sqlite3_column_int(self.lookup, 1))
            } else {
                (id, 0)
            };
            sqlite3_reset(self.lookup);
            result
        }
    }
}

// ---------- tiny xorshift PRNG (avoids pulling in the `rand` crate) ----------

struct XorShift(u64);
impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed ^ 0x9E3779B97F4A7C15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

// ---------- shared clock: one atomic, refreshed once/sec, no locks on the read path ----------

static EPOCH_SECS: AtomicU64 = AtomicU64::new(0);

fn spawn_date_updater() {
    let now_secs = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    // Set synchronously before returning so worker threads never observe epoch 0.
    EPOCH_SECS.store(now_secs(), Ordering::Relaxed);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        EPOCH_SECS.store(now_secs(), Ordering::Relaxed);
    });
}

fn format_http_date(epoch_secs: u64) -> [u8; 29] {
    let secs = epoch_secs as i64;
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // civil_from_days (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i64;
    let m_ = (if mp < 10 { mp + 3 } else { mp - 9 }) as i64;
    let y = if m_ <= 2 { y + 1 } else { y };

    let dow = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 was a Thursday
    const DOWS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let s = format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DOWS[dow as usize],
        d,
        MONTHS[(m_ - 1) as usize],
        y,
        h,
        m,
        s
    );
    let mut out = [0u8; 29];
    out.copy_from_slice(s.as_bytes());
    out
}

// ---------- best-effort core affinity (macOS Mach API; a scheduling *hint*, not a
// hard pin like Linux's sched_setaffinity — Apple doesn't guarantee placement) ----------

#[allow(non_camel_case_types)]
type thread_t = u32;
#[allow(non_camel_case_types)]
type kern_return_t = i32;

#[repr(C)]
struct ThreadAffinityPolicy {
    affinity_tag: i32,
}

const THREAD_AFFINITY_POLICY: c_int = 4;
const THREAD_AFFINITY_POLICY_COUNT: u32 = 1;

extern "C" {
    fn pthread_mach_thread_np(thread: libc::pthread_t) -> thread_t;
    fn thread_policy_set(
        thread: thread_t,
        flavor: c_int,
        policy_info: *mut i32,
        count: u32,
    ) -> kern_return_t;
}

unsafe fn pin_to_affinity_tag(tag: i32) {
    let mach_thread = pthread_mach_thread_np(libc::pthread_self());
    let mut policy = ThreadAffinityPolicy { affinity_tag: tag };
    thread_policy_set(
        mach_thread,
        THREAD_AFFINITY_POLICY,
        &mut policy as *mut ThreadAffinityPolicy as *mut i32,
        THREAD_AFFINITY_POLICY_COUNT,
    );
}

// ---------- raw socket / kqueue plumbing ----------

unsafe fn make_listener() -> RawFd {
    let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
    assert!(fd >= 0, "socket() failed");

    let one: c_int = 1;
    libc::setsockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_REUSEPORT,
        &one as *const _ as *const c_void,
        std::mem::size_of::<c_int>() as u32,
    );
    libc::setsockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_REUSEADDR,
        &one as *const _ as *const c_void,
        std::mem::size_of::<c_int>() as u32,
    );

    let mut addr: libc::sockaddr_in = std::mem::zeroed();
    addr.sin_family = libc::AF_INET as u8;
    addr.sin_port = PORT.to_be();
    addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();

    let rc = libc::bind(
        fd,
        &addr as *const _ as *const libc::sockaddr,
        std::mem::size_of::<libc::sockaddr_in>() as u32,
    );
    assert_eq!(rc, 0, "bind() failed");

    let rc = libc::listen(fd, 1024);
    assert_eq!(rc, 0, "listen() failed");

    set_nonblocking(fd);
    fd
}

unsafe fn set_nonblocking(fd: RawFd) {
    let flags = libc::fcntl(fd, libc::F_GETFL, 0);
    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    let one: c_int = 1;
    libc::setsockopt(
        fd,
        libc::IPPROTO_TCP,
        libc::TCP_NODELAY,
        &one as *const _ as *const c_void,
        std::mem::size_of::<c_int>() as u32,
    );
}

unsafe fn kqueue_add_read(kq: RawFd, fd: RawFd) {
    let ev = libc::kevent {
        ident: fd as usize,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null());
}

fn errno() -> c_int {
    unsafe { *libc::__error() }
}

// ---------- per-connection request buffer + parsing ----------

struct Conn {
    buf: [u8; CONN_BUF],
    len: usize,
}

/// Flat fd-indexed connection table: no hashing, just direct indexing.
/// fds are small dense integers assigned by the kernel, so this stays compact.
type ConnTable = Vec<Option<Conn>>;

fn conn_insert(conns: &mut ConnTable, fd: RawFd, conn: Conn) {
    let idx = fd as usize;
    if idx >= conns.len() {
        conns.resize_with(idx + 1, || None);
    }
    conns[idx] = Some(conn);
}

fn conn_remove(conns: &mut ConnTable, fd: RawFd) {
    if let Some(slot) = conns.get_mut(fd as usize) {
        *slot = None;
    }
}

enum Route {
    Json,
    Db,
    NotFound,
}

/// Scans `buf[..len]` for one full HTTP request terminated by CRLFCRLF.
/// Returns (route, total_bytes_consumed) if a full request is present.
unsafe fn parse_one(buf: *const u8, len: usize) -> Option<(Route, usize)> {
    // find "\r\n\r\n"
    if len < 4 {
        return None;
    }
    let mut end = None;
    let mut i = 0usize;
    while i + 3 < len {
        if *buf.add(i) == b'\r'
            && *buf.add(i + 1) == b'\n'
            && *buf.add(i + 2) == b'\r'
            && *buf.add(i + 3) == b'\n'
        {
            end = Some(i + 4);
            break;
        }
        i += 1;
    }
    let total = end?;

    // Parse request line: "GET /path HTTP/1.1"
    let mut sp1 = None;
    let mut j = 0usize;
    while j < total {
        if *buf.add(j) == b' ' {
            sp1 = Some(j);
            break;
        }
        j += 1;
    }
    let sp1 = sp1?;
    let mut sp2 = None;
    let mut k = sp1 + 1;
    while k < total {
        if *buf.add(k) == b' ' {
            sp2 = Some(k);
            break;
        }
        k += 1;
    }
    let sp2 = sp2?;
    let path = std::slice::from_raw_parts(buf.add(sp1 + 1), sp2 - sp1 - 1);

    let route = if path == b"/json" {
        Route::Json
    } else if path == b"/db" {
        Route::Db
    } else {
        Route::NotFound
    };
    Some((route, total))
}

// ---------- response building: zero heap allocation on the hot path ----------

const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
const DB_HEADER_PREFIX: &[u8] =
    b"HTTP/1.1 200 OK\r\nServer: unsafe-rs\r\nContent-Type: application/json\r\nContent-Length: ";

/// The /json response is fully static except for a 29-byte Date field, so it's
/// built once per thread and only that slice is patched in place thereafter —
/// no allocation, no copying of the rest of the response, ever again.
struct JsonTemplate {
    buf: Vec<u8>,
    date_offset: usize,
    cached_epoch: u64,
}

impl JsonTemplate {
    fn new() -> Self {
        let body: &[u8] = b"{\"message\":\"Hello, World!\"}";
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(
            b"HTTP/1.1 200 OK\r\nServer: unsafe-rs\r\nContent-Type: application/json\r\nContent-Length: ",
        );
        buf.extend_from_slice(body.len().to_string().as_bytes());
        buf.extend_from_slice(b"\r\nDate: ");
        let date_offset = buf.len();
        buf.extend_from_slice(&[b' '; 29]);
        buf.extend_from_slice(b"\r\n\r\n");
        buf.extend_from_slice(body);
        JsonTemplate {
            buf,
            date_offset,
            cached_epoch: 0,
        }
    }

    /// Patches the Date field in place if the wall-clock second has ticked over.
    fn refresh(&mut self, epoch: u64, date_bytes: &[u8; 29]) {
        if epoch != self.cached_epoch {
            self.buf[self.date_offset..self.date_offset + 29].copy_from_slice(date_bytes);
            self.cached_epoch = epoch;
        }
    }
}

/// Writes an unsigned decimal integer into `buf`, returns the number of bytes written.
fn write_uint(buf: &mut [u8], mut n: u32) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    i
}

/// Builds the /db response into a caller-owned scratch buffer (stack-allocated by
/// the caller, reused every request) and returns the number of bytes written.
fn build_db_response(scratch: &mut [u8; 256], id: i32, random_number: i32, date: &[u8; 29]) -> usize {
    let mut body = [0u8; 48];
    let mut bp = 0usize;
    body[bp..bp + 6].copy_from_slice(b"{\"id\":");
    bp += 6;
    bp += write_uint(&mut body[bp..], id as u32);
    body[bp..bp + 16].copy_from_slice(b",\"randomNumber\":");
    bp += 16;
    bp += write_uint(&mut body[bp..], random_number as u32);
    body[bp] = b'}';
    bp += 1;

    let mut pos = 0usize;
    scratch[pos..pos + DB_HEADER_PREFIX.len()].copy_from_slice(DB_HEADER_PREFIX);
    pos += DB_HEADER_PREFIX.len();
    pos += write_uint(&mut scratch[pos..], bp as u32);
    scratch[pos..pos + 8].copy_from_slice(b"\r\nDate: ");
    pos += 8;
    scratch[pos..pos + 29].copy_from_slice(date);
    pos += 29;
    scratch[pos..pos + 4].copy_from_slice(b"\r\n\r\n");
    pos += 4;
    scratch[pos..pos + bp].copy_from_slice(&body[..bp]);
    pos += bp;
    pos
}

// ---------- worker thread ----------

fn worker_loop(idx: usize) {
    unsafe {
        pin_to_affinity_tag(idx as i32);

        let listen_fd = make_listener();
        let kq = libc::kqueue();
        assert!(kq >= 0);
        kqueue_add_read(kq, listen_fd);

        let world = WorldDb::open();
        let mut rng = XorShift::new(std::process::id() as u64 ^ (idx as u64) ^ 0xA5A5_A5A5);
        let mut conns: ConnTable = Vec::new();

        let mut json_tpl = JsonTemplate::new();
        let mut date_epoch: u64 = 0;
        let mut date_bytes: [u8; 29] = *b"Thu, 01 Jan 1970 00:00:00 GMT";
        let mut db_scratch = [0u8; 256];

        let mut events: [libc::kevent; 256] = std::mem::zeroed();

        loop {
            let n = libc::kevent(
                kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as c_int,
                std::ptr::null(),
            );
            if n < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                break;
            }

            let epoch = EPOCH_SECS.load(Ordering::Relaxed);
            if epoch != date_epoch {
                date_bytes = format_http_date(epoch);
                date_epoch = epoch;
            }
            json_tpl.refresh(epoch, &date_bytes);

            for ev in &events[..n as usize] {
                let fd = ev.ident as RawFd;

                if fd == listen_fd {
                    loop {
                        let cfd = libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut());
                        if cfd < 0 {
                            break;
                        }
                        set_nonblocking(cfd);
                        kqueue_add_read(kq, cfd);
                        conn_insert(
                            &mut conns,
                            cfd,
                            Conn {
                                buf: [0u8; CONN_BUF],
                                len: 0,
                            },
                        );
                    }
                    continue;
                }

                let close_conn = handle_readable(
                    fd,
                    &mut conns,
                    &world,
                    &mut rng,
                    &json_tpl,
                    &date_bytes,
                    &mut db_scratch,
                );
                if close_conn {
                    conn_remove(&mut conns, fd);
                    libc::close(fd);
                }
            }
        }
    }
}

/// Returns true if the connection should be closed.
#[allow(clippy::too_many_arguments)]
unsafe fn handle_readable(
    fd: RawFd,
    conns: &mut ConnTable,
    world: &WorldDb,
    rng: &mut XorShift,
    json_tpl: &JsonTemplate,
    date_bytes: &[u8; 29],
    db_scratch: &mut [u8; 256],
) -> bool {
    let conn = match conns.get_mut(fd as usize).and_then(|slot| slot.as_mut()) {
        Some(c) => c,
        None => return true,
    };

    let space = CONN_BUF - conn.len;
    if space == 0 {
        return true; // request too large for this demo's fixed buffer
    }
    let n = libc::read(
        fd,
        conn.buf.as_mut_ptr().add(conn.len) as *mut c_void,
        space,
    );
    if n == 0 {
        return true; // peer closed
    }
    if n < 0 {
        return errno() != libc::EAGAIN && errno() != libc::EWOULDBLOCK;
    }
    conn.len += n as usize;

    loop {
        let parsed = parse_one(conn.buf.as_ptr(), conn.len);
        let Some((route, consumed)) = parsed else {
            break;
        };

        let ok = match route {
            Route::Json => write_all(fd, &json_tpl.buf),
            Route::Db => {
                let (id, rn) = world.fetch_random(rng);
                let len = build_db_response(db_scratch, id, rn, date_bytes);
                write_all(fd, &db_scratch[..len])
            }
            Route::NotFound => write_all(fd, NOT_FOUND),
        };
        if !ok {
            return true;
        }

        // shift any pipelined bytes down to the front
        let remaining = conn.len - consumed;
        if remaining > 0 {
            std::ptr::copy(
                conn.buf.as_ptr().add(consumed),
                conn.buf.as_mut_ptr(),
                remaining,
            );
        }
        conn.len = remaining;
        if remaining == 0 {
            break;
        }
    }
    false
}

unsafe fn write_all(fd: RawFd, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = libc::write(fd, data.as_ptr() as *const c_void, data.len());
        if n < 0 {
            let e = errno();
            if e == libc::EAGAIN || e == libc::EWOULDBLOCK || e == libc::EINTR {
                continue;
            }
            return false;
        }
        data = &data[n as usize..];
    }
    true
}

fn main() {
    println!("seeding sqlite World table at {DB_PATH} ({WORLD_ROWS} rows)...");
    seed_database();

    spawn_date_updater();

    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    println!("listening on 0.0.0.0:{PORT} with {n} worker threads (SO_REUSEPORT)");

    let mut handles = Vec::new();
    for i in 0..n {
        handles.push(std::thread::spawn(move || worker_loop(i)));
    }
    for h in handles {
        let _ = h.join();
    }
}
