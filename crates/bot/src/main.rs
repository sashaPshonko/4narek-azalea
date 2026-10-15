//! Worker ≈ 4narek-old на Azalea (process-per-bot).
//! Vanilla physics/keepalive — штатные Azalea; без патчей mineflayer.

mod afk;
mod ah;
mod anarchy;
mod boot;
mod captcha;
mod clan;
mod cmd;
mod delay;
mod funtime_price;
mod item_parse;
mod listing;
mod money;
mod safe_inv;
mod sell;
mod state;
mod storage;
mod trash;
mod warp;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use azalea::{
    prelude::*,
    protocol::{connect::Proxy, packets::game::ClientboundGamePacket},
    JoinOpts,
};
use ipc::{emit, emit_text, WorkerMsg};
use regex::Regex;
use serde_json::json;
use socks5_impl::protocol::UserKey;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = boot::read_boot().await?;
    ipc::pretty_log::init_bot(&boot.username);
    delay::init_bot_delay_profile(&boot.username);
    info!("старт · an{} · {}", boot.anarchy, boot.item);

    let g = boot::global();
    tokio::spawn(boot::stdin_loop(g.state.clone()));

    let proxy = parse_proxy(&boot.proxy)?;
    let account = Account::offline(&boot.username);
    let opts = JoinOpts::new().server_proxy(proxy);
    let server = boot.server.clone();

    ClientBuilder::new()
        .set_handler(handle)
        .start_with_opts(account, server.as_str(), opts)
        .await;
    Ok(())
}

#[derive(Clone, Component, Default)]
struct HandleState {
    phase: Arc<AtomicU8>,
    ah_armed: Arc<AtomicBool>,
}

fn parse_proxy(url: &str) -> anyhow::Result<Proxy> {
    let raw = if url.contains("://") {
        url.to_string()
    } else {
        format!("socks5://{url}")
    };
    let u = url::Url::parse(&raw)?;
    let host = u.host_str().unwrap_or("127.0.0.1");
    let port = u.port().unwrap_or(1080);
    let addr = format!("{host}:{port}").parse()?;
    let auth = match (u.username(), u.password()) {
        (user, Some(pass)) if !user.is_empty() => {
            Some(UserKey::new(user, pass.to_string()))
        }
        _ => None,
    };
    Ok(Proxy::new(addr, auth))
}

