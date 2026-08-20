# Vendored RBS signatures

Ruby's own type signatures — `core/` for the classes the interpreter provides (`String`, `Array`,
`Hash`, `Integer`, `Kernel`, …) and `stdlib/` for the libraries that ship with it (`Set`, `CSV`,
`URI`, `Pathname`, …). Copied verbatim from the [`rbs`](https://github.com/ruby/rbs) gem; the
version is in `VERSION`.

## Why this is here

`workspace::rbs` prefers an `rbs-*` gem found on disk, because that one matches the Ruby the
project actually runs. This copy is the rung below it: on a machine with no Ruby installed —
the case ya-lsp exists for — it is the only source of built-in signatures there is. It is
extracted to a cache directory at first use, because rubydex keys documents by file URL and
go-to-definition has to hand the editor a file it can open.

## Updating

```bash
gem fetch rbs -v X.Y.Z && gem unpack rbs-X.Y.Z.gem
rsync -a --include='*/' --include='*.rbs' --exclude='*' rbs-X.Y.Z/core   vendor/rbs/
rsync -a --include='*/' --include='*.rbs' --exclude='*' rbs-X.Y.Z/stdlib vendor/rbs/
cp rbs-X.Y.Z/BSDL rbs-X.Y.Z/COPYING vendor/rbs/
echo X.Y.Z > vendor/rbs/VERSION
```

Only `.rbs` files are taken. `build.rs` embeds whatever is here, so nothing else needs changing —
but do re-run `cargo test`, since the fixtures assert the vendored tree parses and that
`String#upcase` is in it.

Bumping the version changes the answers ya-lsp gives for core classes. Between rbs 3.10.0 and
4.1.3 that was 76 declarations added and 82 removed out of ~3,640 — about 2%, and most of the
removals were methods re-homed onto an ancestor, so they still resolve. Treat a bump as a change
in behaviour, not as a dependency update.

## Licence

The signatures are part of the `rbs` gem and are licensed BSD-2-Clause and the Ruby licence —
`BSDL` and `COPYING`, both copied here unmodified. They are not covered by ya-lsp's own MIT
licence.
