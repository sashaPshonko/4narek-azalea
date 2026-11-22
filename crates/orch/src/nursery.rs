//! `nursery502` — питомник: прогрев новых ников без работы на АХ.
//!
//! Зачем: по банам 13.11–20.11 «4.3.1 AB» получают почти только ники младше ~3 суток. Питомник держит
//! новые ники на анархии в режиме `warmup` (anti-AFK, прогулки, мусор — см. `bot/src/warmup.rs`), а
//! когда нику исполнилось `WARMUP_DAYS` суток, он «готов» — его можно ставить на ферму вместо забаненного
//! (со своим же прокси).
//!
//! Простой и отдельный от фермы: ни казначея, ни цен, ни Go-каталога. Из общего — только расписание
//! (`schedule.json`, как у фермы v9: бот `i` идёт по слоту `i % 3`) и Go для привязки FunAuth.
//!
//! Файлы (в корне фермы, запуск из него: `./target/release/nursery502`):
//! - `bots/nursery.json` — ники питомника: `[{username, password, anarchy, target?, proxy, goType?, born_ms?}]`.
//!   `anarchy` — где греется (лучше у каждого своя, например 310–320: несколько новых ников, одинаково стоящих
//!   на одной анархии, заметнее), `target` — анархия фермы, куда пойдёт после прогрева.
//!   Перечитывается каждые 10 с: добавить/убрать ник можно без перезапуска.
//! - `nursery-state.json` — время рождения, бан, привязка (пишет сам).
//! - `nursery-status.json` — сводка для людей: возраст, готов ли, онлайн ли.
//! - `bot-slots.jsonl` — пробы раз в 30 с (как у фермы, с `"nursery": true`), для анализа банов.
//! - `bots/ban-history.json` — баны питомника дописываются с `schedule: "nursery-warmup"`.
//!
//! Переменные: `WARMUP_DAYS` (3).