async fn handle(bot: Client, event: Event, state: HandleState) -> anyhow::Result<()> {
    let g = boot::global();
    match event {
        Event::Spawn => {
            let boot = g.boot.clone();
            info!("логин → /reg + /l");
            crate::cmd::say(&bot, format!("/reg {}", boot.password));
            crate::cmd::say(&bot, format!("/l {}", boot.password));
            state.phase.store(1, Ordering::SeqCst);

            let bot2 = bot.clone();
            let phase = state.phase.clone();
            let ah_armed = state.ah_armed.clone();
            let inner = g.state.clone();
            let boot2 = boot.clone();
            // Один farm-поток на бота. Повторный Spawn (hub→анка) не плодит второй.
            if ah_armed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("boot rt");
                // Boot один раз; АХ-цикл перезапускаем после panic (пропал Inventory и т.п.).
                let boot_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(async {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        if phase.load(Ordering::SeqCst) < 2 {
                            phase.store(2, Ordering::SeqCst);
                        }
                        if !anarchy::wait_until_on_anarchy(&bot2, &boot2, &inner).await {
                            if inner.lock().banned {
                                warn!("бан — АХ не стартую");
                            } else {
                                warn!("на анку не зашли — АХ не стартую");
                            }
                            return false;
                        }
                        // Как old.mjs: 5с на анке без «чтобы двигаться» / «привяжите».
                        // Сразу verified → Go снимал needsBind и фронт врал «все привязаны».
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        {
                            let st = inner.lock();
                            if st.banned || st.shutdown || st.funauth_restarting {
                                return false;
                            }
                        }
                        if !inner.lock().funauth_verified {
                            inner.lock().funauth_verified = true;
                            emit(&WorkerMsg {
                                name: "funauth_verified".into(),
                                username: Some(boot2.username.clone()),
                                ..empty_msg()
                            });
                        }
                        phase.store(3, Ordering::SeqCst);
                        clan::maybe_leave_foreign_clan(&bot2, &boot2, &inner).await;
                        !(inner.lock().banned || inner.lock().shutdown)
                    })
                }));
                match boot_ok {
                    Ok(false) => {
                        ah_armed.store(false, Ordering::SeqCst);
                        return;
                    }
                    Err(e) => {
                        warn!("farm boot panic: {e:?}");
                        ah_armed.store(false, Ordering::SeqCst);
                        return;
                    }
                    Ok(true) => {}
                }

                loop {
                    if inner.lock().banned || inner.lock().shutdown {
                        break;
                    }
                    // ждём Inventory после transfer/respawn
                    for _ in 0..50 {
                        if safe_inv::ready(&bot2) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    let bot_loop = bot2.clone();
                    let boot_loop = boot2.clone();
                    let inner_loop = inner.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        rt.block_on(ah::run_loop(bot_loop, boot_loop, inner_loop));
                    }));
                    if inner.lock().banned || inner.lock().shutdown {
                        break;
                    }
                    let wait = match &result {
                        Ok(()) => {
                            warn!("АХ цикл вышел — рестарт через 3с");
                            Duration::from_secs(3)
                        }
                        Err(e) => {
                            warn!("farm thread panic: {e:?} — рестарт через 5с");
                            Duration::from_secs(5)
                        }
                    };
                    std::thread::sleep(wait);
                }
                ah_armed.store(false, Ordering::SeqCst);
            });
        }
        Event::Chat(m) => {
            let text = m.content();
            if !text.is_empty() {
                // все сообщения сервера / игроков — как в old 💬
                ipc::pretty_log::line(
                    &g.boot.username,
                    ipc::pretty_log::LineKind::Chat,
                    &text,
                );
                handle_chat(&bot, &text);
            }
            // инвайт / leave true — из click_event (после патча azalea-chat)
            let formatted = m.message();
            if let Some(accept) = clan::find_clan_accept_cmd(&formatted) {
                let bot2 = bot.clone();
                let boot2 = g.boot.clone();
                let st = g.state.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("clan accept rt");
                    rt.block_on(clan::handle_clan_accept(bot2, boot2, st, accept));
                });
            }
            if let Some(leave) = clan::find_clan_leave_true(&formatted) {
                g.state.lock().pending_clan_leave_confirm = Some(leave);
            }
        }
        Event::Packet(packet) => match packet.as_ref() {
            ClientboundGamePacket::OpenScreen(p) => {
                g.state.lock().last_window_title = p.title.to_string();
                g.state.lock().last_window_id = p.container_id;
            }
            ClientboundGamePacket::ContainerSetContent(p) if p.container_id != 0 => {
                g.state.lock().last_window_id = p.container_id;
                g.state.lock().window_tick = Some(std::time::Instant::now());
            }
            ClientboundGamePacket::Transfer(_) => {
                anarchy::clear_anarchy(&g.state, "transfer");
            }
            ClientboundGamePacket::SetObjective(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), true);
            }
            ClientboundGamePacket::SetDisplayObjective(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), true);
            }
            ClientboundGamePacket::SetScore(p) => {
                // только touch last_anarchy_board / join — не сброс (шум)
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), false);
            }
            ClientboundGamePacket::TabList(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), false);
            }
            ClientboundGamePacket::SetTitleText(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), false);
            }
            ClientboundGamePacket::SetSubtitleText(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), false);
            }
            ClientboundGamePacket::BossEvent(p) => {
                maybe_scoreboard_anarchy(&bot, &format!("{p:?}"), false);
            }
            other => {
                captcha::on_packet(&g.captcha_maps, other);
            }
        },
        Event::Disconnect(reason) => {
            let r = format!("{reason:?}");
            warn!("disconnect {r}");
            let low = r.to_lowercase();
            if low.contains("забанен") || low.contains("banned") {
                mark_banned(&r);
            } else if is_wrong_password_text(&r) {
                warn!("неверный пароль (kick) → exit 71");
                emit(&WorkerMsg {
                    name: "auth_fault".into(),
                    username: Some(g.boot.username.clone()),
                    reason: Some(r.clone()),
                    data: Some(json!({ "kind": "bad_password" })),
                    ..empty_msg()
                });
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(400));
                    std::process::exit(71);
                });
            }
            emit(&WorkerMsg {
                name: "kicked".into(),
                username: Some(g.boot.username.clone()),
                reason: Some(r),
                ..empty_msg()
            });
        }
        _ => {}
    }
    Ok(())
}

