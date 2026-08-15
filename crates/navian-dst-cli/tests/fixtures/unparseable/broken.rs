// Fixture: deliberately UNPARSEABLE Rust. syn cannot parse this, so the scanner
// records it as a parse failure. Under `--deny` the gate must refuse to certify
// (exit 2), not pass green on a file it never actually saw.

fn broken( this is not valid rust <<<>>> {
    let = ;
