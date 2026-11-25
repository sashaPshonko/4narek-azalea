//! Worker ≈ 4narek-old на Azalea (process-per-bot).
//! Vanilla physics/keepalive — штатные Azalea; без патчей mineflayer.
//!
//! Один режим: farm. `ah::run_loop` + казначей-роль.
//!
//! **Казначей** — роль, передаваемая между farm-ботами. Орк вычисляет,
//! кто казначей (max `next_break_at` среди `is_active`), и шлёт
//! `OrchCmd::TreasurerRole`. Тот, кто казначей:
//!   - принимает `/pay` от других farm-ботов (`treasurer_nick`);
//!   - раздаёт излишек в `sell_pass` по `state.balances_snapshot`
//!     (получает от орка через `OrchCmd::BalancesSnapshot`).
//!
//! **FlushToTreasurer** — орк перед уходом казначея в сон шлёт
//! `FlushToTreasurer { to }`. `boot::stdin_loop` ставит
//! `state.treasurer_flush_to = Some(to)`. Тикер в `main()` (раз в 2с)
//! видит это, берёт `Client` из `Global.bot_client` и запускает
//! `flush_treasurer_loop`. Тот долбит `/balance` → `/pay to (bal - KEEP_SUM)`
//! **до успеха** (без таймаута), потом шлёт
//! `WorkerMsg { name: "treasurer_flushed", ok: true }`. Орк ждёт этого
//! сообщения и только тогда убивает процесс.
//!
//! **`treasurer_flushed { ok: false }`** — воркер шлёт, если flush
//! не может продолжаться (`banned`, `shutdown`, `to = None`). Орк
//! снимет `flush_waiting` и (в новой версии orch.rs) оставит
//! `owes_treasurer` — дошлёт при следующем респавне. Без этого орк
//! ждал бы вечно, а воркер висел бы живым, но не работал (клинч).
//!
//! **Перезапись `to` во время flush.** Если орк пришлёт новый
//! `FlushToTreasurer`, пока текущий ещё в полёте — воркер **не**
//! сбрасывает `treasurer_flush_to` в `None` после текущего цикла,
//! а перезапускает цикл на новый `to`. Так новый flush не теряется.
//!
//! **`my_nick_seen`** — любой бот при появлении `myNick` в табе/чате шлёт
//! орку. Орк логирует. Если я **казначей** — дополнительно ставлю
//! `need_sell = true`, чтобы `sell_pass` (в `ah::run_loop`) сделал
//! `/balance` и `distribute_treasurer` (который раздаёт ботам, а
//! **остаток** отправляет на `myNick`). **Никаких отдельных потоков** —
//! один цикл, никаких гонок с `flush_treasurer_loop`.
//!
//! **`chat_mentions_nick`** — тупой case-insensitive `contains` по подстроке.
//! Без границ слова. Так проще: если ник `Vasya`, то `Vasya123` в чате
//! тоже триггернёт.

mod afk;
mod ah;
mod anarchy;
mod auth;
mod boot;
mod captcha;
mod cmd;
mod delay;
mod funtime_price;
mod health;
mod invis;
mod item_parse;
mod listing;
mod money;
mod owner;
mod rent;
mod rpack;
mod safe_inv;
mod sell;
mod state;
mod storage;
mod titles;
mod trash;
mod warmup;
mod warp;
mod wear;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use azalea::app::PluginGroup;
use azalea::{
    prelude::*,
    protocol::{connect::Proxy, packets::game::ClientboundGamePacket},
    JoinOpts,
};
use ipc::{emit, emit_text, BotBoot, WorkerMsg};
use regex::Regex;
use serde_json::json;
use socks5_impl::protocol::UserKey;
use tracing::{info, warn};

