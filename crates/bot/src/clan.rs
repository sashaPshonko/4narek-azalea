//! Клан: invest/withdraw, инвайт accept, чужой клан на старте, drain+leave.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use azalea_chat::click_event::ClickEvent;
use azalea_chat::FormattedText;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use regex::Regex;
use tracing::{info, warn};

use crate::delay;
use crate::state::BotState;

const CLAN_INVEST_COOLDOWN: Duration = Duration::from_secs(3 * 60);
const CLAN_INVEST_MIN: f64 = 1_000_000.0;

pub fn save_sum(_boot: &BotBoot, state: &BotState) -> Option<f64> {
    if let Some(k) = state.capital_keep {
        if k > 0.0 {
            return Some(k.floor());
        }
    }
    let mut prices: Vec<f64> = Vec::new();
    for entry in &state.catalog {
        let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !id.ends_with("1.21") {
            continue;
        }
        let unit = entry.get("priceSell").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if unit.is_finite() && unit > 0.0 {
            prices.push(unit);
        }
    }
    if prices.is_empty() {
        return None;
    }
    prices.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    if prices.len() == 1 {
        return Some(prices[0].floor());
    }
    Some((prices[0] + prices[1]).floor())
}

pub async fn after_sell_balance(bot: &Client, boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    crate::cmd::say(bot, "/balance");
    delay::rnd("BASE_DELAY").await;

    let (balance, keep) = {
        let s = state.lock();
        (s.balance, save_sum(boot, &s))
    };
    let Some(bal) = balance else {
        warn!("clan → баланс неизвестен");
        return;
    };
    let Some(keep) = keep else {
        return;
    };

    if bal > keep {
        let invest = bal - keep;
        if invest > CLAN_INVEST_MIN {
            let ok_cd = {
                let mut s = state.lock();
                if s.last_clan_invest
                    .map(|t| t.elapsed() >= CLAN_INVEST_COOLDOWN)
                    .unwrap_or(true)
                {
                    s.last_clan_invest = Some(Instant::now());
                    true
                } else {
                    false
                }
            };
            if ok_cd {
                let sum = invest.floor() as i64;
                info!(
                    "клан → вложил {} (keep {})",
                    crate::money::money(invest),
                    crate::money::money(keep)
                );
                crate::cmd::say(bot, format!("/clan invest {sum}"));
                delay::rnd("BASE_DELAY").await;
            }
        }
    } else if bal < keep / 2.0 {
        let want = ((keep / 2.0) - bal).floor() as i64;
        if want > 0 {
            info!("клан → снял {}", crate::money::money(want as f64));
            state.lock().awaiting_withdraw = true;
            crate::cmd::say(bot, format!("/clan withdraw {want}"));
            delay::rnd("BASE_DELAY").await;
            state.lock().awaiting_withdraw = false;
        }
    }

    maybe_treasury_ok(boot, state);
}

fn maybe_treasury_ok(boot: &BotBoot, state: &Arc<Mutex<BotState>>) {
    let mut s = state.lock();
    if !s.treasury_empty_reported {
        return;
    }
    let Some(keep) = save_sum(boot, &s) else {
        return;
    };
    let Some(bal) = s.balance else {
        return;
    };
    if bal >= keep / 2.0 {
        s.treasury_empty_reported = false;
        emit(&WorkerMsg {
            name: "treasury_ok".into(),
            username: Some(boot.username.clone()),
            ..crate::empty_msg()
        });
    }
}

