// Fixture: additional TIME sources (both MEDIUM confidence).

pub fn offset() {
    // LEAK: time::OffsetDateTime::now_utc — wall-clock read (MEDIUM).
    let _ = time::OffsetDateTime::now_utc();
}

pub async fn tokio_instant() {
    // LEAK: tokio::time::Instant::now — runtime clock (MEDIUM, not the HIGH
    // std Instant::now).
    let _ = tokio::time::Instant::now();
}