/// KEEP_SUM — оставляем себе как farm-бот.
pub const KEEP_SUM_BASE: f64 = 15_000_000.0;
pub fn keep_sum() -> f64 { ipc::balance::k(KEEP_SUM_BASE) }
/// При `bal > KEEP_SUM + EXCESS_THRESHOLD` (= 50кк) farm-бот шлёт излишек (bal − KEEP_SUM) казначею.
pub const EXCESS_THRESHOLD_BASE: f64 = 35_000_000.0;
pub fn excess_threshold() -> f64 { ipc::balance::k(EXCESS_THRESHOLD_BASE) }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let boot = boot::read_boot().await?;
    ipc::balance::init(boot.anarchy);
    ipc::pretty_log::init_bot(&boot.username);
    delay::init_bot_delay_profile(&boot.username);
    info!(
        "старт · an{} · {} · role={} · treasurer={:?}",
        boot.anarchy, boot.item, boot.go_type, boot.treasurer_nick
    );

    let g = boot::global();
    // Живость воркера: мёртвый сокет (как в JS, 90с без входящих) и ошибки прокси (health.rs).
    health::spawn_watchdog();
    tokio::spawn(boot::stdin_loop(
        g.state.clone(),
        g.pay_tx.clone(),
        g.balances_tx.clone(),
    ));

    // ============================================================
    // ТИКЕР flush_treasurer (смена казначея)
    //
    // `stdin_loop` только ставит `state.treasurer_flush_to = Some(to)`.
    // Запуск `flush_treasurer_loop` — здесь: `Client` уже создан
    // `ClientBuilder` (или скоро будет), тикер подождёт, пока
    // `Global.bot_client` появится (`Event::Spawn` заполняет).
    // ============================================================
    {
        let g_flush = g.state.clone();
        let boot_flush = g.boot.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;

                // Перерыв в хабе: раз в 20с, пока мы ещё на анке, шлём /hub.
                {
                    let need = {
                        let st = g_flush.lock();
                        st.parked
                            && !st.banned
                            && !st.shutdown
                            && st.on_anarchy
                            && !st.captcha_busy
                            && st
                                .park_hub_sent
                                .map_or(true, |t| t.elapsed() > Duration::from_secs(20))
                    };
                    if need {
                        let bot_now = boot::global().bot_client.lock().clone();
                        if let Some(bot) = bot_now {
                            info!("перерыв → /hub");
                            cmd::say(&bot, "/hub");
                            g_flush.lock().park_hub_sent = Some(std::time::Instant::now());
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            anarchy::clear_anarchy(&g_flush, "перерыв: /hub");
                        }
                    }
                }

                // Выравнивание балансов: приказ орка «перекинь X боту Y».
                let peer = {
                    let mut st = g_flush.lock();
                    if st.banned || st.shutdown || !st.on_anarchy || st.treasurer_flush_in_flight {
                        None
                    } else {
                        st.pending_peer_pay.take()
                    }
                };
                if let Some((to, amount)) = peer {
                    let bot_now = boot::global().bot_client.lock().clone();
                    if let Some(bot) = bot_now {
                        let st2 = g_flush.clone();
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("peer pay rt");
                            rt.block_on(async move {
                                if amount > 0 && bot.player_uuid_by_username(&to).is_some() {
                                    info!(
                                        "выравнивание → /pay {to} {}",
                                        crate::money::money(amount as f64)
                                    );
                                    sell::pay_once(&bot, &st2, &to, amount).await;
                                    delay::rnd("BASE_DELAY").await;
                                    cmd::say(&bot, "/balance");
                                } else {
                                    info!("выравнивание → {to} нет в табе, пропуск");
                                }
                            });
                        });
                    }
                }

                let (need_flush, already, to_opt) = {
                    let st = g_flush.lock();
                    (
                        st.treasurer_flush_to.is_some(),
                        st.treasurer_flush_in_flight,
                        st.treasurer_flush_to.clone(),
                    )
                };
                if !need_flush || already {
                    continue;
                }
                let Some(to) = to_opt else { continue };

                let bot_opt = boot::global().bot_client.lock().clone();
                let Some(bot) = bot_opt else {
                    // Client ещё не готов (Spawn не пришёл) — ждём
                    // следующего тика (2с).
                    continue;
                };

                info!("main: запуск flush_treasurer → {to}");
                g_flush.lock().treasurer_flush_in_flight = true;

                let g2 = g_flush.clone();
                let boot2 = boot_flush.clone();
                tokio::spawn(async move {
                    flush_treasurer_loop(bot, boot2, g2).await;
                    // flush_treasurer_loop сам сбросит
                    // treasurer_flush_in_flight и treasurer_flush_to
                    // (или оставит их, если пришёл новый to — см. F).
                });
            }
        });
    }

    let proxy = parse_proxy(&boot.proxy)?;
    let account = Account::offline(&boot.username);
    let opts = JoinOpts::new().server_proxy(proxy);
    let server = boot.server.clone();

    // Встроенный ответ azalea на ресурспак («принял»+«загружен» мгновенно, без скачивания) заменён
    // ванильным порядком из rpack.rs.
    ClientBuilder::new_without_plugins()
        .add_plugins(azalea::DefaultPlugins)
        .add_plugins(
            azalea::bot::DefaultBotPlugins
                .build()
                .disable::<azalea::accept_resource_packs::AcceptResourcePacksPlugin>(),
        )
        .add_plugins(rpack::VanillaResourcePackPlugin)
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
        // Настройки клиента как у TLauncher (запись 01.11.2026: ru_ru, дальность 8, листинг вкл,
        // частицы minimal, skinParts 127, правая рука). Должны уйти ДО входа в игру,
        // поэтому ставим на Init.
        Event::Init => {
            auth::on_new_connection();
            bot.set_client_information(azalea::ClientInformation {
                language: "ru_ru".to_owned(),
                view_distance: 8,
                allows_listing: true,
                particle_status: azalea::protocol::common::client_information::ParticleStatus::Minimal,
                ..Default::default()
            });
        }
        Event::Spawn => {
            health::on_spawn();
            // Сохраняем Client в Global — нужен для flush_treasurer_loop
            // из тикера в main().
            *g.bot_client.lock() = Some(bot.clone());

            let boot = g.boot.clone();
            // /reg и /l — только по подсказке сервера (auth.rs); здесь лишь запасной /l, если подсказки не будет
            auth::on_spawn(&bot, &boot.password);
            state.phase.store(1, Ordering::SeqCst);

            let bot2 = bot.clone();
            let phase = state.phase.clone();
            let ah_armed = state.ah_armed.clone();
            let inner = g.state.clone();
            let boot2 = boot.clone();

            if ah_armed.swap(true, Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("boot rt");

                let boot_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(async {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        if phase.load(Ordering::SeqCst) < 2 {
                            phase.store(2, Ordering::SeqCst);
                        }
                        // **НОВОЕ:** новый Spawn = новый заход. Сбрасываем
                        // `funauth_verified`, чтобы через 5с снова отправить
                        // `funauth_verified` (орк поставит `bindings = true`).
                        // Иначе после реконнекта флаг уже `true` — не отправится.
                        inner.lock().funauth_verified = false;
                        if !anarchy::wait_until_on_anarchy(&bot2, &boot2, &inner).await {
                            if inner.lock().banned {
                                warn!("бан — цикл не стартую");
                            } else {
                                warn!("на анку не зашли — цикл не стартую");
                            }
                            return false;
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        {
                            let st = inner.lock();
                            if st.banned || st.shutdown || st.funauth_restarting {
                                return false;
                            }
                        }

                        // **НОВОЕ:** периодический funauth_verified. Шлём
                        // сразу и потом каждые 30с, пока бот на анке и не
                        // banned/shutdown/funauth_restarting. Орк
                        // дедуплицирует (bindings.json → true), а Go-сервер
                        // снимает needBind. Это чинит дыру «Go рестартовал,
                        // а орк не переслал funauth_verified — фронт снова
                        // показывает просит привязку».
                        emit(&WorkerMsg {
                            name: "funauth_verified".into(),
                            username: Some(boot2.username.clone()),
                            ..empty_msg()
                        });
                        inner.lock().funauth_verified = true;
                        phase.store(3, Ordering::SeqCst);

                        // Фоновый тикер: раз в 30с, пока живы. Выходит сам
                        // при banned/shutdown/funauth_restarting или не на
                        // анке.
                        //
                        // ВАЖНО: не `tokio::spawn`, а `std::thread::spawn`
                        // с собственным current_thread runtime.
                        // `tokio::spawn` внутри `rt.block_on` умрёт вместе
                        // с `block_on`, а нам нужно, чтобы тикер жил, пока
                        // крутится `ah::run_loop` (это отдельный
                        // `rt.block_on`).
                        {
                            let inner_tick = inner.clone();
                            let boot_tick = boot2.clone();
                            std::thread::spawn(move || {
                                let rt_tick = match tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                {
                                    Ok(rt) => rt,
                                    Err(e) => {
                                        warn!("funauth_verified ticker: rt build: {e:#}");
                                        return;
                                    }
                                };
                                rt_tick.block_on(async move {
                                    loop {
                                        tokio::time::sleep(Duration::from_secs(30)).await;
                                        let (banned, shutdown, restarting, on_anarchy, verified) = {
                                            let st = inner_tick.lock();
                                            (
                                                st.banned,
                                                st.shutdown,
                                                st.funauth_restarting,
                                                st.on_anarchy,
                                                st.funauth_verified,
                                            )
                                        };
                                        if banned || shutdown || restarting {
                                            break;
                                        }
                                        if !on_anarchy {
                                            continue;
                                        }
                                        if !verified {
                                            continue;
                                        }
                                        emit(&WorkerMsg {
                                            name: "funauth_verified".into(),
                                            username: Some(boot_tick.username.clone()),
                                            ..empty_msg()
                                        });
                                    }
                                });
                            });
                        }

                        !(inner.lock().banned || inner.lock().shutdown)
                    })
                }));
                match boot_ok {
                    Ok(false) => {
                        ah_armed.store(false, Ordering::SeqCst);
                        return;
                    }
                    Err(e) => {
                        warn!("boot panic: {e:?}");
                        ah_armed.store(false, Ordering::SeqCst);
                        return;
                    }
                    Ok(true) => {}
                }

                // Режим «майник»: `MYNICK_MODE=1` — аккаунт владельца заходит на анархию, пишет в общий чат
                // «!текст» (казначеи реагируют на чат), ждёт перевод и выходит. Коды выхода: 0 — получено,
                // 2 — не дождались, 1 — бан/ошибка. Настройки: MYNICK_WAIT_SEC, MYNICK_AFTER_PAY_SEC, MYNICK_CHAT.
                if std::env::var("MYNICK_MODE").is_ok() {
                    let env_u = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
                    let wait = env_u("MYNICK_WAIT_SEC", 360);
                    let after = env_u("MYNICK_AFTER_PAY_SEC", 8);
                    // несколько фраз через «|» — на каждую отправку берём случайную
                    let phrases: Vec<String> = std::env::var("MYNICK_CHAT")
                        .unwrap_or_else(|_| "!всем привет".into())
                        .split('|')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .map(|t| if t.starts_with('!') { t } else { format!("!{t}") })
                        .collect();
                    let phrases = if phrases.is_empty() { vec!["!всем привет".to_string()] } else { phrases };
                    let mut pick = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos() as usize)
                        .unwrap_or(0);
                    info!("[mynick] на анархии, жду 10 с, пишу в чат и жду перевод до {wait} с");
                    std::thread::sleep(Duration::from_secs(10));
                    let started = std::time::Instant::now();
                    let mut last_chat: Option<std::time::Instant> = None;
                    let mut total = 0.0f64;
                    let mut last_pay: Option<std::time::Instant> = None;
                    let mut seen_pay_at: Option<std::time::Instant> = None;
                    let code = loop {
                        if inner.lock().banned {
                            warn!("[mynick] бан");
                            break 1;
                        }
                        if last_chat.map_or(true, |t| t.elapsed() >= Duration::from_secs(75)) {
                            pick = pick.wrapping_mul(1103515245).wrapping_add(12345);
                            let mut phrase = phrases[(pick >> 8) % phrases.len()].clone();
                            if phrase.contains("{random_player}") {
                                let me = boot2.username.to_lowercase();
                                let mut names: Vec<String> = bot2
                                    .tab_list()
                                    .values()
                                    .map(|p| p.profile.name.clone())
                                    .filter(|n| !n.is_empty() && n.to_lowercase() != me)
                                    .collect();
                                names.sort();
                                pick = pick.wrapping_mul(1103515245).wrapping_add(12345);
                                if names.is_empty() {
                                    // в табе никого — берём другую фразу без подстановки
                                    if let Some(p) = phrases.iter().find(|p| !p.contains("{random_player}")) {
                                        phrase = p.clone();
                                    }
                                } else {
                                    phrase = phrase.replace("{random_player}", &names[(pick >> 8) % names.len()]);
                                }
                            }
                            cmd::say(&bot2, &phrase);
                            last_chat = Some(std::time::Instant::now());
                        }
                        let pay = inner.lock().last_payment.clone();
                        if let Some((nick, amount, at)) = pay {
                            if seen_pay_at != Some(at) {
                                seen_pay_at = Some(at);
                                total += amount;
                                last_pay = Some(std::time::Instant::now());
                                info!("[mynick] получено {} от {nick} (всего {})", money::money(amount), money::money(total));
                            }
                        }
                        if let Some(t) = last_pay {
                            if t.elapsed() >= Duration::from_secs(after) {
                                break 0;
                            }
                        } else if started.elapsed() >= Duration::from_secs(wait) {
                            break 2;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    };
                    info!("[mynick] выход {code}: получено {}", money::money(total));
                    std::thread::sleep(Duration::from_secs(1));
                    std::process::exit(code);
                }

                // Проверка аренды вручную: `RENT_TEST=1` — один раз прогнать цепочку аренды настоящим кодом бота
                // (rent.rs) на анархии и выйти. Платит 1.5 млн. Ожидание перед стартом — `RENT_TEST_WAIT` секунд.
                if std::env::var("RENT_TEST").is_ok() {
                    let wait = std::env::var("RENT_TEST_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(20u64);
                    info!("RENT_TEST: на анархии, жду {wait} с и запускаю аренду");
                    std::thread::sleep(Duration::from_secs(wait));
                    let res = rt.block_on(rent::run_rent(&bot2, &inner));
                    info!("RENT_TEST итог: {res:?}");
                    std::thread::sleep(Duration::from_secs(3));
                    std::process::exit(if matches!(res, rent::RentOutcome::Done) { 0 } else { 1 });
                }

                // FARM: АХ-цикл с рестартами.
                loop {
                    if inner.lock().banned || inner.lock().shutdown {
                        break;
                    }
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
                        if boot_loop.warmup {
                            rt.block_on(warmup::run_loop(bot_loop, boot_loop, inner_loop));
                        } else if let Some(icfg) = invis::config_for(&boot_loop) {
                            rt.block_on(invis::run_loop(bot_loop, boot_loop, inner_loop, icfg));
                        } else {
                            rt.block_on(ah::run_loop(bot_loop, boot_loop, inner_loop));
                        }
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
                if !g.state.lock().parked {
                    ipc::pretty_log::line(
                        &g.boot.username,
                        ipc::pretty_log::LineKind::Chat,
                        &text,
                    );
                }
                handle_chat(&bot, &text);
            }
        }
        Event::Packet(packet) => {
            rpack::log_server_probe(packet.as_ref());
            match packet.as_ref() {
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
                let mut st = g.state.lock();
                st.seen_my_nicks.clear();
                st.tab_names.clear();
                st.sidebar_balance = None;
            }
            ClientboundGamePacket::SetPlayerTeam(p) => {
                on_team_packet(&bot, p);
            }
            ClientboundGamePacket::PlayerInfoUpdate(p) => {
                on_player_info(&g.state, &g.boot, p);
            }
            ClientboundGamePacket::PlayerInfoRemove(p) => {
                let mut st = g.state.lock();
                for uuid in &p.profile_ids {
                    st.seen_my_nicks.remove(uuid);
                }
            }
            other => {
                captcha::on_packet(&g.captcha_maps, other);
            }
            }
        }
        Event::ConnectionFailed(err) => health::on_connection_failed(err),
        Event::Disconnect(reason) => {
            auth::on_new_connection();
            g.state.lock().sidebar_balance = None;
            let r = format!("{reason:?}");
            warn!("disconnect {r}");
            // СРАЗУ помечаем, что мы не на анке — иначе ah::run_loop
            // продолжит долбить /ah search в лобби после респавна.
            {
                let mut st = g.state.lock();
                st.on_anarchy = false;
            }
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

fn on_player_info(
    state: &Arc<parking_lot::Mutex<state::BotState>>,
    boot: &ipc::BotBoot,
    p: &azalea::protocol::packets::game::ClientboundPlayerInfoUpdate,
) {
    {
        let mut st = state.lock();
        for entry in &p.entries {
            let name = entry.profile.name.as_str();
            if !name.is_empty() {
                st.tab_names.insert(name.to_lowercase());
            }
        }
    }

    let Some(my_nick) = boot.my_nick.as_deref() else {
        return;
    };

    for entry in &p.entries {
        let name = entry.profile.name.as_str();
        if !name.eq_ignore_ascii_case(my_nick) {
            continue;
        }
        let uuid = entry.profile.uuid;
        {
            let mut st = state.lock();
            if !st.seen_my_nicks.insert(uuid) {
                continue;
            }
        }
        info!(
            "{}: myNick {name} (uuid={uuid}) в табе → my_nick_seen",
            boot.username
        );
        emit(&WorkerMsg {
            name: "my_nick_seen".into(),
            username: Some(boot.username.clone()),
            data: Some(json!({ "nick": name, "anarchy": boot.anarchy })),
            ..empty_msg()
        });

        // Если я казначей — ставим need_sell = true. `ah::run_loop`
        // сделает `sell_pass`, `sell_pass` сделает `/balance` +
        // `distribute_treasurer` (который раздаст ботам и **остаток**
        // отправит myNick). Никаких отдельных потоков — гонок нет.
        {
            let mut st = state.lock();
            if st.is_treasurer && !st.need_sell {
                st.need_sell = true;
                info!(
                    "{}: myNick {name} в табе, я казначей → need_sell = true",
                    boot.username
                );
            }
        }
    }
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
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(500));
        std::process::exit(0);
    });
}

