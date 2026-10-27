# Fuzzing the store decoder

`cargo-fuzz` needs nightly; the checks in `AGENTS.md` never run this.

```sh
cd crates/store/fuzz
cargo +nightly fuzz run decode
```

`decode` covers the field decoder and the frame reader, the two places raw
bytes reach the store.