#[path = "schedule.rs"]
mod schedule;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use ipc::{BotBoot, WorkerMsg};
use schedule::{now_unix_ms, Schedule, ScheduleConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const TICK: Duration = Duration::from_secs(10);
const PROBE_EVERY_MS: u64 = 30_000;
const MIN_MS: u64 = 60_000;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Deserialize)]
struct NurseryBot {
    username: String,
    password: String,
    anarchy: u16,
    proxy: String,
    #[serde(default, rename = "goType")]
    go_type: Option<String>,
    #[serde(default)]
    born_ms: Option<u64>,
    /// анархия фермы, куда ник пойдёт после прогрева (греется на `anarchy`)
    #[serde(default)]
    target: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BotRec {
    born_ms: u64,
    #[serde(default)]
    banned: bool,
    #[serde(default)]
    ban_reason: Option<String>,
    #[serde(default)]
    banned_ms: Option<u64>,
    /// "need_bind" | "2fa" | "ok"
    #[serde(default)]
    bind: Option<String>,
    #[serde(default)]
    ready_logged: bool,
    #[serde(default)]
    online_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NurseryState {
    #[serde(default)]
    bots: HashMap<String, BotRec>,
}

struct Worker {
    child: Child,
    on_anarchy: bool,
    verified_sent: bool,
}

enum Ev {
    Line(String, String),
    Exit(String),
}

struct Runtime {
    root: PathBuf,
    bot_bin: PathBuf,
    warmup_days: u64,
    state: NurseryState,
    workers: HashMap<String, Worker>,
    /// не раньше этого времени снова запускать (бэкофф после выхода/ошибок)
    retry_after: HashMap<String, u64>,
    fails: HashMap<String, u32>,
    go_tx: mpsc::UnboundedSender<Value>,
    ev_tx: mpsc::UnboundedSender<Ev>,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(p: &Path) -> T {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_json_atomic(p: &Path, v: &impl Serialize) {
    let tmp = p.with_extension("json.tmp");
    match serde_json::to_string_pretty(v) {
        Ok(s) => {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
        Err(e) => warn!("запись {}: {e}", p.display()),
    }
}

fn load_bots(root: &Path) -> Vec<NurseryBot> {
    let p = root.join("bots/nursery.json");
    match std::fs::read_to_string(&p) {
        Ok(s) => match serde_json::from_str::<Vec<NurseryBot>>(&s) {
            Ok(v) => v,
            Err(e) => {
                warn!("bots/nursery.json: {e}");
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

fn bot_bin_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("bot502")))
        .unwrap_or_else(|| PathBuf::from("target/release/bot502"))
}

impl Runtime {
    fn rec(&mut self, b: &NurseryBot, now: u64) -> &mut BotRec {
        self.state
            .bots
            .entry(b.username.clone())
            .or_insert_with(|| BotRec { born_ms: b.born_ms.unwrap_or(now), ..Default::default() })
    }

    async fn spawn(&mut self, b: &NurseryBot) -> Result<()> {
        let boot = BotBoot {
            username: b.username.clone(),
            password: b.password.clone(),
            anarchy: b.anarchy,
            item: "netherite".into(),
            go_type: b.go_type.clone().unwrap_or_else(|| "netherite-1.21".into()),
            proxy: b.proxy.clone(),
            server: "mc.funtime.su".into(),
            my_nick: None,
            owner_nick: None,
            owner_reason: None,
            treasurer_nick: None,
            warmup: true,
        };
        let mut child = Command::new(&self.bot_bin)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", self.bot_bin.display()))?;
        let mut stdin = child.stdin.take().context("stdin")?;
        stdin.write_all((serde_json::to_string(&boot)? + "\n").as_bytes()).await?;
        stdin.flush().await?;
        // stdin держим открытым внутри задачи до выхода воркера (закрытие = EOF для бота)
        let stdout = child.stdout.take().context("stdout")?;
        let name = b.username.clone();
        let tx = self.ev_tx.clone();
        tokio::spawn(async move {
            let _keep = stdin;
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(Ev::Line(name.clone(), line));
            }
            let _ = tx.send(Ev::Exit(name));
        });
        info!("{}: запуск (прогрев, an{})", b.username, b.anarchy);
        self.workers.insert(b.username.clone(), Worker { child, on_anarchy: false, verified_sent: false });
        Ok(())
    }

    fn kill(&mut self, name: &str, why: &str) {
        if let Some(w) = self.workers.get_mut(name) {
            info!("{name}: стоп ({why})");
            let _ = w.child.start_kill();
        }
    }

    fn backoff(&mut self, name: &str, ms: u64) {
        self.retry_after.insert(name.to_string(), now_unix_ms() + ms);
    }

    fn record_ban(&self, name: &str, anarchy: u16, reason: &str) {
        let p = self.root.join("bots/ban-history.json");
        let mut h: Value = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = h.as_object_mut() {
            obj.insert(
                name.to_string(),
                json!({
                    "username": name,
                    "anarchy": anarchy,
                    "banned_at": (now_unix_ms() / 1000).to_string(),
                    "reason": reason,
                    "schedule": "nursery-warmup",
                    "replaced_by": "",
                }),
            );
            write_json_atomic(&p, &h);
        }
    }

    fn on_line(&mut self, name: &str, line: &str, bots: &[NurseryBot]) {
        let Ok(msg) = serde_json::from_str::<WorkerMsg>(line) else { return };
        let Some(b) = bots.iter().find(|b| b.username == name).cloned() else { return };
        let reason = msg.reason.clone().unwrap_or_default();
        match msg.name.as_str() {
            "success" => {
                if let Some(w) = self.workers.get_mut(name) {
                    w.on_anarchy = true;
                }
                self.fails.remove(name);
                info!("{name}: на анархии");
            }
            "hub" => {
                if let Some(w) = self.workers.get_mut(name) {
                    w.on_anarchy = false;
                }
            }
            "banned" => {
                warn!("{name}: БАН — {reason}");
                let now = now_unix_ms();
                let rec = self.rec(&b, now);
                rec.banned = true;
                rec.banned_ms = Some(now);
                rec.ban_reason = Some(reason.clone());
                self.record_ban(name, b.anarchy, &reason);
                self.kill(name, "бан");
            }
            "kicked" => warn!("{name}: кик — {reason}"),
            "funauth_bind" => {
                warn!("{name}: нужна привязка FunAuth → Go");
                let _ = self.go_tx.send(json!({ "action": "funauth_bind", "nick": name, "password": b.password, "anarchy": b.anarchy }));
                self.rec(&b, now_unix_ms()).bind = Some("need_bind".into());
                self.kill(name, "привязка");
                self.backoff(name, 5 * MIN_MS);
            }
            "funauth_2fa" => {
                warn!("{name}: FunAuth 2FA → Go");
                let _ = self.go_tx.send(json!({ "action": "funauth_2fa", "nick": name, "anarchy": b.anarchy }));
                self.rec(&b, now_unix_ms()).bind = Some("2fa".into());
                self.kill(name, "2fa");
                self.backoff(name, 10 * MIN_MS);
            }
            "funauth_verified" => {
                let first = self.workers.get_mut(name).map(|w| !std::mem::replace(&mut w.verified_sent, true)).unwrap_or(false);
                if first {
                    let _ = self.go_tx.send(json!({ "action": "funauth_verified", "nick": name, "anarchy": b.anarchy }));
                }
                self.rec(&b, now_unix_ms()).bind = Some("ok".into());
            }
            "auth_fault" => {
                warn!("{name}: auth_fault — {reason}");
                self.backoff(name, 2 * MIN_MS);
            }
            "staff_check" => {
                warn!("{name}: проверка читов → Go");
                let _ = self.go_tx.send(json!({ "action": "staff_check", "username": name, "anarchy": b.anarchy, "reason": reason }));
            }
            _ => {}
        }
    }

    fn on_exit(&mut self, name: &str) {
        let was_on = self.workers.remove(name).map(|w| w.on_anarchy).unwrap_or(false);
        if self.state.bots.get(name).map(|r| r.banned).unwrap_or(false) {
            return;
        }
        // уже стоит более длинная пауза (привязка/2fa/прокси) — не трогаем
        if self.retry_after.get(name).copied().unwrap_or(0) > now_unix_ms() {
            return;
        }
        let n = if was_on { 0 } else { self.fails.get(name).copied().unwrap_or(0) + 1 };
        self.fails.insert(name.to_string(), n);
        // 30 с, 1, 2, 4 … до 15 мин, если не доходит до анархии
        let ms = (30_000u64 << n.min(5)).min(15 * MIN_MS);
        info!("{name}: вышел — повтор через {} с", ms / 1000);
        self.backoff(name, ms);
    }

    async fn tick(&mut self, bots: &[NurseryBot], sched: &Schedule, last_probe: &mut u64) {
        let now = now_unix_ms();

        // кого нет в конфиге — гасим
        let gone: Vec<String> = self.workers.keys().filter(|n| !bots.iter().any(|b| &b.username == *n)).cloned().collect();
        for n in gone {
            self.kill(&n, "убран из nursery.json");
        }

        for (i, b) in bots.iter().enumerate() {
            let rec = self.rec(b, now).clone();
            let running = self.workers.contains_key(&b.username);
            if rec.banned {
                if running {
                    self.kill(&b.username, "бан");
                }
                continue;
            }
            let active = sched.is_active(i % 3, now);
            let may = self.retry_after.get(&b.username).copied().unwrap_or(0) <= now;
            if active && !running && may {
                if let Err(e) = self.spawn(b).await {
                    warn!("{}: запуск не удался: {e:#}", b.username);
                    self.backoff(&b.username, 2 * MIN_MS);
                }
            } else if !active && running {
                self.kill(&b.username, "перерыв по расписанию");
            }
            let age_days = now.saturating_sub(rec.born_ms) / DAY_MS;
            if age_days >= self.warmup_days && !rec.ready_logged {
                info!(
                    "{}: ГОТОВ — прогрев {} сут (грелся на an{}, прокси свой) — можно ставить на ферму{}",
                    b.username,
                    age_days,
                    b.anarchy,
                    b.target.map(|t| format!(" an{t}")).unwrap_or_default()
                );
                self.rec(b, now).ready_logged = true;
            }
        }

        if now.saturating_sub(*last_probe) >= PROBE_EVERY_MS {
            let dt = if *last_probe == 0 { 0 } else { now - *last_probe };
            *last_probe = now;
            let mut out = String::new();
            for b in bots {
                let live = self.workers.get(&b.username).map(|w| w.on_anarchy).unwrap_or(false);
                if live && dt > 0 && dt < 5 * MIN_MS {
                    self.rec(b, now).online_ms += dt;
                }
                out.push_str(
                    &json!({ "anarchy": b.anarchy, "bot": b.username, "inv": 0, "live": live, "on_ah": 0,
                             "on_anarchy": live, "nursery": true, "ts": now / 1000 })
                    .to_string(),
                );
                out.push('\n');
            }
            if !out.is_empty() {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.root.join("bot-slots.jsonl")) {
                    let _ = f.write_all(out.as_bytes());
                }
            }
            self.write_status(bots, now);
        }
    }

    fn write_status(&self, bots: &[NurseryBot], now: u64) {
        let rows: Vec<Value> = bots
            .iter()
            .map(|b| {
                let r = self.state.bots.get(&b.username).cloned().unwrap_or_default();
                let age_h = now.saturating_sub(r.born_ms) as f64 / HOUR_MS as f64;
                json!({
                    "bot": b.username,
                    "anarchy": b.anarchy,
                    "target": b.target,
                    "age_h": (age_h * 10.0).round() / 10.0,
                    "online_h": ((r.online_ms as f64 / HOUR_MS as f64) * 10.0).round() / 10.0,
                    "ready": !r.banned && age_h >= (self.warmup_days * 24) as f64,
                    "running": self.workers.contains_key(&b.username),
                    "on_anarchy": self.workers.get(&b.username).map(|w| w.on_anarchy).unwrap_or(false),
                    "banned": r.banned,
                    "ban_reason": r.ban_reason,
                    "bind": r.bind,
                })
            })
            .collect();
        write_json_atomic(&self.root.join("nursery-status.json"), &json!({ "warmup_days": self.warmup_days, "bots": rows }));
        write_json_atomic(&self.root.join("nursery-state.json"), &self.state);
    }
}

async fn go_loop(url: String, mut rx: mpsc::UnboundedReceiver<Value>) {
    let mut pending: Vec<Value> = Vec::new();
    loop {
        match connect_async(&url).await {
            Ok((ws, _)) => {
                info!("Go WS открыт ({url})");
                let (mut sink, mut stream) = ws.split();
                for v in pending.drain(..) {
                    let _ = sink.send(Message::Text(v.to_string().into())).await;
                }
                loop {
                    tokio::select! {
                        m = stream.next() => match m {
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            _ => {}
                        },
                        out = rx.recv() => match out {
                            Some(v) => {
                                if sink.send(Message::Text(v.to_string().into())).await.is_err() {
                                    pending.push(v);
                                    break;
                                }
                            }
                            None => return,
                        }
                    }
                }
            }
            Err(e) => warn!("Go WS: {e}"),
        }
        // пока нет связи — копим (не больше 100)
        while let Ok(v) = rx.try_recv() {
            if pending.len() < 100 {
                pending.push(v);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    ipc::pretty_log::init_orch();
    let root = std::env::current_dir()?;
    let warmup_days: u64 = std::env::var("WARMUP_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let go_ws = std::fs::read_to_string(root.join("orch.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("go_ws").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| "ws://127.0.0.1:8080/ws".into());

    let (go_tx, go_rx) = mpsc::unbounded_channel();
    tokio::spawn(go_loop(go_ws, go_rx));
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();

    let mut rt = Runtime {
        bot_bin: bot_bin_path(),
        state: read_json(&root.join("nursery-state.json")),
        root: root.clone(),
        warmup_days,
        workers: HashMap::new(),
        retry_after: HashMap::new(),
        fails: HashMap::new(),
        go_tx,
        ev_tx,
    };
    info!("питомник: прогрев {warmup_days} сут, бинарь {}", rt.bot_bin.display());

    let sched_cfg = ScheduleConfig::load(&root);
    let sched = Schedule::new(0, "nursery", 3, &sched_cfg);
    let mut bots = load_bots(&root);
    info!("питомник: ников {}", bots.len());
    let mut last_probe = 0u64;
    let mut ticker = tokio::time::interval(TICK);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                bots = load_bots(&root);
                rt.tick(&bots, &sched, &mut last_probe).await;
            }
            Some(ev) = ev_rx.recv() => match ev {
                Ev::Line(n, l) => rt.on_line(&n, &l, &bots),
                Ev::Exit(n) => rt.on_exit(&n),
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }
    info!("питомник: выход — гашу воркеров");
    let names: Vec<String> = rt.workers.keys().cloned().collect();
    for n in names {
        rt.kill(&n, "выход питомника");
    }
    rt.write_status(&bots, now_unix_ms());
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
