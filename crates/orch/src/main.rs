//! Orch на Rust: process-per-bot (как 502b.mjs).
//! Конфиг: `orch.json`. Боты/прокси/овнер: `farm_root/bots/{an}b.json`, `ip.json`, `clan-owners.json`.

mod clan_setup;
mod config;
mod listing_store;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use ipc::{
    pretty_log::{self, LineKind},
    BotBoot, OrchCmd, WorkerMsg,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Deserialize)]
struct BotJson {
    username: String,
    password: String,
    anarchy: u16,
    #[serde(default)]
    item: String,
    #[serde(rename = "goType")]
    go_type: String,
    ip: String,
}

#[derive(Debug)]
struct BotRuntime {
    cfg: BotJson,
    proxy: String,
    success: bool,
    /// На анке (не в хабе). Pricing-active только при on_anarchy.
    on_anarchy: bool,
    banned: bool,
    manual_stop: bool,
    presence_inactive: bool,
    /// Флап: много вылетов/хабов за окно → временно не в bots_per_type.
    unstable_until: Option<std::time::Instant>,
    /// Моменты смены «годен для pricing» (для детекта флаппа).
    pricing_edges: std::collections::VecDeque<std::time::Instant>,
    last_pricing_ok: bool,
    items: Value,
    inventory: Value,
    /// Последний баланс с воркера (для capital_plan).
    balance: Option<f64>,
    balance_at: Option<std::time::Instant>,
    banned_at: Option<String>,
    banned_reason: Option<String>,
    /// bad_password | proxy_error
    auth_fault: Option<String>,
    auth_fault_at: Option<String>,
    auth_fault_reason: Option<String>,
}

struct WorkerHandle {
    child: Child,
    stdin: ChildStdin,
}

struct OrchState {
    bots: HashMap<String, BotRuntime>,
    workers: HashMap<String, WorkerHandle>,
    bot_bin: PathBuf,
    /// Путь bots/{an}b.json — рядом banned-memory.json.
    bots_path: PathBuf,
    shutting_down: bool,
    /// Координация UUID лотов между ботами.
    items_buying: Value,
    listings: listing_store::ListingStore,
    /// anarchy → owner nick (clan-owners.json)
    clan_owners: HashMap<u16, String>,
    clan_setup: Arc<clan_setup::ClanSetupGate>,
    /// Последний смерженный каталог с priceSell (для capital_plan).
    last_catalog: Vec<Value>,
    clan_mates: u32,
    clan_treasury: Option<f64>,
    clan_treasury_at: Option<std::time::Instant>,
}

type GoTx = mpsc::UnboundedSender<Value>;
type RestartTx = mpsc::UnboundedSender<String>;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_log::init_orch();

    let loaded = config::load()?;
    let farm_root = loaded.farm_root.clone();
    let anarchy = loaded.cfg.anarchy;
    let local_mode = loaded.cfg.local_mode;
    let ws_url = loaded.cfg.go_ws.clone();
    let bot_bin = config::bot_bin_path()?;
    info!("bot={}", bot_bin.display());

    let bots_path = farm_root.join(format!("bots/{anarchy}b.json"));
    let ip_path = farm_root.join("ip.json");
    let bots = load_bots(&bots_path, &ip_path)?;

    let listings = listing_store::ListingStore::new(listing_store::default_dir(&farm_root));
    let clan_owners = load_clan_owners(&farm_root);
    let clan_setup = clan_setup::ClanSetupGate::new(farm_root.clone(), local_mode);

    let (go_tx, go_rx) = mpsc::unbounded_channel::<Value>();
    let (restart_tx, mut restart_rx) = mpsc::unbounded_channel::<String>();

    let state = Arc::new(Mutex::new(OrchState {
        bots,
        workers: HashMap::new(),
        bot_bin,
        bots_path,
        shutting_down: false,
        items_buying: json!([]),
        listings,
        clan_owners,
        clan_setup,
        last_catalog: Vec::new(),
        clan_mates: 1,
        clan_treasury: None,
        clan_treasury_at: None,
    }));

    tokio::spawn(go_ws_loop(state.clone(), go_tx.clone(), go_rx, ws_url));

    let st_r = state.clone();
    let go_r = go_tx.clone();
    let restart_tx_loop = restart_tx.clone();
    tokio::spawn(async move {
        while let Some(name) = restart_rx.recv().await {
            sleep(Duration::from_secs(15)).await;
            info!("рестарт {name}");
            if let Err(e) =
                spawn_worker(st_r.clone(), go_r.clone(), restart_tx_loop.clone(), name.clone())
                    .await
            {
                error!("restart {name}: {e:#}");
            }
        }
    });

    sleep(Duration::from_secs(2)).await;
    start_all(state.clone(), go_tx.clone(), restart_tx.clone()).await;

    let st2 = state.clone();
    let go2 = go_tx.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(30)).await;
            let _ = push_presence(&st2, &go2).await;
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("ctrl-c → shutdown");
    {
        let mut g = state.lock().await;
        g.shutting_down = true;
        for (name, _wh) in g.workers.drain() {
            info!("stop {name}");
        }
    }
    Ok(())
}

