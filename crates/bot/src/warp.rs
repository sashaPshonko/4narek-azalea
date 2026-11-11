//! Прогулка `/warp` раз в 10–15 мин (порт maybePromenadeWarp). Только команда, без ходьбы.

use std::time::{Duration, Instant};

use azalea::prelude::*;
use rand::Rng;
use tracing::info;

use crate::cmd;
use crate::delay;

const WARP_NAMES: &[&str] = &["mine", "casino", "portal", "palach", "fisher", "stash", "wagon"];
const COOLDOWN_BASE_MS: u64 = 10 * 60_000;
const COOLDOWN_JITTER_MS: u64 = 5 * 60_000;

pub struct PromenadeState {
    last_warp: Option<Instant>,
    worker_started: Instant,
    cooldown_ms: u64,
}

impl Default for PromenadeState {
    fn default() -> Self {
        Self {
            last_warp: None,
            worker_started: Instant::now() - Duration::from_secs(3600),
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

    let warp = random_warp();
    info!("прогулка → /warp {warp} (10–15м)");
    if crate::safe_inv::try_close(bot) {
        delay::rnd("POLL").await;
    }
    cmd::say(bot, format!("/warp {warp}"));
    // waitWarpTeleport ≈ 2–4с + settle
    delay::rnd("BASE_DELAY").await;

    // даём телепорту завершиться (раньше здесь была ходьба 3.5–5 с)
    tokio::time::sleep(Duration::from_millis(
        3500 + rand::thread_rng().gen_range(0..1500),
    ))
    .await;
    info!("прогулка → конец ({warp})");
    st.last_warp = Some(Instant::now());
    st.cooldown_ms =
        COOLDOWN_BASE_MS + rand::thread_rng().gen_range(0..=COOLDOWN_JITTER_MS);
    true
}