fn maybe_scoreboard_anarchy(bot: &Client, blob: &str, objective_level: bool) {
    let g = boot::global();
    let an = g.boot.anarchy;
    if anarchy::scoreboard_mentions_anarchy(blob, an) {
        anarchy::mark_anarchy_joined(&g.boot, &g.state);
        return;
    }
    // уход в хаб: objective/display без номера анки
    anarchy::on_scoreboard_packet(&g.state, an, blob, objective_level);
}

fn mark_banned(reason: &str) {
    let g = boot::global();
    {
        let mut st = g.state.lock();
        if st.banned {
            return;
        }
        st.banned = true;
        st.shutdown = true;
        st.on_anarchy = false;
    }
    warn!("бан: {reason}");
    emit(&WorkerMsg {
        name: "banned".into(),
        username: Some(g.boot.username.clone()),
        reason: Some(reason.to_string()),
        ..empty_msg()
    });
    // орк подхватит exit и не рестартит (manual_stop)
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(500));
        std::process::exit(0);
    });
}

fn handle_chat(bot: &Client, text: &str) {
    let g = boot::global();
    let low = text.to_lowercase();
    let pass = &g.boot.password;
    let user = &g.boot.username;

    clan::on_clan_chat(text, &g.state);

    // проверка читов → только в Go (без idle / AnyDesk)
    if is_staff_check_text(text) {
        warn!("проверка читов → Go");
        emit(&WorkerMsg {
            name: "staff_check".into(),
            username: Some(user.clone()),
            reason: Some(text.chars().take(1500).collect()),
            data: Some(json!({ "anarchy": g.boot.anarchy })),
            ..empty_msg()
        });
        return;
    }

    // май: ⚡ FunTime-реклама хаба → сброс анки → цикл снова /an
    if anarchy::on_lobby_broadcast(&g.state, text) {
        return;
    }

    // лимобо/хаб: команды недоступны
    if text.contains("Здесь нет команд") {
        anarchy::clear_anarchy(&g.state, "нет команд");
        return;
    }

    if low.contains("введите номер с картинки")
        || (text.contains("BotFilter >>") && low.contains("картинк"))
    {
        let start = {
            let mut st = g.state.lock();
            if st.captcha_busy {
                false
            } else {
                st.captcha_busy = true;
                st.captcha_ok = false;
                st.captcha_wrong = false;
                true
            }
        };
        if start {
            emit_text(&format!("{user} - решаю капчу"));
            emit(&WorkerMsg {
                name: "captcha".into(),
                username: Some(user.clone()),
                reason: Some("solving".into()),
                ..empty_msg()
            });
            let bot2 = bot.clone();
            let maps = g.captcha_maps.clone();
            let pass = pass.clone();
            let user2 = user.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("captcha rt");
                let res = rt.block_on(captcha::handle_captcha_login(
                    &bot2, &maps, &pass, &user2,
                ));
                let g = boot::global();
                g.state.lock().captcha_busy = false;
                match res {
                    Ok(()) => {
                        emit_text(&format!("{user2} - капча решена"));
                    }
                    Err(e) => {
                        warn!("капча fail: {e:#}");
                        emit_text(&format!("{user2} - капча fail: {e}"));
                        emit(&WorkerMsg {
                            name: "captcha".into(),
                            username: Some(user2),
                            reason: Some(format!("fail:{e}")),
                            ..empty_msg()
                        });
                    }
                }
            });
        }
    } else if low.contains("проверка пройдена") {
        let mut st = g.state.lock();
        st.captcha_busy = false;
        st.captcha_ok = true;
        st.captcha_wrong = false;
        drop(st);
        emit_text(&format!("{user} - капча ок"));
    } else if low.contains("капчу неправильно") {
        g.state.lock().captcha_wrong = true;
        emit_text(&format!("{user} - капча неправильно"));
    } else if low.contains("зарегистрируйтесь") || text.contains("/reg <") {
        if g.state.lock().captcha_busy {
            return;
        }
        crate::cmd::say(bot, format!("/reg {pass}"));
        crate::cmd::say(bot, format!("/l {pass}"));
    } else if low.contains("авторизируйтесь") || text.contains("/l <") || text.contains("/login") {
        if g.state.lock().captcha_busy {
            return;
        }
        crate::cmd::say(bot, format!("/l {pass}"));
    } else if low.contains("вы забанены") {
        mark_banned(text);
    } else if is_wrong_password_text(text) {
        warn!("неверный пароль → exit 71");
        emit_text("неверный пароль");
        emit(&WorkerMsg {
            name: "auth_fault".into(),
            username: Some(user.clone()),
            reason: Some(text.chars().take(500).collect()),
            data: Some(json!({ "kind": "bad_password" })),
            ..empty_msg()
        });
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            std::process::exit(71);
        });
    } else if low.contains("не хватает монет") {
        // как JS: needAdd → sell + новый /ah, не treasury_empty
        emit_text("не хватает монет");
        g.state.lock().need_sell = true;
    } else if low.contains("уже онлайн") {
        emit_text("ник уже онлайн");
    } else if low.contains("подтвердите вход")
        || (low.contains("личные сообщения") && low.contains("подтвердите"))
    {
        // Ник уже привязан к TG — только /2fa.
        {
            let mut st = g.state.lock();
            if st.funauth_restarting {
                return;
            }
            st.funauth_restarting = true;
            st.shutdown = true;
        }
        emit_text("funauth 2fa → рестарт воркера");
        emit(&WorkerMsg {
            name: "funauth_2fa".into(),
            username: Some(user.clone()),
            ..empty_msg()
        });
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            std::process::exit(0);
        });
    } else if low.contains("чтобы двигаться") || low.contains("привяжите аккаунт") {
        // Акк ещё не привязан к TG/ВК — Go делает FunAuth /bind (новый TG).
        // funauth_restarting рвёт 5с-таймер verified (см. spawn после wait_until_on_anarchy).
        {
            let mut st = g.state.lock();
            if st.funauth_restarting {
                return;
            }
            st.funauth_verified = false;
            st.funauth_restarting = true;
            st.shutdown = true;
        }
        emit_text("funauth bind → рестарт воркера");
        emit(&WorkerMsg {
            name: "funauth_bind".into(),
            username: Some(user.clone()),
            ..empty_msg()
        });
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            std::process::exit(0);
        });
    } else if text.contains("Данная команда недоступна в режиме AFK") {
        g.state.lock().afk = true;
        ipc::pretty_log::line(user, ipc::pretty_log::LineKind::Afk, "сервер сказал AFK");
    } else if low.contains("не так быстро")
        || low.contains("уже купили")
        || low.contains("этот товар уже купили")
    {
        // АХ browse И хранилище: GUI часто остаётся со старым слотом → reload 50
        g.state.lock().need_reload_ah = true;
    } else if text.contains("не можете выкидывать этот предмет") {
        g.state.lock().has_dangerous_trash = true;
        g.state.lock().need_rtp = true;
    } else if text.contains("переставлять предметы раз в минуту") {
        let sec = Regex::new(r"(?i)Подождите\s+(\d+)\s*сек")
            .ok()
            .and_then(|re| {
                re.captures(text)
                    .and_then(|c| c.get(1))
                    .and_then(|m| m.as_str().parse::<u64>().ok())
            })
            .unwrap_or(60);
        g.state.lock().storage_unlist_until =
            Some(std::time::Instant::now() + Duration::from_secs(sec + 1));
        warn!("хранилище → CD перестановки {sec}с");
    } else if text.contains("[✔] Предметы успешно перевыставлены!") {
        let metas: Vec<listing::ListingMeta> = {
            let mut st = g.state.lock();
            st.last_storage_reset = std::time::Instant::now();
            st.need_reset = false;
            st.listings.all_metas()
        };
        if metas.is_empty() {
            warn!("storage reset: память пуста — try-sell слать нечего");
        } else {
            info!("storage reset: try-sell на {} предмет(ов)", metas.len());
            for meta in metas {
                emit(&WorkerMsg {
                    name: "try-sell".into(),
                    username: Some(user.clone()),
                    id: Some(meta.catalog_id),
                    price: Some(meta.price),
                    ..empty_msg()
                });
            }
        }
    } else if text.contains("[☃] Вы успешно купили") {
        let from_chat = parse_chat_price(text);
        let mut st = g.state.lock();
        let known = st.buying_item.price;
        let price = if known > 0.0 && known.is_finite() {
            known
        } else {
            from_chat.unwrap_or(0.0)
        };
        let id = st.buying_item.id.clone();
        if id.is_empty() {
            warn!("buy confirm без BuyingItem (price={price}) — skip");
        } else {
            let enchants = st.buying_item.enchants.clone();
            let durability = st.buying_item.durability;
            st.buying_item = state::BuyingItem::default();
            st.need_sell = true;
            drop(st);
            let dur_pct = durability.map(|d| (d * 100.0).round() as i32);
            info!(
                "{}",
                ipc::pretty_log::ah_buy_line(
                    "купил",
                    &id,
                    &crate::money::money(price),
                    dur_pct,
                )
            );
            emit(&WorkerMsg {
                name: "buy".into(),
                username: Some(user.clone()),
                id: Some(id),
                price: Some(price),
                data: Some(json!({ "enchants": enchants, "durability": durability })),
                ..empty_msg()
            });
        }
    } else if text.contains("[☃] У Вас купили") {
        let price = parse_chat_price(text).unwrap_or(0.0);
        let meta = {
            let mut st = g.state.lock();
            st.need_sell = true;
            st.listings.take_sold(price)
        };
        if let Some(meta) = meta {
            emit(&WorkerMsg {
                name: "sell".into(),
                username: Some(user.clone()),
                id: Some(meta.catalog_id),
                price: Some(price),
                data: Some(json!({
                    "enchants": meta.enchants,
                    "durability": meta.durability,
                })),
                ..empty_msg()
            });
        } else {
            warn!(
                "sell: нет памяти listing id={} price={price}",
                if price.is_finite() {
                    (price as i64 % 10).to_string()
                } else {
                    "?".into()
                }
            );
        }
    } else if text.contains("[☃] Не удалось выставить")
        || text.contains("переполнено Хранилище")
    {
        g.state.lock().sell_ack = state::SellAck::Full;
    } else if text.contains("Вы не можете продать Воздух") {
        g.state.lock().sell_ack = state::SellAck::Empty;
    } else if text.contains("[☃] Вы пытаетесь выставить")
        && text.contains("слишком дорого! Введите команду продажи ещё раз, чтобы подтвердить!")
    {
        g.state.lock().sell_ack = state::SellAck::Confirm;
    } else if text.contains("[☃] Максимальная цена") {
        handle_funtime_max(text);
    } else if text.contains("[☃] Минимальная цена") {
        handle_funtime_min(text);
    } else if text.contains("выставлен на продажу за") {
        if let Some(p) = parse_try_sell_price(text) {
            let (id, confirmed) = {
                let mut st = g.state.lock();
                st.last_try_sell_price = Some(p);
                let conf = st.listings.confirm_pending(p);
                if let Some(ref m) = conf {
                    listing::mirror_op(
                        &g.boot.username,
                        "confirm",
                        json!({ "price": p }),
                    );
                    (Some(m.catalog_id.clone()), true)
                } else {
                    (None, false)
                }
            };
            if confirmed {
                if let Some(id) = id {
                    emit(&WorkerMsg {
                        name: "try-sell".into(),
                        username: Some(user.clone()),
                        id: Some(id),
                        price: Some(p),
                        ..empty_msg()
                    });
                }
            } else {
                warn!("try-sell: не подтвердился listing id={} price={p}", p as i64 % 10);
            }
            g.state.lock().sell_ack = state::SellAck::Ok;
        }
    } else if text.contains("[☃] Предмет") && (low.contains("выстав") || low.contains("продаж"))
    {
        g.state.lock().sell_ack = state::SellAck::Ok;
    } else if text.contains("[⚔] Ошибка: Баланс казны меньше") {
        g.state.lock().treasury_empty_reported = true;
        emit(&WorkerMsg {
            name: "treasury_empty".into(),
            username: Some(user.clone()),
            ..empty_msg()
        });
    } else if let Some(trea) = parse_clan_treasury(text) {
        emit(&WorkerMsg {
            name: "treasury".into(),
            username: Some(user.clone()),
            balance: Some(trea),
            ..empty_msg()
        });
    } else if let Some(bal) = parse_balance(text) {
        g.state.lock().balance = Some(bal);
        emit(&WorkerMsg {
            name: "balance".into(),
            username: Some(user.clone()),
            balance: Some(bal),
            ..empty_msg()
        });
    } else if text.contains("[⚔] Помощь по Кланам:") {
        let awaiting_withdraw = g.state.lock().awaiting_withdraw;
    
        if awaiting_withdraw {
            info!("клан → /clan withdraw не принят → запускаю clan_setup");
    
            g.state.lock().awaiting_withdraw = false;
    
            emit(&WorkerMsg {
                name: "clan_setup".into(),
                username: Some(user.clone()),
                reason: Some("withdraw_help".into()),
                data: Some(json!({
                    "anarchy": g.boot.anarchy,
                })),
                ..empty_msg()
            });
        }
    }
}

