//! Короткие суммы для логов: 450014 → «450k», 4_300_000 → «4.3kk».

pub fn money(v: f64) -> String {
    let v = v.abs();
    if v >= 1_000_000.0 {
        let kk = v / 1_000_000.0;
        if (kk - kk.round()).abs() < 0.05 {
            format!("{}kk", kk.round() as i64)
        } else {
            format!("{kk:.1}kk")
        }
    } else if v >= 10_000.0 {
        format!("{}k", (v / 1000.0).round() as i64)
    } else if v >= 1000.0 {
        let k = v / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as i64)
        } else {
            format!("{k:.1}k")
        }
    } else {
        format!("{}", v.round() as i64)
    }
}
