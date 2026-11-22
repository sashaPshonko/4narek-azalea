//! Порт `4narek-1.12/delay/delay.mjs` + `delay.json` + `lib/ah-buy-tempo.mjs`.
//! Без per-bot scale/jitter — все боты в одном ритме.

use std::sync::OnceLock;
use std::time::Duration;

use rand::Rng;
use serde_json::Value;

static TIMING: OnceLock<Value> = OnceLock::new();
static AH_TEMPO: OnceLock<AhTempoProfile> = OnceLock::new();

/// Темп покупки АХ — два независимых диапазона по строкам.
/// row=1 (слот 0–8)  → row1_min..=row1_max
/// row=2 (слот 9–17) → row2_min..=row2_max
#[derive(Debug, Clone, Copy)]
struct AhTempoProfile {
    row1_min: u32,
    row1_max: u32,
    row2_min: u32,
    row2_max: u32,
    row3_min: u32,
    row3_max: u32,
    row4_min: u32,
    row4_max: u32,
}

fn timing() -> &'static Value {
    TIMING.get_or_init(|| {
        serde_json::from_str(include_str!("../delay.json")).expect("delay.json")
    })
}

/// Самая короткая задержка перед кликом на лот (мс). От неё зависит окно выбора между ботами в Go
/// (lot_claims.go, 1 с): все решения о владельце лота должны дойти до клика.
pub const AH_BUY_DELAY_MIN_MS: u32 = 2000;
/// Нижняя граница для темпа из AH_BUY_TEMPO: окно выбора Go (1 с) + запас на дорогу бот↔Go.
pub const AH_BUY_DELAY_FLOOR_MS: u32 = 1200;

/// Темп покупки из окружения (эксперимент на одной анархии, ставится при запуске её оркестратора):
/// `AH_BUY_TEMPO="row1_min,row1_max,row2_min,row2_max"` в мс. Без переменной — обычный темп.
fn tempo_from_env() -> Option<AhTempoProfile> {
    let raw = std::env::var("AH_BUY_TEMPO").ok()?;
    let v: Vec<u32> = raw.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let ok = (v.len() == 4 || v.len() == 8) && v[0] >= AH_BUY_DELAY_FLOOR_MS && v[0] <= v[1] && v[2] <= v[3] && v[2] >= v[0];
    if !ok {
        tracing::warn!("AH_BUY_TEMPO={raw:?} не принят (нужно 4 числа, row1_min >= {AH_BUY_DELAY_FLOOR_MS}) — обычный темп");
        return None;
    }
    // 8 чисел: ещё строки 3 и 4 (слоты 18–26, 27–35); при 4 числах они берут диапазон 2-й строки
    let (r3, r4) = if v.len() == 8 { ((v[4], v[5]), (v[6], v[7])) } else { ((v[2], v[3]), (v[2], v[3])) };
    Some(AhTempoProfile {
        row1_min: v[0], row1_max: v[1], row2_min: v[2], row2_max: v[3],
        row3_min: r3.0, row3_max: r3.1, row4_min: r4.0, row4_max: r4.1,
    })
}

pub fn init_bot_delay_profile(_username: &str) {
    match tempo_from_env() {
        Some(p) => {
            tracing::info!("темп покупки из AH_BUY_TEMPO: {p:?}");
            let _ = AH_TEMPO.set(p);
        }
        // обычный темп; раньше был 2000–2400 / 2400–3000 + добавок 0–600 — тот же размах без второго рандома
        None => {
            let _ = AH_TEMPO.set(AhTempoProfile {
                row1_min: AH_BUY_DELAY_MIN_MS,
                row1_max: 3000,
                row2_min: 2400,
                row2_max: 3600,
                row3_min: 2400,
                row3_max: 3600,
                row4_min: 2400,
                row4_max: 3600,
            });
        }
    }
}

/// Самая короткая задержка клика у этого бота (мс).
pub fn ah_buy_min_ms() -> u32 {
    AH_TEMPO.get().map_or(AH_BUY_DELAY_MIN_MS, |p| p.row1_min)
}


fn ah_tempo() -> AhTempoProfile {
    *AH_TEMPO.get().unwrap_or(&AhTempoProfile {
        row1_min: 600,
        row1_max: 1300,
        row2_min: 1300,
        row2_max: 1700,
        row3_min: 1300,
        row3_max: 1700,
        row4_min: 1300,
        row4_max: 1700,
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
    let (min, max) = match row {
        0 | 1 => (p.row1_min, p.row1_max),
        2 => (p.row2_min, p.row2_max),
        3 => (p.row3_min, p.row3_max),
        _ => (p.row4_min, p.row4_max),
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
        v
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
/// Сколько строк АХ покупаем (по 9 слотов). По умолчанию 2; `AH_BUY_ROWS=4` — ещё строки 3 и 4.
pub fn ah_buy_rows() -> usize {
    static ROWS: OnceLock<usize> = OnceLock::new();
    *ROWS.get_or_init(|| {
        std::env::var("AH_BUY_ROWS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| (2..=4).contains(n))
            .unwrap_or(2)
    })
}
