# Feature: `json:` Disk Kind for OpenVMM CLI with Shared Cache Defaults

## Problem

Booting a known disk image from the OpenVMM CLI today is painful. You need to know the full blob URL, set `OPENVMM_AUTO_CACHE_PATH`, and type out the full layered disk spec:

```bash
export OPENVMM_AUTO_CACHE_PATH=/some/path
openvmm --disk memdiff::autocache::blob:vhd1:https://example.blob.core.windows.net/vhds/SOME_IMAGE.vhd
```

Meanwhile, petri already knows about available images and caches them automatically under `~/.cache/petri`. The two systems don't share image definitions or cache path defaults, so CLI users have to rediscover URLs and configure caching manually.

## Goal

```
openvmm --disk memdiff::json:images/my-image.json
```

That's it. No env vars, no URLs to remember. Petri and the CLI share the same JSON image definitions and the same default cache directory.

## Design

**JSON files describe disk identity only** — not consumption-time concerns like memdiff or autocache:

```json
{
  "format": "vhd1",
  "url": "https://example.blob.core.windows.net/vhds/SOME_IMAGE.vhd"
}
```

The user controls wrapping explicitly via CLI layering (e.g., `memdiff::json:...`). When the CLI resolves a `json:` disk backed by a remote blob, it automatically applies autocache using a shared default cache directory — no env var needed.

## Work Items

1. **Unify cache defaults** — Make the CLI default to the same cache directory petri uses (`~/.cache/petri` on Linux, platform equivalents elsewhere via `petri_disk_cache_dir()`) instead of requiring `OPENVMM_AUTO_CACHE_PATH`.

2. **Define a JSON disk image schema and ship preset files** — A minimal schema describing a disk's source (format + URL). Ship preset JSON files for known images alongside the existing `vmm_test_images` definitions.

3. **Add `json:` disk kind to CLI parser** — Extend `DiskCliKind::from_str()` in `cli_args.rs` to accept `json:<path>`, parse the JSON file, resolve to a blob disk, and automatically wrap in autocache using the shared cache directory.

## Out of Scope

- VHDX-over-blob support (requires a `DiskAsFile` abstraction — separate effort)
- Named aliases like `--disk my-image` (nice-to-have on top of JSON files, but not part of this work)
