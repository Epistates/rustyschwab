# Live conformance fixtures

One `<endpoint>.shape.json` per read-only endpoint exercised by
`tests/live_conformance.rs`: the recorded *shape* of Schwab's response (key
sets and scalar types, never values or account data).

Refresh them against the live API with real credentials:

```bash
SCHWAB_LIVE_CONFORMANCE=record cargo test -p schwab-rs --test live_conformance -- --nocapture
```

Recording merges the live shape into the existing fixture, so fields Schwab
only sends for some symbols or sessions accumulate over time. Review the
diff before committing: a new key is Schwab adding a field (consider typing
it); a key vanishing from every run or a scalar changing type is a wire
change the bindings must follow.
