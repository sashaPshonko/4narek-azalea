//! Прогрев нового ника (питомник, `BotBoot.warmup = true`).
//!
//! По данным банов (ban-history + bot-slots, 13.11–20.11): «4.3.1 AB» получают почти только ники младше
//! ~3 суток, у ботов старше — около 0,01 бана на бот-сутки против ~0,2. Гипотеза: аккаунт должен
//! «отлежаться» без нагрузки. Поэтому здесь бот просто присутствует на анархии:
//!
//! - anti-AFK жестом мыши раз в 15–60 с (те же жесты из записей руки, что у фермы);
//! - прогулка `/warp` раз в 10–15 мин (как у фермы);
//! - выброс мусора из инвентаря раз в 5–10 мин и `/rtp`, если мусор опасный.
//!
//! Чего нет: АХ, хранилища, продажи, `/pay`, роли казначея, чата. Расписание (вход/перерывы) задаёт
//! `nursery502` по тому же `schedule.json`, что и ферма.

use std::sync::Arc;
use std::time::{Duration, Instant};

use azalea::prelude::*;
use ipc::BotBoot;
use parking_lot::Mutex;
use rand::Rng;
use tracing::{info, warn};

use crate::{afk, anarchy, state::BotState, trash, warp};

const GESTURE_MIN_S: u64 = 15;
const GESTURE_MAX_S: u64 = 60;
const TRASH_MIN_S: u64 = 5 * 60;
const TRASH_MAX_S: u64 = 10 * 60;

fn rand_secs(lo: u64, hi: u64) -> Duration {
    Duration::from_secs(rand::thread_rng().gen_range(lo..=hi))
}

fn stop(state: &Arc<Mutex<BotState>>) -> bool {
    let st = state.lock();
    st.shutdown || st.banned
}

/// Спать `d`, просыпаясь раз в секунду: выйти сразу при shutdown/бане или если слетели с анархии.
async fn idle(state: &Arc<Mutex<BotState>>, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if stop(state) || !state.lock().on_anarchy {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub async fn run_loop(bot: Client, boot: Arc<BotBoot>, state: Arc<Mutex<BotState>>) {
    if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
        warn!("прогрев: нет анки — стоп");
        return;
    }
    info!("прогрев · an{} · без АХ: anti-AFK, прогулки, мусор", boot.anarchy);

    let mut promenade = warp::PromenadeState::default();
    let mut next_trash = Instant::now() + rand_secs(60, TRASH_MIN_S);

    loop {
        if stop(&state) {
            break;
        }
        if !state.lock().on_anarchy {
            warn!("прогрев: слетели с анки — снова /an");
            if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
                break;
            }
        }

        // жест мышью (он же снимает AFK, если сервер его поставил)
        afk::un_afk(&bot).await;
        state.lock().afk = false;

        warp::maybe_promenade(&bot, &mut promenade).await;

        if Instant::now() >= next_trash {
            if trash::drop_trash(&bot, &boot, &state).await {
                state.lock().need_rtp = true;
            }
            trash::do_rtp_if_needed(&bot, &state).await;
            next_trash = Instant::now() + rand_secs(TRASH_MIN_S, TRASH_MAX_S);
        }

        idle(&state, rand_secs(GESTURE_MIN_S, GESTURE_MAX_S)).await;
    }
}
