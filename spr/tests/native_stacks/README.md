# Native stack scenarios

> **Historical.** The Rust suites (`tests/native_stacks.rs`, `tests/sync.rs`,
> with the fake GitHub in `tests/common/`) are the maintained tests and run
> with `cargo test`. The Python harness in this directory is kept for
> reference only; it does not cover `jj spr sync` and is not kept up to date.

End-to-end checks of `jj spr diff`, `land` and `close` with
`spr.nativeStacks`, run against a fake GitHub that serves jj-spr's REST and
GraphQL calls from a local bare Git repository and models GitHub's stacked
pull requests. The stack rules follow the fake GitHub in
[jj-stack](https://github.com/bos/jj-stack), whose authors checked them against
the real API; the fake's module documentation lists which behaviours are
assumptions.

There are two implementations of the same scenarios, under the same names:

- **Rust** (`tests/native_stacks.rs`, with the fake in `tests/common/`) runs
  as part of `cargo test`:

  ```shell
  cargo test --test native_stacks
  SPR_TEST_VERBOSE=1 cargo test --test native_stacks land_bottom -- --nocapture
  ```

- **Python** (this directory) uses only the standard library (3.9+) and runs
  against any jj-spr binary, which makes it handy for checking a release
  build or another platform:

  ```shell
  python3 spr/tests/native_stacks/run_scenarios.py --jj-spr target/release/jj-spr
  python3 spr/tests/native_stacks/run_scenarios.py --jj-spr target/release/jj-spr -k land -v
  ```

Both need `jj` and `git` on `PATH`. Each scenario runs in a temporary
directory with its own jj and Git configuration, so your own configuration is
not used or changed.

When GitHub's behaviour turns out to differ from the fake, change both fakes
(`tests/common/fake_github.rs` and `fake_github.py`) and add the scenario to
both suites.
