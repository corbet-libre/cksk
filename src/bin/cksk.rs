#![forbid(unsafe_code)]

// cksk [--version|-V] [--size WxH] [--dump <out.png>] [--dump-pages <dir>] [--source <file|url>]
//
// A frame CLIENT for clck's kiosk display socket: it connects to the Unix socket (retrying with
// backoff while clck is not up yet), renders the current kiosk board at its own configured size
// and writes it as a Netpbm PAM image (TUPLTYPE RGB_ALPHA, straight alpha, MAXVAL 255) roughly
// once a second, and promptly after a data refresh. clck sends nothing back: it shows each image
// 1:1 when the size matches its kiosk output and scaled to fit otherwise (clck's README, "Frame
// input: Netpbm PAM over a Unix socket"). Pure socket client: no Wayland, no PAM authentication,
// no display-server link at all -- see `src/gatus.rs`'s own header.
//
// Config precedence per field: $XDG_CONFIG_HOME/cksk/config.json (falling back to the previous
// nixwatch-frames location, then ~/.config) -- fields `gatus_url` / `socket_path` / `host_feed`
// / `size` -- then the GATUS_URL / CLCK_SOCKET env vars, then a neutral built-in default. The
// size is `--size WxH`, else the file's `size`, else 1920x1080; it applies to --dump too.
//
// --dump renders exactly one frame headlessly to a PNG (tiny_skia::Pixmap::save_png) through the
// same Show::render_named the live path uses -- no socket connection needed, so it verifies the
// render (fonts, layout, Gatus parsing) in CI or over SSH. --dump-pages writes every board the
// current feed warrants (overview, each detail page, system), which proves that no row the
// source returns is invisible.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cksk::Show;
use serde::Deserialize;

// A neutral placeholder -- the real URL is a per-host runtime config value, never baked in here.
const DEFAULT_GATUS_URL: &str = "https://status.example.com/api/v1/endpoints/statuses";

// The render size when neither --size nor the config file's `size` names one: archlxc's kiosk
// screen (the MSI on DP-3) at scale 1.
const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
// clck refuses an image whose width or height is outside 1..=8192 (its README "Frame input").
const MAX_SIDE: u32 = 8192;

// Frame cadence: stream at least once a second, and promptly (next tick) after the poller in
// GatusBoard actually refreshes -- see GatusBoard::version.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(200);
const MAX_FRAME_INTERVAL: Duration = Duration::from_secs(1);

// Reconnect backoff: clck may not be up yet (or may be mid-restart) when this starts. Only a
// connection that lasted at least MAX_BACKOFF resets it, so a server that keeps closing at once
// is retried at the capped pace, never in a tight loop.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    gatus_url: Option<String>,
    socket_path: Option<String>,
    /// Optional host-collected rows (network interfaces, pool aggregates) for the
    /// system board. Written by a privileged collector outside this process
    /// (cksk itself stays unprivileged); absent file reads as pending rows.
    host_feed: Option<String>,
    /// The render size, `WxH` (e.g. `"1920x1080"`); `--size` wins over it.
    size: Option<String>,
}

fn config_base() -> Option<std::path::PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok()
}

/// New location first, previous `nixwatch-frames` location as a migration fallback so an
/// existing session keeps working across the rename with no flag day.
fn config_path() -> Option<std::path::PathBuf> {
    let base = config_base()?;
    let fresh = base.join("cksk").join("config.json");
    if fresh.exists() {
        return Some(fresh);
    }
    let legacy = base.join("nixwatch-frames").join("config.json");
    if legacy.exists() {
        return Some(legacy);
    }
    Some(fresh)
}

fn load_file_config() -> FileConfig {
    let Some(path) = config_path() else {
        return FileConfig::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return FileConfig::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("cksk: {}: {e} (ignoring)", path.display());
            FileConfig::default()
        }
    }
}

