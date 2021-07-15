# hotlib [![Actions Status](https://github.com/mitchmindtree/hotlib/workflows/hotlib/badge.svg)](https://github.com/mitchmindtree/hotlib/actions) [![Crates.io](https://img.shields.io/crates/v/hotlib.svg)](https://crates.io/crates/hotlib) [![Crates.io](https://img.shields.io/crates/l/hotlib.svg)](https://github.com/mitchmindtree/hotlib/blob/master/LICENSE-MIT) [![docs.rs](https://docs.rs/hotlib/badge.svg)](https://docs.rs/hotlib/)

A library for watching, dynamically compiling, and hot-loading Rust libraries.

Try running:

```
cargo run --example demo
```

And while the demo is running, try editing the `test_crate/src/lib.rs` file.
Each time you write your changes to disk, the demo should automatically detect
the change, return the package, build the package and then load the
corresponding dynamic library.
