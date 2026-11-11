//! botview — прозрачный прокси между клиентом (TLauncher) и FunTime, с записью пакетов окон.
//!
//! Заменяет `bot-view` на Node. Клиент заходит на `viewHost:viewPort`, прокси соединяется с сервером через SOCKS5,
//! подставляет ник из конфига (`username`), а дальше **пересылает байты как есть**: капчу (BotFilter) клиент
//! проходит сам, ничего не кэшируется и не переигрывается. Параллельно пакеты окон, чата и кликов пишутся в
//! JSONL (`packetLog`), в том же виде, что писал `bot-view`:
//!   {"t": ISO, "dir": "cb"|"sb"|"sys", "name": ..., ...}
//!
//! Запуск: `./target/release/botview [путь/к/config.json]` (по умолчанию `../bot-view/config.json`).
//! Поля конфига те же, что у `bot-view`: username, password, host, port, socks, viewHost, viewPort, packetLog.
//! Если `password` задан, после входа в игру прокси сам отправит `/reg <пароль>` и `/l <пароль>`
//! (`autoLogin: false` выключает).

use std::{
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use azalea::protocol::{
    connect::{Connection, Proxy, RawReadConnection, RawWriteConnection},
    packets::{
        config::{ClientboundConfigPacket, ServerboundConfigPacket},
        game::{ClientboundGamePacket, ServerboundChatCommand, ServerboundGamePacket},
        handshake::{ClientboundHandshakePacket, ServerboundHandshakePacket, ServerboundIntention},
        login::{
            ClientboundLoginCompression, ClientboundLoginFinished, ClientboundLoginPacket,
            ServerboundHello, ServerboundLoginAcknowledged, ServerboundLoginPacket,
        },
        ClientIntention,
    },
    read::deserialize_packet,
    write::serialize_packet,
};
use azalea_inventory::ItemStack;
use parking_lot::Mutex;
use serde_json::{json, Value};
use socks5_impl::protocol::UserKey;
use tokio::{net::TcpListener, sync::mpsc};
use tracing::{info, warn};

#[derive(Clone)]
struct Cfg {
    username: String,
    password: String,
    host: String,
    port: u16,
    socks: Option<String>,
    view_addr: String,
    packet_log: PathBuf,
    auto_login: bool,
}

fn load_cfg(path: &Path) -> Result<Cfg> {
    let raw = std::fs::read_to_string(path).map_err(|e| anyhow!("config {}: {e}", path.display()))?;
    let v: Value = serde_json::from_str(&raw)?;
    let s = |k: &str, d: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
    let dir = path.parent().unwrap_or(Path::new("."));
    Ok(Cfg {
        username: s("username", "player"),
        password: s("password", ""),
        host: s("host", "mc.funtime.su"),
        port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(25565) as u16,
        socks: v.get("socks").and_then(|x| x.as_str()).map(|x| x.to_string()),
        view_addr: format!("{}:{}", s("viewHost", "0.0.0.0"), v.get("viewPort").and_then(|x| x.as_u64()).unwrap_or(25566)),
        packet_log: dir.join(s("packetLog", "packets.jsonl")),
        auto_login: v.get("autoLogin").and_then(|x| x.as_bool()).unwrap_or(true),
    })
}

fn parse_proxy(url: &str) -> Result<Proxy> {
    let raw = if url.contains("://") { url.to_string() } else { format!("socks5://{url}") };
    let u = url::Url::parse(&raw)?;
    let host = u.host_str().unwrap_or("127.0.0.1");
    let port = u.port().unwrap_or(1080);
    let addr = format!("{host}:{port}").parse()?;
    let auth = match (u.username(), u.password()) {
        (user, Some(pass)) if !user.is_empty() => Some(UserKey::new(user, pass.to_string())),
        _ => None,
    };
    Ok(Proxy::new(addr, auth))
}

fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let ms = d.subsec_millis();
    // дата без внешних крейтов (civil from days)
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dd = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[derive(Clone)]
struct PacketLog(Arc<Mutex<std::fs::File>>);

impl PacketLog {
    fn open(path: &Path) -> Result<Self> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self(Arc::new(Mutex::new(f))))
    }
    fn write(&self, dir: &str, name: &str, mut extra: Value) {
        if let Some(o) = extra.as_object_mut() {
            o.insert("t".into(), json!(now_iso()));
            o.insert("dir".into(), json!(dir));
            o.insert("name".into(), json!(name));
        }
        let mut f = self.0.lock();
        let _ = writeln!(f, "{extra}");
    }
}

