# cksk

<!-- ccid:security:begin (rendered by ccid render; do not edit by hand) -->
**Security and dependencies:** watched by Renovate (with OSV vulnerability alerts); an update is merged only when its tests pass. See the [security policy](SECURITY.md).
<!-- ccid:security:end -->

Server-monitor boards for a clck kiosk display: polls status sources, renders big-type
status screens in rotation, and streams frames to the display server. No nix required.

```console
$ cargo build --release
$ ./target/release/cksk --dump board.png --screen overview --source https://status.example.com/api/v1/endpoints/statuses
$ ./target/release/cksk --dump-pages /tmp/boards --source https://status.example.com/api/v1/endpoints/statuses
$ GATUS_URL=... CLCK_SOCKET=$XDG_RUNTIME_DIR/clck.sock ./target/release/cksk --size 1920x1080
```

Boards (30s each, hard cut): `overview` (verdict + one row per group), the detail
pages (service details at room-readable type), `system` (compute/memory/temps
columns plus network + pool rows). Header (`corbet status` + clock) is identical
on every board.

Detail pages are derived from the feed, not from a fixed group list: groups are
packed three columns at a time in canonical order (mail, certificates, ci, nodes,
caches, github, seaweedfs, backups, clouds, watchdog) and any group the source
grows after that list is appended, so every row the source returns appears on
some page. `--dump-pages DIR` writes `overview`, every `page-N`, and `system` as
PNGs -- the proof of what the show contains without watching it rotate.
Live rotation is strictly adaptive: overview/system core loop, troubled pages
pin until recovery, healthy detail pages appear as proof-of-life every fifth
cycle. Gatus data older than 90s is labeled stale instead of shown green.

## Frames: Netpbm PAM to clck

cksk connects to clck's Unix socket and writes one
[Netpbm PAM](https://netpbm.sourceforge.net/doc/pam.html) image about once a second
(sooner after a data refresh): a `P7` header with `WIDTH`, `HEIGHT`, `DEPTH 4`,
`MAXVAL 255` and `TUPLTYPE RGB_ALPHA`, then the pixels with straight alpha (the boards
are opaque). clck sends nothing back, so there is no handshake; it shows each image 1:1
when the size matches its kiosk output and scales it to fit otherwise. clck's README,
"Frame input: Netpbm PAM over a Unix socket", has the limits: the same user only, each
side 1 to 8192. When the connection fails or ends, cksk reconnects with backoff (250 ms
doubling to 5 s).

The size is cksk's own: `--size WxH`, else the config file's `size`, else `1920x1080`
(scale 1). It applies to `--dump` and `--dump-pages` too. Render at the kiosk output's
pixel size to get it 1:1.

Socket: the config file's `socket_path`, else `CLCK_SOCKET`, else
`$XDG_RUNTIME_DIR/clck.sock`, clck's default. `clck kioskd` listens on its own
`$XDG_RUNTIME_DIR/clck-live.sock`; point a second cksk at it with `CLCK_SOCKET`.

## Config

`$XDG_CONFIG_HOME/cksk/config.json` (the previous `nixwatch-frames` location as a
fallback): `gatus_url`, `socket_path`, `size` (`"1920x1080"`), optional `host_feed`
(path to the host collector JSON with link rates and pool aggregates the kiosk cannot
see itself; missing/stale reads as pending rows; see `contrib/host-collector/` for the
privileged feed script). Then `GATUS_URL` / `CLCK_SOCKET`, then built-in defaults.

License: FSL-1.1-ALv2, see `LICENSE.md`.
