//! Порт `4narek-1.12/delay/delay.mjs` + `delay.json` + `lib/ah-buy-tempo.mjs`.
//! Per-bot scale/jitter — боты не ходят в одном ритме.

use std::sync::OnceLock;
use std::time::Duration;

use rand::Rng;
use serde_json::Value;

static TIMING: OnceLock<Value> = OnceLock::new();
static PROFILE: OnceLock<DelayProfile> = OnceLock::new();
static AH_TEMPO: OnceLock<AhTempoProfile> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
struct DelayProfile {
    scale: f64,
    jitter_max: u32,
}

/// Как JS `createAhTempoProfile`: step × номер строки + extra jitter.
#[derive(Debug, Clone, Copy)]
struct AhTempoProfile {
    step_min: u32,
    step_max: u32,
    extra_jitter_max: u32,
}

fn timing() -> &'static Value {
    TIMING.get_or_init(|| {
        serde_json::from_str(include_str!("../delay.json")).expect("delay.json")
    })
}

/// FNV-1a 32-bit как `hashUsername` в bot-tempo.mjs.
fn hash_username(username: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in username.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16777619);
    }
    h
}

pub fn init_bot_delay_profile(username: &str) {
    let h = hash_username(username);
    let scale = 0.82 + f64::from(h % 45) / 100.0; // 0.82–1.26
    let jitter_max = 80 + ((h >> 4) % 320); // 80–399
    let _ = PROFILE.set(DelayProfile { scale, jitter_max });

    // bot-tempo.mjs createAhTempoProfile
    let step_min = 900 + (h % 301); // 900–1200
    let step_max = (step_min + 300 + ((h >> 3) % 401)).min(1800); // ≤1800
    let extra_jitter_max = 250 + ((h >> 5) % 550); // 250–799
    let _ = AH_TEMPO.set(AhTempoProfile {
        step_min,
        step_max,
        extra_jitter_max,
    });
}

fn ah_tempo() -> AhTempoProfile {
    *AH_TEMPO.get().unwrap_or(&AhTempoProfile {
        step_min: 900,
        step_max: 1800,
        extra_jitter_max: 0,
    })
}

const SLOTS_PER_ROW: usize = 9;

/// Номер строки АХ: 1 для 0–8, 2 для 9–17 (как JS `ahRowNumber`).
pub fn ah_row_number(slot: usize) -> u32 {
    (slot / SLOTS_PER_ROW) as u32 + 1
}

/// Задержка клика buy: step × строка + jitter профиля (как JS `ahBuyDelayMs`).
/// Без общего scale из delay.json — отдельный темп.
pub fn ah_buy_delay_ms(slot: usize) -> u64 {
    let row = ah_row_number(slot);
    let p = ah_tempo();
    let mut rng = rand::thread_rng();
    let step = if p.step_max <= p.step_min {
        p.step_min
    } else {
        rng.gen_range(p.step_min..=p.step_max)
    };
    let jitter = if p.extra_jitter_max > 0 {
        rng.gen_range(0..=p.extra_jitter_max)
    } else {
        0
    };
    u64::from(step) * u64::from(row) + u64::from(jitter)
}

pub fn ah_buy_delay(slot: usize) -> Duration {
    Duration::from_millis(ah_buy_delay_ms(slot))
}

fn profile() -> DelayProfile {
    *PROFILE.get().unwrap_or(&DelayProfile {
        scale: 1.0,
        jitter_max: 0,
    })
}

/// Миллисекунды по ключу из delay.json (+ scale/jitter профиля).
pub fn get_ms(key: &str) -> u64 {
    let range = timing()
        .get(key)
        .unwrap_or_else(|| panic!("нет задержки: {key}"));
    let mut rng = rand::thread_rng();
    let base = if let Some(n) = range.as_u64() {
        n
    } else if let Some(n) = range.as_i64() {
        n.max(0) as u64
    } else {
        let min = range
            .get("min")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
            .unwrap_or(0);
        let max = range
            .get("max")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
            .unwrap_or(min);
        if max <= min {
            min
        } else {
            rng.gen_range(min..=max)
        }
    };
    let p = profile();
    let jitter = if p.jitter_max > 0 {
        rng.gen_range(0..=p.jitter_max) as u64
    } else {
        0
    };
    ((base as f64) * p.scale).round().max(0.0) as u64 + jitter
}

pub fn duration(key: &str) -> Duration {
    Duration::from_millis(get_ms(key))
}

pub async fn rnd(key: &str) {
    tokio::time::sleep(duration(key)).await;
}

/// Как old `rndPoll`: sleep кусками, abort → false.
pub async fn rnd_poll(key: &str, interval_ms: u64, mut should_abort: impl FnMut() -> bool) -> bool {
    let mut remaining = get_ms(key);
    let step = interval_ms.max(50);
    while remaining > 0 {
        if should_abort() {
            return false;
        }
        let chunk = remaining.min(step);
        tokio::time::sleep(Duration::from_millis(chunk)).await;
        remaining = remaining.saturating_sub(chunk);
    }
    !should_abort()
}

/// Фиксированные константы ack/попыток из old (не в delay.json).
pub const SELL_LIST_ACK_TIMEOUT_MS: u64 = 1000;
pub const SELL_SLOT_MAX_ATTEMPTS: u32 = 3;