fn item_json(it: &ItemStack) -> Value {
    if it.as_present().is_none() {
        return Value::Null;
    }
    serde_json::to_value(it).unwrap_or_else(|_| json!(format!("{it:?}")))
}

fn log_clientbound(log: &PacketLog, p: &ClientboundGamePacket) {
    match p {
        ClientboundGamePacket::OpenScreen(p) => log.write(
            "cb",
            "open_window",
            json!({"windowId": p.container_id, "menu": format!("{:?}", p.menu_type), "title": p.title.to_string()}),
        ),
        ClientboundGamePacket::ContainerSetContent(p) => {
            let items: Vec<Value> = p
                .items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.as_present().is_some())
                .map(|(i, it)| json!({"slot": i, "item": item_json(it)}))
                .collect();
            log.write(
                "cb",
                "window_items",
                json!({"windowId": p.container_id, "stateId": p.state_id, "items": items, "carried": item_json(&p.carried_item)}),
            )
        }
        ClientboundGamePacket::ContainerSetSlot(p) => log.write(
            "cb",
            "set_slot",
            json!({"windowId": p.container_id, "stateId": p.state_id, "slot": p.slot, "item": item_json(&p.item_stack)}),
        ),
        ClientboundGamePacket::ContainerClose(p) => {
            log.write("cb", "close_window", json!({"windowId": p.container_id}))
        }
        ClientboundGamePacket::SystemChat(p) => {
            log.write("cb", "chat", json!({"text": p.content.to_string(), "overlay": p.overlay}))
        }
        ClientboundGamePacket::Disconnect(p) => {
            log.write("cb", "disconnect", json!({"text": p.reason.to_string()}))
        }
        ClientboundGamePacket::Login(_) => log.write("sys", "login", json!({})),
        _ => {}
    }
}

fn log_serverbound(log: &PacketLog, p: &ServerboundGamePacket) {
    match p {
        ServerboundGamePacket::ContainerClick(p) => {
            let changed: Vec<Value> = p
                .changed_slots
                .iter()
                .map(|(slot, st)| json!({"location": slot, "item": format!("{st:?}")}))
                .collect();
            log.write(
                "sb",
                "window_click",
                json!({
                    "windowId": p.container_id, "stateId": p.state_id, "slot": p.slot_num,
                    "mouseButton": p.button_num, "mode": format!("{:?}", p.click_type),
                    "changedSlots": changed, "cursorItem": format!("{:?}", p.carried_item),
                }),
            )
        }
        ServerboundGamePacket::ContainerClose(p) => {
            log.write("sb", "close_window", json!({"windowId": p.container_id}))
        }
        ServerboundGamePacket::ChatCommand(p) => {
            log.write("sb", "chat", json!({"text": format!("/{}", p.command)}))
        }
        ServerboundGamePacket::Chat(p) => log.write("sb", "chat", json!({"text": p.message})),
        ServerboundGamePacket::SetCarriedItem(p) => {
            log.write("sb", "held_item_slot", json!({"slot": format!("{p:?}")}))
        }
        _ => {}
    }
}

/// Разрешаем адрес сервера в `SocketAddr` (IPv4 предпочтительнее).
async fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr> {
    let addrs: Vec<_> = tokio::net::lookup_host((host, port)).await?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| anyhow!("не нашёл адрес {host}"))
}

type UpLogin = Connection<ClientboundLoginPacket, ServerboundLoginPacket>;

