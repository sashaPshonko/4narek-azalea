//! Ждём вход на анку: scoreboard с номером /an (как 4narek-old, май 2026).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use azalea::{entity::Position, Vec3};
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::delay;
use crate::state::BotState;

/// Первые N мс после старта воркера ⚡-рекламу хаба игнорим (как LOBBY_IGNORE_MS).
pub const LOBBY_IGNORE: Duration = Duration::from_secs(15);

/// FunTime ⚡-рассылки хаба (майская логика: → сброс анки → снова /an).
const LOBBY_BROADCAST_MARKERS: &[&str] = &[
    "⚡ Наша группа ВК vk.com/funtime",
    "⚡ Наш Телеграм t.me/funtime",
    "⚡ Наш Дискорд dd.FunTime.su",
    "⚡ Наш Сайт FunTime.su",
    "⚡ Наши сообщества и соц. сети /links",
    "⚡ Вы играете на FunTime! play.funtime.su",
    // Новые маркеры (октябрь 2026) - сервер изменил формат
    "★ FunTime.su ★",
    "Добро пожаловать на FunTime",
    "Наши социальные сети – /links",
    "Приятной игры и новых побед",
];

/// `bot.position()` паникует во время transfer — только Option.
pub fn try_y(bot: &Client) -> Option<f64> {
    bot.get_component::<Position>().map(|p| Vec3::from(&p).y)
}

pub fn try_pos(bot: &Client) -> Option<Vec3> {
    bot.get_component::<Position>().map(|p| Vec3::from(&p))
}

pub fn is_lobby_broadcast(text: &str) -> bool {
    let is_match = LOBBY_BROADCAST_MARKERS.iter().any(|m| text.contains(m));
    is_match
}

/// ⚡-реклама хаба → сброс on_anarchy (цикл АХ сам шлёт /an).
pub fn on_lobby_broadcast(state: &Arc<Mutex<BotState>>, text: &str) -> bool {
    if !is_lobby_broadcast(text) {
        return false;
    }
    let started = state.lock().worker_started_at;
    if started.elapsed() < LOBBY_IGNORE {
        return true; // съели, но не сбрасываем
    }
    if !state.lock().on_anarchy {
        return true;
    }
    let preview: String = text.trim().chars().take(50).collect();
    clear_anarchy(state, &format!("лобби⚡ «{preview}»"));
    true
}

/// Пока нет scoreboard с номером анки — шлём /an и ждём.
/// `false` = бан / shutdown / таймаут.
pub async fn wait_until_on_anarchy(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> bool {
    if state.lock().banned || state.lock().shutdown {
        return false;
    }

    if state.lock().on_anarchy {
        settle_after_join(state).await;
        return !state.lock().banned;
    }

    let deadline = Instant::now() + Duration::from_secs(90);

    while Instant::now() < deadline {
        if state.lock().banned || state.lock().shutdown {
            return false;
        }

        if state.lock().on_anarchy {
            info!("анка → scoreboard an{} ок", boot.anarchy);
            settle_after_join(state).await;
            return !state.lock().banned;
        }

        crate::cmd::say(bot, format!("/an{}", boot.anarchy));
        info!("/an{} → жду scoreboard", boot.anarchy);

        // Ждём scoreboard максимум 3 секунды.
        // Если не пришёл — снова отправляем /an.
        let retry_at = Instant::now() + Duration::from_secs(3);

        while Instant::now() < retry_at {
            if state.lock().banned || state.lock().shutdown {
                return false;
            }

            if state.lock().on_anarchy {
                info!("анка → scoreboard an{} ок", boot.anarchy);
                settle_after_join(state).await;
                return !state.lock().banned;
            }

            delay::rnd("POLL").await;
        }
    }

    warn!("анка → за 90с нет scoreboard an{}", boot.anarchy);
    false
}

async fn settle_after_join(state: &Arc<Mutex<BotState>>) {
    let joined = state.lock().anarchy_joined_at.unwrap_or_else(Instant::now);
    let wait_until = joined + delay::duration("WAIT_ANARCHY");
    let left = wait_until.saturating_duration_since(Instant::now());
    if left > Duration::from_millis(200) {
        info!("анка → пауза {}с", left.as_secs().max(1));
        let end = Instant::now() + left;
        while Instant::now() < end {
            if !state.lock().on_anarchy || state.lock().banned {
                return;
            }
            delay::rnd("POLL").await;
        }
    }
}

pub fn scoreboard_mentions_anarchy(blob: &str, anarchy: u16) -> bool {
    blob.contains(&anarchy.to_string())
}

pub fn mark_anarchy_joined(
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) {
    if state.lock().banned {
        return;
    }

    let mut st = state.lock();

    st.last_anarchy_board_at = Some(Instant::now());

    if st.on_anarchy {
        return;
    }

    st.on_anarchy = true;
    st.anarchy_joined_at = Some(Instant::now());

    drop(st);

    info!("анархия {} — scoreboard подтверждён", boot.anarchy);

    emit(&WorkerMsg {
        name: "success".into(),
        username: Some(boot.username.clone()),
        ..crate::empty_msg()
    });
}

pub fn clear_anarchy(state: &Arc<Mutex<BotState>>, why: &str) {
    let username = {
        let mut st = state.lock();
        if !st.on_anarchy {
            return;
        }
        st.on_anarchy = false;
        st.anarchy_joined_at = None;
        st.username.clone()
    };
    warn!("уход с анки ({why})");
    // Орк снимает бота с pricing-presence (хаб ≠ активный).
    emit(&WorkerMsg {
        name: "hub".into(),
        username: Some(username),
        reason: Some(why.to_string()),
        ..crate::empty_msg()
    });
}

/// Objective/display без номера анки, пока считали себя на анке → хаб.
/// SetScore не трогаем (шум на анке). Игнор первые 15с после join.
pub fn on_scoreboard_packet(
    state: &Arc<Mutex<BotState>>,
    anarchy: u16,
    blob: &str,
    is_objective_level: bool,
) {
    if scoreboard_mentions_anarchy(blob, anarchy) {
        let mut st = state.lock();
        st.last_anarchy_board_at = Some(Instant::now());
        return;
    }
    if !is_objective_level {
        return;
    }
    let (on, joined_at) = {
        let st = state.lock();
        (st.on_anarchy, st.anarchy_joined_at)
    };
    if !on {
        return;
    }
    if let Some(j) = joined_at {
        if j.elapsed() < Duration::from_secs(15) {
            return;
        }
    }
    clear_anarchy(state, &format!("scoreboard без an{anarchy}"));
}
