//! The relay feed: MQTT over WebSocket, the same retained tiles the web client shows.
//!
//! The kiosk stays a dumb renderer: this module only turns the relay's `internal/`
//! tree into the endpoint list the boards already draw (one group per site screen,
//! one endpoint per tile). Liveness comes from `_online` alone: past three of its
//! intervals, on a lost connection, or on any state but `online`, every endpoint
//! loses its result, so a board can show unknown or stale but never old green.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
use rumqttc::Transport;
use serde_json::Value;

use crate::model::{Endpoint, GResult};

/// Where the relay's broker is, and who may read the internal tree.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
}

/// (kind, title, [(tile id, tile title)]) of one screen.
type ScreenLayout = (String, String, Vec<(String, String)>);

#[derive(Default)]
struct State {
    /// screen -> (kind, title, [(tile id, tile title)])
    layouts: HashMap<String, ScreenLayout>,
    /// (screen, tile) -> tile state (`s` of the tile message)
    tiles: HashMap<(String, String), String>,
    /// Interval of the last `online` message and when it arrived.
    online: Option<(Duration, Instant)>,
    connected: bool,
}

impl State {
    fn live(&self) -> bool {
        self.connected && self.online.is_some_and(|(iv, at)| at.elapsed() <= iv * 3)
    }

    fn endpoints(&self) -> Vec<Endpoint> {
        let live = self.live();
        let mut sites: Vec<_> = self
            .layouts
            .iter()
            .filter(|(_, (kind, _, _))| kind == "site")
            .collect();
        sites.sort_by(|a, b| a.1 .1.cmp(&b.1 .1));
        let mut out = Vec::new();
        for (screen, (_, title, tiles)) in sites {
            for (id, tile_title) in tiles {
                let results = match (live, self.tiles.get(&(screen.clone(), id.clone()))) {
                    (true, Some(s)) if s == "ok" || s == "issue" => vec![GResult {
                        success: s == "ok",
                        duration: 0,
                        status: None,
                    }],
                    _ => Vec::new(), // unknown, greyed, no data: not up, not down
                };
                out.push(Endpoint {
                    name: tile_title.clone(),
                    group: title.clone(),
                    key: format!("{screen}/{id}"),
                    results,
                });
            }
        }
        out
    }
}

pub struct Shared {
    pub data: Mutex<Vec<Endpoint>>,
    pub version: AtomicU64,
    /// Last time a fresh `online` arrived.
    pub updated: Mutex<Option<Instant>>,
}

fn publish(state: &State, shared: &Shared) {
    *shared.data.lock().unwrap() = state.endpoints();
    shared.version.fetch_add(1, Ordering::Relaxed);
}

/// Starts the feed on its own thread (a single-threaded async runtime inside).
pub fn spawn(cfg: RelayConfig, shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run(cfg, shared));
    });
}

async fn run(cfg: RelayConfig, shared: Arc<Shared>) {
    let mut opts = MqttOptions::new(format!("cksk-{}", std::process::id()), &cfg.url, 443);
    opts.set_keep_alive(Duration::from_secs(30));
    if cfg.url.starts_with("wss://") {
        opts.set_transport(Transport::wss_with_default_config());
    } else {
        opts.set_transport(Transport::Ws);
    }
    if let (Some(u), Some(p)) = (&cfg.user, &cfg.password) {
        opts.set_credentials(u, p);
    }
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    let mut st = State::default();
    let mut last_err = String::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            ev = eventloop.poll() => match ev {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    st.connected = true;
                    last_err.clear();
                    eprintln!("cksk: relay connected");
                    let _ = client.subscribe("internal/#", QoS::AtLeastOnce).await;
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    let topic = String::from_utf8_lossy(&p.topic).into_owned();
                    handle(&mut st, &shared, &topic, &p.payload);
                    publish(&st, &shared);
                }
                Ok(_) => {}
                Err(e) => {
                    st.connected = false;
                    st.online = None; // a retained `online` after a reconnect must not look live
                    let msg = e.to_string();
                    if msg != last_err {
                        eprintln!("cksk: relay: {msg}");
                        last_err = msg;
                    }
                    publish(&st, &shared);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
            _ = tick.tick() => {
                // Liveness can lapse with no message at all.
                let was = shared.data.lock().unwrap().iter().any(|e| !e.results.is_empty());
                if was && !st.live() {
                    publish(&st, &shared);
                }
            }
        }
    }
}

fn handle(st: &mut State, shared: &Shared, topic: &str, payload: &[u8]) {
    let Some(rest) = topic.strip_prefix("internal/") else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(payload) else {
        return;
    };
    if rest == "_online" {
        if v["state"] == "online" {
            let iv = v["interval_s"].as_u64().unwrap_or(60).max(1);
            st.online = Some((Duration::from_secs(iv), Instant::now()));
            *shared.updated.lock().unwrap() = Some(Instant::now());
        } else {
            st.online = None;
        }
        return;
    }
    let Some((screen, what)) = rest.split_once('/') else {
        return;
    };
    if what == "_layout" {
        let tiles = v["tiles"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some((
                            t["id"].as_str()?.to_string(),
                            t["title"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        st.layouts.insert(
            screen.to_string(),
            (
                v["kind"].as_str().unwrap_or("").to_string(),
                v["title"].as_str().unwrap_or(screen).to_string(),
                tiles,
            ),
        );
    } else if !what.starts_with('_') {
        st.tiles.insert(
            (screen.to_string(), what.to_string()),
            v["s"].as_str().unwrap_or("unknown").to_string(),
        );
    }
}