fn load_bots(bots_path: &Path, ip_path: &Path) -> Result<HashMap<String, BotRuntime>> {
    let raw = std::fs::read_to_string(bots_path)?;
    let list: Vec<BotJson> = serde_json::from_str(&raw)?;
    let ip_map: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(ip_path)?)?;
    let remembered = load_remembered_bans(bots_path);
    let mut out = HashMap::new();
    for b in list {
        let proxy = ip_map
            .get(&b.ip)
            .cloned()
            .with_context(|| format!("нет proxy для ip-слота {}", b.ip))?;
        let key = b.username.to_lowercase();
        let remembered_ban = remembered.get(&key).cloned();
        let banned = remembered_ban.is_some();
        out.insert(
            b.username.clone(),
            BotRuntime {
                cfg: b,
                proxy,
                success: false,
                on_anarchy: false,
                banned,
                manual_stop: banned,
                presence_inactive: false,
                unstable_until: None,
                pricing_edges: std::collections::VecDeque::new(),
                last_pricing_ok: false,
                items: json!({}),
                inventory: json!([]),
                balance: None,
                balance_at: None,
                banned_at: remembered_ban.as_ref().and_then(|v| {
                    v.get("banned_at")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                }),
                banned_reason: remembered_ban.as_ref().and_then(|v| {
                    v.get("reason")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                }),
                auth_fault: None,
                auth_fault_at: None,
                auth_fault_reason: None,
            },
        );
    }
    info!(
        "bots.json: {} ботов (бан в памяти: {})",
        out.len(),
        out.values().filter(|b| b.banned).count()
    );
    Ok(out)
}

fn bans_memory_path(bots_path: &Path) -> PathBuf {
    bots_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("banned-memory.json")
}

fn load_remembered_bans(bots_path: &Path) -> HashMap<String, Value> {
    let path = bans_memory_path(bots_path);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, Value>>(&raw) else {
        return HashMap::new();
    };
    map
}

fn remember_ban(bots_path: &Path, username: &str, reason: Option<&str>, banned_at: Option<&str>) {
    let path = bans_memory_path(bots_path);
    let mut map = load_remembered_bans(bots_path);
    map.insert(
        username.to_lowercase(),
        json!({
            "username": username,
            "reason": reason.unwrap_or(""),
            "banned_at": banned_at.unwrap_or(""),
        }),
    );
    if let Ok(raw) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, raw);
    }
}

async fn start_all(state: Arc<Mutex<OrchState>>, go_tx: GoTx, restart_tx: RestartTx) {
    let names: Vec<String> = {
        let g = state.lock().await;
        g.bots.keys().cloned().collect()
    };
    for (i, name) in names.into_iter().enumerate() {
        if i > 0 {
            sleep(Duration::from_secs(8)).await;
        }
        if let Err(e) =
            spawn_worker(state.clone(), go_tx.clone(), restart_tx.clone(), name.clone()).await
        {
            error!("spawn {name}: {e:#}");
        }
    }
}

async fn spawn_worker(
    state: Arc<Mutex<OrchState>>,
    go_tx: GoTx,
    restart_tx: RestartTx,
    username: String,
) -> Result<()> {
    let (boot, bot_bin) = {
        let mut g = state.lock().await;
        if g.shutting_down {
            return Ok(());
        }
        if let Some(old) = g.workers.remove(&username) {
            let _ = old.child;
        }
        let (banned, manual_stop) = {
            let rt = g.bots.get(&username).context("unknown bot")?;
            (rt.banned, rt.manual_stop)
        };
        if banned || manual_stop {
            warn!("skip {username}");
            return Ok(());
        }
        let mates = g.bots.len().max(1) as u32;
        g.clan_mates = mates;
        let boot = {
            let anarchy = g.bots.get(&username).context("unknown bot")?.cfg.anarchy;
            let expected = g.clan_owners.get(&anarchy).cloned();
            let rt = g.bots.get_mut(&username).context("unknown bot")?;
            rt.success = false;
            BotBoot {
                username: rt.cfg.username.clone(),
                password: rt.cfg.password.clone(),
                anarchy: rt.cfg.anarchy,
                item: rt.cfg.item.clone(),
                go_type: rt.cfg.go_type.clone(),
                clan_mates: mates,
                proxy: rt.proxy.clone(),
                server: "mc.funtime.su".into(),
                expected_clan_owner: expected,
            }
        };
        (boot, g.bot_bin.clone())
    };

    if !bot_bin.exists() {
        bail!("нет бинаря {} — сначала: cargo build -p bot", bot_bin.display());
    }

    pretty_log::line(&boot.username, LineKind::Info, "spawn");
    let mut child = Command::new(&bot_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bot_bin.display()))?;

    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    let boot_line = serde_json::to_string(&boot)? + "\n";
    stdin.write_all(boot_line.as_bytes()).await?;
    // listing state с диска
    {
        let st = {
            let mut g = state.lock().await;
            g.listings.export_for(&boot.username)
        };
        let cmd = OrchCmd::ListingState { data: st };
        let line = serde_json::to_string(&cmd)? + "\n";
        stdin.write_all(line.as_bytes()).await?;
    }
    stdin.flush().await?;

    {
        let mut g = state.lock().await;
        g.workers.insert(
            username.clone(),
            WorkerHandle { child, stdin },
        );
    }

    let st = state.clone();
    let go = go_tx.clone();
    let restart = restart_tx.clone();
    let uname = username.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Err(e) = handle_worker_line(&st, &go, &uname, &line).await {
                warn!("{uname} ipc: {e:#}");
            }
        }
        drop(lines);
        let code = {
            let mut g = st.lock().await;
            if let Some(mut wh) = g.workers.remove(&uname) {
                wh.child.try_wait().ok().flatten().and_then(|s| s.code())
            } else {
                None
            }
        };
        pretty_log::line(&uname, LineKind::Warn, &format!("exit code={code:?}"));
        let should_restart = {
            let mut g = st.lock().await;
            let shutting = g.shutting_down;
            if let Some(rt) = g.bots.get_mut(&uname) {
                rt.success = false;
                rt.on_anarchy = false;
                note_pricing_edge(rt, false);
                rt.items = json!({});
                rt.inventory = json!([]);
                if let Some(kind) = auth_fault_kind_from_exit(code) {
                    let now = chrono_like_now();
                    rt.auth_fault = Some(kind.to_string());
                    rt.auth_fault_at = Some(now);
                    if rt.auth_fault_reason.is_none() {
                        rt.auth_fault_reason = Some(format!("exit {code:?}"));
                    }
                    if kind == "bad_password" {
                        rt.manual_stop = true;
                    }
                }
                let block = rt.manual_stop
                    || rt.banned
                    || rt.auth_fault.as_deref() == Some("bad_password");
                !shutting && !block
            } else {
                false
            }
        };
        let _ = push_presence(&st, &go).await;
        if should_restart {
            pretty_log::line(&uname, LineKind::Info, "очередь рестарта");
            let _ = restart.send(uname);
        }
    });

    Ok(())
}