fn is_wrong_password_text(text: &str) -> bool {
    let s = text.to_lowercase();
    s.contains("неправильный пароль")
        || s.contains("неверный пароль")
        || s.contains("wrong password")
        || s.contains("incorrect password")
}

fn parse_clan_treasury(text: &str) -> Option<f64> {
    if !text.contains("Баланс клана") && !text.to_lowercase().contains("баланс клана") {
        return None;
    }
    let re = Regex::new(r"(?i)Баланс\s+клана:\s*([\d.\s\u00a0,$]+)").ok()?;
    let caps = re.captures(text)?;
    let raw = caps.get(1)?.as_str();
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<f64>().ok().filter(|n| *n >= 0.0)
}

fn handle_funtime_max(text: &str) {
    use crate::funtime_price::{decide_funtime_max_price, FuntimeMaxDecide};
    let g = boot::global();
    let raw = parse_chat_price(text);
    let mut st = g.state.lock();
    let catalog_sell = st.held_sell_price.unwrap_or(0.0);
    let book_floor = st.held_book_floor.unwrap_or(0.0);
    let book_mid = st.held_book_mid.unwrap_or(0.0);
    let id = st.held_sell_id.clone().unwrap_or_default();
    let held_dur = st.held_durability.unwrap_or(1.0);
    let Some(raw) = raw else {
        st.sell_ack = state::SellAck::Skip;
        return;
    };
    // как JS: прочность < 90% → skip слот, не долбим FunTime max
    if held_dur < 0.9 {
        info!(
            "макс. цена: прочность {}% — skip слот",
            (held_dur * 100.0).floor() as i32
        );
        st.need_sell_price = None;
        st.sell_ack = state::SellAck::Skip;
        return;
    }
    match decide_funtime_max_price(raw, catalog_sell, book_floor, book_mid) {
        FuntimeMaxDecide::Ok { list_price } => {
            info!("макс. цена FunTime ≈ книга/каталог → retry {id} @ {list_price}");
            st.need_sell_price = Some(list_price);
            st.sell_ack = state::SellAck::Retry;
        }
        FuntimeMaxDecide::Skip { reason, .. } => {
            warn!("макс. цена FunTime → skip {id}: {reason} (в Go не пишем)");
            st.need_sell_price = None;
            st.sell_ack = state::SellAck::Skip;
        }
    }
}

