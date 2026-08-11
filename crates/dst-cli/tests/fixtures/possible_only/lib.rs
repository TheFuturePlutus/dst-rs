// Fixture: exactly ONE low-confidence POSSIBLE-RANDOM finding and nothing else.
//
// `Config::from_entropy()` is shaped like the RNG-seeding idiom but on an
// UNRECOGNIZED receiver, so the scanner reports it as POSSIBLE-RANDOM (not a
// hard RANDOM leak). Used to prove that:
//   * the human report lists POSSIBLE-RANDOM (it is not silently swallowed), and
//   * `--deny` does NOT fail on it (only hard leaks gate CI).

pub struct Config;

impl Config {
    pub fn from_entropy() -> Self {
        Config
    }
}

pub fn build() -> Config {
    Config::from_entropy()
}
