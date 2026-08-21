# tsgo Patches

This directory contains custom patches to apply to the [typescript-go](https://github.com/microsoft/typescript-go) repository when building tsgo from source.

## Adding Patches

1. Patches are applied in **lexicographic order** (sorted by filename)
2. Use a numbered prefix to control order: `001-fix-something.patch`, `002-add-feature.patch`
3. Patches must be in standard `git diff` format (created with `git format-patch` or `git diff`)

## Creating a Patch

```bash
# From a typescript-go checkout with your changes:
git diff > /path/to/monoripple/patches/tsgo/001-my-change.patch

# Or for committed changes:
git format-patch -1 HEAD --stdout > /path/to/monoripple/patches/tsgo/001-my-change.patch
```

## Patch Requirements

- Patches are applied with `git apply` against a clean checkout at the pinned commit
- Patches must apply cleanly (no conflicts)
- If a patch fails to apply, the build will fail with an error indicating which patch failed

## Current Commit

See `TSGO_COMMIT` in `build.rs` for the currently pinned typescript-go commit SHA.