/// `--dump` needs only this -- resolving a socket path it will never dial is pointless work with
/// a real failure mode (a bare `nix run`/CI/SSH invocation with no `$XDG_RUNTIME_DIR` at all would
/// otherwise fail `--dump` for a reason that has nothing to do with what `--dump` actually does).
fn resolve_gatus_url(file: &FileConfig) -> String {
    file.gatus_url
        .clone()
        .or_else(|| std::env::var("GATUS_URL").ok())
        .unwrap_or_else(|| DEFAULT_GATUS_URL.to_string())
}

/// `WxH`, each side a plain decimal number in 1..=MAX_SIDE.
fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let side = |v: &str| -> Option<u32> {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        v.parse().ok().filter(|n| (1..=MAX_SIDE).contains(n))
    };
    s.split_once('x')
        .and_then(|(w, h)| Some((side(w)?, side(h)?)))
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, each 1..={MAX_SIDE} (e.g. 1920x1080)"))
}

/// `--size`, else the config file's `size`, else DEFAULT_SIZE. A size that does not parse is a
/// configuration error and stops cksk, rather than silently rendering at some other size.
fn resolve_size(file: &FileConfig) -> (u32, u32) {
    let (from, value) = match (arg("--size"), &file.size) {
        (Some(v), _) => ("--size", v),
        (None, Some(v)) => ("config size", v.clone()),
        (None, None) => return DEFAULT_SIZE,
    };
    parse_size(&value).unwrap_or_else(|e| {
        eprintln!("cksk: {from} {value:?}: {e}");
        std::process::exit(2);
    })
}

/// The socket: the config file's `socket_path`, else `CLCK_SOCKET`, else
/// `$XDG_RUNTIME_DIR/clck.sock` (clck's own default). `None`: nothing to dial.
fn socket_path(
    configured: Option<String>,
    clck_socket: Option<OsString>,
    runtime_dir: Option<OsString>,
) -> Option<PathBuf> {
    configured
        .map(PathBuf::from)
        .or_else(|| clck_socket.map(PathBuf::from))
        .or_else(|| {
            runtime_dir
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d).join("clck.sock"))
        })
}

struct Resolved {
    gatus_url: String,
    socket_path: PathBuf,
    host_feed: Option<String>,
    size: (u32, u32),
}

fn resolve_config() -> Resolved {
    let file = load_file_config();
    let gatus_url = resolve_gatus_url(&file);
    let size = resolve_size(&file);
    let socket_path = socket_path(
        file.socket_path,
        std::env::var_os("CLCK_SOCKET"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .unwrap_or_else(|| {
        // Same channel clck's own default resolves against (its README/socket module) --
        // if this session has neither a configured socket_path nor XDG_RUNTIME_DIR, there is
        // no sane guess left to make; fail loudly rather than dial a made-up path.
        eprintln!(
            "cksk: no socket_path configured and $XDG_RUNTIME_DIR is unset -- \
                 cannot locate clck's kiosk socket"
        );
        std::process::exit(1);
    });
    Resolved {
        gatus_url,
        socket_path,
        host_feed: file.host_feed,
        size,
    }
}

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == flag)
        .and_then(|i| a.get(i + 1).cloned())
}

