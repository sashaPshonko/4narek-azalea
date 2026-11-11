//! Анти-AFK: только мышь. Никакой ходьбы, прыжков и прочих движений тела.
//!
//! Каждый жест генерируется заново на основе реальных записей руки
//! (4narek-1.12/motion-records, TLauncher, шаг 50 мс): `mouse-gestures.json`, 99 жестов.
//! Генератор не рисует кривую с нуля: берёт форму скорости у двух реальных жестов и смешивает
//! (у живой мыши профиль несимметричный: быстрый старт, затухающий хвост), длину и размах берёт
//! из распределения записей, добавляет неровность шагов и сводит всё в целые шаги по 0.15°
//! (GCD мыши) с точной суммой. Двух одинаковых жестов не бывает, а статистика остаётся как у
//! записи. Направление каждого следующего жеста противоположно предыдущему (по yaw всегда,
//! по pitch предпочтительно).

use std::sync::OnceLock;
use std::time::Duration;

use azalea::prelude::*;
use parking_lot::Mutex;
use rand::Rng;
use tokio::time::Instant;
use tracing::info;

use crate::delay;

const GCD_DEG: f32 = 0.15; // шаг мыши, как vanilla
const TICK_MS: u64 = 50; // тик игры: ротация уходит на сервер не чаще раза за тик

struct Gesture {
    /// Шаги мыши по тикам (1 = 0.15°).
    yaw: Vec<i32>,
    pitch: Vec<i32>,
}

fn gestures() -> &'static [Gesture] {
    static G: OnceLock<Vec<Gesture>> = OnceLock::new();
    G.get_or_init(|| {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../mouse-gestures.json")).unwrap_or_default();
        let ints = |x: &serde_json::Value| -> Vec<i32> {
            x.as_array()
                .map(|a| a.iter().filter_map(|n| n.as_i64()).map(|n| n as i32).collect())
                .unwrap_or_default()
        };
        v.get("gestures")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .map(|g| Gesture {
                        yaw: ints(&g["yaw"]),
                        pitch: ints(&g["pitch"]),
                    })
                    .filter(|g| !g.yaw.is_empty() && g.yaw.len() == g.pitch.len())
                    .collect()
            })
            .unwrap_or_default()
    })
}

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
    mouse_gesture(bot).await;
}

/// Порог «мелкого» жеста (для статистики в тестах): чистый поворот по yaw меньше этого.
#[cfg(test)]
const SMALL_YAW_DEG: f32 = 3.0;

/// Направление прошлого жеста (знак yaw, знак pitch; 0 = ещё не было). Процесс = один бот.
static LAST_DIR: Mutex<(i8, i8)> = Mutex::new((0, 0));

fn sign(v: i32) -> i8 {
    v.signum() as i8
}

fn net(units: &[i32]) -> i32 {
    units.iter().sum()
}

/// Смещения (yaw, pitch) в градусах от старта по тикам ровно как в записи; pitch можно
/// отзеркалить (нужно только чтобы не упереться в границу).
fn plan(g: &Gesture, flip_pitch: bool) -> Vec<(f32, f32)> {
    let sp = if flip_pitch { -1.0 } else { 1.0 };
    let mut cy = 0i32;
    let mut cp = 0i32;
    g.yaw
        .iter()
        .zip(&g.pitch)
        .map(|(dy, dp)| {
            cy += dy;
            cp += dp;
            (cy as f32 * GCD_DEG, cp as f32 * sp * GCD_DEG)
        })
        .collect()
}

/// Кумулятивная форма жеста по одной оси: 0 → 1 в сторону чистого смещения. `None`, если смещения нет.
fn cumulative_shape(steps: &[i32]) -> Option<Vec<f32>> {
    let total: i32 = steps.iter().sum();
    if total == 0 {
        return None;
    }
    let sgn = total.signum() as f32;
    let tot = total.abs() as f32;
    let mut acc = 0f32;
    Some(
        steps
            .iter()
            .map(|v| {
                acc += *v as f32 * sgn;
                acc / tot
            })
            .collect(),
    )
}

