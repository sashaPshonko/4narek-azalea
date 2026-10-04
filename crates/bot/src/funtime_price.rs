//! Порт `lib/funtime-max-price.mjs` + фильтр по bookFloor/bookMid из Go.

/// Fallback, если Go ещё не прислал пол книги.
pub const FUNTIME_MAX_TRUST_RATIO: f64 = 0.9;
/// FunTime max ниже пола книги × это → skip (шум / кап далеко от рынка).
pub const FUNTIME_MAX_FLOOR_RATIO: f64 = 0.95;
/// FunTime min выше потолка книги × это → не set_min и не list.
pub const FUNTIME_MIN_MID_RATIO: f64 = 1.05;

pub enum FuntimeMaxDecide {
    Ok { list_price: f64 },
    Skip { reason: String, list_price: f64 },
}

pub enum FuntimeMinDecide {
    /// Поднять лот и (обычно) каталог в Go.
    Ok { list_price: f64, set_go_min: bool },
    Skip { reason: String, list_price: f64 },
}

fn apply_catalog_marker(raw: f64, catalog_sell: f64, ceil: bool) -> f64 {
    let marker = (catalog_sell as i64).rem_euclid(100) as f64;
    if ceil {
        let base = (raw / 10000.0).ceil() * 10000.0;
        return base + marker;
    }
    let mut list_price = (raw / 10000.0).floor() * 10000.0 + marker;
    if list_price > raw {
        list_price = (raw / 10000.0).floor() * 10000.0 - 100.0 + marker;
    }
    if list_price <= 0.0 {
        list_price = raw;
    }
    list_price
}

/// FunTime «Максимальная цена» — не рыночная. Не пишем в Go.
/// `book_floor` / `book_mid` — перцентили из Go (0 = нет данных).
pub fn decide_funtime_max_price(
    funtime_raw: f64,
    catalog_sell: f64,
    book_floor: f64,
    _book_mid: f64,
) -> FuntimeMaxDecide {
    if !funtime_raw.is_finite() || funtime_raw <= 0.0 {
        return FuntimeMaxDecide::Skip {
            reason: "bad_funtime_max".into(),
            list_price: 0.0,
        };
    }
    if !catalog_sell.is_finite() || catalog_sell <= 0.0 {
        return FuntimeMaxDecide::Skip {
            reason: "no_catalog".into(),
            list_price: funtime_raw,
        };
    }
    let list_price = apply_catalog_marker(funtime_raw, catalog_sell, false);

    if book_floor > 0.0 {
        let floor_gate = book_floor * FUNTIME_MAX_FLOOR_RATIO;
        if list_price < floor_gate {
            return FuntimeMaxDecide::Skip {
                reason: format!(
                    "funtime_max {list_price} < book_floor {book_floor} (×{FUNTIME_MAX_FLOOR_RATIO})"
                ),
                list_price,
            };
        }
    } else if list_price < catalog_sell * FUNTIME_MAX_TRUST_RATIO {
        return FuntimeMaxDecide::Skip {
            reason: format!(
                "funtime_max {list_price} << catalog {catalog_sell} (<{}%)",
                (FUNTIME_MAX_TRUST_RATIO * 100.0).round() as i32
            ),
            list_price,
        };
    }
    FuntimeMaxDecide::Ok { list_price }
}

/// Мин. цена FunTime → ceil к 10k + маркер; set_min в Go только если не выше book mid.
pub fn decide_funtime_min_price(
    funtime_raw: f64,
    catalog_sell: f64,
    nacenka: f64,
    book_floor: f64,
    book_mid: f64,
) -> FuntimeMinDecide {
    if !funtime_raw.is_finite() || funtime_raw <= 0.0 {
        return FuntimeMinDecide::Skip {
            reason: "bad_funtime_min".into(),
            list_price: 0.0,
        };
    }
    if !catalog_sell.is_finite() || catalog_sell <= 0.0 {
        return FuntimeMinDecide::Skip {
            reason: "no_catalog".into(),
            list_price: funtime_raw,
        };
    }
    let list_price = apply_catalog_marker(funtime_raw, catalog_sell, true) + nacenka.max(0.0);

    if book_mid > 0.0 {
        let mid_cap = book_mid * FUNTIME_MIN_MID_RATIO;
        if list_price > mid_cap {
            return FuntimeMinDecide::Skip {
                reason: format!(
                    "funtime_min {list_price} > book_mid {book_mid} (×{FUNTIME_MIN_MID_RATIO})"
                ),
                list_price,
            };
        }
    }
    // Пол есть, а min FunTime ниже пола — всё равно list (сервер требует),
    // но в Go set_min не тянем вверх от мусора ниже рынка.
    let set_go_min = if book_floor > 0.0 && list_price < book_floor * 0.85 {
        false
    } else {
        true
    };
    FuntimeMinDecide::Ok {
        list_price,
        set_go_min,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_skips_below_book_floor() {
        let d = decide_funtime_max_price(900_000.0, 998_004.0, 980_000.0, 1_200_000.0);
        match d {
            FuntimeMaxDecide::Skip { .. } => {}
            FuntimeMaxDecide::Ok { list_price } => panic!("expected skip, got ok {list_price}"),
        }
    }

    #[test]
    fn max_ok_near_floor() {
        let d = decide_funtime_max_price(1_000_000.0, 1_000_004.0, 980_000.0, 1_200_000.0);
        match d {
            FuntimeMaxDecide::Ok { list_price } => assert!(list_price <= 1_000_000.0),
            FuntimeMaxDecide::Skip { reason, .. } => panic!("skip: {reason}"),
        }
    }

    #[test]
    fn min_skips_above_book_mid() {
        let d = decide_funtime_min_price(1_500_000.0, 1_000_004.0, 300_000.0, 900_000.0, 1_200_000.0);
        match d {
            FuntimeMinDecide::Skip { .. } => {}
            FuntimeMinDecide::Ok { list_price, .. } => panic!("expected skip, got {list_price}"),
        }
    }

    #[test]
    fn min_ok_under_mid() {
        let d = decide_funtime_min_price(1_000_000.0, 1_000_004.0, 0.0, 900_000.0, 1_200_000.0);
        match d {
            FuntimeMinDecide::Ok {
                list_price,
                set_go_min,
            } => {
                assert!(list_price >= 1_000_000.0);
                assert!(set_go_min);
            }
            FuntimeMinDecide::Skip { reason, .. } => panic!("skip: {reason}"),
        }
    }
}