fn main() {
    // Version discovery must be safe for packaging and monitoring probes: answer before reading
    // config, resolving a runtime socket, constructing the board, or doing any I/O beyond
    // stdout. Keep both spellings expected of a conventional command-line program.
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("cksk {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if let Some(out) = arg("--dump") {
        let file = load_file_config();
        let gatus_url = resolve_gatus_url(&file);
        let source = arg("--source").unwrap_or(gatus_url);
        let screen = arg("--screen").unwrap_or_else(|| "overview".to_string());
        // --feed <path> lets a preview render host rows from a fixture without
        // touching the live config file.
        let feed = arg("--feed").or(file.host_feed.clone());

        let (w, h) = resolve_size(&file);
        let show = Show::with_feed(source, feed);
        let pm = show.render_named(&screen, w, h).unwrap_or_else(|e| {
            eprintln!("cksk: --dump --screen {screen}: {e}");
            std::process::exit(2);
        });
        pm.save_png(&out).expect("save png");
        eprintln!("cksk: dumped {screen} {w}x{h} -> {out}");
        return;
    }

    // --dump-pages renders every board the current feed warrants (overview, every
    // detail page, system) into a directory: the proof that the show contains every
    // row the source returned, without watching the kiosk rotate.
    if let Some(dir) = arg("--dump-pages") {
        let file = load_file_config();
        let gatus_url = resolve_gatus_url(&file);
        let source = arg("--source").unwrap_or(gatus_url);
        let feed = arg("--feed").or(file.host_feed.clone());
        let (w, h) = resolve_size(&file);
        let show = Show::with_feed(source, feed);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
            eprintln!("cksk: create {dir}: {e}");
            std::process::exit(2);
        });
        let pages = show.pages(h);
        let mut boards: Vec<String> = vec!["overview".to_string(), "system".to_string()];
        boards.extend((1..=pages.len()).map(|i| format!("page-{i}")));
        for id in &boards {
            let pm = show
                .render_named(id, w, h)
                .unwrap_or_else(|e| panic!("render {id}: {e}"));
            let path = std::path::Path::new(&dir).join(format!("{id}.png"));
            pm.save_png(&path).expect("save png");
            eprintln!("cksk: dumped {id} -> {}", path.display());
        }
        let rows: usize = pages
            .iter()
            .map(|p| p.groups.iter().map(|(_, i)| i.len()).sum::<usize>())
            .sum();
        eprintln!(
            "cksk: {} boards, {} detail pages, {rows} rows on the detail pages",
            boards.len(),
            pages.len()
        );
        return;
    }

    let resolved = resolve_config();
    let (w, h) = resolved.size;
    eprintln!(
        "cksk: gatus_url={} socket={} size={w}x{h}",
        resolved.gatus_url,
        resolved.socket_path.display()
    );
    let show = Show::with_feed(resolved.gatus_url, resolved.host_feed);
    run_client(&resolved.socket_path, &show, resolved.size);
}

/// Connect-stream-reconnect forever. A disconnect (clck restarted, the socket vanished, the lock
/// that published it ended) is not fatal -- it just goes back to the top of this loop with backoff.
fn run_client(socket_path: &std::path::Path, show: &Show, size: (u32, u32)) {
    run_client_with(socket_path, show, size, std::thread::sleep, |_, _| false);
}