fn handle_funtime_min(text: &str) {
    use crate::funtime_price::{decide_funtime_min_price, FuntimeMinDecide};
    let g = boot::global();
    let raw = parse_chat_price(text).unwrap_or(0.0);
    let mut st = g.state.lock();
    let catalog_sell = st.held_sell_price.unwrap_or(raw);
    let nacenka = st.held_nacenka.unwrap_or(0.0);
    let book_floor = st.held_book_floor.unwrap_or(0.0);
    let book_mid = st.held_book_mid.unwrap_or(0.0);
    let id = st.held_sell_id.clone().unwrap_or_default();
    if id.is_empty() || catalog_sell <= 0.0 {
        st.sell_ack = state::SellAck::Skip;
        return;
    }
    match decide_funtime_min_price(raw, catalog_sell, nacenka, book_floor, book_mid) {
        FuntimeMinDecide::Skip { reason, .. } => {
            warn!("мин. цена FunTime → skip {id}: {reason}");
            st.need_sell_price = None;
            st.sell_ack = state::SellAck::Skip;
        }
        FuntimeMinDecide::Ok {
            list_price,
            set_go_min,
        } => {
            st.need_sell_price = Some(list_price);
            st.sell_ack = state::SellAck::Retry;
            if text.to_lowercase().contains("круш") || !set_go_min {
                if !set_go_min {
                    info!("мин. цена FunTime → retry {id} @ {list_price} без set_min в Go");
                }
                return;
            }
            drop(st);
            emit(&WorkerMsg {
                name: "set_min_price".into(),
                username: Some(g.boot.username.clone()),
                id: Some(id),
                price: Some(list_price),
                ..empty_msg()
            });
        }
    }
}

