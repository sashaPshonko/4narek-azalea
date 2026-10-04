//! Порт `lib/afk-simple.mjs`: WASD или look по шагам мыши 0.15° (GCD).

use std::f32::consts::PI;
use std::time::Duration;

use azalea::prelude::*;
use azalea::WalkDirection;
use rand::Rng;
use tracing::info;

use crate::delay;

const LOOK_GCD_STEP: f32 = 0.15; // градусы, как vanilla mouse
const YAW_DEADBAND: f32 = 0.04 * 180.0 / PI; // ~2.3°
const TICK_MS: u64 = 50;

pub async fn un_afk(bot: &Client) {
    un_afk_inner(bot, false).await;
}

pub async fn un_afk_quiet(bot: &Client) {
    un_afk_inner(bot, false).await;
}

async fn un_afk_inner(bot: &Client, keep_window: bool) {
    if !keep_window && crate::safe_inv::try_close(bot) {
        delay::rnd("POLL").await;
    }
    bot.walk(WalkDirection::None);

    let mut rng = rand::thread_rng();
    if rng.gen_bool(0.65) {
        wasd_tap(bot).await;
    } else {
        look_sweep(bot).await;
    }
    bot.walk(WalkDirection::None);
}

async fn wasd_tap(bot: &Client) {
    let dirs = [
        WalkDirection::Forward,
        WalkDirection::Backward,
        WalkDirection::Left,
        WalkDirection::Right,
    ];
    let mut rng = rand::thread_rng();
    let dir = dirs[rng.gen_range(0..dirs.len())];
    let hold = rng.gen_range(160u64..=420);
    info!("anti-AFK → WASD {dir:?} {hold}мс");
    tokio::time::sleep(Duration::from_millis(rng.gen_range(40..=120))).await;
    bot.walk(dir);
    let until = std::time::Instant::now() + Duration::from_millis(hold);
    while std::time::Instant::now() < until {
        let left = until.saturating_duration_since(std::time::Instant::now());
        tokio::time::sleep(left.min(Duration::from_millis(40))).await;
    }
    bot.walk(WalkDirection::None);
    tokio::time::sleep(Duration::from_millis(rng.gen_range(60..=180))).await;
}

async fn look_sweep(bot: &Client) {
    let Ok((yaw, pitch)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bot.direction()))
    else {
        info!("anti-AFK → look skip (нет direction)");
        return;
    };
    let mut rng = rand::thread_rng();
    let sign = if rng.gen_bool(0.5) { 1.0f32 } else { -1.0 };
    let target_yaw = normalize_yaw(yaw + sign * rng.gen_range(0.45..1.25) * 180.0 / PI);
    let target_pitch = clamp_pitch(pitch + rng.gen_range(-0.12f32..0.08) * 180.0 / PI);
    let max_ms = rng.gen_range(500u64..=1100);
    info!("anti-AFK → look (GCD 0.15°) ~{:.1}с", max_ms as f64 / 1000.0);
    let until = std::time::Instant::now() + Duration::from_millis(max_ms);
    let mut ticks = 0u32;
    while std::time::Instant::now() < until {
        if look_tick_toward(bot, target_yaw, target_pitch) {
            break;
        }
        ticks += 1;
        tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
    }
    tracing::debug!("anti-AFK → look ticks={ticks}");
}

fn look_tick_toward(bot: &Client, target_yaw: f32, target_pitch: f32) -> bool {
    let Ok((cur_yaw, cur_pitch)) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bot.direction()))
    else {
        return true;
    };
    let mut rng = rand::thread_rng();
    let dy = delta_yaw(target_yaw, cur_yaw);
    let mut next_yaw = cur_yaw;
    if dy.abs() > YAW_DEADBAND {
        let units = ((dy.abs() / LOOK_GCD_STEP).floor() as i32).clamp(1, 12);
        let step = units.min(rng.gen_range(3..=10));
        next_yaw = normalize_yaw(cur_yaw + dy.signum() * step as f32 * LOOK_GCD_STEP);
    }
    let mut next_pitch = cur_pitch;
    let dp = target_pitch - cur_pitch;
    if dp.abs() > LOOK_GCD_STEP {
        let pu = ((dp.abs() / LOOK_GCD_STEP).floor() as i32).clamp(1, 4);
        next_pitch = clamp_pitch(cur_pitch + dp.signum() * pu as f32 * LOOK_GCD_STEP);
    }
    bot.set_direction(next_yaw, next_pitch);
    delta_yaw(target_yaw, next_yaw).abs() <= YAW_DEADBAND * 2.0
        && (target_pitch - next_pitch).abs() <= LOOK_GCD_STEP * 2.0
}

fn normalize_yaw(mut a: f32) -> f32 {
    while a > 180.0 {
        a -= 360.0;
    }
    while a < -180.0 {
        a += 360.0;
    }
    a
}

fn delta_yaw(a: f32, b: f32) -> f32 {
    let mut d = (a - b) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d < -180.0 {
        d += 360.0;
    }
    d
}

fn clamp_pitch(p: f32) -> f32 {
    // old: -0.85..0.55 rad ≈ -48.7°..31.5°
    p.clamp(-48.7, 31.5)
}
