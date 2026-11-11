//! Детерминированное расписание перерывов farm-ботов («три вставки»).
//!
//! Всё в **абсолютных** unix-мс. День по MSK (UTC+3): `day = (now_ms + 3ч) / 24ч`.
//!
//! Идея: **работа подряд — ровно 6 ч, потом перерыв 2 ч** (с джиттером ≤6.5 ч), три бота идут по кругу
//! со сдвигом так, что перерывы разных ботов не пересекаются: всегда работают минимум двое, а в
//! промежутках между перерывами — все трое. Цикл 8 ч повторяется трижды в сутки.
//!
//! Почему не длинный ночной сон: до 06.11.2026 ночью бот приносил ≈1.4 кк/ч (цены дрейфовали), но после
//! перезапуска Go с привязкой цены к рынку ночь 07.11 дала ≈5.8 кк на бота в час, почти как день. Тогда
//! выгоднее держать бота онлайн почти всё время, а перерывы ставить в самые слабые часы кривой; по
//! модели это +40% к расписанию «ночь 10 ч + день 2 ч». Если кривая снова изменится (например, ночные
//! продажи опять просядут), вернуть длинный ночной сон: копия лежит в scratchpad (`schedule.night10.*`).
//!
//! Перерывы (МСК): A 02–04, 10–12, 18–20; B 06–08, 14–16, 22–00; C 04–06, 12–14, 20–22.
//! Онлайн ≈18 ч на бота, подряд ≤6 ч.
//!
//! Почему важна именно непрерывная работа: 07.11.2026 FunTime забанил 6 из 14 ботов («п. 4.3.1 AB»,
//! 93 дня); длинные серии онлайна без отдыха — главный подозреваемый, поэтому серия ограничена жёстко.
//!
//! Роли («слоты») раздаются ботам детерминированной перестановкой от (anarchy, filename): при N=3
//! все три слота заняты. Каждая граница каждого перерыва независимо гуляет на ±`jitter_minutes`
//! (по умолчанию 30 мин), по-новому каждый день.
//!
//! **Для казначея.** Все перерывы здесь «короткие»: роль казначея уходит к другому боту заранее
//! (`next_treasurer_break_at` учитывает любые перерывы), а `keep_cover: true` не даёт оркестратору
//! усыпить последнего бодрствующего (ворота сна в `main.rs`). «Длинных» перерывов нет, но слоты
//! по-прежнему умеют `long` (ночной сон), если расписание вернут.
//!
//! Расписание задаётся в `schedule.json` (`slots`); без файла работают встроенные значения.
//!   - `next_long_break_at` — когда следующий **ночной** перерыв (начало или конец текущего);
//!     по нему тикер казначея в `main.rs` выбирает казначея «кто дольше протянет».
//!   - `next_short_break_at` — когда следующий **короткий** перерыв.
//!   - `next_break_at` — смешанный, для логов и `debug_state`.
//!   - `is_in_long_break` / `is_in_short_break` — что сейчас.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Один слот-роль: ночные и короткие перерывы в МСК, `["HH:MM", "HH:MM"]` (начало, конец).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SlotConfig {
    #[serde(default)]
    pub long: Vec<[String; 2]>,
    #[serde(default)]
    pub short: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleConfig {
    #[serde(default = "default_salt")]
    pub hash_salt: String,
    /// Джиттер ±N мин **на начало и на конец каждого перерыва**, независимо. 0 = жёстко. Дефолт 30.
    #[serde(default = "default_jitter_minutes", alias = "long_jitter_minutes")]
    pub jitter_minutes: u64,
    #[serde(default = "default_slots")]
    pub slots: Vec<SlotConfig>,
    /// `true` — оркестратор не усыпляет последнего бодрствующего бота анархии (ворота сна и подъём
    /// казначея). `false` — ночью можно уйти в ноль онлайна.
    #[serde(default = "default_keep_cover")]
    pub keep_cover: bool,
    /// Режим «цикл»: `work_min` работы, `rest_min` перерыва, цикл повторяется без привязки к суткам.
    /// Боты анархии сдвинуты по фазе на `(work+rest)/slots`. Если задан, `slots` для перерывов не
    /// используются (кроме подсчёта слотов, если `slots` в цикле не задан).
    #[serde(default)]
    pub cycle: Option<CycleCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CycleCfg {
    pub work_min: u64,
    pub rest_min: u64,
    /// Сколько фаз (обычно 3 — по числу ботов).
    #[serde(default = "default_cycle_slots")]
    pub slots: usize,
}

fn default_cycle_slots() -> usize {
    3
}

fn default_keep_cover() -> bool {
    true
}

fn default_salt() -> String {
    "default".to_string()
}

fn default_jitter_minutes() -> u64 {
    15
}

fn slot(long: &[(&str, &str)], short: &[(&str, &str)]) -> SlotConfig {
    let conv = |v: &[(&str, &str)]| {
        v.iter()
            .map(|(a, b)| [a.to_string(), b.to_string()])
            .collect::<Vec<_>>()
    };
    SlotConfig {
        long: conv(long),
        short: conv(short),
    }
}

/// Встроенные слоты (МСК): ротация «6 ч работы, 2 ч перерыв», три бота со сдвигом. См. шапку модуля.
fn default_slots() -> Vec<SlotConfig> {
    vec![
        slot(&[], &[("02:00", "04:00"), ("10:00", "12:00"), ("18:00", "20:00")]),
        slot(&[], &[("06:00", "08:00"), ("14:00", "16:00"), ("22:00", "00:00")]),
        slot(&[], &[("04:00", "06:00"), ("12:00", "14:00"), ("20:00", "22:00")]),
    ]
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            hash_salt: default_salt(),
            jitter_minutes: default_jitter_minutes(),
            slots: default_slots(),
            keep_cover: true,
            cycle: None,
        }
    }
}