/// Подключение к серверу через SOCKS5 и вход под ником из конфига. Повторяем при сбое прокси.
async fn connect_upstream(cfg: &Cfg, protocol_version: i32) -> Result<(UpLogin, azalea::auth::game_profile::GameProfile)> {
    let addr = resolve(&cfg.host, cfg.port).await?;
    let proxy = cfg.socks.as_deref().map(parse_proxy).transpose()?;
    let uuid = azalea::Account::offline(&cfg.username).uuid_or_offline();
    let mut last_err = anyhow!("нет попыток");
    for attempt in 1..=6 {
        let res: Result<(UpLogin, azalea::auth::game_profile::GameProfile)> = tokio::time::timeout(Duration::from_secs(12), async {
            let mut conn = match &proxy {
                Some(p) => Connection::new_with_proxy(&addr, p.clone()).await?,
                None => Connection::new(&addr).await?,
            };
            conn.write(ServerboundIntention {
                protocol_version,
                hostname: cfg.host.clone(),
                port: cfg.port,
                intention: ClientIntention::Login,
            })
            .await?;
            let mut conn = conn.login();
            conn.write(ServerboundHello { name: cfg.username.clone(), profile_id: uuid }).await?;
            loop {
                match conn.read().await.map_err(|e| anyhow!("login read: {e}"))? {
                    ClientboundLoginPacket::LoginCompression(p) => {
                        conn.set_compression_threshold(p.compression_threshold)
                    }
                    ClientboundLoginPacket::LoginFinished(p) => return Ok((conn, p.game_profile)),
                    ClientboundLoginPacket::LoginDisconnect(p) => {
                        return Err(anyhow!("сервер отклонил вход: {}", p.reason))
                    }
                    ClientboundLoginPacket::Hello(_) => {
                        return Err(anyhow!("сервер просит шифрование (online-mode) — не поддерживается"))
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(anyhow!("таймаут 12 с (прокси/сервер не отвечают)")));
        match res {
            Ok(r) => return Ok(r),
            Err(e) => {
                warn!("попытка {attempt}/6 подключиться к серверу: {e}");
                last_err = e;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err)
}

struct Phase {
    cb_finished: AtomicBool,
    sb_finished: AtomicBool,
}

impl Phase {
    fn play(&self) -> bool {
        self.cb_finished.load(Ordering::SeqCst) && self.sb_finished.load(Ordering::SeqCst)
    }
}

/// Сервер → клиент.
async fn pump_cb(
    mut from: RawReadConnection,
    mut to: RawWriteConnection,
    phase: Arc<Phase>,
    log: PacketLog,
    inject: mpsc::UnboundedSender<String>,
    auto_login: Option<String>,
) {
    let mut sent_login = false;
    loop {
        let raw = match from.read().await {
            Ok(r) => r,
            Err(e) => {
                info!("сервер закрыл соединение: {e}");
                break;
            }
        };
        if phase.play() {
            match deserialize_packet::<ClientboundGamePacket>(&mut Cursor::new(&raw[..])) {
                Ok(p) => {
                    log_clientbound(&log, &p);
                    if let (ClientboundGamePacket::Login(_), false, Some(pw)) = (&p, sent_login, &auto_login) {
                        sent_login = true;
                        let _ = inject.send(format!("reg {pw}"));
                        let _ = inject.send(format!("l {pw}"));
                    }
                }
                Err(_) => {}
            }
        } else if let Ok(ClientboundConfigPacket::FinishConfiguration(_)) =
            deserialize_packet::<ClientboundConfigPacket>(&mut Cursor::new(&raw[..]))
        {
            phase.cb_finished.store(true, Ordering::SeqCst);
        }
        if to.write(&raw).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

/// Клиент → сервер (и команды, которые прокси подставляет сам).
async fn pump_sb(
    mut from: RawReadConnection,
    mut to: RawWriteConnection,
    phase: Arc<Phase>,
    log: PacketLog,
    mut inject: mpsc::UnboundedReceiver<String>,
) {
    loop {
        tokio::select! {
            biased;
            cmd = inject.recv() => {
                let Some(cmd) = cmd else { continue };
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let pkt = ServerboundGamePacket::ChatCommand(ServerboundChatCommand { command: cmd.clone() });
                if let Ok(bytes) = serialize_packet(&pkt) {
                    log.write("sys", "inject", json!({"command": cmd.split(' ').next()}));
                    if to.write(&bytes).await.is_err() { break; }
                }
            }
            raw = from.read() => {
                let raw = match raw {
                    Ok(r) => r,
                    Err(e) => { info!("клиент закрыл соединение: {e}"); break; }
                };
                if phase.play() {
                    if let Ok(p) = deserialize_packet::<ServerboundGamePacket>(&mut Cursor::new(&raw[..])) {
                        log_serverbound(&log, &p);
                    }
                } else if let Ok(ServerboundConfigPacket::FinishConfiguration(_)) =
                    deserialize_packet::<ServerboundConfigPacket>(&mut Cursor::new(&raw[..]))
                {
                    phase.sb_finished.store(true, Ordering::SeqCst);
                }
                if to.write(&raw).await.is_err() { break; }
            }
        }
    }
    let _ = to.shutdown().await;
}

async fn handle_client(stream: tokio::net::TcpStream, cfg: Cfg, log: PacketLog) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut client = Connection::<ServerboundHandshakePacket, ClientboundHandshakePacket>::wrap(stream);
    let ServerboundHandshakePacket::Intention(intent) = client.read().await.map_err(|e| anyhow!("handshake: {e}"))?;
    if intent.intention != ClientIntention::Login {
        // пинг списка серверов и т. п. — не обслуживаем
        return Ok(());
    }
    let mut client = client.login();
    let ServerboundLoginPacket::Hello(hello) = client.read().await.map_err(|e| anyhow!("login hello: {e}"))? else {
        return Err(anyhow!("ожидал Hello"));
    };
    info!("клиент {} заходит, соединяюсь с {}:{} как {}", hello.name, cfg.host, cfg.port, cfg.username);

    let (mut up, profile) = connect_upstream(&cfg, intent.protocol_version).await?;
    info!("сервер принял вход ({}), отдаю клиенту", profile.name);

    // клиенту: сжатие и профиль (ник — тот, что назначил сервер)
    client.write(ClientboundLoginCompression { compression_threshold: 256 }).await?;
    client.set_compression_threshold(256);
    client.write(ClientboundLoginFinished { game_profile: profile }).await?;
    loop {
        match client.read().await.map_err(|e| anyhow!("ждал LoginAcknowledged: {e}"))? {
            ServerboundLoginPacket::LoginAcknowledged(_) => break,
            _ => {}
        }
    }
    up.write(ServerboundLoginAcknowledged).await?;

    let (c_r, c_w) = client.config().into_split_raw();
    let (u_r, u_w) = up.config().into_split_raw();
    log.write("sys", "session", json!({"client": hello.name, "as": cfg.username}));

    let phase = Arc::new(Phase { cb_finished: AtomicBool::new(false), sb_finished: AtomicBool::new(false) });
    let (tx, rx) = mpsc::unbounded_channel();
    let auto = (cfg.auto_login && !cfg.password.is_empty()).then(|| cfg.password.clone());
    let a = tokio::spawn(pump_cb(u_r, c_w, phase.clone(), log.clone(), tx, auto));
    let b = tokio::spawn(pump_sb(c_r, u_w, phase, log.clone(), rx));
    // как только одна сторона закрылась, гасим вторую
    tokio::select! {
        _ = a => {}
        _ = b => {}
    }
    log.write("sys", "session_end", json!({}));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let path = std::env::args().nth(1).unwrap_or_else(|| "../bot-view/config.json".into());
    let cfg = load_cfg(Path::new(&path))?;
    let log = PacketLog::open(&cfg.packet_log)?;
    let listener = TcpListener::bind(&cfg.view_addr).await?;
    info!(
        "botview: слушаю {} → {}:{} как {} (прокси {}), лог {}",
        cfg.view_addr,
        cfg.host,
        cfg.port,
        cfg.username,
        cfg.socks.as_deref().map(|s| s.rsplit('@').next().unwrap_or(s)).unwrap_or("нет"),
        cfg.packet_log.display()
    );
    let current: Arc<Mutex<Option<tokio::task::AbortHandle>>> = Arc::new(Mutex::new(None));
    loop {
        let (stream, peer) = listener.accept().await?;
        // новый клиент вытесняет прежнюю сессию (зависшую на подключении или оборванную)
        if let Some(h) = current.lock().take() {
            info!("{peer}: прерываю прежнюю сессию");
            h.abort();
        }
        let (cfg, log) = (cfg.clone(), log.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = handle_client(stream, cfg, log).await {
                warn!("сессия: {e:#}");
            }
        });
        *current.lock() = Some(task.abort_handle());
    }
}
