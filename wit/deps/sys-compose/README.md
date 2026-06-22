# Vendored: sys:compose/types WIT

Source: `~/git/webassembly-component-orchestration/wit/sys-compose/`
(types.wit + package.wit only; other interfaces deferred).

License: Apache-2.0 (matching upstream).

We vendor only `types.wit` (and the package header) because that's
what `compose:dynlink` transitively needs. The full sys:compose suite
(plan/emit/exec/blobs/trust/etc.) isn't pulled in until we use it.