/// The production client never stops. The two injected callbacks make the reconnect state machine
/// behavior-testable without weakening that contract: tests can observe a retry, suppress the real
/// sleep, and stop after a fake clck has received the eventual frame.
fn run_client_with<S, F>(
    socket_path: &std::path::Path,
    show: &Show,
    size: (u32, u32),
    mut sleep: S,
    mut stop_after: F,
) where
    S: FnMut(Duration),
    F: FnMut(usize, RetryReason) -> bool,
{
    let mut backoff = INITIAL_BACKOFF;
    let mut attempts = 0usize;
    loop {
        let connected = Instant::now();
        let reason = match connect_and_stream(socket_path, show, size) {
            Ok(()) => {
                eprintln!("cksk: disconnected from clck; reconnecting");
                if connected.elapsed() >= MAX_BACKOFF {
                    backoff = INITIAL_BACKOFF; // a connection that actually lasted resets the backoff
                }
                RetryReason::Disconnected
            }
            Err(e) => {
                eprintln!("cksk: connect to {} failed: {e}", socket_path.display());
                RetryReason::ConnectFailed
            }
        };
        attempts += 1;
        if stop_after(attempts, reason) {
            return;
        }
        sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryReason {
    /// The stream ended (clck exited or restarted, a newer client replaced this one, or clck
    /// closed the connection).
    Disconnected,
    /// The socket could not be connected: clck is not up, or no lock currently publishes it.
    ConnectFailed,
}

/// `Err`: the socket could not be connected. `Ok`: connected and streamed until a write failed.
/// No handshake: clck sends nothing, so the first image goes out right after the connect.
fn connect_and_stream(
    socket_path: &std::path::Path,
    show: &Show,
    (width, height): (u32, u32),
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    eprintln!("cksk: connected; streaming {width}x{height}");

    let mut last_version = None::<u64>;
    let mut last_sent = Instant::now() - MAX_FRAME_INTERVAL; // send the first frame immediately
    loop {
        let version = show.version();
        let due = last_sent.elapsed() >= MAX_FRAME_INTERVAL;
        let refreshed = last_version != Some(version);
        if due || refreshed {
            // Wall-clock rotation: the board follows Unix time, so every viewer agrees
            // on the current screen without any coordination.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let Some(pm) = show.render_current(width, height, now_secs) else {
                std::thread::sleep(MIN_FRAME_INTERVAL);
                continue;
            };
            if let Err(e) = stream.write_all(&pam_frame(&pm)) {
                // A write failure means the connection is gone (clck closed it, e.g. a newer
                // client took over, or clck itself exited) -- go back to reconnect/backoff.
                eprintln!("cksk: write failed: {e}");
                return Ok(());
            }
            last_version = Some(version);
            last_sent = Instant::now();
        }
        std::thread::sleep(MIN_FRAME_INTERVAL);
    }
}

/// One rendered board as a Netpbm PAM image: `TUPLTYPE RGB_ALPHA`, `DEPTH 4`, `MAXVAL 255`, then
/// the pixels row-major, top-down. tiny-skia holds premultiplied pixels and PAM alpha is straight,
/// so each pixel is demultiplied (the boards are opaque, so in practice it is a plain copy).
fn pam_frame(pm: &tiny_skia::Pixmap) -> Vec<u8> {
    let header = format!(
        "P7\nWIDTH {}\nHEIGHT {}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
        pm.width(),
        pm.height()
    );
    let mut out = Vec::with_capacity(header.len() + pm.data().len());
    out.extend_from_slice(header.as_bytes());
    for px in pm.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Read};
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// What a fake clck reads off the socket: one PAM header (as `KEY value` lines up to ENDHDR)
    /// and its raster.
    fn read_image(stream: &mut BufReader<UnixStream>) -> (Vec<(String, String)>, Vec<u8>) {
        let mut fields = Vec::new();
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line, "P7\n", "not a PAM image");
        loop {
            line.clear();
            stream.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line == "ENDHDR" {
                break;
            }
            let (k, v) = line.split_once(' ').unwrap();
            fields.push((k.to_string(), v.to_string()));
        }
        let get = |k: &str| -> usize {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.parse().unwrap())
                .unwrap()
        };
        let mut raster = vec![0u8; get("WIDTH") * get("HEIGHT") * get("DEPTH")];
        stream.read_exact(&mut raster).unwrap();
        (fields, raster)
    }

    /// clck is not listening yet when cksk starts (connect fails, backoff), then comes up and
    /// says nothing at all: cksk must not wait for any greeting, and its first write must be a
    /// complete PAM image at its OWN configured size.
    #[test]
    fn a_failed_connect_backs_off_then_the_first_write_is_a_pam_image_at_the_configured_size() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let test_dir =
            std::env::temp_dir().join(format!("cksk-startup-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&test_dir).unwrap();
        let socket_path = test_dir.join("clck.sock");
        let source_path = test_dir.join("gatus.json");
        fs::write(&source_path, b"[]").unwrap();

        let received = Arc::new(AtomicBool::new(false));
        let server_received = Arc::clone(&received);
        let mut server = None;
        let show = Show::new(source_path.to_string_lossy().into_owned());
        let mut retries = Vec::new();
        let mut sleeps = Vec::new();
        run_client_with(
            &socket_path,
            &show,
            (64, 32),
            |duration| {
                sleeps.push(duration);
                // clck comes up during the first backoff.
                if server.is_none() {
                    let listener = UnixListener::bind(&socket_path).unwrap();
                    let got = Arc::clone(&server_received);
                    server = Some(thread::spawn(move || {
                        let (conn, _) = listener.accept().unwrap();
                        // Bounded: a client waiting for a greeting fails here instead of hanging.
                        conn.set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let mut reader = BufReader::new(conn.try_clone().unwrap());
                        let (fields, raster) = read_image(&mut reader);
                        let want: Vec<(String, String)> = [
                            ("WIDTH", "64"),
                            ("HEIGHT", "32"),
                            ("DEPTH", "4"),
                            ("MAXVAL", "255"),
                            ("TUPLTYPE", "RGB_ALPHA"),
                        ]
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                        assert_eq!(fields, want);
                        assert!(raster.iter().any(|b| *b != 0), "an empty frame");
                        assert!(raster.chunks(4).all(|px| px[3] == 255), "a board is opaque");
                        got.store(true, Ordering::SeqCst);
                        conn.shutdown(Shutdown::Both).unwrap();
                    }));
                }
            },
            |attempt, reason| {
                retries.push(reason);
                attempt >= 2
            },
        );

        server.unwrap().join().unwrap();
        assert!(received.load(Ordering::SeqCst));
        assert_eq!(
            retries,
            vec![RetryReason::ConnectFailed, RetryReason::Disconnected]
        );
        // a connection that ended at once does not reset the backoff
        assert_eq!(sleeps, vec![INITIAL_BACKOFF]);

        fs::remove_dir_all(test_dir).unwrap();
    }

    /// The writing half of the round trip: for this 3x2 board the writer emits exactly
    /// `tests/fixtures/board-3x2.pam`, the same file clck's reader test decodes (clck
    /// `src/netpbm.rs`, "the_cksk_fixture_decodes_to_the_expected_premultiplied_pixels").
    #[test]
    fn the_frame_writer_emits_the_shared_fixture() {
        let mut pm = tiny_skia::Pixmap::new(3, 2).unwrap();
        let premultiplied = [
            (255, 0, 0, 255),
            (0, 255, 0, 255),
            (0, 0, 255, 255),
            (128, 128, 128, 128), // straight white at alpha 128
            (0, 0, 0, 0),
            (10, 20, 30, 255),
        ];
        for (px, (r, g, b, a)) in pm.pixels_mut().iter_mut().zip(premultiplied) {
            *px = tiny_skia::PremultipliedColorU8::from_rgba(r, g, b, a).unwrap();
        }
        assert_eq!(
            pam_frame(&pm),
            include_bytes!("../../tests/fixtures/board-3x2.pam"),
        );
    }

    #[test]
    fn a_size_is_width_x_height_each_one_to_8192() {
        assert_eq!(parse_size("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_size("1x8192"), Ok((1, 8192)));
        for bad in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x1080",
            "1920x0",
            "8193x1",
            "1920X1080",
            "+1920x1080",
            "1920x1080x2",
            " 1920x1080",
            "1920.0x1080",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// No migration fallback: clck's own default name only, even where an old socket file of
    /// the previous product is still lying around.
    #[test]
    fn the_socket_is_configured_then_clck_socket_then_the_runtime_dirs_clck_sock() {
        let dir = std::env::temp_dir().join(format!("cksk-socket-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nixlock.sock"), b"").unwrap();
        let rt = Some(dir.clone().into_os_string());
        assert_eq!(
            socket_path(None, None, rt.clone()),
            Some(dir.join("clck.sock"))
        );
        assert_eq!(
            socket_path(None, Some("/run/x.sock".into()), rt.clone()),
            Some(PathBuf::from("/run/x.sock"))
        );
        assert_eq!(
            socket_path(Some("/run/y.sock".into()), Some("/run/x.sock".into()), rt),
            Some(PathBuf::from("/run/y.sock"))
        );
        assert_eq!(socket_path(None, None, None), None);
        assert_eq!(socket_path(None, None, Some("".into())), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
