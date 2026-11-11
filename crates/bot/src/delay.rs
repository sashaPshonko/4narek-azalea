//! Порт `4narek-1.12/delay/delay.mjs` + `delay.json` + `lib/ah-buy-tempo.mjs`.
//! Без per-bot scale/jitter — все боты в одном ритме.

use std::sync::OnceLock;
use std::time::Duration;

use rand::Rng;
use serde_json::Value;

static TIMING: OnceLock<Value> = OnceLock::new();
static AH_TEMPO: OnceLock<AhTempoProfile> = OnceLock::new();
/// Личный коэффициент темпа продажи (0.85–1.35), по хешу ника: ритм у ботов разный.
static SELL_SCALE: OnceLock<f64> = OnceLock::new();

/// Темп покупки АХ — два независимых диапазона по строкам.
/// row=1 (слот 0–8)  → row1_min..=row1_max
/// row=2 (слот 9–17) → row2_min..=row2_max
#[derive(Debug, Clone, Copy)]
struct AhTempoProfile {
    row1_min: u32,
    row1_max: u32,
    row2_min: u32,
    row2_max: u32,
}

fn timing() -> &'static Value {
    TIMING.get_or_init(|| {
        serde_json::from_str(include_str!("../delay.json")).expect("delay.json")
    })
}

/// Самая короткая задержка перед кликом на лот (мс). От неё зависит окно выбора между ботами в Go
/// (lot_claims.go, 1 с): все решения о владельце лота должны дойти до клика.
pub const AH_BUY_DELAY_MIN_MS: u32 = 2000;

pub fn init_bot_delay_profile(username: &str) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in username.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let _ = SELL_SCALE.set(0.85 + (h % 1000) as f64 / 1000.0 * 0.5);
    let _ = AH_TEMPO.set(AhTempoProfile {
        row1_min: AH_BUY_DELAY_MIN_MS,
        row1_max: 2400,
        row2_min: 2400,
        row2_max: 3000,
    });
}

fn ah_tempo() -> AhTempoProfile {
    *AH_TEMPO.get().unwrap_or(&AhTempoProfile {
        row1_min: 600,
        row1_max: 1300,
        row2_min: 1300,
        row2_max: 1700,
    })
}

const SLOTS_PER_ROW: usize = 9;

/// Номер строки АХ: 1 для 0–8, 2 для 9–17 (как JS `ahRowNumber`).
pub fn ah_row_number(slot: usize) -> u32 {
    (slot / SLOTS_PER_ROW) as u32 + 1
}

/// Задержка клика buy: рандом из диапазона по строке.
/// row=1: 600–1300мс, row=2: 1300–1700мс.
pub fn ah_buy_delay_ms(slot: usize) -> u64 {
    let row = ah_row_number(slot);
    let p = ah_tempo();
    let mut rng = rand::thread_rng();
    let (min, max) = if row <= 1 {
        (p.row1_min, p.row1_max)
    } else {
        (p.row2_min, p.row2_max)
    };
    if max <= min {
        u64::from(min)
    } else {
        u64::from(rng.gen_range(min..=max))
    }
}

pub fn ah_buy_delay(slot: usize) -> Duration {
    Duration::from_millis(ah_buy_delay_ms(slot))
}

/// Миллисекунды по ключу из delay.json. Без scale/jitter профиля —
/// значение ровно из delay.json (число или rng.gen_range(min..=max)).
pub fn get_ms(key: &str) -> u64 {
    let range = timing()
        .get(key)
        .unwrap_or_else(|| panic!("нет задержки: {key}"));
    let mut rng = rand::thread_rng();
    let scale = if key.starts_with("SELL_") {
        SELL_SCALE.get().copied().unwrap_or(1.0)
    } else {
        1.0
    };
    if let Some(n) = range.as_u64() {
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
        let v = if max <= min { min } else { rng.gen_range(min..=max) };
        (v as f64 * scale).round() as u64
    }
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

/// Пауза между предметами при продаже: почти всегда небольшая, но каждый ~4-й раз человек отвлекается
/// (3–7 с). Равномерный узкий ритм у всех ботов выглядит как скрипт.
pub async fn sell_gap() {
    let ms = {
        let mut rng = rand::thread_rng();
        if rng.gen_range(0..4) == 0 {
            rng.gen_range(3000..7000)
        } else {
            rng.gen_range(300..1200)
        }
    };
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Фиксированные константы ack/попыток из old (не в delay.json).
pub const SELL_LIST_ACK_TIMEOUT_MS: u64 = 1000;
pub const SELL_SLOT_MAX_ATTEMPTS: u32 = 3;