fn parse_try_sell_price(text: &str) -> Option<f64> {
    const MARK: &str = "выставлен на продажу за";
    let i = text.find(MARK)?;
    let tail = &text[i + MARK.len()..];
    let digits: String = tail.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn parse_balance(text: &str) -> Option<f64> {
    let low = text.to_lowercase();
    if !low.contains("баланс") && !low.contains("монет") {
        return None;
    }
    parse_chat_price(text)
}

fn parse_chat_price(text: &str) -> Option<f64> {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_digit() || c == ',' || c == '.' || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut best: Option<f64> = None;
    for part in cleaned.split_whitespace() {
        let p = part.replace(',', "");
        if let Ok(n) = p.parse::<f64>() {
            if n >= 1.0 {
                best = Some(best.map(|b: f64| b.max(n)).unwrap_or(n));
            }
        }
    }
    best
}

pub fn empty_msg() -> WorkerMsg {
    WorkerMsg {
        name: String::new(),
        username: None,
        reason: None,
        items: None,
        data: None,
        id: None,
        price: None,
        balance: None,
        lots: None,
        extra: serde_json::json!({}),
    }
}

fn is_staff_check_text(raw: &str) -> bool {
    let lower = raw.to_lowercase();
    if lower.contains("вызваны на проверку читов") {
        return true;
    }
    if lower.contains("проверку читов") && lower.contains("признание") {
        return true;
    }
    if lower.contains("проверка выполняется через") && lower.contains("anydesk") {
        return true;
    }
    if lower.contains("anydesk") && lower.contains("rustdesk") {
        return true;
    }
    false
}