fn handle_chat(bot: &Client, text: &str) {
    let g = boot::global();
    let low = text.to_lowercase();
    if rent::chat_hint(text) {
        info!("{}: аренда → чат: {}", g.boot.username, text);
    }
    // «[V] Успешная покупка!» после клика по подтверждению аренды — аренда прошла
    if rent::is_purchase_ok(text) {
        let mut st = g.state.lock();
        if st.rent_awaiting_confirm {
            st.rent_confirmed = true;
            info!("{}: аренда → подтверждено сообщением «Успешная покупка!»", g.boot.username);
        }
    }
    {
        // режим «инвиз»: складываем чат для разбора в invis.rs (не больше 200 строк)
        let mut st = g.state.lock();
        if st.invis_on {
            if st.invis_chat.len() > 200 {
                st.invis_chat.drain(..100);
            }
            st.invis_chat.push((std::time::Instant::now(), text.to_string()));
        }
    }
    let pass = &g.boot.password;
    let user = &g.boot.username;

    if let Some(my_nick) = g.boot.my_nick.as_deref() {
        if !my_nick.is_empty() && chat_mentions_nick(text, my_nick) {
            info!("{}: myNick {my_nick} в чате → my_nick_seen", user);
            emit(&WorkerMsg {
                name: "my_nick_seen".into(),
                username: Some(user.clone()),
                data: Some(json!({ "nick": my_nick, "anarchy": g.boot.anarchy })),
                ..empty_msg()
            });

            // Если я казначей — ставим need_sell = true. `sell_pass`
            // сам сделает /balance и distribute_treasurer (который
            // остаток отправит myNick). Гонок нет — один цикл.
            {
                let mut st = g.state.lock();
                if st.is_treasurer && !st.need_sell {
                    st.need_sell = true;
                    info!(
                        "{}: myNick {my_nick} в чате, я казначей → need_sell = true",
                        user
                    );
                }
            }
        }
    }

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

    if anarchy::on_lobby_broadcast(&g.state, text) {
        return;
    }

    if text.contains("Здесь нет команд") {
        anarchy::clear_anarchy(&g.state, "нет команд");
        return;
    }

    if text.contains("Вы уже подключены к этому серверу") {
        anarchy::mark_anarchy_joined(&g.boot, &g.state);
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
    } else if auth::is_authed_text(text) {
        auth::on_authed();
    } else if let Some(kind) = auth::classify(text) {
        let busy = g.state.lock().captcha_busy;
        auth::on_prompt(bot, kind, pass, busy);
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
        emit_text("не хватает монет");
    } else if low.contains("уже онлайн") {
        emit_text("ник уже онлайн");
    } else if low.contains("подтвердите вход")
        || (low.contains("личные сообщения") && low.contains("подтвердите"))
    {
        {
            let mut st = g.state.lock();
            if st.funauth_restarting {
                return;
            }
            // **НОВОЕ:** 2FA = привязки нет. Сбрасываем флаг, чтобы
            // после респавна `funauth_verified` **не** отправлялся
            // (орк поставит `bindings = false` при `funauth_2fa`).
            st.funauth_verified = false;
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
    } else if low.contains("чтобы двигаться и писать в чат") {
        // Аккаунт не привязан: сервер просит привязку этой фразой (раньше ловили по «/vk» + «привяз…»).
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
        // Перед отправкой try-sell освежаем память по слотам хранилища (если бот ещё в нём): цены и лоты берём
        // из того, что реально лежит на витрине.
        if storage::refresh_listing_memory_if_in_storage(bot, &g.boot, &g.state) {
            info!("storage reset: память лотов обновлена по хранилищу");
        }
        let metas: Vec<listing::ListingMeta> = {
            let mut st = g.state.lock();
            st.last_storage_reset = std::time::Instant::now();
            st.last_reset_click_at = None;
            st.need_reset = false;
            st.listings.all_metas()
        };
        if g.state.lock().invis_on {
            // режим «инвиз»: try-sell на каждый лот витрины шлёт invis.rs
        } else if metas.is_empty() {
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
    } else if text.contains("[☃] Вы успешно купили") && wear::on_purchase_confirmed(&g.state) {
        // броня для себя: в Go ни buy, ни buying не уходят; наденется в sell_pass
        info!("броня для себя куплена — надену в прогулке-продаже");
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
            // купили на `price` — ведём баланс сами до следующего /balance в проходе продажи
            st.balance = st.balance.map(|b| balance_after_spend(b, price));
            drop(st);
            let dur_pct = durability.map(|d| (d * 100.0).round() as i32);
            ipc::pretty_log::info_ansi(&ipc::pretty_log::ah_buy_line(
                "купил",
                &id,
                &crate::money::money(price),
                dur_pct,
            ));
            emit(&WorkerMsg {
                name: "buy".into(),
                username: Some(user.clone()),
                id: Some(id),
                price: Some(price),
                data: Some(json!({ "enchants": enchants, "durability": durability })),
                ..empty_msg()
            });
        }
    } else if text.contains("[☃] У Вас купили") && g.state.lock().invis_on {
        // режим инвизов: продажу считает invis.rs (poll чата → `sell`); «памяти листингов» здесь нет, она нужна только обычным ботам
    } else if text.contains("[☃] У Вас купили") {
        let price = parse_chat_price(text).unwrap_or(0.0);
        let has_items = crate::sell::count_our_items(bot, &g.boot, &g.state) > 0;
        let meta = {
            let mut st = g.state.lock();
            if has_items {
                st.need_sell = true;
            }
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
        {
            let mut st = g.state.lock();
            st.sell_ack = state::SellAck::Full;
            st.sell_full = true;
        }
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
    } else if text.contains("выставлен на продажу за") && g.state.lock().invis_on {
        // режим «инвиз»: try-sell шлёт invis.rs (памяти лотов по маркеру цены тут нет)
        g.state.lock().sell_ack = state::SellAck::Ok;
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
    } else if text.contains("получено") && text.contains("От игрока") {
        if let Some(nick) = extract_payer(text) {
            let amount = parse_chat_price(text).unwrap_or(0.0);
            info!(
                "получен перевод от {nick}: {}",
                crate::money::money(amount)
            );
            let mut st = g.state.lock();
            st.balance = st.balance.map(|b| b + amount.max(0.0));
            st.last_payment = Some((nick, amount, std::time::Instant::now()));
        }
    } else if text.contains("Успешно! Игроку") && text.contains("отправлено") {
        if let Some(nick) = extract_pay_target(text) {
            info!("[pay] успешно → {nick}");
            let mut st = g.state.lock();
            st.last_my_nick_pay = Some(std::time::Instant::now());
        }
    } else if text.contains("Необходимо подтверждение") {
        info!("[pay] требует подтверждения — flush повторит");
    } else if text.contains("Указанный игрок не найден") {
        // Флаг `in_flight` принадлежит самому циклу flush_treasurer: если сбросить его здесь,
        // тикер (раз в 2 с) запустит второй такой же цикл, и каждый шлёт /balance.
        info!("[pay] получатель не найден — flush повторит");
    } else if let Some(bal) = parse_balance(text) {
        on_balance(bot, bal);
    }
}

/// Баланс узнан (ответ на `/balance` в чате или строка «Монет» в сайдбаре): сообщаем оркестратору и
/// отправляем излишек казначею.
pub fn on_balance(bot: &Client, bal: f64) {
    let g = boot::global();
    let user = &g.boot.username;
        g.state.lock().balance = Some(bal);
        emit(&WorkerMsg {
            name: "balance".into(),
            username: Some(user.clone()),
            balance: Some(bal),
            ..empty_msg()
        });

        // Если сейчас идёт flush_treasurer — НЕ запускаем excess_money.
        if g.state.lock().treasurer_flush_to.is_some() {
            return;
        }

        // Если я казначей — distribute делает sell_pass.
        if g.state.lock().is_treasurer {
            return;
        }

        // Выравнивание идёт (у кого-то < 13кк) — излишек казначею не шлём.
        if g.state.lock().equalizing {
            return;
        }

        // Иначе — excess_money: bal > KEEP_SUM + EXCESS → /pay treasurer_nick.
        if bal > keep_sum() + excess_threshold() {
            let excess = (bal - keep_sum()).floor();
            if excess > 0.0 {
                let treasurer_nick = g.state.lock().treasurer_nick.clone();
                if let Some(treasurer) = treasurer_nick {
                    let amount = excess as i64;
                    info!(
                        "farm → излишек {} → /pay {}",
                        crate::money::money(excess),
                        treasurer
                    );
                    let bot2 = bot.clone();
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("excess pay rt");
                        rt.block_on(async move {
                            afk::un_afk(&bot2).await;
                            if !crate::cmd::pay(&bot2, &treasurer, amount) {
                                return;
                            }
                            let ms = 500 + (rand::random::<u64>() % 501);
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            if !crate::cmd::pay(&bot2, &treasurer, amount) {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            info!(
                                "farm → /pay {treasurer} {amount} — отправлено ×2"
                            );
                        });
                    });
                } else {
                    warn!("excess_money: treasurer_nick не задан");
                }
            }
        }
}

/// Строка сайдбара TAB «$ Монет: 712,071,992» → число.
fn parse_sidebar_balance(text: &str) -> Option<f64> {
    let i = text.find("Монет:")? + "Монет:".len();
    let digits: String = text[i..].chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { None } else { digits.parse::<f64>().ok() }
}

/// Пакет команды сайдбара: баланс обновляем только по `Change` — `Add` при входе несёт заглушку «0».
fn on_team_packet(bot: &Client, p: &azalea::protocol::packets::game::ClientboundSetPlayerTeam) {
    use azalea::protocol::packets::game::c_set_player_team::Method;
    if !p.name.starts_with("TAB-Sidebar") {
        return;
    }
    let Method::Change(params) = &p.method else { return };
    let text = format!("{}{}", params.player_prefix, params.player_suffix);
    let Some(bal) = parse_sidebar_balance(&text) else { return };
    let g = boot::global();
    let changed = {
        let mut st = g.state.lock();
        let changed = st.sidebar_balance != Some(bal) || st.balance != Some(bal);
        st.sidebar_balance = Some(bal);
        changed
    };
    if changed {
        on_balance(bot, bal);
    }
}

/// Баланс без команды, если он уже известен по сайдбару; иначе `/balance`.
/// `true` — отправили `/balance` (ответ придёт позже); `false` — баланс уже взят из сайдбара, ждать нечего.
pub fn request_balance(bot: &Client, state: &Arc<parking_lot::Mutex<state::BotState>>) -> bool {
    let known = state.lock().sidebar_balance;
    match known {
        Some(bal) => {
            state.lock().balance = Some(bal);
            on_balance(bot, bal);
            false
        }
        None => {
            cmd::say(bot, "/balance");
            true
        }
    }
}

fn extract_pay_target(text: &str) -> Option<String> {
    let re = Regex::new(r"Игроку\s+(\S+)\s+отправлено").ok()?;
    re.captures(text)?.get(1).map(|m| m.as_str().to_string())
}

/// Фоновый flush_treasurer: шлёт `/balance` → `/pay to (bal - KEEP_SUM)`
/// **до успеха** (без таймаута). Спавнится из тикера в `main()`.
///
/// **B2 (клинч).** Выходит с `treasurer_flushed { ok: false }`, если
/// flush не может продолжаться (`banned`, `shutdown`, `to = None`).
/// Иначе орк ждёт вечно, а воркер висит живым, но не работает.
///
/// **B4 (успех на маленьких суммах).** Допуск не `+1kk` всегда, а
/// `+min(1kk, amount/2)`. Иначе при `amount < 1kk` условие «успех»
/// истинно всегда, даже если `/pay` не прошёл.
///
/// **F (перезапись `to`).** Если во время текущего flush орк прислал
/// новый `FlushToTreasurer` (сменился казначей), не сбрасываем
/// `treasurer_flush_to` в `None`, а перезапускаем цикл на новый `to`.
pub async fn flush_treasurer_loop(
    bot: Client,
    boot: Arc<BotBoot>,
    state: Arc<parking_lot::Mutex<state::BotState>>,
) {
    let mut to = {
        let st = state.lock();
        st.treasurer_flush_to.clone()
    };
    if to.is_none() {
        warn!("flush_treasurer: to = None — выхожу, шлю ok=false");
        emit(&WorkerMsg {
            name: "treasurer_flushed".into(),
            username: Some(boot.username.clone()),
            data: Some(json!({
                "ok": false,
                "to": "",
                "amount": 0,
                "reason": "to_none",
            })),
            ..empty_msg()
        });
        state.lock().treasurer_flush_in_flight = false;
        return;
    }

    'outer: loop {
        let to_now = to.clone().unwrap();
        info!("flush_treasurer старт → {to_now}");
        state.lock().treasurer_flush_in_flight = true;
        let mut balance_misses = 0u32;

        loop {
            // B2. banned/shutdown — выходим с ok=false, чтобы орк снял
            // flush_waiting и оставил owes_treasurer (дошлёт при респавне).
            if state.lock().banned || state.lock().shutdown {
                warn!(
                    "flush_treasurer: banned/shutdown — выхожу, шлю ok=false (to={to_now})"
                );
                emit(&WorkerMsg {
                    name: "treasurer_flushed".into(),
                    username: Some(boot.username.clone()),
                    data: Some(json!({
                        "ok": false,
                        "to": to_now,
                        "amount": 0,
                        "reason": "banned_or_shutdown",
                    })),
                    ..empty_msg()
                });
                state.lock().treasurer_flush_in_flight = false;
                // F. Не сбрасываем treasurer_flush_to — пусть орк сам
                // решит (owes_treasurer у него уже стоит).
                return;
            }

            // F. Проверяем, не прислал ли орк новый to. Если да —
            // перезапускаем цикл на новый to (не теряя новый flush).
            {
                let cur = state.lock().treasurer_flush_to.clone();
                if let Some(new_to) = cur {
                    if new_to != to_now {
                        info!(
                            "flush_treasurer: to изменился {to_now} → {new_to}, перезапуск"
                        );
                        to = Some(new_to);
                        continue 'outer;
                    }
                } else {
                    // to стал None, но мы в полёте — значит, кто-то
                    // (орк) уже сбросил. Завершаем текущий цикл без
                    // treasurer_flushed (орк уже не ждёт? — на всякий
                    // случай шлём ok=false).
                    warn!(
                        "flush_treasurer: treasurer_flush_to стал None во время flush — выхожу"
                    );
                    emit(&WorkerMsg {
                        name: "treasurer_flushed".into(),
                        username: Some(boot.username.clone()),
                        data: Some(json!({
                            "ok": false,
                            "to": to_now,
                            "amount": 0,
                            "reason": "to_cleared",
                        })),
                        ..empty_msg()
                    });
                    state.lock().treasurer_flush_in_flight = false;
                    return;
                }
            }

            // Сброс баланса и запрос /balance.
            {
                let mut st = state.lock();
                st.balance = None;
            }
            afk::un_afk(&bot).await;
            state.lock().afk = false;
            delay::rnd("BASE_DELAY").await;
            let _ = request_balance(&bot, &state);

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if state.lock().balance.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            let bal_opt = {
                let st = state.lock();
                st.balance
            };
            let bal = match bal_opt {
                Some(b) => b,
                None => {
                    // пауза растёт: 15 → 30 → 60 → … → 5 мин, чтобы не долбить молчащего бота
                    balance_misses += 1;
                    let wait = (15u64 << (balance_misses - 1).min(5)).min(300);
                    warn!("flush_treasurer: /balance не ответил — retry через {wait} с");
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
            };
            info!("flush_treasurer: bal = {}", crate::money::money(bal));
            balance_misses = 0;

            let to_send = (bal - keep_sum()).floor();
            if to_send < 200_000.0 {
                info!(
                    "flush_treasurer: излишек {} < MIN_PAY — всё, выходим",
                    crate::money::money(to_send)
                );
                emit(&WorkerMsg {
                    name: "treasurer_flushed".into(),
                    username: Some(boot.username.clone()),
                    data: Some(json!({
                        "ok": true,
                        "to": to_now,
                        "amount": 0,
                        "reason": "no_excess",
                    })),
                    ..empty_msg()
                });
                // F. Перед выходом проверяем: не появился ли новый to.
                let cur = state.lock().treasurer_flush_to.clone();
                if let Some(new_to) = cur {
                    if new_to != to_now {
                        info!(
                            "flush_treasurer: после no_excess пришёл новый to {new_to} — перезапуск"
                        );
                        to = Some(new_to);
                        continue 'outer;
                    }
                }
                state.lock().treasurer_flush_to = None;
                state.lock().treasurer_flush_in_flight = false;
                state.lock().treasurer_last_flush_at = Some(std::time::Instant::now());
                state.lock().treasurer_last_flush_amount = Some(0);
                return;
            }

            let amount = to_send as i64;

            // Две команды: первая — запрос, вторая — подтверждение
            // (сервер FunTime). Пауза рандомная, чтобы не спамить.
            delay::rnd("BASE_DELAY").await;
            if !crate::cmd::pay(&bot, &to_now, amount) {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let ms = 500 + (rand::random::<u64>() % 501);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if !crate::cmd::pay(&bot, &to_now, amount) {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;

            info!(
                "flush_treasurer → /pay {to_now} {} — отправлено ×2",
                crate::money::money(amount as f64)
            );

            tokio::time::sleep(Duration::from_secs(5)).await;

            // Проверяем баланс после.
            {
                let mut st = state.lock();
                st.balance = None;
            }
            crate::cmd::say(&bot, "/balance");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if state.lock().balance.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let after = {
                let st = state.lock();
                st.balance.unwrap_or(bal)
            };
            info!("flush_treasurer: bal после = {}", crate::money::money(after));

            // B4. Проверка успеха. delta = сколько реально ушло.
            // Допуск = min(1kk, amount/2). Для маленьких сумм допуск
            // маленький — не «успех всегда».
            let delta = bal - after;
            let tolerance = 1_000_000.0_f64.min((amount as f64) / 2.0);
            let sent_ok = delta >= (amount as f64) - tolerance;

            if sent_ok {
                if delta > (amount as f64) * 1.5 {
                    // Деньги ушли, но больше, чем хотели — возможно,
                    // сервер обработал обе /pay. Не считаем провалом
                    // (деньги у казначея), но предупреждаем в лог.
                    warn!(
                        "flush_treasurer: переплата! отправили {amount}, ушло ~{}, to={to_now}",
                        delta as i64
                    );
                }
                info!(
                    "flush_treasurer: успех, отдано ~{}",
                    crate::money::money(amount as f64)
                );
                emit(&WorkerMsg {
                    name: "treasurer_flushed".into(),
                    username: Some(boot.username.clone()),
                    data: Some(json!({
                        "ok": true,
                        "to": to_now,
                        "amount": amount,
                        "after": after,
                    })),
                    ..empty_msg()
                });
                // F. Перед выходом проверяем: не появился ли новый to.
                let cur = state.lock().treasurer_flush_to.clone();
                if let Some(new_to) = cur {
                    if new_to != to_now {
                        info!(
                            "flush_treasurer: после успеха пришёл новый to {new_to} — перезапуск"
                        );
                        to = Some(new_to);
                        continue 'outer;
                    }
                }
                state.lock().treasurer_flush_to = None;
                state.lock().treasurer_flush_in_flight = false;
                state.lock().treasurer_last_flush_at = Some(std::time::Instant::now());
                state.lock().treasurer_last_flush_amount = Some(amount);
                return;
            }

            warn!(
                "flush_treasurer: /pay не прошёл (bal до={}, после={}, отправили {}, delta={}), retry через 10с",
                crate::money::money(bal),
                crate::money::money(after),
                crate::money::money(amount as f64),
                crate::money::money(delta),
            );
            tokio::time::sleep(Duration::from_secs(10)).await;
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
fn extract_payer(text: &str) -> Option<String> {
    let re = Regex::new(r"От игрока\s+(\S+)\s+получено").ok()?;
    re.captures(text)?.get(1).map(|m| m.as_str().to_string())
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

/// Тупой case-insensitive поиск подстроки ника в тексте.
/// **Без границ слова** — `Vasya` найдётся в `Vasya123`, `[Vasya]`, `VasyaPetrovich`.
fn chat_mentions_nick(text: &str, nick: &str) -> bool {
    if nick.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    let needle = nick.to_lowercase();
    lower.contains(&needle)
}

/// Баланс после траты (не уходит ниже нуля).
fn balance_after_spend(balance: f64, price: f64) -> f64 {
    if !price.is_finite() || price <= 0.0 {
        return balance;
    }
    (balance - price).max(0.0)
}

#[cfg(test)]
mod balance_tests {
    use super::*;

    #[test]
    fn spend_reduces_balance_but_not_below_zero() {
        assert_eq!(balance_after_spend(5_000_000.0, 1_200_000.0), 3_800_000.0);
        assert_eq!(balance_after_spend(1_000_000.0, 2_000_000.0), 0.0);
        assert_eq!(balance_after_spend(1_000_000.0, 0.0), 1_000_000.0); // цена неизвестна — не трогаем
        assert_eq!(balance_after_spend(1_000_000.0, f64::NAN), 1_000_000.0);
    }
}

#[cfg(test)]
mod sidebar_balance_tests {
    use super::parse_sidebar_balance;

    #[test]
    fn reads_the_coins_line() {
        assert_eq!(parse_sidebar_balance("╠ $ Монет: 712,071,992"), Some(712_071_992.0));
        assert_eq!(parse_sidebar_balance("╠ $ Монет: 0"), Some(0.0));
        assert_eq!(parse_sidebar_balance("╠ ⛁ Токенов: 5"), None);
        assert_eq!(parse_sidebar_balance("╠ $ Монет: "), None);
    }
}
