# Patched upstream crates

Copies of crates.io packages with Arke's fixes on top. `[patch.crates-io]` in the workspace
`Cargo.toml` uses them instead of the published version. They keep their own license
(`LICENSE-APACHE`) and are listed in `THIRD_PARTY_LICENSES.md` like any other dependency.

When gpui-pre is upgraded:
1. Copy the new version over the folder.
2. Re-apply each change below.
3. Drop a change once upstream has it.

## gpui-pre-windows 0.3.11

Zed's Windows platform crate (Apache-2.0), unchanged except for:

- `src/directx_renderer.rs`: the two path intermediate textures (one of them 4x MSAA) are made on
  the first frame that draws a path, and dropped when the window is resized, instead of being kept
  for every window at all times. Together they take 20 of the 28 bytes per window pixel. On GPUs
  that share system memory (integrated graphics), that memory counts as the process's private
  bytes: about 70 MB for a maximized window on a 2880x1800 screen. Arke draws paths only for the
  progress ring of running file operations. (#95)
