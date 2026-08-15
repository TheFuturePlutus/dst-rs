// Fixture: DECOYS for the HIGH-confidence gate. Every construct here looks like
// a High determinism source but is benign/shadowed/foreign, so `scan --deny`
// (high threshold) MUST exit 0. These are the regression guards for the
// shadow/qualifier guards — if any starts emitting a High finding, the gate
// breaks and this fixture's `--deny` test fails.

// ── Locally-defined names that shadow std sources (bare use = user's own) ──

pub struct SystemTime;
impl SystemTime {
    // A user type named `SystemTime` with its own `now` — NOT std.
    pub fn now() -> u64 {
        0
    }
}

pub struct OsRng;

pub fn thread_rng() -> u8 {
    // A user free fn named `thread_rng` — NOT rand's.
    0
}

pub fn getrandom(_b: &mut [u8]) -> u8 {
    // A user free fn named `getrandom` — NOT the crate.
    0
}

// A user enum whose variant is named `OsRng` — foreign-qualified, NOT rand's.
pub enum RngKind {
    OsRng,
}

// A foreign module exposing a `TcpStream` — `mock::TcpStream` is NOT std::net.
pub mod mock {
    pub struct TcpStream;
    impl TcpStream {
        pub fn pair() {}
    }
}

// User modules whose names collide with std sub-modules or ecosystem crates.
// Calls THROUGH them must not fire High (the qualifier/crate-shadow guards).
pub mod net {
    pub struct TcpStream;
    impl TcpStream {
        pub fn connect(_: &str) {}
    }
}

pub mod time {
    pub struct Instant;
    impl Instant {
        pub fn now() {}
    }
}

pub mod reqwest {
    pub fn get(_: &str) {}
}

pub mod mycrate {
    pub fn getrandom(_b: &mut [u8]) -> u8 {
        0
    }
}

pub fn exercise() {
    // Locally-shadowed bare names — user's own, must not be High.
    let _t = SystemTime::now();
    let _r = OsRng;
    let _rng = thread_rng();
    let mut b = [0u8; 4];
    let _g = getrandom(&mut b);

    // Foreign-qualified look-alikes — must not be High.
    let _k = RngKind::OsRng;
    mock::TcpStream::pair();
    let _fg = mycrate::getrandom(&mut b);

    // Modules named like std sub-modules / crates — must not be High.
    net::TcpStream::connect("x");
    time::Instant::now();
    reqwest::get("x");

    // These names appear only in a comment and must not be flagged:
    // getrandom::getrandom, rand::thread_rng, std::net::TcpStream::connect.
}
