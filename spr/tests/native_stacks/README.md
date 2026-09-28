# Native stack scenarios

End-to-end checks of `jj spr diff`, `land` and `close` with
`spr.nativeStacks`, run against a fake GitHub (`fake_github.py`) that serves
jj-spr's REST and GraphQL calls from a local bare Git repository and models
GitHub's stacked pull requests. The stack rules follow the fake GitHub in
[jj-stack](https://github.com/bos/jj-stack), whose authors checked them against
the real API; the module docstring lists which behaviours are assumptions.

Both files use only the Python standard library (3.9+).

```shell
cargo build
python3 spr/tests/native_stacks/run_scenarios.py --jj-spr target/debug/jj-spr
python3 spr/tests/native_stacks/run_scenarios.py --jj-spr target/debug/jj-spr -k land -v
```

`-k` selects scenarios by name and `-v` prints every command with its output.
Each scenario runs in a temporary directory with its own jj config, so your
own configuration is not used or changed.
