# Vendored: compose:dynlink WIT

Source: `~/git/webassembly-component-orchestration/wit/compose-dynlink/`
(commit reference vendored at integration time).

License: Apache-2.0 (matching upstream).

We vendor rather than submodule so we control the integration cadence
independent of upstream's development. To refresh:

```
cp ~/git/webassembly-component-orchestration/wit/compose-dynlink/*.wit \
   sqlite-wasm/wit/deps/compose-dynlink/
```

Then commit with a reference to the upstream commit hash.

Used by sqlite-wasm to:
- Implement `linker` on the host (`compose:dynlink/linker`)
- Build Fiji functions that target `dynlink-guest`
- Expose the SQLite runtime as a virtual provider via `endpoint`

See `PLAN-compose-integration.md` in the repo root for the integration plan.
