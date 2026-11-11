# assets

The icon is Anko's face (cropped from her avatar).

This repository keeps the tree text-only, so the binary assets live here as
Base64 text:

- `icon.png.base64` — 256×256 PNG used for the window and tray icons
- `icon.ico.base64` — multi-size ICO used for the exe resource and shortcuts

`build.rs` decodes them into `OUT_DIR` at build time, so a plain
`cargo build` just works. If real `icon.png` / `icon.ico` files are placed in
this folder, they take precedence over the `.base64` forms.

To restore the raw files by hand:

```sh
base64 -d icon.png.base64 > icon.png
base64 -d icon.ico.base64 > icon.ico
```

`icon-spark.*` in the author's local tree was the first-draft spark icon and
is not part of this repository.
