# Debug bundles as regression tests

A live-engine failure leaves a bundle in `debug_dir` (docs/development.md, "When the live engine
fails"). Once the failure is fixed, keep the bundle here so it stays fixed:

```bash
rtex bundle replay DEBUG_DIR/engine-…              # fails (exit 1) before the fix
rtex bundle anonymize DEBUG_DIR/engine-… fixtures/bundles/<short-name>
# read fixtures/bundles/<short-name>/source.tex and rtex-preamble.tex: the document's words
# are replaced, but braced arguments of commands other than text commands are kept
rtex bundle replay fixtures/bundles/<short-name>    # passes (exit 0) after the fix
```

`crates/rtex-cli/tests/bundles.rs` replays every directory here; each must answer `ok` or
`ok_degraded`. A bundle that needs project files (images, `\input` files) cannot be replayed
alone: reduce it to a source that reproduces the failure without them.