async fn handle_worker_line(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    username: &str,
    line: &str,
) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    // string alert
    if line.starts_with('"') {
        pretty_log::line(username, LineKind::Info, line.trim_matches('"'));
        return Ok(());
    }
    let msg: WorkerMsg = serde_json::from_str(line)?;
    match msg.name.as_str() {
        "success" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.success = true;
                rt.on_anarchy = true;
                rt.auth_fault = None;
                rt.auth_fault_at = None;
                rt.auth_fault_reason = None;
                note_pricing_edge(rt, true);
            }
            pretty_log::line(username, LineKind::Ok, "на анархии → success");
            drop(g);
            let _ = push_presence(state, go_tx).await;
            let _ = send_fleet(state, go_tx).await;
            let _ = go_tx.send(json!({ "action": "info" }));
        }
        "hub" => {
            let why = msg.reason.as_deref().unwrap_or("?");
            pretty_log::line(username, LineKind::Warn, &format!("хаб / не на анке: {why}"));
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.on_anarchy = false;
                note_pricing_edge(rt, false);
            }
            drop(g);
            let _ = push_presence(state, go_tx).await;
        }
        "items" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                if let Some(items) = msg.items {
                    rt.items = items;
                }
            }
            drop(g);
            // витрина АХ сменилась — сразу в Go (inventory догонит 30с тикер)
            let _ = push_presence(state, go_tx).await;
        }
        "inventory" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                if let Some(data) = msg.data {
                    rt.inventory = data;
                }
            }
        }
        "treasury_empty" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.presence_inactive = true;
            }
            drop(g);
            let _ = push_presence(state, go_tx).await;
        }
        "treasury_ok" => {
            let mut g = state.lock().await;
            if let Some(rt) = g.bots.get_mut(username) {
                rt.presence_inactive = false;
            }
            drop(g);
            let _ = push_presence(state, go_tx).await;
        }
        "kicked" | "banned" => {
            let kind = if msg.name == "banned" { "бан" } else { "кик" };
            pretty_log::line(
                username,
                LineKind::Err,
                &format!("{kind}: {}", msg.reason.as_deref().unwrap_or("?")),
            );
            if msg.name == "banned" {
                let (anarchy, bots_path, banned_at, reason) = {
                    let mut g = state.lock().await;
                    let bots_path = g.bots_path.clone();
                    if let Some(rt) = g.bots.get_mut(username) {
                        rt.banned = true;
                        rt.manual_stop = true;
                        rt.banned_at = Some(chrono_like_now());
                        rt.banned_reason = msg.reason.clone();
                        (
                            Some(rt.cfg.anarchy),
                            bots_path,
                            rt.banned_at.clone(),
                            rt.banned_reason.clone(),
                        )
                    } else {
                        (None, bots_path, None, None)
                    }
                };
                remember_ban(
                    &bots_path,
                    username,
                    reason.as_deref(),
                    banned_at.as_deref(),
                );
                if let Some(an) = anarchy {
                    maybe_notify_owner_banned(state, username, an).await?;
                }
            }
            let _ = push_presence(state, go_tx).await;
            let _ = send_fleet(state, go_tx).await;
        }
        "buy" | "sell" | "try-sell" => {
            forward_trade(go_tx, &msg)?;
        }
        "set_min_price" => {
            let price = msg.price.map(|p| p.round() as i64);
            send_go(
                go_tx,
                json!({
                    "action": "set_min_price",
                    "type": msg.id,
                    "price": price,
                }),
            )?;
        }
        "buying" => {
            handle_buying_claim(state, go_tx, username, &msg).await?;
        }
        "listing" => {
            let data = msg.data.clone().unwrap_or(json!({}));
            let result = {
                let mut g = state.lock().await;
                g.listings.handle(username, &data)
            };
            let cmd = OrchCmd::ListingRes {
                req_id: data.get("reqId").and_then(|v| v.as_u64()).unwrap_or(0),
                result,
            };
            send_cmd_to(state, username, &cmd).await?;
        }
        "captcha" => {
            pretty_log::line(username, LineKind::Warn, "captcha");
        }
        "auth_fault" => {
            let kind = msg
                .data
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("bad_password")
                .to_string();
            {
                let mut g = state.lock().await;
                if let Some(rt) = g.bots.get_mut(username) {
                    rt.auth_fault = Some(kind.clone());
                    rt.auth_fault_at = Some(chrono_like_now());
                    rt.auth_fault_reason = msg.reason.clone();
                    if kind == "bad_password" {
                        rt.manual_stop = true;
                    }
                }
            }
            pretty_log::line(
                username,
                LineKind::Err,
                &format!("auth_fault {kind}"),
            );
            let _ = push_presence(state, go_tx).await;
        }
        "funauth_bind" => {
            pretty_log::line(username, LineKind::Warn, "funauth bind → Go + рестарт");
            let (anarchy, password) = {
                let g = state.lock().await;
                g.bots
                    .get(username)
                    .map(|rt| (rt.cfg.anarchy, rt.cfg.password.clone()))
                    .unwrap_or((0, String::new()))
            };
            if password.is_empty() {
                warn!("funauth_bind {username}: нет пароля в bots.json");
            } else {
                // Как JS requestFunauthBind — очередь TG-привязки на Go.
                send_go(
                    go_tx,
                    json!({
                        "action": "funauth_bind",
                        "nick": username,
                        "password": password,
                        "anarchy": anarchy,
                    }),
                )?;
            }
            let mut g = state.lock().await;
            if let Some(wh) = g.workers.get_mut(username) {
                let _ = wh.child.start_kill();
            }
        }
        "funauth_2fa" => {
            pretty_log::line(username, LineKind::Warn, "funauth 2fa → рестарт воркера");
            let anarchy = {
                let g = state.lock().await;
                g.bots.get(username).map(|rt| rt.cfg.anarchy)
            };
            // Как JS requestFunauthTwoFa — очередь 2fa на Go.
            send_go(
                go_tx,
                json!({
                    "action": "funauth_2fa",
                    "nick": username,
                    "anarchy": anarchy,
                }),
            )?;
            // Поясной ремень: если exit воркера завис — убьём сами (рестарт по exit-handler).
            let mut g = state.lock().await;
            if let Some(wh) = g.workers.get_mut(username) {
                let _ = wh.child.start_kill();
            }
        }
        "funauth_verified" => {
            let anarchy = {
                let g = state.lock().await;
                g.bots.get(username).map(|rt| rt.cfg.anarchy)
            };
            send_go(
                go_tx,
                json!({
                    "action": "funauth_verified",
                    "nick": username,
                    "anarchy": anarchy,
                }),
            )?;
        }
        "balance" => {
            if let Some(bal) = msg.balance {
                pretty_log::line(
                    username,
                    LineKind::Ok,
                    &format!("баланс {}", format_money(bal)),
                );
                {
                    let mut g = state.lock().await;
                    if let Some(rt) = g.bots.get_mut(username) {
                        rt.balance = Some(bal);
                        rt.balance_at = Some(std::time::Instant::now());
                    }
                }
                push_capital_plan_to_bots(state).await?;
            }
        }
        "treasury" => {
            if let Some(bal) = msg.balance {
                {
                    let mut g = state.lock().await;
                    g.clan_treasury = Some(bal);
                    g.clan_treasury_at = Some(std::time::Instant::now());
                }
                pretty_log::line(
                    username,
                    LineKind::Info,
                    &format!("казна {}", format_money(bal)),
                );
                push_capital_plan_to_bots(state).await?;
            }
        }
        "ah_lots" => {
            if let Some(lots) = &msg.lots {
                // Go ahBookWire.price — int; serde_json f64 даёт `1000000.0` → unmarshal fail.
                let lots = coerce_ah_lots_prices(lots);
                let n = lots.as_array().map(|a| a.len()).unwrap_or(0);
                if n > 0 {
                    tracing::info!("ah_lots → Go · {n} лотов");
                }
                send_go(
                    go_tx,
                    json!({ "action": "ah_lots", "lots": lots }),
                )?;
            }
        }
        "staff_check" => {
            let anarchy = {
                let from_data = msg
                    .data
                    .as_ref()
                    .and_then(|d| d.get("anarchy"))
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16);
                if from_data.is_some() {
                    from_data
                } else {
                    let g = state.lock().await;
                    g.bots.get(username).map(|rt| rt.cfg.anarchy)
                }
            };
            pretty_log::line(username, LineKind::Warn, "проверка читов → Go");
            send_go(
                go_tx,
                json!({
                    "action": "staff_check",
                    "username": username,
                    "anarchy": anarchy,
                    "reason": msg.reason.as_deref().unwrap_or(""),
                }),
            )?;
        }
        "clan_setup" => {
            pretty_log::line(
                username,
                LineKind::Warn,
                &format!(
                    "clan_setup: {}",
                    msg.reason.as_deref().unwrap_or("?")
                ),
            );
            let anarchy = msg
                .data
                .as_ref()
                .and_then(|d| d.get("anarchy"))
                .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                .unwrap_or(0) as u16;
            let reason = msg.reason.clone().unwrap_or_else(|| "clan".into());
            let (gate, local) = {
                let g = state.lock().await;
                (g.clan_setup.clone(), g.clan_setup.local_mode())
            };
            if local {
                gate.request(go_tx, anarchy, &reason, username, false).await;
            } else {
                send_go(
                    go_tx,
                    json!({
                        "action": "clan_needed",
                        "username": username,
                        "anarchy": anarchy,
                        "reason": reason,
                    }),
                )?;
            }
        }
        other => {
            pretty_log::line(username, LineKind::Info, &format!("msg={other}"));
        }
    }
    Ok(())
}

