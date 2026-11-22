//! Вход на анку: чат «Вы уже подключены к этому серверу».
//! Слетели: ⚡-хаб, «нет команд», transfer, disconnect. Скорборд не трогаем.

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

fn captcha_busy(state: &Arc<Mutex<BotState>>) -> bool {
    state.lock().captcha_busy
}

/// Капча в чате — не /an и не «уже на анке». Иначе сбиваем стену и солвер.
async fn wait_captcha_idle(state: &Arc<Mutex<BotState>>, max: Duration) -> bool {
    let until = Instant::now() + max;
    let mut logged = false;
    loop {
        if state.lock().banned || state.lock().shutdown {
            return false;
        }
        if !captcha_busy(state) {
            return true;
        }
        if Instant::now() >= until {
            warn!("капча → слишком долго, /an не шлю");
            return false;
        }
        if !logged {
            info!("капча → жду, /an не шлю");
            logged = true;
        }
        delay::rnd("POLL").await;
    }
}

/// Пока нет «уже подключены» — шлём /an и ждём.
/// `false` = бан / shutdown / таймаут / капча зависла.
pub async fn wait_until_on_anarchy(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> bool {
    if state.lock().banned || state.lock().shutdown {
        return false;
    }

    // Перерыв в хабе: ждём команды вернуться (бан/shutdown — выходим).
    if state.lock().parked {
        let mut logged = false;
        while state.lock().parked {
            if state.lock().banned || state.lock().shutdown {
                return false;
            }
            if !logged {
                info!("перерыв → стою в хабе");
                logged = true;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        info!("перерыв кончился → шевелю мышью и на анку");
        // Бот стоял в хабе без движений (AFK): перед /an обязательно anti-AFK жест.
        if !state.lock().banned && !state.lock().shutdown {
            crate::afk::un_afk(bot).await;
        }
    }

    if captcha_busy(state) && !wait_captcha_idle(state, Duration::from_secs(120)).await {
        return false;
    }

    if state.lock().on_anarchy {
        settle_after_join(state, &boot.go_type).await;
        return !state.lock().banned;
    }

    let mut deadline = Instant::now() + Duration::from_secs(90);

    while Instant::now() < deadline {
        if state.lock().banned || state.lock().shutdown {
            return false;
        }

        if captcha_busy(state) {
            if !wait_captcha_idle(state, Duration::from_secs(120)).await {
                return false;
            }
            deadline = Instant::now() + Duration::from_secs(90);
            continue;
        }

        if state.lock().on_anarchy {
            info!("анка an{} → уже подключены", boot.anarchy);
            settle_after_join(state, &boot.go_type).await;
            return !state.lock().banned;
        }

        crate::cmd::say(bot, format!("/an{}", boot.anarchy));
        info!("/an{} → жду «уже подключены»", boot.anarchy);

        // 3с на ответ. Нет фразы — ещё раз /an.
        // Капча — /an не шлём, пока не решат.
        let retry_at = Instant::now() + Duration::from_secs(3);

        while Instant::now() < retry_at {
            if state.lock().banned || state.lock().shutdown {
                return false;
            }

            if captcha_busy(state) {
                break;
            }

            if state.lock().on_anarchy {
                info!("анка an{} → уже подключены", boot.anarchy);
                settle_after_join(state, &boot.go_type).await;
                return !state.lock().banned;
            }

            delay::rnd("POLL").await;
        }
    }

    warn!("анка → за 90с нет «уже подключены» an{}", boot.anarchy);
    false
}

async fn settle_after_join(
    state: &Arc<Mutex<BotState>>,
    go_type: &str,
) {
    // owner не ходит на АХ — пауза «устояться» ему не нужна
    if go_type == "owner" {
        return;
    }
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

pub fn mark_anarchy_joined(boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    if state.lock().banned {
        return;
    }

    let mut st = state.lock();

    if st.captcha_busy {
        return;
    }

    if st.on_anarchy {
        return;
    }

    st.on_anarchy = true;
    st.anarchy_joined_at = Some(Instant::now());

    drop(st);

    info!("анархия {} — «уже подключены»", boot.anarchy);

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

