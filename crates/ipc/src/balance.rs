//! Пороги балансов фарм-ботов. Базовые значения заданы под мечи (KEEP 15кк); для анархии из `invis.json`
//! (зелья дешёвые) все пороги пропорционально уменьшаются до `keep_sum` (по умолчанию 5кк).
use std::sync::OnceLock;

pub const BASE_KEEP: f64 = 15_000_000.0;
const INVIS_DEFAULT_KEEP: f64 = 5_000_000.0;

static SCALE: OnceLock<f64> = OnceLock::new();

/// Вызвать один раз при старте процесса (бот и оркестратор) с номером анархии.
pub fn init(anarchy: u16) {
    let _ = SCALE.set(read_scale(anarchy));
}

fn read_scale(anarchy: u16) -> f64 {
    let Ok(raw) = std::fs::read_to_string("invis.json") else { return 1.0 };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return 1.0 };
    if v.get("enabled").and_then(|x| x.as_bool()) != Some(true) {
        return 1.0;
    }
    let listed = v
        .get("anarchies")
        .and_then(|x| x.as_array())
        .is_some_and(|a| a.iter().any(|n| n.as_u64() == Some(anarchy as u64)));
    if !listed {
        return 1.0;
    }
    let keep = v.get("keep_sum").and_then(|x| x.as_f64()).unwrap_or(INVIS_DEFAULT_KEEP);
    if keep > 0.0 { keep / BASE_KEEP } else { 1.0 }
}

pub fn scale() -> f64 {
    SCALE.get().copied().unwrap_or(1.0)
}

/// Базовый порог → порог для этой анархии.
pub fn k(base: f64) -> f64 {
    base * scale()
}