impl ScheduleConfig {
    pub fn load(farm_root: &Path) -> Self {
        let path = farm_root.join("schedule.json");
        if path.is_file() {
            match std::fs::read_to_string(&path) {
                Ok(raw) => match serde_json::from_str::<ScheduleConfig>(&raw) {
                    Ok(cfg) => {
                        tracing::info!("schedule.json загружен: {:?}", cfg);
                        return cfg;
                    }
                    Err(e) => tracing::warn!("schedule.json parse: {e} — дефолты"),
                },
                Err(e) => tracing::warn!("schedule.json read: {e} — дефолты"),
            }
        } else {
            tracing::info!("schedule.json нет — дефолты");
        }
        ScheduleConfig::default()
    }
}

const DAY_MS: u64 = 24 * 3600 * 1000;
const HOUR_MS: u64 = 3600 * 1000;
const MIN_MS: u64 = 60 * 1000;
const MSK_OFFSET_MS: u64 = 3 * HOUR_MS;
/// Перерыв после джиттера не короче 2 часов.
const MIN_BREAK_MS: u64 = 2 * HOUR_MS;

/// Перерыв внутри суток: начало (минуты от 00:00 МСК), длина, ночной ли.
#[derive(Debug, Clone, Copy)]
struct BreakTpl {
    start_min: u64,
    len_min: u64,
    long: bool,
}

pub struct Schedule {
    #[allow(dead_code)]
    anarchy: u16,
    salt: u64,
    n_bots: usize,
    jitter_ms: u64,
    keep_cover: bool,
    cycle: Option<CycleCfg>,
    /// Шаблоны слотов; перерывы отсортированы по началу.
    slots: Vec<Vec<BreakTpl>>,
}

fn parse_hm(s: &str) -> Option<u64> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    if h >= 24 || m >= 60 {
        return None;
    }
    Some(h * 60 + m)
}

fn build_slots(cfg: &ScheduleConfig) -> Vec<Vec<BreakTpl>> {
    let mut out = Vec::new();
    for s in &cfg.slots {
        let mut breaks = Vec::new();
        for (list, long) in [(&s.long, true), (&s.short, false)] {
            for [a, b] in list {
                let (Some(start), Some(end)) = (parse_hm(a), parse_hm(b)) else {
                    tracing::warn!("schedule.json: плохое время перерыва {a}-{b} — пропуск");
                    continue;
                };
                let len = (end + 24 * 60 - start) % (24 * 60);
                if len == 0 {
                    continue;
                }
                breaks.push(BreakTpl {
                    start_min: start,
                    len_min: len,
                    long,
                });
            }
        }
        breaks.sort_by_key(|b| b.start_min);
        out.push(breaks);
    }
    if out.is_empty() {
        return build_slots(&ScheduleConfig::default());
    }
    out
}

impl Schedule {
    pub fn new(anarchy: u16, filename: &str, n_bots: usize, cfg: &ScheduleConfig) -> Self {
        let salt = hash_salt(anarchy, filename, &cfg.hash_salt);
        Self {
            anarchy,
            salt,
            n_bots,
            jitter_ms: cfg.jitter_minutes * MIN_MS,
            keep_cover: cfg.keep_cover,
            cycle: cfg.cycle.clone().filter(|c| c.work_min > 0 && c.rest_min > 0 && c.slots > 0),
            slots: build_slots(cfg),
        }
    }

    /// Держать ли хотя бы одного бодрствующего бота на анархии (см. `ScheduleConfig::keep_cover`).
    pub fn keep_cover(&self) -> bool {
        self.keep_cover
    }

    pub fn is_active(&self, index: usize, now_ms: u64) -> bool {
        if index >= self.n_bots {
            return true;
        }
        !self.in_break(index, now_ms, None)
    }

    /// Сейчас в ночном (длинном) перерыве?
    pub fn is_in_long_break(&self, index: usize, now_ms: u64) -> bool {
        index < self.n_bots && self.in_break(index, now_ms, Some(true))
    }