/// Значение кумулятивной формы в нормированный момент `t` (0..=1), линейная интерполяция;
/// форма начинается в 0 до первого тика.
fn shape_at(shape: &[f32], t: f32) -> f32 {
    let n = shape.len();
    let x = t.clamp(0.0, 1.0) * n as f32; // 0..n
    if x <= 0.0 {
        return 0.0;
    }
    let hi = (x.ceil() as usize).clamp(1, n);
    let lo = hi - 1;
    let a = if lo == 0 { 0.0 } else { shape[lo - 1] };
    let b = shape[hi - 1];
    a + (b - a) * (x - lo as f32)
}

/// Разложение целого `total` по долям `weights` (сумма долей не обязана быть 1) так, чтобы
/// сумма шагов была ровно `total`: метод наибольших остатков.
fn split_units(total: i32, weights: &[f32]) -> Vec<i32> {
    let sum: f32 = weights.iter().sum();
    if sum <= 0.0 || weights.is_empty() {
        return vec![0; weights.len()];
    }
    let target: Vec<f32> = weights.iter().map(|w| total as f32 * w / sum).collect();
    let mut out: Vec<i32> = target.iter().map(|t| t.floor() as i32).collect();
    let mut left = total - out.iter().sum::<i32>();
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by(|a, b| {
        let fa = target[*a] - target[*a].floor();
        let fb = target[*b] - target[*b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    for i in order {
        if left <= 0 {
            break;
        }
        out[i] += 1;
        left -= 1;
    }
    out
}

/// Ограничивает шаг по модулю `cap`: излишек переносится в ближайшие тики, где ещё есть запас.
/// Сумма шагов не меняется (если вся ось не помещается в `cap * n`, лишнее обрезается).
fn cap_steps(steps: &mut [i32], cap: i32) {
    for i in 0..steps.len() {
        if steps[i].abs() <= cap {
            continue;
        }
        let sgn = steps[i].signum();
        let mut excess = steps[i].abs() - cap;
        steps[i] = sgn * cap;
        // ближайшие тики по очереди: i±1, i±2, …
        for d in 1..steps.len() {
            for j in [i.wrapping_sub(d), i + d] {
                if excess == 0 {
                    break;
                }
                if j < steps.len() && steps[j].signum() != -sgn {
                    let room = cap - steps[j].abs().min(cap);
                    let take = excess.min(room);
                    steps[j] += sgn * take;
                    excess -= take;
                }
            }
            if excess == 0 {
                break;
            }
        }
    }
}

/// Приблизительно нормальная добавка (сумма трёх равномерных), σ ≈ `scale`.
fn jitter(rng: &mut impl Rng, scale: f32) -> f32 {
    (rng.gen::<f32>() + rng.gen::<f32>() + rng.gen::<f32>() - 1.5) * 2.0 * scale
}

/// Одна ось нового жеста: форма — смесь двух реальных (`a`, `b`), длина `n`, чистое смещение `total`
/// (со знаком), неровность шагов `noise`. Шаги целые, сумма ровно `total`.
fn synth_axis(
    a: &[f32],
    b: &[f32],
    w: f32,
    n: usize,
    total: i32,
    noise: f32,
    rng: &mut impl Rng,
) -> Vec<i32> {
    // доли смещения по тикам = приращения смешанной формы с неровностью
    let mut prev = 0f32;
    let mut weights = Vec::with_capacity(n);
    for k in 1..=n {
        let t = k as f32 / n as f32;
        let c = w * shape_at(a, t) + (1.0 - w) * shape_at(b, t);
        let inc = (c - prev).max(0.0);
        prev = c;
        weights.push((inc * (1.0 + jitter(rng, noise))).max(0.0));
    }
    if weights.iter().sum::<f32>() <= 0.0 {
        weights = vec![1.0; n];
    }
    let mag = split_units(total.abs(), &weights);
    let sgn = total.signum();
    mag.into_iter().map(|v| v * sgn).collect()
}

/// Новый жест на основе записей. Yaw уходит в сторону, противоположную прошлому жесту; pitch по
/// возможности тоже в другую сторону и не упирается в границу. Возвращает жест и новое направление.
fn generate(
    lib: &[Gesture],
    last: (i8, i8),
    pitch_now: f32,
    rng: &mut impl Rng,
) -> (Gesture, (i8, i8)) {
    let yaw_want: i32 = if last.0 == 0 {
        if rng.gen_bool(0.5) { 1 } else { -1 }
    } else {
        -(last.0 as i32)
    };
    // любые реальные жесты со смещением по yaw, в том числе совсем мелкие (меньше 3°):
    // у живой руки они бывают, и сторона у них тоже определена знаком смещения
    let usable: Vec<usize> = (0..lib.len()).filter(|i| net(&lib[*i].yaw) != 0).collect();
    let pool: Vec<usize> = if usable.is_empty() { (0..lib.len()).collect() } else { usable };

    let ia = pool[rng.gen_range(0..pool.len())];
    let ib = pool[rng.gen_range(0..pool.len())];
    let (ga, gb) = (&lib[ia], &lib[ib]);
    let w: f32 = rng.gen();

    // длина — между длинами двух жестов, ±1 тик
    let n_mix = w * ga.yaw.len() as f32 + (1.0 - w) * gb.yaw.len() as f32;
    let n = ((n_mix.round() as i32) + rng.gen_range(-1..=1)).clamp(3, 30) as usize;

    // размах — у одного из двух (а не среднее между ними: среднее сгладило бы и мелкие, и
    // крупные жесты), с небольшим разбросом; не меньше одного шага мыши
    let amp_src = if rng.gen_bool(0.5) { &ga.yaw } else { &gb.yaw };
    let factor = 0.85 + rng.gen::<f32>() * 0.35;
    let yaw_total = (((net(amp_src).abs() as f32) * factor).round() as i32).max(1) * yaw_want;

    let shape_a = cumulative_shape(&ga.yaw);
    let shape_b = cumulative_shape(&gb.yaw);
    let (sa, sb) = match (shape_a, shape_b) {
        (Some(a), Some(b)) => (a, b),
        // запасной путь: равномерная форма
        _ => {
            let flat: Vec<f32> = (1..=n).map(|k| k as f32 / n as f32).collect();
            (flat.clone(), flat)
        }
    };
    let mut yaw = synth_axis(&sa, &sb, w, n, yaw_total, 0.18, rng);
    // не больше 25° за тик: иначе это уже не рука (излишек уходит в соседние тики, сумма не меняется)
    let cap = (25.0 / GCD_DEG) as i32;
    cap_steps(&mut yaw, cap);

    // pitch: размах — как у случайного реального жеста; форма та же, что у yaw, но со своей
    // неровностью; знак — противоположный прошлому, если не упирается в границу
    let pitch_src = &lib[pool[rng.gen_range(0..pool.len())]];
    let mut pitch_mag = ((net(&pitch_src.pitch).abs() as f32) * (0.85 + rng.gen::<f32>() * 0.35)).round() as i32;
    let mut p_dir: i32 = if last.1 != 0 && rng.gen_bool(0.85) {
        -(last.1 as i32)
    } else if rng.gen_bool(0.5) {
        1
    } else {
        -1
    };
    let out = |p: f32| p < -48.7 || p > 31.5;
    let end = |dir: i32| pitch_now + (pitch_mag * dir) as f32 * GCD_DEG;
    if out(end(p_dir)) && !out(end(-p_dir)) {
        p_dir = -p_dir;
    } else if out(end(p_dir)) {
        pitch_mag = 0;
    }
    let mut pitch = synth_axis(&sa, &sb, w, n, pitch_mag * p_dir, 0.35, rng);
    cap_steps(&mut pitch, cap);
    let pitch_sign = if pitch_mag == 0 { last.1 } else { p_dir as i8 };

    (Gesture { yaw, pitch }, (yaw_want as i8, pitch_sign))
}

async fn mouse_gesture(bot: &Client) {
    let lib = gestures();
    if lib.is_empty() {
        return;
    }
    let Ok((yaw, pitch)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bot.direction()))
    else {
        info!("anti-AFK → мышь skip (нет direction)");
        return;
    };

    // `ThreadRng` не `Send`: всё случайное считаем ДО цикла с `await`.
    let (steps, jitter_ms) = {
        let mut rng = rand::thread_rng();
        let last = *LAST_DIR.lock();
        let (g, new_last) = generate(lib, last, pitch, &mut rng);
        *LAST_DIR.lock() = new_last;
        let steps = plan(&g, false);
        // в записи шаг между сэмплами 48–52 мс (p5–p95): тик клиента гуляет на пару мс
        let jitter: Vec<i64> = (0..steps.len()).map(|_| rng.gen_range(-2i64..=2)).collect();
        (steps, jitter)
    };

    info!(
        "anti-AFK → мышь {:+.1}° / {:+.1}° за {} тиков",
        steps.last().map(|s| s.0).unwrap_or(0.0),
        steps.last().map(|s| s.1).unwrap_or(0.0),
        steps.len()
    );

    // ровный такт 50 мс: следующий тик считаем от старта, а не от конца прошлого sleep
    let started = Instant::now();
    for (k, (dy, dp)) in steps.iter().enumerate() {
        bot.set_direction(normalize_yaw(yaw + dy), clamp_pitch(pitch + dp));
        let at_ms = (TICK_MS * (k as u64 + 1)) as i64 + jitter_ms[k];
        tokio::time::sleep_until(started + Duration::from_millis(at_ms.max(1) as u64)).await;
    }
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

fn clamp_pitch(p: f32) -> f32 {
    // old: -0.85..0.55 rad ≈ -48.7°..31.5°
    p.clamp(-48.7, 31.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn library_is_real_recorded_gestures() {
        let lib = gestures();
        assert!(lib.len() >= 50, "в записи должно быть много жестов: {}", lib.len());
        for g in lib {
            assert!(g.yaw.len() >= 3 && g.yaw.len() <= 30);
            // ни одного серверного поворота/телепорта в жесте: максимум 30° за тик
            let peak = g.yaw.iter().chain(&g.pitch).map(|x| x.abs()).max().unwrap();
            assert!(peak as f32 * GCD_DEG < 30.0);
        }
        // у живой руки жесты короткие: медиана до 12 тиков (0.6 с)
        let mut ticks: Vec<usize> = lib.iter().map(|g| g.yaw.len()).collect();
        ticks.sort();
        assert!(ticks[ticks.len() / 2] <= 12);
        // в записи есть обе стороны, иначе чередовать нечем
        assert!(lib.iter().any(|g| net(&g.yaw) > 0));
        assert!(lib.iter().any(|g| net(&g.yaw) < 0));
    }

    #[test]
    fn replay_is_exactly_the_recording() {
        for g in gestures() {
            let p = plan(g, false);
            assert_eq!(p.len(), g.yaw.len());
            let (mut cy, mut cp) = (0, 0);
            for (i, (dy, dp)) in g.yaw.iter().zip(&g.pitch).enumerate() {
                cy += dy;
                cp += dp;
                assert!((p[i].0 - cy as f32 * GCD_DEG).abs() < 1e-3);
                assert!((p[i].1 - cp as f32 * GCD_DEG).abs() < 1e-3);
                // каждый шаг кратен шагу мыши
                for v in [p[i].0, p[i].1] {
                    let k = (v / GCD_DEG).round();
                    assert!((v - k * GCD_DEG).abs() < 1e-3);
                }
            }
        }
    }

    fn med(mut v: Vec<f32>) -> f32 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    fn zero_share(gs: &[Gesture]) -> f32 {
        let (mut z, mut n) = (0usize, 0usize);
        for g in gs {
            for v in &g.yaw {
                n += 1;
                if *v == 0 {
                    z += 1;
                }
            }
        }
        z as f32 / n as f32
    }

    fn draw(n: usize, seed: u64) -> Vec<Gesture> {
        let lib = gestures();
        let mut rng = StdRng::seed_from_u64(seed);
        let mut last = (0i8, 0i8);
        let mut pitch = 0.0f32;
        let mut out = Vec::new();
        for _ in 0..n {
            let (g, nl) = generate(lib, last, pitch, &mut rng);
            pitch += net(&g.pitch) as f32 * GCD_DEG;
            last = nl;
            out.push(g);
        }
        out
    }

    #[test]
    fn generated_gestures_are_valid_mouse_input() {
        for g in draw(2000, 3) {
            assert!(g.yaw.len() >= 3 && g.yaw.len() <= 30);
            assert_eq!(g.yaw.len(), g.pitch.len());
            // шаги целые по 0.15° (тип i32), за тик не больше 25° — это не прыжок
            let peak = g.yaw.iter().chain(&g.pitch).map(|x| x.abs()).max().unwrap();
            assert!((peak as f32) * GCD_DEG <= 25.0 + 1e-3);
            assert!(net(&g.yaw) != 0, "жест без смещения по yaw");
        }
    }

    #[test]
    fn generator_never_repeats_a_gesture() {
        let gs = draw(3000, 5);
        let mut seen = std::collections::HashSet::new();
        let dup = gs.iter().filter(|g| !seen.insert((g.yaw.clone(), g.pitch.clone()))).count();
        assert!(dup * 200 < gs.len(), "повторов слишком много: {dup} из {}", gs.len());
        // и не копирует записи один в один
        let lib: std::collections::HashSet<_> =
            gestures().iter().map(|g| (g.yaw.clone(), g.pitch.clone())).collect();
        let copies = gs.iter().filter(|g| lib.contains(&(g.yaw.clone(), g.pitch.clone()))).count();
        assert!(copies * 200 < gs.len(), "копий записей: {copies}");
    }

    #[test]
    fn generated_statistics_match_the_recordings() {
        let lib: Vec<Gesture> = gestures().iter().map(|g| Gesture { yaw: g.yaw.clone(), pitch: g.pitch.clone() }).collect();
        let gen = draw(3000, 9);
        let ticks = |gs: &[Gesture]| med(gs.iter().map(|g| g.yaw.len() as f32).collect());
        let amp = |gs: &[Gesture]| med(gs.iter().map(|g| net(&g.yaw).abs() as f32 * GCD_DEG).collect());
        let (lt, gt) = (ticks(&lib), ticks(&gen));
        let (la, ga) = (amp(&lib), amp(&gen));
        assert!((gt / lt - 1.0).abs() < 0.30, "длина: запись {lt}, генератор {gt}");
        assert!((ga / la - 1.0).abs() < 0.35, "размах yaw: запись {la}°, генератор {ga}°");
        let (lz, gz) = (zero_share(&lib), zero_share(&gen));
        assert!((lz - gz).abs() < 0.15, "доля нулевых шагов: запись {lz}, генератор {gz}");
    }

    #[test]
    fn small_gestures_exist_like_in_the_recordings() {
        let small = |gs: &[Gesture]| {
            gs.iter().filter(|g| (net(&g.yaw).abs() as f32) * GCD_DEG < SMALL_YAW_DEG).count() as f32
                / gs.len() as f32
        };
        let lib: Vec<Gesture> = gestures().iter().map(|g| Gesture { yaw: g.yaw.clone(), pitch: g.pitch.clone() }).collect();
        let (ls, gs) = (small(&lib), small(&draw(3000, 13)));
        assert!(gs > 0.03, "мелких жестов почти нет: {gs}");
        assert!((ls - gs).abs() < 0.12, "доля мелких жестов: запись {ls}, генератор {gs}");
    }

    #[test]
    fn yaw_direction_always_differs_from_previous() {
        let lib = gestures();
        let mut rng = StdRng::seed_from_u64(7);
        let mut last = (0i8, 0i8);
        let mut prev_yaw = 0i8;
        for n in 0..500 {
            let (g, nl) = generate(lib, last, 0.0, &mut rng);
            let s = sign(net(&g.yaw));
            if n > 0 {
                assert_ne!(s, prev_yaw, "жест {n} пошёл в ту же сторону, что и прошлый");
            }
            prev_yaw = s;
            last = nl;
        }
    }

    #[test]
    fn pitch_goes_to_the_other_side_when_possible() {
        let lib = gestures();
        let mut rng = StdRng::seed_from_u64(11);
        let mut last = (0i8, 0i8);
        let (mut same, mut total) = (0, 0);
        for _ in 0..500 {
            let prev_p = last.1;
            let (g, nl) = generate(lib, last, 0.0, &mut rng);
            let p = sign(net(&g.pitch));
            if prev_p != 0 && p != 0 {
                total += 1;
                if p == prev_p {
                    same += 1;
                }
            }
            last = nl;
        }
        assert!(same * 4 < total, "pitch слишком часто в ту же сторону: {same}/{total}");
    }

    #[test]
    fn pitch_does_not_run_into_the_limit() {
        let lib = gestures();
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..300 {
            let (g, _) = generate(lib, (1, 1), 30.0, &mut rng);
            let end = 30.0 + net(&g.pitch) as f32 * GCD_DEG;
            assert!(end <= 31.5 + 0.2, "pitch ушёл за верхнюю границу: {end}");
        }
    }

    #[test]
    fn split_units_keeps_the_exact_sum() {
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..500 {
            let n = rng.gen_range(3..30);
            let w: Vec<f32> = (0..n).map(|_| rng.gen::<f32>()).collect();
            let total = rng.gen_range(0..400);
            assert_eq!(split_units(total, &w).iter().sum::<i32>(), total);
        }
    }
}

#[cfg(test)]
mod stats_report {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    #[ignore]
    fn print_stats() {
        let lib = gestures();
        let mut rng = StdRng::seed_from_u64(42);
        let mut last = (0i8, 0i8);
        let mut gen = Vec::new();
        let mut pitch = 0.0f32;
        for _ in 0..3000 {
            let (g, nl) = generate(lib, last, pitch, &mut rng);
            pitch += net(&g.pitch) as f32 * GCD_DEG;
            last = nl;
            gen.push(g);
        }
        let q = |mut v: Vec<f32>, p: f32| { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[((v.len() - 1) as f32 * p) as usize] };
        let rep = |name: &str, gs: &[Gesture]| {
            let t: Vec<f32> = gs.iter().map(|g| g.yaw.len() as f32).collect();
            let a: Vec<f32> = gs.iter().map(|g| net(&g.yaw).abs() as f32 * GCD_DEG).collect();
            let pk: Vec<f32> = gs.iter().map(|g| g.yaw.iter().map(|x| x.abs()).max().unwrap() as f32 * GCD_DEG).collect();
            let ap: Vec<f32> = gs.iter().map(|g| net(&g.pitch).abs() as f32 * GCD_DEG).collect();
            println!("{name:10} тиков p10/50/90 = {:.0}/{:.0}/{:.0} | |yaw|° = {:.1}/{:.1}/{:.1} | пик шага° = {:.1}/{:.1}/{:.1} | |pitch|° = {:.1}/{:.1}/{:.1}",
                q(t.clone(),0.1), q(t.clone(),0.5), q(t,0.9), q(a.clone(),0.1), q(a.clone(),0.5), q(a,0.9),
                q(pk.clone(),0.1), q(pk.clone(),0.5), q(pk,0.9), q(ap.clone(),0.1), q(ap.clone(),0.5), q(ap,0.9));
        };
        let libv: Vec<Gesture> = lib.iter().map(|g| Gesture { yaw: g.yaw.clone(), pitch: g.pitch.clone() }).collect();
        rep("записи", &libv);
        rep("генератор", &gen);
    }
}
