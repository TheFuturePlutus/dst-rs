---
name: Bug report
about: A reproducible problem in navian-dst
title: ''
labels: bug
---

**What happened**
A clear, concise description of the bug.

**Reproduction**
The smallest repro you can share. DST failures are seed-deterministic — if a seed-loop
test surfaced it, please include the failing **seed** and a minimal test so we can
replay it exactly:

```rust
// e.g. fails on SEED=7
```

**Expected behavior**
What you expected to happen instead.

**Environment**
- navian-dst / navian-dst-cli version (e.g. 0.1.0):
- Rust version (`rustc --version`):
- OS:
