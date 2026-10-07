# drand-rs

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/sagikazarmark/drand-rs/ci.yaml?style=flat-square)](https://github.com/sagikazarmark/drand-rs/actions/workflows/ci.yaml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/sagikazarmark/drand-rs/badge?style=flat-square)](https://securityscorecards.dev/viewer/?uri=github.com/sagikazarmark/drand-rs)
[![crates.io](https://img.shields.io/crates/v/drand?style=flat-square)](https://crates.io/crates/drand)
[![docs.rs](https://img.shields.io/docsrs/drand?style=flat-square)](https://docs.rs/drand)

**Fetch and strictly verify [drand](https://drand.love) randomness beacons in Rust.**

| Crate | |
|---|---|
| [`drand`](crates/drand) | A `no_std`/wasm verifier core (chain parameters, round arithmetic, v1/v2 beacon parsing, strict BLS verification) plus an async multi-relay client. |
| [`restate-drand`](crates/restate-drand) | drand randomness in [Restate](https://restate.dev) handlers: commit to a future round, wait durably, fetch and verify the beacon. |

See each crate's README for usage, features and the security model.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