fn forward_trade(go_tx: &GoTx, msg: &WorkerMsg) -> Result<()> {
    let price = msg.price.map(|p| p.round() as i64);
    let mut payload = json!({
        "action": msg.name,
        "type": msg.id,
        "price": price,
    });
    if let Some(data) = &msg.data {
        if let Some(obj) = payload.as_object_mut() {
            if let Some(ench) = data.get("enchants") {
                obj.insert("enchants".into(), ench.clone());
            }
            if let Some(dur) = data.get("durability") {
                obj.insert("durability".into(), dur.clone());
            }
        }
    }
    send_go(go_tx, payload)
}

async fn handle_buying_claim(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    username: &str,
    msg: &WorkerMsg,
) -> Result<()> {
    let uuid = msg
        .data
        .as_ref()
        .and_then(|d| d.get("uuid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(uuid) = uuid else {
        return Ok(());
    };
    let updated = {
        let mut g = state.lock().await;
        let mut list = g
            .items_buying
            .as_array()
            .cloned()
            .unwrap_or_default();
        list.retain(|raw| {
            let u = match raw {
                Value::String(s) => s.as_str(),
                Value::Object(o) => o.get("uuid").and_then(|v| v.as_str()).unwrap_or(""),
                _ => "",
            };
            u != uuid
        });
        list.push(json!({ "uuid": uuid, "username": username }));
        g.items_buying = Value::Array(list.clone());
        g.items_buying.clone()
    };
    let cmd = OrchCmd::ItemsBuying {
        data: updated.clone(),
    };
    broadcast_cmd(state, &cmd).await?;
    send_go(
        go_tx,
        json!({ "action": "add", "json_data": uuid }),
    )?;
    Ok(())
}

fn coerce_ah_lots_prices(lots: &Value) -> Value {
    let Some(arr) = lots.as_array() else {
        return lots.clone();
    };
    let out: Vec<Value> = arr
        .iter()
        .map(|lot| {
            let mut o = match lot.as_object() {
                Some(m) => m.clone(),
                None => return lot.clone(),
            };
            if let Some(p) = o.get("price") {
                let n = p
                    .as_i64()
                    .or_else(|| p.as_u64().map(|u| u as i64))
                    .or_else(|| p.as_f64().map(|f| f.round() as i64));
                if let Some(n) = n {
                    o.insert("price".into(), json!(n));
                }
            }
            Value::Object(o)
        })
        .collect();
    Value::Array(out)
}

fn send_go(go_tx: &GoTx, v: Value) -> Result<()> {
    let _ = go_tx.send(v);
    Ok(())
}

async fn send_fleet(state: &Arc<Mutex<OrchState>>, go_tx: &GoTx) -> Result<()> {
    let (types, roster) = {
        let g = state.lock().await;
        let mut types = Vec::new();
        let mut roster = Vec::new();
        for rt in g.bots.values() {
            if !types.contains(&rt.cfg.go_type) {
                types.push(rt.cfg.go_type.clone());
            }
            roster.push(json!({
                "username": rt.cfg.username,
                "anarchy": rt.cfg.anarchy,
                "go_type": rt.cfg.go_type,
                "success": rt.success,
                "banned": rt.banned,
            }));
        }
        (types, roster)
    };
    send_go(
        go_tx,
        json!({ "action": "fleet", "types": types, "bots": roster }),
    )
}

async fn push_presence(state: &Arc<Mutex<OrchState>>, go_tx: &GoTx) -> Result<()> {
    let payload = {
        let mut g = state.lock().await;
        // Go ждёт flat map itemId→count (как JS collectPresenceItemCounts), НЕ per-user.
        let mut items: HashMap<String, u32> = HashMap::new();
        let mut inventory: HashMap<String, u32> = HashMap::new();
        let mut active_types = Vec::new();
        let mut bots_per_type: HashMap<String, u32> = HashMap::new();
        let mut treasury_empty = Vec::new();
        let mut banned = Vec::new();
        let mut auth_faults = Vec::new();
        let mut roster = Vec::new();
        let mut pricing_n = 0u32;
        let mut live_n = 0u32;
        let mut hub_n = 0u32;
        let mut unstable_n = 0u32;

        let worker_alive: HashMap<String, bool> = g
            .bots
            .keys()
            .map(|u| (u.clone(), g.workers.contains_key(u)))
            .collect();

        for rt in g.bots.values_mut() {
            let live = worker_alive.get(&rt.cfg.username).copied().unwrap_or(false) && rt.success;
            if live {
                live_n += 1;
            }
            let has_stock = bot_has_pricing_stock(rt);
            let unstable = rt
                .unstable_until
                .map(|t| t > std::time::Instant::now())
                .unwrap_or(false);
            if unstable {
                unstable_n += 1;
            }
            if live && !rt.on_anarchy {
                hub_n += 1;
            }

            // Сток: на анке или (жив, но в хабе/казне) с остатком — чтобы held не обнулялся вскидкой.
            let include_stock = live && (rt.on_anarchy || has_stock);
            // Pricing-бот: только на анке, не флап, казна ок (или есть сток как treasury_empty).
            let count_as_bot = live
                && rt.on_anarchy
                && !unstable
                && (!rt.presence_inactive || has_stock);

            if include_stock {
                accumulate_presence_counts(&rt.items, &mut items);
                accumulate_presence_counts(&rt.inventory, &mut inventory);
            }
            if count_as_bot {
                pricing_n += 1;
                if !active_types.contains(&rt.cfg.go_type) {
                    active_types.push(rt.cfg.go_type.clone());
                }
                *bots_per_type.entry(rt.cfg.go_type.clone()).or_insert(0) += 1;
            }
            if live && rt.presence_inactive {
                treasury_empty.push(rt.cfg.go_type.clone());
            }
            if rt.banned {
                banned.push(json!({
                    "username": rt.cfg.username,
                    "anarchy": rt.cfg.anarchy,
                    "go_type": rt.cfg.go_type,
                    "banned_at": rt.banned_at,
                    "reason": rt.banned_reason,
                    "kind": "ban",
                    "ip": proxy_host_hint(&rt.proxy),
                }));
            }
            if let Some(kind) = &rt.auth_fault {
                auth_faults.push(json!({
                    "username": rt.cfg.username,
                    "anarchy": rt.cfg.anarchy,
                    "go_type": rt.cfg.go_type,
                    "kind": kind,
                    "at": rt.auth_fault_at,
                    "reason": rt.auth_fault_reason,
                    "ip": proxy_host_hint(&rt.proxy),
                }));
            }
            roster.push(json!({
                "username": rt.cfg.username,
                "anarchy": rt.cfg.anarchy,
                "go_type": rt.cfg.go_type,
                "success": rt.success,
                "on_anarchy": rt.on_anarchy,
                "banned": rt.banned,
                "unstable": unstable,
                "live": live,
            }));
        }

        let clan_owners_presence: Vec<Value> = g
            .clan_owners
            .iter()
            .map(|(an, nick)| json!({ "anarchy": an, "username": nick }))
            .collect();

        if active_types.is_empty() {
            warn!(
                "presence → active_types=[] live={live_n} hub={hub_n} unstable={unstable_n} pricing={pricing_n} (Go на паузе)"
            );
        } else {
            info!(
                "presence → active_types={active_types:?} pricing={pricing_n} live={live_n} hub={hub_n} unstable={unstable_n}"
            );
        }

        json!({
            "action": "presence",
            "items": items,
            "inventory": inventory,
            "active_types": active_types,
            "bots_per_type": bots_per_type,
            "treasury_empty_types": treasury_empty,
            "banned": banned,
            "auth_faults": auth_faults,
            "clan_owners": clan_owners_presence,
            "bots": roster,
            "fleet_stats": {
                "live": live_n,
                "pricing": pricing_n,
                "hub": hub_n,
                "unstable": unstable_n,
            },
        })
    };
    send_go(go_tx, payload)
}

/// ≥3 полных флаппа (6 рёбер) за 15м → 5м не в pricing.
fn note_pricing_edge(rt: &mut BotRuntime, ok: bool) {
    if rt.last_pricing_ok == ok {
        return;
    }
    rt.last_pricing_ok = ok;
    let now = std::time::Instant::now();
    rt.pricing_edges.push_back(now);
    let window = std::time::Duration::from_secs(15 * 60);
    while rt
        .pricing_edges
        .front()
        .map(|t| now.duration_since(*t) > window)
        .unwrap_or(false)
    {
        rt.pricing_edges.pop_front();
    }
    // 6 рёбер ≈ 3 цикла down→up или up→down
    if rt.pricing_edges.len() >= 6 {
        rt.unstable_until = Some(now + std::time::Duration::from_secs(5 * 60));
        pretty_log::line(
            &rt.cfg.username,
            LineKind::Warn,
            &format!(
                "unstable: {} переключений за 15м → 5м вне pricing",
                rt.pricing_edges.len()
            ),
        );
    }
}

/// Склеить в Go-формат `map[string]int`: массив id / {id:count} / [{id|go_type,count}].
fn accumulate_presence_counts(raw: &Value, out: &mut HashMap<String, u32>) {
    match raw {
        Value::Array(arr) => {
            for el in arr {
                match el {
                    Value::String(id) if !id.is_empty() => {
                        *out.entry(id.clone()).or_insert(0) += 1;
                    }
                    Value::Object(o) => {
                        if let Some(id) = o
                            .get("id")
                            .or_else(|| o.get("go_type"))
                            .and_then(|v| v.as_str())
                        {
                            let n = o.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                            *out.entry(id.to_string()).or_insert(0) += n.max(1);
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(n) = v.as_u64() {
                    if n > 0 {
                        *out.entry(k.clone()).or_insert(0) += n as u32;
                    }
                } else if v.as_str().is_some() {
                    *out.entry(k.clone()).or_insert(0) += 1;
                }
            }
        }
        _ => {}
    }
}

/// Есть ли сырой товар у бота (до агрегации).
fn bot_has_pricing_stock(rt: &BotRuntime) -> bool {
    match &rt.items {
        Value::Array(a) if !a.is_empty() => return true,
        Value::Object(o) if o.values().any(|v| v.as_u64().unwrap_or(0) > 0) => return true,
        _ => {}
    }
    match &rt.inventory {
        Value::Array(a) if !a.is_empty() => return true,
        Value::Object(o) if !o.is_empty() => return true,
        _ => {}
    }
    false
}

fn load_clan_owners(farm_root: &Path) -> HashMap<u16, String> {
    let path = farm_root.join("clan-owners.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        warn!("нет {}", path.display());
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    let Some(obj) = v.as_object() else {
        return out;
    };
    for (k, val) in obj {
        let Ok(an) = k.parse::<u16>() else {
            continue;
        };
        if let Some(nick) = val.get("username").and_then(|x| x.as_str()) {
            out.insert(an, nick.to_string());
        }
    }
    info!("clan-owners → {} анк", out.len());
    out
}

async fn maybe_notify_owner_banned(
    state: &Arc<Mutex<OrchState>>,
    banned_nick: &str,
    anarchy: u16,
) -> Result<()> {
    let owner = {
        let g = state.lock().await;
        g.clan_owners.get(&anarchy).cloned()
    };
    let Some(owner) = owner else {
        return Ok(());
    };
    if !owner.eq_ignore_ascii_case(banned_nick) {
        return Ok(());
    }
    info!("овнер {owner} забанен на an{anarchy} → owner_banned воркерам");
    let cmd = OrchCmd::OwnerBanned {
        owner: Some(owner.clone()),
    };
    let nicks: Vec<String> = {
        let g = state.lock().await;
        g.bots
            .iter()
            .filter(|(_, rt)| rt.cfg.anarchy == anarchy && !rt.banned)
            .map(|(n, _)| n.clone())
            .collect()
    };
    for nick in nicks {
        let _ = send_cmd_to(state, &nick, &cmd).await;
    }
    Ok(())
}

async fn go_ws_loop(
    state: Arc<Mutex<OrchState>>,
    go_tx: GoTx,
    mut go_rx: mpsc::UnboundedReceiver<Value>,
    url: String,
) {
    loop {
        info!("Go WS → {url}");
        match connect_async(&url).await {
            Ok((ws, _)) => {
                let (mut sink, mut stream) = ws.split();
                info!("Go WS open");
                let _ = send_fleet(&state, &go_tx).await;
                let _ = push_presence(&state, &go_tx).await;
                // запросить цены (как 502b requestInfo)
                let _ = go_tx.send(json!({ "action": "info" }));

                loop {
                    tokio::select! {
                        msg = stream.next() => {
                            match msg {
                                Some(Ok(Message::Text(t))) => {
                                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                                        if let Err(e) = on_go_message(&state, &go_tx, v).await {
                                            warn!("go msg: {e:#}");
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                Some(Err(e)) => {
                                    error!("go ws: {e}");
                                    break;
                                }
                                _ => {}
                            }
                        }
                        out = go_rx.recv() => {
                            match out {
                                Some(v) => {
                                    if sink.send(Message::Text(v.to_string().into())).await.is_err() {
                                        break;
                                    }
                                }
                                None => return,
                            }
                        }
                    }
                }
            }
            Err(e) => error!("go connect: {e}"),
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn on_go_message(
    state: &Arc<Mutex<OrchState>>,
    go_tx: &GoTx,
    v: Value,
) -> Result<()> {
    // Go шлёт { prices: {id: sell}, catalog: [...] } — не готовый data[].
    if v.get("prices").is_some() {
        let merged = merge_go_catalog_prices(&v);
        info!("Go prices → {} позиций в каталоге", merged.len());
        if merged.is_empty() {
            return Ok(());
        }
        // per-bot: только его go_type (как applyPricesToBots)
        let mut g = state.lock().await;
        g.last_catalog = merged.clone();
        let targets: Vec<(String, String)> = g
            .workers
            .keys()
            .map(|name| {
                let gt = g
                    .bots
                    .get(name)
                    .map(|b| b.cfg.go_type.clone())
                    .unwrap_or_default();
                (name.clone(), gt)
            })
            .collect();
        for (name, go_type) in targets {
            let bot_items: Vec<Value> = merged
                .iter()
                .filter(|it| {
                    let ty = it.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    catalog_type_matches_go(ty, &go_type, name)
                })
                .cloned()
                .collect();
            let cmd = OrchCmd::Update {
                data: bot_items,
                all: merged.clone(),
            };
            let line = serde_json::to_string(&cmd)? + "\n";
            if let Some(wh) = g.workers.get_mut(&name) {
                if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
                    warn!("stdin {name}: {e}");
                } else {
                    tracing::debug!("→ {name} prices ({go_type})");
                }
            }
        }
    } else if v.get("action").and_then(|a| a.as_str()) == Some("json_update") {
        if let Some(data) = v.get("data") {
            let merged = {
                let mut g = state.lock().await;
                let merged = merge_go_json_update(&g.items_buying, data);
                g.items_buying = merged.clone();
                merged
            };
            let cmd = OrchCmd::ItemsBuying { data: merged };
            broadcast_cmd(state, &cmd).await?;
        }
    } else if v.get("action").and_then(|a| a.as_str()) == Some("run_clan_setup") {
        let anarchy = v
            .get("anarchy")
            .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|i| i as u64)))
            .unwrap_or(0) as u16;
        let reason = v
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("go_dispatch");
        let nicks = v
            .get("nicks")
            .and_then(|n| n.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        info!("Go run_clan_setup an{anarchy} nicks={nicks} reason={reason}");
        let gate = {
            let g = state.lock().await;
            g.clan_setup.clone()
        };
        gate.request(go_tx, anarchy, reason, "go", true).await;
    } else if v.get("action").and_then(|a| a.as_str()) == Some("capital_plan")
        || v.get("type").and_then(|a| a.as_str()) == Some("capital_plan")
    {
        let keep = v
            .get("keepSum")
            .or_else(|| v.get("keep_sum"))
            .and_then(|x| x.as_f64());
        let max_buy = v
            .get("maxBuyPrice")
            .or_else(|| v.get("max_buy_price"))
            .and_then(|x| x.as_f64());
        let cmd = OrchCmd::CapitalPlan {
            keep_sum: keep,
            max_buy_price: max_buy,
        };
        broadcast_cmd(state, &cmd).await?;
    }
    Ok(())
}

fn merge_go_catalog_prices(v: &Value) -> Vec<Value> {
    let prices = match v.get("prices").and_then(|p| p.as_object()) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let catalog = match v.get("catalog").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in catalog {
        let id = item.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let sell = prices.get(id).and_then(|p| {
            p.as_f64()
                .or_else(|| p.as_i64().map(|i| i as f64))
                .or_else(|| p.as_u64().map(|u| u as f64))
        });
        let Some(ps) = sell else { continue };
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("priceSell".into(), json!(ps));
        }
        out.push(obj);
    }
    out
}

fn catalog_type_matches_go(catalog_type: &str, go_type: &str, catalog_name: &str) -> bool {
    if go_type.is_empty() {
        return true;
    }
    if catalog_type == go_type {
        return true;
    }
    const ARMOR: &str = "netherite_armor-1.21";
    const PIECES: &[(&str, &str)] = &[
        ("netherite_helmet-1.21", "netherite_helmet"),
        ("netherite_chestplate-1.21", "netherite_chestplate"),
        ("netherite_leggings-1.21", "netherite_leggings"),
        ("netherite_boots-1.21", "netherite_boots"),
    ];
    if go_type == ARMOR {
        if PIECES.iter().any(|(t, _)| *t == catalog_type) {
            return true;
        }
        if catalog_type == ARMOR {
            return catalog_name.is_empty()
                || PIECES.iter().any(|(_, n)| *n == catalog_name);
        }
        return false;
    }
    if catalog_type == ARMOR {
        return PIECES
            .iter()
            .any(|(t, n)| *t == go_type && *n == catalog_name);
    }
    let a = catalog_type.split('-').next().unwrap_or(catalog_type);
    let b = go_type.split('-').next().unwrap_or(go_type);
    a == b
}

/// Go шлёт только UUID — сохраняем username из локального списка (как mergeGoJsonUpdate).
fn merge_go_json_update(local: &Value, go_uuids: &Value) -> Value {
    let go_arr = match go_uuids.as_array() {
        Some(a) => a,
        None => {
            return if local.is_array() {
                local.clone()
            } else {
                json!([])
            };
        }
    };
    let prev = local.as_array().cloned().unwrap_or_default();
    let mut owner_by_uuid: HashMap<String, String> = HashMap::new();
    for raw in &prev {
        if let Some((u, Some(owner))) = normalize_buying_entry(raw) {
            owner_by_uuid.insert(u, owner);
        }
    }
    let mut in_go = std::collections::HashSet::new();
    let mut out = Vec::new();
    for uuid_v in go_arr {
        let u = match uuid_v {
            Value::String(s) => s.clone(),
            Value::Object(o) => o
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => continue,
        };
        if u.is_empty() {
            continue;
        }
        in_go.insert(u.clone());
        if let Some(owner) = owner_by_uuid.get(&u) {
            out.push(json!({ "uuid": u, "username": owner }));
        } else {
            out.push(json!({ "uuid": u, "username": Value::Null }));
        }
    }
    for raw in &prev {
        if let Some((u, Some(owner))) = normalize_buying_entry(raw) {
            if !in_go.contains(&u) {
                out.push(json!({ "uuid": u, "username": owner }));
            }
        }
    }
    Value::Array(out)
}

fn normalize_buying_entry(raw: &Value) -> Option<(String, Option<String>)> {
    match raw {
        Value::String(s) if !s.is_empty() => Some((s.clone(), None)),
        Value::Object(o) => {
            let u = o.get("uuid").and_then(|v| v.as_str())?.to_string();
            if u.is_empty() {
                return None;
            }
            let owner = o
                .get("username")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some((u, owner))
        }
        _ => None,
    }
}

fn top_two_sell_keep(catalog: &[Value]) -> Option<f64> {
    let mut prices: Vec<f64> = Vec::new();
    for entry in catalog {
        if let Some(p) = entry.get("priceSell").and_then(|v| v.as_f64()) {
            if p.is_finite() && p > 0.0 {
                prices.push(p);
            }
        }
    }
    if prices.is_empty() {
        return None;
    }
    prices.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    if prices.len() == 1 {
        return Some(prices[0].floor());
    }
    Some((prices[0] + prices[1]).floor())
}

fn top_sell_price(catalog: &[Value]) -> Option<f64> {
    let mut best = 0.0;
    for entry in catalog {
        if let Some(p) = entry.get("priceSell").and_then(|v| v.as_f64()) {
            if p.is_finite() && p > best {
                best = p;
            }
        }
    }
    if best > 0.0 {
        Some(best)
    } else {
        None
    }
}

async fn push_capital_plan_to_bots(state: &Arc<Mutex<OrchState>>) -> Result<()> {
    let plans: Vec<(String, Option<f64>, Option<f64>)> = {
        let g = state.lock().await;
        let mut out = Vec::new();
        for name in g.workers.keys() {
            let go_type = g
                .bots
                .get(name)
                .map(|b| b.cfg.go_type.clone())
                .unwrap_or_default();
            let bot_cat: Vec<Value> = g
                .last_catalog
                .iter()
                .filter(|it| {
                    let ty = it.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let n = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    catalog_type_matches_go(ty, &go_type, n)
                })
                .cloned()
                .collect();
            let keep = top_two_sell_keep(&bot_cat);
            let max_buy = top_sell_price(&bot_cat).map(|t| (t * 0.9).floor());
            out.push((name.clone(), keep, max_buy));
        }
        out
    };
    for (name, keep, max_buy) in plans {
        if let (Some(k), Some(m)) = (keep, max_buy) {
            info!(
                "capital → {} keep={} maxBuy={}",
                name,
                format_money(k),
                format_money(m)
            );
        }
        let cmd = OrchCmd::CapitalPlan {
            keep_sum: keep,
            max_buy_price: max_buy,
        };
        let line = serde_json::to_string(&cmd)? + "\n";
        let mut g = state.lock().await;
        if let Some(wh) = g.workers.get_mut(&name) {
            if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
                warn!("stdin {name}: {e}");
            }
        }
    }
    Ok(())
}

async fn broadcast_cmd(state: &Arc<Mutex<OrchState>>, cmd: &OrchCmd) -> Result<()> {
    let line = serde_json::to_string(cmd)? + "\n";
    let mut g = state.lock().await;
    for (name, wh) in g.workers.iter_mut() {
        if let Err(e) = wh.stdin.write_all(line.as_bytes()).await {
            warn!("stdin {name}: {e}");
        }
    }
    Ok(())
}

async fn send_cmd_to(state: &Arc<Mutex<OrchState>>, username: &str, cmd: &OrchCmd) -> Result<()> {
    let line = serde_json::to_string(cmd)? + "\n";
    let mut g = state.lock().await;
    if let Some(wh) = g.workers.get_mut(username) {
        wh.stdin.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn format_money(v: f64) -> String {
    let v = v.abs();
    if v >= 1_000_000.0 {
        let kk = v / 1_000_000.0;
        if (kk - kk.round()).abs() < 0.05 {
            format!("{}kk", kk.round() as i64)
        } else {
            format!("{kk:.1}kk")
        }
    } else if v >= 10_000.0 {
        format!("{}k", (v / 1000.0).round() as i64)
    } else if v >= 1000.0 {
        let k = v / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as i64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        format!("{}", v.round() as i64)
    }
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // epoch seconds — Go UI tolerates; better than empty
    format!("{secs}")
}

fn auth_fault_kind_from_exit(code: Option<i32>) -> Option<&'static str> {
    match code {
        Some(71) => Some("bad_password"),
        Some(72) => Some("proxy_error"),
        _ => None,
    }
}

fn proxy_host_hint(proxy: &str) -> String {
    if let Some(at) = proxy.rfind('@') {
        return proxy[at + 1..].to_string();
    }
    proxy
        .trim_start_matches("socks5://")
        .trim_start_matches("socks://")
        .to_string()
}
