//! Капча FunTime: кэш map → склейка (node) → POST 127.0.0.1:8799 → pred в чат.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::{
    core::direction::Direction,
    ecs::prelude::With,
    entity::{
        metadata::{ItemFrame, ItemFrameDirection, ItemFrameItem, Rotation},
        LookDirection, Position,
    },
    prelude::*,
    protocol::packets::game::ClientboundGamePacket,
    registry::builtin::ItemKind,
    world::MinecraftEntityId,
};
use azalea_inventory::components::MapId;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tracing::{info, warn};

const MAP_SIZE: usize = 128 * 128;
const MIN_FRAMES: usize = 12;
const DEFAULT_SOLVER: &str = "http://127.0.0.1:8799";

#[derive(Clone, Default)]
pub struct CaptchaMaps {
    /// map_id → 128×128 color indices
    pub maps: HashMap<i32, Vec<u8>>,
    pub seq: u64,
    pub updated_at: HashMap<i32, u64>,
}

impl CaptchaMaps {
    pub fn apply_patch(&mut self, map_id: i32, width: u8, height: u8, start_x: u8, start_y: u8, colors: &[u8]) {
        let buf = self
            .maps
            .entry(map_id)
            .or_insert_with(|| vec![0u8; MAP_SIZE]);
        let w = width as usize;
        let h = height as usize;
        let sx = start_x as usize;
        let sy = start_y as usize;
        let mut i = 0usize;
        for y in 0..h {
            for x in 0..w {
                if i >= colors.len() {
                    break;
                }
                let dx = sx + x;
                let dy = sy + y;
                if dx < 128 && dy < 128 {
                    buf[dy * 128 + dx] = colors[i];
                }
                i += 1;
            }
        }
        self.seq += 1;
        self.updated_at.insert(map_id, self.seq);
        // не раздувать кэш
        if self.maps.len() > 160 {
            let mut ids: Vec<(i32, u64)> = self
                .updated_at
                .iter()
                .map(|(&k, &v)| (k, v))
                .collect();
            ids.sort_by_key(|(_, s)| *s);
            for (id, _) in ids.into_iter().take(self.maps.len().saturating_sub(128)) {
                self.maps.remove(&id);
                self.updated_at.remove(&id);
            }
        }
    }
}

#[derive(Clone)]
struct FrameInfo {
    map_id: i32,
    rotate: i32,
    x: f64,
    y: f64,
    z: f64,
    view: String,
    facing: String,
}

pub fn on_packet(maps: &Arc<Mutex<CaptchaMaps>>, packet: &ClientboundGamePacket) {
    let ClientboundGamePacket::MapItemData(p) = packet else {
        return;
    };
    let Some(patch) = &p.color_patch.0 else {
        return;
    };
    if patch.map_colors.is_empty() {
        return;
    }
    maps.lock().apply_patch(
        p.map_id as i32,
        patch.width,
        patch.height,
        patch.start_x,
        patch.start_y,
        &patch.map_colors,
    );
}

/// Как flayercaptcha `EntityUtils.getViewDirection(yaw, pitch)`.
/// Mineflayer отдаёт радианы; сюда — градусы Minecraft (как LookDirection / facing→yaw).
fn view_from_look_deg(y_rot_deg: f32, x_rot_deg: f32) -> String {
    let mut yaw_deg = 180.0 - y_rot_deg as f64;
    if yaw_deg > 180.0 {
        yaw_deg -= 360.0;
    }
    if yaw_deg < -180.0 {
        yaw_deg += 360.0;
    }
    let yaw_deg = yaw_deg.round();
    let pitch_deg = (-(x_rot_deg as f64)).round();
    if pitch_deg >= 60.0 {
        return "down".into();
    }
    if pitch_deg <= -60.0 {
        return "up".into();
    }
    if (-45.0..=44.9).contains(&yaw_deg) {
        "south".into()
    } else if (45.0..=134.9).contains(&yaw_deg) {
        "west".into()
    } else if (-135.0..=-45.1).contains(&yaw_deg) {
        "east".into()
    } else {
        "north".into()
    }
}

/// ItemFrame facing → entity yaw/pitch (Minecraft hanging entity), затем getViewDirection.
/// LookDirection у рамок в Azalea часто врёт (все «down») — facing надёжнее.
fn view_from_facing(dir: Direction) -> String {
    let (yaw, pitch) = match dir {
        Direction::South => (0.0, 0.0),
        Direction::West => (90.0, 0.0),
        Direction::North => (180.0, 0.0),
        Direction::East => (-90.0, 0.0),
        Direction::Up => (0.0, -90.0),
        Direction::Down => (0.0, 90.0),
    };
    view_from_look_deg(yaw, pitch)
}

fn collect_frames(bot: &Client) -> Vec<FrameInfo> {
    let mut out = Vec::new();
    let mut ecs = bot.ecs.lock();
    let mut q = ecs.query_filtered::<(
        &MinecraftEntityId,
        &Position,
        &ItemFrameItem,
        Option<&Rotation>,
        Option<&ItemFrameDirection>,
    ), With<ItemFrame>>();
    for (_id, pos, item, rot, dir) in q.iter(&ecs) {
        let stack = &item.0;
        let Some(data) = stack.as_present() else {
            continue;
        };
        if data.kind != ItemKind::FilledMap && data.kind != ItemKind::Map {
            continue;
        }
        let Some(map) = data.get_component::<MapId>() else {
            continue;
        };
        let rotate = rot.map(|r| r.0).unwrap_or(0) & 7;
        let facing = match dir.map(|d| d.0) {
            Some(Direction::North) => "north",
            Some(Direction::South) => "south",
            Some(Direction::East) => "east",
            Some(Direction::West) => "west",
            Some(Direction::Up) => "up",
            Some(Direction::Down) => "down",
            None => "?",
        };
        let view = match dir.map(|d| d.0) {
            Some(d) => view_from_facing(d),
            None => "south".into(),
        };
        out.push(FrameInfo {
            map_id: map.id,
            rotate,
            x: pos.x,
            y: pos.y,
            z: pos.z,
            view,
            facing: facing.into(),
        });
    }
    out
}

fn pick_wall(frames: &[FrameInfo], maps: &CaptchaMaps) -> Vec<FrameInfo> {
    // Сырой пул с картами — прямоугольник 4×3 выбирает JS pickCaptchaWallFrames
    // (как 4narek capture.mjs). Тут только «есть map bytes».
    frames
        .iter()
        .filter(|f| maps.maps.contains_key(&f.map_id))
        .cloned()
        .collect()
}

fn assemble_helper_path() -> PathBuf {
    if let Ok(p) = std::env::var("CAPTCHA_ASSEMBLE") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for rel in [
                "scripts/assemble-captcha.mjs",
                "../scripts/assemble-captcha.mjs",
                "../../scripts/assemble-captcha.mjs",
            ] {
                let c = dir.join(rel);
                if c.exists() {
                    return c;
                }
            }
        }
    }
    // crates/bot → 4narek-azalea/scripts
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("scripts/assemble-captcha.mjs");
    p
}

fn farm_node_cwd() -> PathBuf {
    if let Ok(p) = std::env::var("CAPTCHA_NODE_CWD") {
        return PathBuf::from(p);
    }
    // cwd скрипта = корень 4narek-azalea (package.json + node_modules/pngjs)
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

async fn assemble_png(
    frames: &[FrameInfo],
    maps: &CaptchaMaps,
    origin: Option<(f64, f64, f64)>,
    bot_look: Option<(f64, f64)>,
) -> anyhow::Result<Vec<u8>> {
    let mut maps_json = serde_json::Map::new();
    for f in frames {
        if let Some(buf) = maps.maps.get(&f.map_id) {
            maps_json.insert(f.map_id.to_string(), json!(B64.encode(buf)));
        }
    }
    let frames_json: Vec<Value> = frames
        .iter()
        .map(|f| {
            json!({
                "mapId": f.map_id,
                "rotate": f.rotate,
                "coordinate": { "x": f.x, "y": f.y, "z": f.z },
                "viewDirection": f.view,
                "facing": f.facing,
            })
        })
        .collect();
    let mut payload = json!({ "frames": frames_json, "maps": maps_json });
    if let Some((x, y, z)) = origin {
        payload
            .as_object_mut()
            .unwrap()
            .insert("origin".into(), json!({ "x": x, "y": y, "z": z }));
    }
    // yaw/pitch бота в радианах (как mineflayer) — фильтр forward в helper
    if let Some((yaw_rad, pitch_rad)) = bot_look {
        let obj = payload.as_object_mut().unwrap();
        obj.insert("botYaw".into(), json!(yaw_rad));
        obj.insert("botPitch".into(), json!(pitch_rad));
    }

    let helper = assemble_helper_path();
    if !helper.exists() {
        anyhow::bail!("нет assemble helper {}", helper.display());
    }

    let mut child = tokio::process::Command::new("node")
        .arg(&helper)
        .current_dir(farm_node_cwd())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(payload.to_string().as_bytes()).await?;
        drop(stdin);
    }
    let out = child.wait_with_output().await?;
    let body: Value = serde_json::from_slice(&out.stdout).unwrap_or(json!({}));
    if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("assemble fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{err} stderr={stderr}");
    }
    if let Some(ids) = body.get("wallMapIds").and_then(|v| v.as_array()) {
        info!(
            "капча → стена {} тайлов ids={:?}",
            ids.len(),
            ids.iter()
                .filter_map(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
                .collect::<Vec<_>>()
        );
    }
    let b64 = body
        .get("pngBase64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no pngBase64"))?;
    Ok(B64.decode(b64)?)
}

async fn solve_remote(png: &[u8], username: &str) -> anyhow::Result<(String, f64)> {
    let base = std::env::var("CAPTCHA_SOLVER_URL").unwrap_or_else(|_| DEFAULT_SOLVER.into());
    let base = base.trim_end_matches('/').to_string();
    let url = format!("{base}/api/solve");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let body = json!({
        "pngBase64": B64.encode(png),
        "username": username,
        "dump": false,
    });
    let res = client.post(&url).json(&body).send().await?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(json!({}));
    if !status.is_success() {
        anyhow::bail!("solve HTTP {status}: {v}");
    }
    let pred = v
        .get("pred")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("no pred"))?
        .to_string();
    if pred.len() != 5 || !pred.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("bad pred {pred}");
    }
    let conf = v.get("conf").and_then(|x| x.as_f64()).unwrap_or(0.0);
    Ok((pred, conf))
}

fn wall_fingerprint(wall: &[FrameInfo]) -> String {
    let mut parts: Vec<String> = wall
        .iter()
        .map(|f| format!("{}:{}:{:.0},{:.0},{:.0}", f.map_id, f.rotate, f.x, f.y, f.z))
        .collect();
    parts.sort();
    parts.join("|")
}

fn bot_look_rad(bot: &Client) -> Option<(f64, f64)> {
    let look = bot.get_component::<LookDirection>()?;
    let yaw_rad = (look.y_rot() as f64).to_radians();
    let pitch_rad = (look.x_rot() as f64).to_radians();
    Some((yaw_rad, pitch_rad))
}

/// Ждём стены с картами, склеиваем, шлём pred в чат. До 3 попыток.
pub async fn handle_captcha_login(
    bot: &Client,
    maps: &Arc<Mutex<CaptchaMaps>>,
    password: &str,
    username: &str,
) -> anyhow::Result<()> {
    info!("капча → старт solve ({username})");
    let mut prev_fp: Option<String> = None;
    let mut min_map_seq: u64 = 0;

    for attempt in 1..=3 {
        {
            let mut st = crate::boot::global().state.lock();
            st.captcha_wrong = false;
            st.captcha_ok = false;
        }

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut captured_png: Option<Vec<u8>> = None;
        let mut fingerprint = String::new();
        let mut seq_at_capture = 0u64;

        while Instant::now() < deadline {
            let frames = collect_frames(bot);
            let maps_snap = maps.lock().clone();
            if attempt > 1 && maps_snap.seq <= min_map_seq {
                tokio::time::sleep(Duration::from_millis(400)).await;
                continue;
            }
            let wall = pick_wall(&frames, &maps_snap);
            if wall.len() >= MIN_FRAMES {
                fingerprint = wall_fingerprint(&wall);
                if prev_fp.as_ref() == Some(&fingerprint) && attempt > 1 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
                let origin = crate::anarchy::try_pos(bot).map(|p| (p.x, p.y, p.z));
                let bot_look = bot_look_rad(bot);
                match assemble_png(&wall, &maps_snap, origin, bot_look).await {
                    Ok(png) => {
                        seq_at_capture = maps_snap.seq;
                        captured_png = Some(png);
                        break;
                    }
                    Err(e) => {
                        warn!("капча assemble: {e:#}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(450)).await;
        }

        let Some(png) = captured_png else {
            warn!("капча → не собрали PNG (попытка {attempt})");
            continue;
        };
        prev_fp = Some(fingerprint);

        info!("капча → solve remote ({} bytes, try {attempt})", png.len());
        let (pred, conf) = match solve_remote(&png, username).await {
            Ok(v) => v,
            Err(e) => {
                warn!("капча solve: {e:#}");
                continue;
            }
        };
        info!("капча → pred={pred} conf={conf:.2}");
        crate::cmd::say(bot, pred.clone());

        let ok = wait_verdict(Duration::from_secs(4)).await;
        if ok {
            info!("капча принята {pred}");
            tokio::time::sleep(Duration::from_millis(400)).await;
            crate::cmd::say(bot, format!("/reg {password}"));
            tokio::time::sleep(Duration::from_millis(600)).await;
            crate::cmd::say(bot, format!("/l {password}"));
            return Ok(());
        }
        min_map_seq = seq_at_capture;
        warn!("капча wrong/timeout pred={pred}, retry");
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    anyhow::bail!("капча не принята за 3 попытки")
}

/// Grace: нет «неправильно» ≈ ок (как в 4narek-old).
async fn wait_verdict(grace: Duration) -> bool {
    let g = crate::boot::global();
    let start = Instant::now();
    while start.elapsed() < grace {
        let st = g.state.lock();
        if st.captcha_wrong {
            drop(st);
            g.state.lock().captcha_wrong = false;
            return false;
        }
        if st.captcha_ok {
            return true;
        }
        drop(st);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}