/// Клик `/clan accept …` из FormattedText (и fallback по Debug).
pub fn find_clan_accept_cmd(message: &FormattedText) -> Option<String> {
    let re = Regex::new(r"(?i)^/clan\s+accept\b").ok()?;
    for part in message.clone().into_iter() {
        let style = &part.get_base().style;
        let cmd = match &style.click_event {
            Some(ClickEvent::RunCommand { command }) => Some(command.as_str()),
            Some(ClickEvent::SuggestCommand { command }) => Some(command.as_str()),
            _ => None,
        };
        if let Some(command) = cmd {
            if re.is_match(command) {
                return Some(command.to_string());
            }
        }
    }
    // Azalea иногда роняет click_event при JSON — ищем в Debug
    let dbg = format!("{message:?}");
    if let Some(m) = Regex::new(r#"(?i)/clan\s+accept\s+[a-zA-Z0-9_\-]+"#)
        .ok()?
        .find(&dbg)
    {
        return Some(m.as_str().to_string());
    }
    None
}

pub fn find_clan_leave_true(message: &FormattedText) -> Option<String> {
    let re = Regex::new(r"(?i)^/clan\s+leave\s+true\b").ok()?;
    for part in message.clone().into_iter() {
        let style = &part.get_base().style;
        let cmd = match &style.click_event {
            Some(ClickEvent::RunCommand { command }) => Some(command.as_str()),
            Some(ClickEvent::SuggestCommand { command }) => Some(command.as_str()),
            _ => None,
        };
        if let Some(command) = cmd {
            if re.is_match(command) {
                return Some(command.to_string());
            }
        }
    }
    let dbg = format!("{message:?}");
    if dbg.to_lowercase().contains("/clan leave true") {
        return Some("/clan leave true".into());
    }
    None
}

pub fn on_clan_chat(text: &str, state: &Arc<Mutex<BotState>>) {
    let mut s = state.lock();
    if text.contains("Помощь по Кланам") || text.contains("Вы не состоите в клане") {
        s.not_in_clan = true;
    }
    if text.contains("Участники:") {
        if let Some(m) = Regex::new(r"(?i)\[Лидер\]\s*([a-zA-Z0-9_]{3,16})")
            .ok()
            .and_then(|re| re.captures(text))
        {
            s.clan_leader = Some(m[1].to_string());
            s.not_in_clan = false;
        }
    }
}

/// Принять инвайт (из event handler — спавн задачи).
pub async fn handle_clan_accept(
    bot: Client,
    boot: Arc<BotBoot>,
    state: Arc<Mutex<BotState>>,
    accept_cmd: String,
) {
    {
        let mut s = state.lock();
        if s.clan_accept_busy || s.owner_ban_drain {
            return;
        }
        s.clan_accept_busy = true;
    }
    info!("клан → {accept_cmd}");
    let _ = crate::safe_inv::try_close(&bot);
    delay::rnd("BASE_DELAY").await;
    crate::cmd::say(&bot, accept_cmd);
    delay::rnd("BASE_DELAY").await;
    state.lock().clan_accept_busy = false;
    let _ = (boot,);
}

/// Старт: свой овнер — ок; чужой — drain+leave; нет клана — clan_setup в Go.
pub async fn maybe_leave_foreign_clan(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) {
    drain_treasury_and_leave(bot, boot, state, true).await;
}

/// `skip_if_our_clan`: на старте не трогаем свой клан.
pub async fn drain_treasury_and_leave(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    skip_if_our_clan: bool,
) {
    if state.lock().owner_ban_leave_done && !skip_if_our_clan {
        return;
    }
    if state.lock().foreign_clan_busy {
        return;
    }
    state.lock().foreign_clan_busy = true;

    {
        let mut s = state.lock();
        s.clan_leader = None;
        s.not_in_clan = false;
        s.pending_clan_leave_confirm = None;
    }

    info!("/clan info…");
    crate::cmd::say(bot, "/clan info");
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let s = state.lock();
        if s.not_in_clan || s.clan_leader.is_some() {
            break;
        }
        drop(s);
        delay::rnd("POLL").await;
    }

    let (leader, not_in, expected) = {
        let s = state.lock();
        (
            s.clan_leader.clone(),
            s.not_in_clan,
            boot.expected_clan_owner.clone(),
        )
    };

    if not_in {
        if skip_if_our_clan {
            warn!(
                "/clan info → not_in_clan → Go clan_needed an{}",
                boot.anarchy
            );
            emit(&WorkerMsg {
                name: "clan_setup".into(),
                username: Some(boot.username.clone()),
                reason: Some("not_in_clan".into()),
                data: Some(serde_json::json!({ "anarchy": boot.anarchy })),
                ..crate::empty_msg()
            });
        } else {
            info!("клана нет — leave skip");
        }
        if state.lock().owner_ban_drain {
            state.lock().owner_ban_leave_done = true;
        }
        state.lock().foreign_clan_busy = false;
        return;
    }

    let Some(leader) = leader else {
        warn!("/clan info → нет ответа (лидер) — clan_setup не зовём");
        if state.lock().owner_ban_drain {
            state.lock().owner_ban_leave_done = true;
        }
        state.lock().foreign_clan_busy = false;
        return;
    };

    if skip_if_our_clan {
        let Some(owner) = expected else {
            warn!(
                "an{}: нет expected_clan_owner — leave skip",
                boot.anarchy
            );
            state.lock().foreign_clan_busy = false;
            return;
        };
        info!("клан лидер {leader} (ожидаем {owner})");
        if leader.eq_ignore_ascii_case(&owner) {
            state.lock().foreign_clan_busy = false;
            return;
        }
        warn!("чужой клан → казна → leave");
    } else {
        warn!("казна → leave (овнер бан / принудительно)");
    }

    for attempt in 1..=4 {
        if state.lock().banned || state.lock().shutdown {
            break;
        }
        let take = 500_000_000i64;
        info!("withdraw попытка {attempt} (~{take})");
        crate::cmd::say(bot, format!("/clan withdraw {take}"));
        delay::rnd("BASE_DELAY").await;
    }

    state.lock().pending_clan_leave_confirm = None;
    info!("/clan leave");
    crate::cmd::say(bot, "/clan leave");
    let leave_deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < leave_deadline {
        if state.lock().pending_clan_leave_confirm.is_some() {
            break;
        }
        delay::rnd("POLL").await;
    }
    let confirm = state
        .lock()
        .pending_clan_leave_confirm
        .clone()
        .unwrap_or_else(|| "/clan leave true".into());
    info!("{confirm}");
    crate::cmd::say(bot, confirm);
    delay::rnd("BASE_DELAY").await;
    if state.lock().owner_ban_drain {
        state.lock().owner_ban_leave_done = true;
    }
    state.lock().foreign_clan_busy = false;
}
