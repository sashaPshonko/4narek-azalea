//! Прогулка `/warp` раз в 25–30 мин (порт maybePromenadeWarp). Без grounding.

use std::time::{Duration, Instant};

use azalea::prelude::*;
use azalea::WalkDirection;
use rand::Rng;
use tracing::info;

use crate::cmd;
use crate::delay;

const WARP_NAMES: &[&str] = &["mine", "casino", "portal", "palach", "fisher", "stash", "wagon"];
const COOLDOWN_BASE_MS: u64 = 25 * 60_000;
const COOLDOWN_JITTER_MS: u64 = 5 * 60_000;
/// ~15% циклов пропуск
const SKIP_BPS: u32 = 1500;

pub struct PromenadeState {
    last_warp: Option<Instant>,
    worker_started: Instant,
    cooldown_ms: u64,
}

impl Default for PromenadeState {
    fn default() -> Self {
        Self {
            last_warp: None,
            worker_started: Instant::now(),
            cooldown_ms: COOLDOWN_BASE_MS,
        }
    }
}

impl PromenadeState {
    fn gap_anchor(&self) -> Instant {
        self.last_warp.unwrap_or(self.worker_started)
    }

    fn ready(&self) -> bool {
        let elapsed = self.gap_anchor().elapsed().as_millis() as u64;
        elapsed >= self.cooldown_ms
    }
}

fn random_warp() -> &'static str {
    let i = rand::thread_rng().gen_range(0..WARP_NAMES.len());
    WARP_NAMES[i]
}

/// Вызвать в начале sell_pass. true = сделали прогулку.
pub async fn maybe_promenade(bot: &Client, st: &mut PromenadeState) -> bool {
    if !st.ready() {
        return false;
    }
    if rand::thread_rng().gen_range(0..10_000u32) < SKIP_BPS {
        // зафиксировать следующий кулдаун как будто сходили
        st.cooldown_ms = COOLDOWN_BASE_MS
            + rand::thread_rng().gen_range(0..=COOLDOWN_JITTER_MS);
        st.last_warp = Some(Instant::now());
        return false;
    }

    let warp = random_warp();
    info!("прогулка → /warp {warp} (25–30м)");
    if crate::safe_inv::try_close(bot) {
        delay::rnd("POLL").await;
    }
    cmd::say(bot, format!("/warp {warp}"));
    // waitWarpTeleport ≈ 2–4с + settle
    delay::rnd("BASE_DELAY").await;
    delay::rnd("WALK_DELAY").await;

    let end = Instant::now()
        + Duration::from_millis(3500 + rand::thread_rng().gen_range(0..1500));
    let dirs = [
        WalkDirection::Forward,
        WalkDirection::Backward,
        WalkDirection::Left,
        WalkDirection::Right,
    ];
    while Instant::now() < end {
        let dir = dirs[rand::thread_rng().gen_range(0..dirs.len())];
        bot.walk(dir);
        tokio::time::sleep(Duration::from_millis(
            400 + rand::thread_rng().gen_range(0..500),
        ))
        .await;
        bot.walk(WalkDirection::None);
        tokio::time::sleep(Duration::from_millis(
            80 + rand::thread_rng().gen_range(0..120),
        ))
        .await;
    }
    bot.walk(WalkDirection::None);
    info!("прогулка → конец ({warp})");
    st.last_warp = Some(Instant::now());
    st.cooldown_ms =
        COOLDOWN_BASE_MS + rand::thread_rng().gen_range(0..=COOLDOWN_JITTER_MS);
    true
}