    /// Сейчас в коротком перерыве?
    pub fn is_in_short_break(&self, index: usize, now_ms: u64) -> bool {
        index < self.n_bots && self.in_break(index, now_ms, Some(false))
    }

    fn in_break(&self, index: usize, now_ms: u64, long: Option<bool>) -> bool {
        let day = day_number(now_ms);
        for d in [day.saturating_sub(1), day, day + 1] {
            for (start, len, is_long) in self.breaks_abs(index, d) {
                if long.map_or(true, |l| l == is_long) && now_ms >= start && now_ms < start + len {
                    return true;
                }
            }
        }
        false
    }

    /// Индекс слота бота — детерминированная перестановка от (anarchy, filename).
    pub fn slot_for(&self, index: usize) -> usize {
        let n = self.cycle.as_ref().map_or(self.slots.len(), |c| c.slots).max(1);
        let base = (hash_u64(self.salt, 0, 0x5A5A) % n as u64) as usize;
        (base + index) % n
    }

    /// Имя ночного окна слота для лога: `00-04` (по МСК, без джиттера).
    pub fn window_name(&self, index: usize) -> String {
        let tpl = &self.slots[self.slot_for(index)];
        match tpl.iter().find(|b| b.long) {
            Some(b) => {
                let end = (b.start_min + b.len_min) % (24 * 60);
                format!("{:02}-{:02}", b.start_min / 60, end / 60)
            }
            None => "none".to_string(),
        }
    }

    /// Компактная строка состояния для лога.
    ///
    /// `next_long_in=` и `next_short_in=` — сколько до ночного и сколько до ближайшего
    /// короткого перерыва (это разные вещи для казначея).
    pub fn debug_state(&self, index: usize, now_ms: u64) -> String {
        let wname = self.window_name(index);
        let active = self.is_active(index, now_ms);
        let next = self.next_break_at(index, now_ms);
        let next_long = self.next_long_break_at(index, now_ms);
        let next_short = self.next_short_break_at(index, now_ms);
        let mins = next.saturating_sub(now_ms) / 60_000;
        let mins_long = next_long.saturating_sub(now_ms) / 60_000;
        let mins_short = next_short.saturating_sub(now_ms) / 60_000;
        let day = day_number(now_ms);
        let now_h = ((now_ms + MSK_OFFSET_MS) / HOUR_MS) % 24;
        let now_m = ((now_ms + MSK_OFFSET_MS) / MIN_MS) % 60;
        let sleep_str = match self
            .breaks_abs(index, day)
            .into_iter()
            .find(|(_, _, long)| *long)
        {
            Some((start, len, _)) => {
                let s_h = ((start + MSK_OFFSET_MS) / HOUR_MS) % 24;
                let s_m = ((start + MSK_OFFSET_MS) / MIN_MS) % 60;
                let e = start + len;
                let e_h = ((e + MSK_OFFSET_MS) / HOUR_MS) % 24;
                let e_m = ((e + MSK_OFFSET_MS) / MIN_MS) % 60;
                format!(
                    "{:02}:{:02}-{:02}:{:02}({}м)",
                    s_h,
                    s_m,
                    e_h,
                    e_m,
                    len / 60_000
                )
            }
            None => "none".to_string(),
        };
        format!(
            "idx={index} win={wname} active={active} now={now_h:02}:{now_m:02} sleep_today={sleep_str} next_in={mins}м next_long_in={mins_long}м next_short_in={mins_short}м"
        )
    }

    /// Все перерывы бота за календарный день `day` (МСК) в абсолютных мс: (начало, длина, ночной).
    /// Начало и конец каждого независимо гуляют на ±jitter; длина после джиттера ≥ 2 ч.
    fn breaks_abs(&self, index: usize, day: u64) -> Vec<(u64, u64, bool)> {
        if let Some(c) = &self.cycle {
            return self.cycle_breaks(index, day, c);
        }
        let tpls = &self.slots[self.slot_for(index)];
        let mut out = Vec::with_capacity(tpls.len());
        for (k, b) in tpls.iter().enumerate() {
            let base_start = (day * DAY_MS + b.start_min * MIN_MS) as i64 - MSK_OFFSET_MS as i64;
            let base_end = base_start + (b.len_min * MIN_MS) as i64;
            let start = (base_start + self.jitter(index, day, k, 0)).max(0) as u64;
            let mut end = (base_end + self.jitter(index, day, k, 1)).max(0) as u64;
            if end < start + MIN_BREAK_MS {
                end = start + MIN_BREAK_MS;
            }
            out.push((start, end - start, b.long));
        }
        out
    }

    /// Режим цикла: перерывы, чьё базовое начало попадает в сутки `day` (МСК).
    /// Фаза бота = слот · период / число слотов + сдвиг анархии (хэш), отсчёт от unix-эпохи.
    /// Джиттера на границах нет: перерывы соседей стыкуются точно (один выходит — другой уходит),
    /// поэтому онлайн всегда ≥ slots−1 из slots; любой разброс дал бы моменты, когда в сети один.
    /// Сдвиг анархии нужен, чтобы смены не совпадали по всей ферме. Работа = work, перерыв = rest.
    fn cycle_breaks(&self, index: usize, day: u64, c: &CycleCfg) -> Vec<(u64, u64, bool)> {
        let work = c.work_min * MIN_MS;
        let rest = c.rest_min * MIN_MS;
        let period = work + rest;
        let anarchy_off = hash_u64(self.salt, 0xC1C1E, 0) % period;
        let phase = (self.slot_for(index) as u64 * period / c.slots.max(1) as u64 + anarchy_off) % period;
        let day_start = (day * DAY_MS) as i64 - MSK_OFFSET_MS as i64;
        let day_end = day_start + DAY_MS as i64;
        // k-й перерыв начинается в phase + work + k·period
        let first = ((day_start - (phase + work) as i64).div_euclid(period as i64)) - 1;
        let mut out = Vec::new();
        let mut k = first;
        loop {
            let base = (phase + work) as i64 + k * period as i64;
            if base >= day_end {
                break;
            }
            if base >= day_start {
                out.push((base.max(0) as u64, rest, false));
            }
            k += 1;
        }
        out
    }

    /// ±jitter в мс; `which`: 0 — начало, 1 — конец (независимо).
    fn jitter(&self, index: usize, day: u64, k: usize, which: u64) -> i64 {
        if self.jitter_ms == 0 {
            return 0;
        }
        let range = 2 * self.jitter_ms + 1;
        let tag = ((index as u64) << 16) | ((k as u64) << 4) | (which + 1);
        let h = hash_u64(self.salt, tag, day);
        (h % range) as i64 - self.jitter_ms as i64
    }

    pub fn next_transition(&self, index: usize, now_ms: u64) -> u64 {
        let active = self.is_active(index, now_ms);
        for step in 1..(24 * 60) {
            let t = now_ms + step * MIN_MS;
            if self.is_active(index, t) != active {
                return t;
            }
        }
        now_ms + DAY_MS
    }

    /// Ближайшее «начало или конец текущего» среди перерывов нужного типа (`None` — любых).
    fn next_edge(&self, index: usize, now_ms: u64, long: Option<bool>) -> u64 {
        if index >= self.n_bots {
            return now_ms + DAY_MS;
        }
        let day = day_number(now_ms);
        let mut best: Option<u64> = None;
        for d in [day.saturating_sub(1), day, day + 1] {
            for (start, len, is_long) in self.breaks_abs(index, d) {
                if !long.map_or(true, |l| l == is_long) {
                    continue;
                }
                let end = start + len;
                let edge = if now_ms < start {
                    start
                } else if now_ms < end {
                    end
                } else {
                    continue;
                };
                best = Some(best.map_or(edge, |b| b.min(edge)));
            }
        }
        best.unwrap_or(now_ms + DAY_MS)
    }

    /// Смешанный: когда ближайший перерыв любого типа (начало или конец текущего).
    /// Для логов и `debug_state`. **Не** использовать для выбора казначея — там `next_long_break_at`.
    pub fn next_break_at(&self, index: usize, now_ms: u64) -> u64 {
        self.next_edge(index, now_ms, None)
    }

    /// Когда следующий **ночной** перерыв. Если сейчас в нём — когда закончится, иначе когда начнётся.
    ///
    /// Нужен тикеру казначея: «кто дольше протянет до ночного перерыва». Короткие игнорируются —
    /// казначея в них не убивают.
    pub fn next_long_break_at(&self, index: usize, now_ms: u64) -> u64 {
        self.next_edge(index, now_ms, Some(true))
    }

    /// Ближайший перерыв **любого** типа для выбора казначея: «кто дольше всех протянет без перерыва».
    ///
    /// Раньше казначея выбирали по `next_long_break_at` (только ночной перерыв), и роль доставалась
    /// боту с самым поздним ночным перерывом на ~20 ч в сутки: казначея в коротком перерыве не убивают,
    /// и он пропускал дневной перерыв (работа подряд до 22 ч). Теперь учитываются и дневные перерывы:
    /// перед любым перерывом роль уходит к другому боту (с передачей денег), и перерыв отрабатывается.
    pub fn next_treasurer_break_at(&self, index: usize, now_ms: u64) -> u64 {
        self.next_edge(index, now_ms, None)
    }

    /// Когда следующий **короткий** перерыв (или конец текущего).
    pub fn next_short_break_at(&self, index: usize, now_ms: u64) -> u64 {
        self.next_edge(index, now_ms, Some(false))
    }

    /// Текущий ночной перерыв: (начало, конец) в абсолютных мс.
    pub fn long_break_span(&self, index: usize, now_ms: u64) -> Option<(u64, u64)> {
        if index >= self.n_bots {
            return None;
        }
        let day = day_number(now_ms);
        for d in [day + 1, day, day.saturating_sub(1)] {
            for (start, len, long) in self.breaks_abs(index, d) {
                if long && now_ms >= start && now_ms < start + len {
                    return Some((start, start + len));
                }
            }
        }
        None
    }
}

fn day_number(now_ms: u64) -> u64 {
    (now_ms + MSK_OFFSET_MS) / DAY_MS
}

fn hash_salt(anarchy: u16, filename: &str, extra: &str) -> u64 {
    fnv1a(&[
        &anarchy.to_le_bytes()[..],
        filename.as_bytes(),
        b"|",
        extra.as_bytes(),
    ])
}

fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for part in parts {
        for &b in *part {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn hash_u64(salt: u64, a: u64, b: u64) -> u64 {
    let mut h = salt;
    h ^= a.wrapping_mul(0x9E3779B97F4A7C15);
    h = h.rotate_left(31).wrapping_mul(0xBF58476D1CE4E5B9);
    h ^= b.wrapping_mul(0x94D049BB133111EB);
    h = h.rotate_left(27).wrapping_mul(0x94D049BB133111EB);
    h ^ (h >> 33)
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ScheduleConfig {
        ScheduleConfig::default()
    }

    #[test]
    fn deterministic() {
        let s1 = Schedule::new(1, "1b", 3, &cfg());
        let s2 = Schedule::new(1, "1b", 3, &cfg());
        let t = now_unix_ms();
        for i in 0..3 {
            assert_eq!(s1.is_active(i, t), s2.is_active(i, t));
        }
    }

    #[test]
    fn three_unique_slots() {
        // При N=3 все три слота должны быть заняты (перестановка).
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            let mut slots: Vec<_> = (0..3).map(|i| s.slot_for(i)).collect();
            slots.sort();
            slots.dedup();
            assert_eq!(slots.len(), 3, "анархия {an}: слоты не уникальны");
        }
    }

    #[test]
    fn always_at_least_one_awake_and_usually_two() {
        // Перерывы разных ботов стоят встык (джиттер ±15 мин может на ≤30 мин сдвинуть пару внахлёст),
        // поэтому: всегда бодрствует хотя бы один, а двое и больше — не менее 95% времени.
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            let start = 20_000u64 * DAY_MS;
            let total = 60 * 24 * 60u64;
            let mut low = 0u64;
            for step in 0..total {
                let t = start + step * MIN_MS;
                let awake = (0..3).filter(|&i| s.is_active(i, t)).count();
                assert!(awake >= 1, "анархия {an}: никого онлайн, минута {step}");
                if awake < 2 {
                    low += 1;
                }
            }
            assert!(
                (low as f64) / (total as f64) < 0.05,
                "анархия {an}: меньше двух ботов {low} из {total} минут"
            );
        }
    }

    #[test]
    fn breaks_respect_limits() {
        // Каждый перерыв 2–3 ч (после джиттера). Отдых в сутки в среднем около 6 ч.
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            for i in 0..3 {
                let mut sum = 0u64;
                let days = 200u64;
                for d in 20_000u64..20_000 + days {
                    let br = s.breaks_abs(i, d);
                    assert_eq!(br.len(), 3, "ожидаем три перерыва в сутки");
                    for &(_, len, long) in &br {
                        let (lo, hi) = (2 * HOUR_MS, 3 * HOUR_MS);
                        let _ = long;
                        assert!(
                            len >= lo && len <= hi,
                            "бот {i} день {d}: перерыв {} мин вне допуска",
                            len / MIN_MS
                        );
                        sum += len;
                    }
                }
                let avg_h = sum as f64 / days as f64 / HOUR_MS as f64;
                assert!(
                    (6.0..=6.7).contains(&avg_h),
                    "анархия {an} бот {i}: средний отдых {avg_h:.2}ч, ждём ≈6ч"
                );
            }
        }
    }

    #[test]
    fn work_blocks_are_not_too_long() {
        // Работа подряд ≤ 6 ч по шаблону, плюс джиттер ±15 мин на каждую границу (≤6.5 ч).
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            let start = 20_000u64 * DAY_MS;
            for i in 0..3 {
                let mut run = 0u64;
                let mut longest = 0u64;
                for step in 0..(60 * 24 * 60u64) {
                    let t = start + step * MIN_MS;
                    if s.is_active(i, t) {
                        run += 1;
                        longest = longest.max(run);
                    } else {
                        run = 0;
                    }
                }
                assert!(longest <= 6 * 60 + 35, "анархия {an} бот {i}: работа подряд {longest} мин");
            }
        }
    }

    #[test]
    fn active_changes_over_time() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let start = now_unix_ms();
        let mut any_change = false;
        let mut prev = [true; 3];
        for step in 0..(48 * 60) {
            let t = start + step * MIN_MS;
            for i in 0..3 {
                let a = s.is_active(i, t);
                if a != prev[i] {
                    any_change = true;
                    prev[i] = a;
                }
            }
        }
        assert!(any_change, "is_active никогда не меняется — баг");
    }

    #[test]
    fn breaks_do_not_overlap_within_bot() {
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            for d in 20_000u64..20_060 {
                for i in 0..3 {
                    let mut br = s.breaks_abs(i, d);
                    br.sort();
                    for w in br.windows(2) {
                        assert!(
                            w[0].0 + w[0].1 <= w[1].0,
                            "анархия {an} бот {i} день {d}: перерывы пересекаются"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn jitter_stays_within_limit() {
        let c = cfg();
        let s = Schedule::new(503, "503b", 3, &c);
        let lim = (c.jitter_minutes * MIN_MS) as i64;
        for d in 20_000u64..20_100 {
            for i in 0..3 {
                for k in 0..4 {
                    for which in 0..2 {
                        let j = s.jitter(i, d, k, which);
                        assert!(j.abs() <= lim, "джиттер {j} больше {lim}");
                    }
                }
            }
        }
    }

    #[test]
    fn config_parses_slots_and_legacy_keys() {
        let raw = r#"{"hash_salt":"x","long_jitter_minutes":10,"slots":[{"long":[["01:00","03:00"]],"short":[["12:00","14:00"]]}]}"#;
        let c: ScheduleConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(c.jitter_minutes, 10);
        let s = Schedule::new(1, "1b", 1, &c);
        assert_eq!(s.window_name(0), "01-03");
        // старый формат (без slots) → встроенные слоты
        let old: ScheduleConfig = serde_json::from_str(
            r#"{"long_break":{"min_hours":7},"short_breaks":{"count":1},"long_jitter_minutes":60}"#,
        )
        .unwrap();
        assert_eq!(old.slots.len(), 3);
        assert_eq!(old.jitter_minutes, 60);
    }

    #[test]
    fn next_break_in_future() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let now = now_unix_ms();
        for i in 0..3 {
            let nb = s.next_break_at(i, now);
            assert!(nb >= now, "next_break_at в прошлом");
        }
    }

    #[test]
    fn debug_state_works() {
        let s = Schedule::new(503, "503b", 3, &cfg());
        let t = now_unix_ms();
        for i in 0..3 {
            let st = s.debug_state(i, t);
            assert!(st.contains("win="));
            assert!(st.contains("active="));
            assert!(st.contains("next_long_in="));
            assert!(st.contains("next_short_in="));
        }
    }

    // ============ тесты на новые методы ============

    #[test]
    fn next_long_break_in_future() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let now = now_unix_ms();
        for i in 0..3 {
            let nb = s.next_long_break_at(i, now);
            assert!(nb >= now, "next_long_break_at в прошлом для бота {i}");
        }
    }

    #[test]
    fn next_short_break_in_future() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let now = now_unix_ms();
        for i in 0..3 {
            let nb = s.next_short_break_at(i, now);
            assert!(nb >= now, "next_short_break_at в прошлом для бота {i}");
        }
    }

    #[test]
    fn next_long_break_at_matches_in_long_break() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let day = 20000u64;
        // Для каждого бота и каждого дня: если сейчас в длинном сне,
        // next_long_break_at == конец этого сна.
        for i in 0..3 {
            for d in 0..7u64 {
                for h in 0..24u64 {
                    let t = (day + d) * DAY_MS + h * HOUR_MS - MSK_OFFSET_MS;
                    if s.is_in_long_break(i, t) {
                        let end = s.next_long_break_at(i, t);
                        assert!(end > t, "бот {i} день {d} час {h}: конец длинного сна в прошлом");
                        // Проверим, что в end уже не спим.
                        assert!(
                            !s.is_in_long_break(i, end),
                            "бот {i} день {d} час {h}: end={end} всё ещё в длинном сне"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn next_short_break_at_matches_in_short_break() {
        let s = Schedule::new(1, "1b", 3, &cfg());
        let day = 20000u64;
        for i in 0..3 {
            for d in 0..3u64 {
                for h in 0..24u64 {
                    let t = (day + d) * DAY_MS + h * HOUR_MS - MSK_OFFSET_MS;
                    if s.is_in_short_break(i, t) {
                        let end = s.next_short_break_at(i, t);
                        assert!(end > t, "бот {i} день {d} час {h}: конец короткого в прошлом");
                        assert!(
                            !s.is_in_short_break(i, end),
                            "бот {i} день {d} час {h}: end={end} всё ещё в коротком"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn next_long_is_later_or_equal_next_break() {
        // Смешанный next_break_at <= next_long_break_at (потому что
        // смешанный учитывает и короткие тоже).
        let s = Schedule::new(1, "1b", 3, &cfg());
        let now = now_unix_ms();
        for i in 0..3 {
            let mixed = s.next_break_at(i, now);
            let long = s.next_long_break_at(i, now);
            assert!(
                mixed <= long,
                "бот {i}: next_break_at ({mixed}) > next_long_break_at ({long}) — короткий должен быть раньше или равен"
            );
        }
    }

    #[test]
    fn is_in_long_and_short_mutually_exclusive_for_active() {
        // Если бот активен — он не в длинном и не в коротком.
        let s = Schedule::new(1, "1b", 3, &cfg());
        let start = now_unix_ms();
        for step in 0..(48 * 60) {
            let t = start + step * MIN_MS;
            for i in 0..3 {
                if s.is_active(i, t) {
                    assert!(!s.is_in_long_break(i, t), "бот {i} активен, но в длинном сне");
                    assert!(!s.is_in_short_break(i, t), "бот {i} активен, но в коротком");
                }
            }
        }
    }

    /// Зеркалит выбор казначея из main.rs: среди активных (не в ночном и не в дневном перерыве) берётся
    /// тот, у кого `next_treasurer_break_at` дальше всех (и до него больше 2 минут).
    fn pick_treasurer(s: &Schedule, t: u64) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for i in 0..3 {
            if !s.is_active(i, t) {
                continue;
            }
            let runup = s.next_treasurer_break_at(i, t).saturating_sub(t);
            if runup < 2 * MIN_MS {
                continue;
            }
            let nb = t + runup;
            if best.map_or(true, |(_, b)| nb > b) {
                best = Some((i, nb));
            }
        }
        best.map(|(i, _)| i)
    }

    #[test]
    fn treasurer_does_not_skip_breaks() {
        // Казначей не должен оставаться без перерыва: каждый бот отрабатывает оба перерыва в сутки,
        // работа подряд ≤ ~11 ч даже с учётом «казначея в перерыве не убивают».
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            let start = 20_000u64 * DAY_MS;
            let mut run = [0u64; 3];
            let mut longest = [0u64; 3];
            let mut handovers = 0u64;
            let mut prev: Option<usize> = None;
            let mut no_treasurer = 0u64;
            let mut treasurer_in_break = 0u64;
            let total = 30 * 24 * 60u64;
            for step in 0..total {
                let t = start + step * MIN_MS;
                let tr = pick_treasurer(&s, t);
                if tr != prev {
                    handovers += 1;
                    prev = tr;
                }
                match tr {
                    None => no_treasurer += 1,
                    Some(ti) => {
                        if !s.is_active(ti, t) {
                            treasurer_in_break += 1;
                        }
                    }
                }
                for i in 0..3 {
                    let works = s.is_active(i, t) || (tr == Some(i));
                    if works {
                        run[i] += 1;
                        longest[i] = longest[i].max(run[i]);
                    } else {
                        run[i] = 0;
                    }
                }
            }
            println!(
                "анархия {an}: смен казначея {:.1} в сутки, работа подряд до {:.1}/{:.1}/{:.1} ч",
                handovers as f64 / 30.0,
                longest[0] as f64 / 60.0,
                longest[1] as f64 / 60.0,
                longest[2] as f64 / 60.0
            );
            for i in 0..3 {
                assert!(
                    longest[i] <= 6 * 60 + 40,
                    "анархия {an} бот {i}: работа подряд {} мин (казначей пропускает перерыв?)",
                    longest[i]
                );
            }
            assert_eq!(treasurer_in_break, 0, "анархия {an}: казначей оказался в перерыве");
            assert!(
                (no_treasurer as f64) / (total as f64) < 0.02,
                "анархия {an}: казначея нет {no_treasurer} из {total} минут"
            );
            // смен казначея должно быть разумное число: не больше ~10 в сутки
            assert!(handovers <= 30 * 10, "анархия {an}: {handovers} смен казначея за 30 дней");
        }
    }

    #[test]
    fn uptime_is_about_18_hours() {
        // Рабочее время в сутки: три перерыва по ≈2 ч → ≈18 ч онлайна;
        // 
        for an in [502u16, 503, 504, 506] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg());
            let start = 20_000u64 * DAY_MS;
            let days = 60u64;
            let mut up = [0u64; 3];
            let mut awake_sum = 0u64;
            for step in 0..(days * 24 * 60) {
                let t = start + step * MIN_MS;
                let mut n = 0;
                for i in 0..3 {
                    if s.is_active(i, t) {
                        up[i] += 1;
                        n += 1;
                    }
                }
                awake_sum += n;
            }
            let total = (days * 24 * 60) as f64;
            let ups: Vec<f64> = up.iter().map(|&u| 24.0 * u as f64 / total).collect();
            let avg_awake = awake_sum as f64 / total;
            println!(
                "анархия {an}: работает {:.1}/{:.1}/{:.1} ч в сутки, ботов онлайн в среднем {:.2} из 3",
                ups[0], ups[1], ups[2], avg_awake
            );
            for u in ups {
                assert!((17.3..=18.0).contains(&u), "анархия {an}: {u:.2} ч работы в сутки");
            }
        }
    }

    fn cycle_cfg() -> ScheduleConfig {
        let mut c = ScheduleConfig::default();
        c.cycle = Some(CycleCfg { work_min: 600, rest_min: 300, slots: 3 });
        c.jitter_minutes = 15;
        c
    }

    #[test]
    fn cycle_work_le_10h_rest_ge_5h_and_two_online() {
        for an in [502u16, 503, 504, 506, 507, 508] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cycle_cfg());
            let start = 1_760_000_000_000u64;
            let mut max_work = 0u64;
            let mut min_rest = u64::MAX;
            for idx in 0..3 {
                let (mut prev, mut run_start, mut first) = (s.is_active(idx, start), start, true);
                for t in (start..start + 20 * DAY_MS).step_by(60_000) {
                    let a = s.is_active(idx, t);
                    if a != prev {
                        let len = t - run_start;
                        if !first {
                            if prev {
                                max_work = max_work.max(len);
                            } else {
                                min_rest = min_rest.min(len);
                            }
                        }
                        first = false;
                        run_start = t;
                        prev = a;
                    }
                }
            }
            assert!(max_work <= 10 * HOUR_MS, "an{an}: работа {} мин", max_work / 60_000);
            assert!(min_rest >= 5 * HOUR_MS, "an{an}: перерыв {} мин", min_rest / 60_000);
            for t in (start..start + 20 * DAY_MS).step_by(60_000) {
                let n = (0..3).filter(|&i| s.is_active(i, t)).count();
                assert!(n >= 2, "an{an}: онлайн {n} в t={t}");
            }
        }
    }

    #[test]
    fn v9_safe_14h_limits() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../schedule-variants/v9-safe-14h-optimized.json")).unwrap();
        let cfg: ScheduleConfig = serde_json::from_str(&raw).unwrap();
        let start = 1_760_000_000_000u64;
        for an in [502u16, 503, 504, 506, 507, 508] {
            let s = Schedule::new(an, &format!("{an}b"), 3, &cfg);
            let (mut max_work, mut min_rest) = (0u64, u64::MAX);
            for idx in 0..3 {
                let (mut prev, mut run_start, mut first) = (s.is_active(idx, start), start, true);
                let mut on_ms = 0u64;
                for t in (start..start + 10 * DAY_MS).step_by(60_000) {
                    let a = s.is_active(idx, t);
                    if a { on_ms += 60_000; }
                    if a != prev {
                        let len = t - run_start;
                        if !first { if prev { max_work = max_work.max(len); } else { min_rest = min_rest.min(len); } }
                        first = false; run_start = t; prev = a;
                    }
                }
                let per_day = on_ms as f64 / HOUR_MS as f64 / 10.0;
                assert!(per_day <= 14.01, "an{an} idx{idx}: {per_day} ч/сут");
            }
            assert!(max_work <= 8 * HOUR_MS, "an{an}: работа {} мин", max_work / 60_000);
            assert!(min_rest >= 5 * HOUR_MS, "an{an}: перерыв {} мин", min_rest / 60_000);
            for t in (start..start + 10 * DAY_MS).step_by(60_000) {
                assert!((0..3).any(|i| s.is_active(i, t)), "an{an}: никого онлайн t={t}");
            }
        }
    }

    #[test]
    fn cycle_anarchies_are_out_of_phase() {
        let t = 1_760_000_000_000u64;
        let sig: Vec<_> = [502u16, 503, 504, 506, 507, 508]
            .iter()
            .map(|an| {
                let s = Schedule::new(*an, &format!("{an}b"), 3, &cycle_cfg());
                s.next_break_at(0, t)
            })
            .collect();
        let mut u = sig.clone();
        u.sort();
        u.dedup();
        assert!(u.len() >= 4, "смены анархий совпадают: {sig:?}");
    }

    #[test]
    fn cycle_uptime_is_about_16_hours() {
        let s = Schedule::new(502, "1b", 3, &cycle_cfg());
        let start = 1_760_000_000_000u64;
        let n = 30 * 24 * 60u64;
        let on = (0..n).filter(|m| s.is_active(0, start + m * 60_000)).count() as f64;
        let h = on / 60.0 / 30.0;
        assert!((15.0..17.0).contains(&h), "онлайн {h} ч/сутки");
    }

    #[test]
    fn cycle_config_parses() {
        let c: ScheduleConfig = serde_json::from_str(
            r#"{"cycle":{"work_min":600,"rest_min":300},"keep_cover":true}"#,
        )
        .unwrap();
        assert_eq!(c.cycle.unwrap().slots, 3);
    }
}

