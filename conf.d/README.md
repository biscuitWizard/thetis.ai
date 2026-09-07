# conf.d — per-module configuration

A module that is not part of every checkout keeps its configuration here, in a
directory of its own, instead of as a block in `thetis.toml`.

```
conf.d/<module>/<module>.toml     the module's settings
conf.d/<module>/.thetis-private   optional: keep the module out of the export
```

Every `*.toml` under this directory is merged into the configuration, at any
depth, sorted by path so the result does not depend on what order the
filesystem hands back directory entries. The order of the whole stack is:

1. `thetis.toml` — what everybody has.
2. these fragments — what a module adds.
3. `thetis.local.toml` (and `THETIS_LOCAL_CONFIG`) — what this machine says,
   which still wins over a module, exactly as it wins over `thetis.toml`.

Tables merge key by key. **Arrays of tables append**: a fragment that declares
one `[[modes]]` adds a mode, it does not replace the list. Arrays of anything
else are replaced, because those are settings rather than registries — that is
also the only way a fragment can shorten one.

The kernel starts with this directory absent or empty, which is the normal
state for a checkout that has none of these modules.

## Why a directory rather than a section

The publish filter (`crates/thetis/src/publish.rs`) removes a directory
carrying a `.thetis-private` marker from every exported tree, whole. It cannot
remove half a file. So a module that must not be published needs its settings
in a directory it owns, and the marker that already hides its code hides its
configuration with it — no second mechanism, and nothing that has to understand
TOML to decide what may leave.

The settings editor follows the same boundary: a key already set in a fragment
is written back to that fragment, not to `thetis.toml`. See
`crates/thetis/src/settings/mod.rs`.
