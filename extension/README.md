# engram browser extension

One codebase, two manifests; only the panel-opening call differs (`sidePanel` in
Chrome, `sidebarAction` in Firefox), behind `shared/shim.js`. The browser sends
the HTML it rendered, so pages behind a login are captured as you see them.

## Loading it for development

```sh
./extension/pack.sh          # copies shared/ into each browser's package
```

- **Chrome** — `chrome://extensions` → Developer mode → Load unpacked →
  `extension/chrome`.
- **Firefox** — `about:debugging#/runtime/this-firefox` → Load Temporary
  Add-on → `extension/firefox/manifest.json`.

## Manual checks

Not automated. Run on both browsers against a running deployment.
**Status: not yet run.**

- [ ] The panel opens from the toolbar: Capture this page, one box, Ask and
      Capture (disabled while empty), Recent, results. Fonts load offline.
- [ ] Pairing: the permission prompt names one host; the auth window closes;
      "Paired with …"; Settings lists a "browser extension" token. A bare host
      is read as `https://`.
- [ ] Capture this page, Capture selection, a page behind a login, a page that
      extracts to nothing (refused), the same page twice (duplicate named).
- [ ] Capture from a tab other than the one the panel opened on asks once for
      read access; the context menu never asks.
- [ ] Typing searches; Capture stores the box as a paste; Ask streams word by
      word; ← Results during a stream keeps the results.
- [ ] Recent refetches only while open.
- [ ] Context menu and `eg` omnibox keyword work with the panel closed, and
      deliver exactly once.
- [ ] Engram stopped: "Server unreachable." Token revoked: "Token no longer
      works; pair again." and the address field returns.
- [ ] Forget removes the host permission; the token stays listed until revoked.

## Releasing

`build.rs` packs both archives at compile time; `/extension/install` links them.
Firefox needs a signed package for a permanent install. An unlisted AMO
submission signs it without listing it:

```sh
# 1. Bump `version` in both manifests.
./extension/pack.sh
cd extension/firefox && zip -r ../../engram-unsigned.zip . && cd ../..
# 2. AMO → Developer Hub → Submit a New Add-on → "On your own site".
# 3. Commit the signed result; build.rs prefers it.
cp ~/Downloads/engram-*.xpi extension/firefox-signed.xpi
git add extension/firefox-signed.xpi
```

There is no `update_url`: a self-hosted install has no canonical host, and
rewriting the manifest per download breaks the signature.

### Icons

`shared/icon-*.png` are rasterized from `assets/icon.svg` and committed:

```sh
for s in 16 32 48 128; do
  magick -background none -density 1200 assets/icon.svg \
    -resize ${s}x${s} -depth 8 -strip PNG32:extension/shared/icon-$s.png
done
```
