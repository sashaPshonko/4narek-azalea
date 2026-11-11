use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use rand::Rng;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::{
    afk,
    anarchy,
    delay,
    item_parse::{
        ah_price, ah_seller_nick, all_enchant_labels, auction_uuid, durability_percent,
        get_slot_kind, maybe_dump_ah_nbt, present, skip_ah_book_crusher,
    },
    safe_inv,
    sell,
    state::{self, BotState},
    storage::{self, StorageResult},
    titles, trash,
};

const FIRST_AH: usize = 0;
const LAST_BUYABLE: usize = 17;
const SLOT_STORAGE: usize = 46;

// Как JS AH_SLOT_RELOAD_MAIN / pickAhBrowseAction — всегда 50.
const SLOT_RELOAD_MAIN: usize = 50;

// подтверждение покупки — как slotGlass в 4narek-old
const SLOT_GLASS: usize = 0;
const SELL_INTERVAL: Duration = Duration::from_secs(55);
const SELL_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Пора ли продавать по времени: с последней продажи прошло не меньше SELL_INTERVAL. Проверяется не по таймеру в
/// цикле, а при заходе в хранилище: заход раз в 60 с задаёт ровный ритм обновления, а регулярная продажа
/// (она же антиафк) пристраивается к нему и не сбивает его.
fn sell_due(since_last_sell: Duration) -> bool {
    since_last_sell >= SELL_INTERVAL
}
async fn run_sell(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    last_sell: &mut Instant,
    sell_blocked_until: &mut Instant,
) {
    let _ = safe_inv::try_close(bot);
    delay::rnd("500MS").await;

    match sell::sell_pass(bot, boot, state).await {
        sell::SellOutcome::Done => {
            *last_sell = Instant::now();
        }
        sell::SellOutcome::Partial => {
            state.lock().need_sell = true;
            *last_sell = Instant::now();
        }
        sell::SellOutcome::Retry => {
            state.lock().need_sell = true;
            *sell_blocked_until = Instant::now() + SELL_RETRY_DELAY;
        }
    }
}

pub async fn run_loop(
    bot: Client,
    boot: Arc<BotBoot>,
    state: Arc<Mutex<BotState>>,
) {
    if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
        warn!("farm loop: нет анки — стоп");
        return;
    }

    info!("цикл АХ · {}", boot.item);

    let mut last_storage = Instant::now();
    let mut last_sell = Instant::now();
    let mut sell_blocked_until = Instant::now();

    loop {
        if state.lock().shutdown || state.lock().banned {
            break;
        }

        if !state.lock().on_anarchy {
            warn!("слетели с анки — снова /an");

            if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
                break;
            }
        }

        if state.lock().afk {
            info!("снимаю AFK");
            afk::un_afk(&bot).await;
            state.lock().afk = false;
        }

        trash::do_rtp_if_needed(&bot, &state).await;

        delay::rnd("BASE_DELAY").await;

        if state.lock().afk {
            afk::un_afk(&bot).await;
            state.lock().afk = false;
        }

        delay::rnd("AH_CMD").await;

        // окно закрыто — меню игрока актуально: какие слоты брони пусты
        crate::wear::refresh_missing(&bot, &state);

        if !open_ah_search(&bot, &boot, &state).await {
            warn!("АХ не открылся — повтор");

            afk::un_afk(&bot).await;

            delay::rnd("CHECK_WINDOW").await;

            continue;
        }

        let mut stale = 0u32;
        // Подряд кругов в хранилище. Если «назад в АХ» не срабатывает (заголовок окна не меняется),
        // без ограничителя бот крутится тут и не обновляет АХ.
        let mut storage_streak = 0u32;

        loop {
            if state.lock().shutdown {
                return;
            }

            // Продажа по времени больше не по таймеру цикла: её ставит заход в хранилище (см. sell_due), чтобы
            // продажа не сбивала ритм обновления раз в 60 с. Остальные триггеры need_sell работают как раньше.

            let Some(inv) = safe_inv::try_handle(&bot) else {
                warn!("АХ → нет Inventory mid-browse");
                break;
            };

            if inv.id() == 0 {
                warn!("АХ закрылся mid-browse");
                break;
            }

            let title = state.lock().last_window_title.to_lowercase();

            // До сброса витрины меньше 15 с (или он просрочен) — принудительную продажу не начинаем: она длится
            // десятки секунд и сдвинула бы ритм обновления. Продажа пойдёт после сброса.
            let reset_blocks_sale = state.lock().sale_blocked_by_reset_now();

            // Если продажа висит дольше SELL_INTERVAL + 30с — storage уступает.
             // Если продажа висит дольше SELL_INTERVAL + 30с — storage уступает.
            // НЕ срабатывает, пока действует пауза после Retry — иначе
            // storage пропускается, а want_sell ещё заблокирован, и бот
            // делает buy/reload-клики в окне хранилища.
            let sell_starving = state.lock().need_sell
                && last_sell.elapsed() > SELL_INTERVAL + Duration::from_secs(30)
                && Instant::now() >= sell_blocked_until
                && !reset_blocks_sale;

            // ============================================================
            // ХРАНИЛИЩЕ
            //
            // ВАЖНО:
            // storage обрабатывается ДО need_sell.
            //
            // При полном инвентаре:
            //
            // открыть storage
            // → снять один слот
            // → StorageResult::Unlisted
            // → continue
            // → снова storage
            // → снять следующий
            //
            // need_sell при этом уже может быть true, но продажа
            // НЕ запускается, пока storage не станет пустым.
            // ============================================================

            if titles::is_storage(&title) && !sell_starving {
                storage_streak += 1;
                if storage_streak > STORAGE_LOOP_MAX {
                    warn!("хранилище → {storage_streak} кругов подряд без выхода, закрываю окно и открываю АХ заново");
                    let _ = safe_inv::try_close(&bot);
                    last_storage = Instant::now();
                    break;
                }
                info!("хранилище → зашёл");

                last_storage = Instant::now();

                // первый заход в хранилище после аренды: снимок окна (какие слоты добавились)
                if std::mem::take(&mut state.lock().rent_dump_pending) {
                    storage::dump_storage_window(&bot, &boot, &state, "after_rent");
                }

                // продажа по времени — сразу после очистки витрины в этом же заходе
                if sell_due(last_sell.elapsed()) {
                    state.lock().need_sell = true;
                }

                let res =
                    storage::handle_storage(&bot, &boot, &state).await;

                    match res {
                        // После снятия одного предмета остаёмся
                        // в цикле и снова обрабатываем storage.
                        StorageResult::Unlisted
                        | StorageResult::Stay
                        | StorageResult::BackToAh => continue,
    
                        // storage сам выполнил продажу — обновляем таймер.
                        StorageResult::Sold => {
                            last_sell = Instant::now();
                            break;
                        }
    
                        // GUI закрылось.
                        StorageResult::Closed => break,
                    }
            }

            storage_streak = 0;

            // ============================================================
            // NEED_SELL
            //
            // До сюда мы доходим только если сейчас НЕ storage.
            // Поэтому при очистке витрины этот блок её не прерывает.
            // ============================================================

            let want_sell = state.lock().need_sell
                && Instant::now() >= sell_blocked_until
                && !reset_blocks_sale;
            if want_sell {
                info!("АХ → needSell → прогулка-продажа");
                run_sell(&bot, &boot, &state, &mut last_sell, &mut sell_blocked_until).await;
                break;
            }

            // ============================================================
            // ПОДТВЕРЖДЕНИЕ ПОКУПКИ
            // ============================================================

            if titles::is_confirm(&title) {
                info!("АХ → подтверждаю покупку");

                // задержка ПЕРЕД кликом (JS: ahGlassDelayMs до safeClickBuy)
                delay::rnd("GLASS").await;
                inv.left_click(SLOT_GLASS);

                continue;
            }

            // ============================================================
            // RTP
            // ============================================================

            if titles::is_rtp(&title) {
                info!("RTP → клик");

                delay::rnd("WINDOW_DELAY").await;
                inv.left_click(0usize);

                state.lock().need_rtp = false;
                state.lock().has_dangerous_trash = false;

                delay::rnd("BASE_DELAY").await;

                run_sell(&bot, &boot, &state, &mut last_sell, &mut sell_blocked_until).await;

                break;
            }

            // ============================================================
            // ПРОВЕРКА, НУЖНО ЛИ ОТКРЫТЬ STORAGE
            //
            // КД-снятие здесь больше НЕ используется.
            // ============================================================

            let inventory_full =
                sell::inventory_full(&bot, &boot, &state);

            let open_storage = {
                let st = state.lock();

                // заход в хранилище — по таймеру сброса (60 с от последнего подтверждённого сброса), а не от начала
                // прошлого захода: иначе подтверждение приходит через несколько секунд после захода и сброс
                // срабатывал бы через раз. Повтор не чаще RESET_RETRY.
                (st.reset_soon_now() && last_storage.elapsed() > state::RESET_RETRY)
                    || st.need_reset
                    || inventory_full
            };

            if open_storage && !sell_starving {
                let inventory_full =
                    sell::inventory_full(&bot, &boot, &state);

                let why = {
                    let st = state.lock();

                    if inventory_full {
                        "инвентарь ≥27"
                    } else if st.need_reset {
                        "needReset"
                    } else {
                        "timer60"
                    }
                };

                info!("хранилище → открываю ({why})");

                delay::rnd("STORAGE_ENTER").await;
                let old_win_id = state.lock().last_window_id;
                inv.left_click(SLOT_STORAGE);

                last_storage = Instant::now();

                // Как в JS: пауза только ПЕРЕД кликом. После клика ждём не случайную паузу, а сам факт открытия окна.
                storage::wait_new_window(&state, old_win_id, Duration::from_secs(3)).await;

                // продажа по времени — сразу после очистки витрины в этом же заходе
                if sell_due(last_sell.elapsed()) {
                    state.lock().need_sell = true;
                }

                let res =
                    storage::handle_storage(&bot, &boot, &state).await;

                match res {
                    StorageResult::Stay
                    | StorageResult::Unlisted
                    | StorageResult::BackToAh => continue,

                    // storage сам выполнил продажу — обновляем таймер,
                    // иначе следующая итерация сразу поставит need_sell
                    // и запустит вторую продажу подряд.
                    StorageResult::Sold => {
                        last_sell = Instant::now();
                        break;
                    }

                    // GUI закрылось.
                    StorageResult::Closed => break,
                }
            }


            flush_ah_book(&bot, &boot, &state);

            report_presence(&bot, &boot, &state);

            let force_reload = state.lock().need_reload_ah;

            if force_reload {
                state.lock().need_reload_ah = false;
            }

            let content_before = window_content_key(&bot);
            // отсчёт задержек до клика идёт от момента, когда мы увидели это содержимое окна
            let seen_at = Instant::now();

            // ============================================================
            // ПОКУПКА
            // ============================================================

            if !force_reload {
                if let Some((slot, label, total_delay)) =
                    best_buy_slot(&bot, &boot, &state, seen_at)
                {
                    ipc::pretty_log::info_ansi(&label);

                    // Как JS (runAhBuySession): задержка ДО клика, отсчёт от момента, когда лот
                    // увидели. Пока ждём, лот проверяется постоянно (await_buy_clearance): тот же
                    // ли он, не занял ли его другой бот, всё ещё ли выгоден. Любое отклонение —
                    // клика не будет, выбираем заново.
                    let Some(picked) = picked_lot(&bot, &state, slot) else {
                        continue;
                    };
                    if let Err(why) = await_buy_clearance(
                        &bot,
                        &boot,
                        &state,
                        seen_at,
                        slot,
                        &picked,
                        total_delay,
                    )
                    .await
                    {
                        info!("АХ: слот {slot} — отказ перед кликом: {why}");
                        // отказались от лота — снимаем заявку в Go, чтобы его мог взять другой бот
                        if let (Some(uuid), false) = (&picked.uuid, picked.wear) {
                            emit(&WorkerMsg {
                                name: "buying_release".into(),
                                username: Some(boot.username.clone()),
                                data: Some(json!({ "uuid": uuid })),
                                ..crate::empty_msg()
                            });
                        }
                        continue;
                    }

                    if !safe_inv::try_shift_click(&bot, slot) {
                        break;
                    }

                    let settle =
                        wait_ah_gui_settle(
                            &bot,
                            &state,
                            &content_before,
                        )
                        .await;

                    match settle {
                        GuiSettle::NoWindow => {
                            warn!("АХ → после buy GUI нет");
                            break;
                        }

                        GuiSettle::OtherMenu => {
                            if titles::is_confirm(
                                &state.lock().last_window_title,
                            ) {
                                info!("АХ → подтверждаю покупку");

                                delay::rnd("GLASS").await;
                                let _ =
                                    safe_inv::try_left_click(
                                        &bot,
                                        SLOT_GLASS,
                                    );
                            }

                            continue;
                        }

                        GuiSettle::Changed => {
                            info!(
                                "АХ → buy in-place (слоты сменились)"
                            );
                        }

                        GuiSettle::Timeout => {
                            info!(
                                "АХ → после buy Анализ, продолжаю цикл"
                            );
                        }
                    }

                    continue;
                }
            }

            // ============================================================
            // RELOAD АХ
            // ============================================================

            info!("АХ → обновляю");

            // как JS safeClickBuy: пауза ДО клика по обновлению, от момента появления слотов
            let browse_delay = delay::duration("AH_BROWSE");
            if !wait_before_click(&bot, &state, seen_at, browse_delay, last_storage).await {
                continue;
            }

            // клик «обновить» только в том же окне, с которого начата итерация (аналог botKey в JS)
            if !safe_inv::try_left_click_expect(
                &bot,
                SLOT_RELOAD_MAIN,
                inv.id() as i32,
            ) {
                break;
            }

            let settle =
                wait_ah_gui_settle(
                    &bot,
                    &state,
                    &content_before,
                )
                .await;


            match settle {
                GuiSettle::NoWindow => {
                    warn!("АХ → после reload GUI нет");
                    break;
                }

                GuiSettle::OtherMenu => {
                    info!(
                        "АХ → после reload окно «{}»",
                        state.lock().last_window_title
                    );

                    continue;
                }

                GuiSettle::Changed => {
                    info!(
                        "АХ → reload in-place (слоты сменились)"
                    );

                    stale = 0;
                }

                GuiSettle::Timeout => {
                    if window_content_key(&bot) == content_before {
                        stale += 1;

                        if stale >= 3 {
                            warn!(
                                "АХ → 3× reload без смены слотов → новый поиск"
                            );

                            break;
                        }

                        info!(
                            "АХ → слоты те же после reload ({stale}/3)"
                        );
                    } else {
                        stale = 0;
                    }
                }
            }
        }

        let _ = safe_inv::try_close(&bot);

        delay::rnd("WINDOW_DELAY").await;
    }
}

async fn open_ah_search(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> bool {
    if safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }

    info!("АХ → поиск «{}»", boot.item);

    for attempt in 1..=6 {
        afk::un_afk_quiet(bot).await;

        if state.lock().afk {
            state.lock().afk = false;
        }

        delay::rnd("AH_CMD").await;

        crate::cmd::say(
            bot,
            format!("/ah search {}", boot.item),
        );

        // Не ждём window_tick.
        // Реальный признак открытого контейнера —
        // Inventory.id != 0 и наличие container_menu.
        if wait_window(
            bot,
            delay::duration("CHECK_WINDOW"),
        )
        .await
        {
            info!("АХ → окно поиска открыто");
            return true;
        }

        if attempt == 3 {
            warn!("АХ → поиск без окна, пробую /ah");

            delay::rnd("AH_CMD").await;

            crate::cmd::say(bot, "/ah");

            if wait_window(
                bot,
                delay::duration("CHECK_WINDOW"),
            )
            .await
            {
                info!("АХ → окно поиска открыто");
                return true;
            }
        }

        delay::rnd("POLL").await;
    }

    false
}

async fn wait_window(
    bot: &Client,
    timeout: Duration,
) -> bool {
    let start = Instant::now();

    while start.elapsed() < timeout {
        // try_contents() возвращает Some только когда есть
        // открытый контейнер с container_menu.
        if safe_inv::try_contents(bot).is_some() {
            return true;
        }

        delay::rnd("POLL").await;
    }

    false
}

/// Столько предметов в инвентаре бота — стоп покупкам независимо от Go. Цель — забить АХ (5 лотов), а не
/// инвентарь: по bot-slots.jsonl медиана инвентаря 2, p90 = 21, поэтому 8 режет только накопление.
const LOCAL_INVENTORY_CAP: usize = 8;
/// Лотов на АХ у бота максимум, и сколько предметов сверх этого он держит про запас (слот АХ освобождается
/// примерно раз в 3 минуты, запаса на 3 предмета хватает, чтобы сразу выставить замену).
const HELD_SPARE: usize = 3;
/// Больше стольких кругов подряд в хранилище — считаем, что бот застрял, и перезапускаем АХ. Обычная очистка
/// витрины — до STORAGE_AH_SLOTS (5) снятий плюс пара служебных кругов.
const STORAGE_LOOP_MAX: u32 = 12;
/// Сверх обычного лимита (AH_LOT_SLOTS + HELD_SPARE) берём только «исключительные» лоты и не больше стольких
/// штук: слот под такой лот окупается лучше обычного (по истории capital_cycles Go: маржа ~54%, уходят за
/// ~9 минут, убыточных 1–2%). Порог маржи тот же, что exceptionalMarginFrac в Go (pridumalnahuy.go).
const HELD_EXCEPTIONAL_EXTRA: usize = 4;
const EXCEPTIONAL_MARGIN_FRAC: f64 = 0.40;
const EXCEPTIONAL_SCORE_MULT: f64 = 1.5;

/// Хватает ли денег на лот по последнему `/balance` (его бот шлёт в проходе продажи). Баланс неизвестен — не блокируем.
fn can_afford(balance: Option<f64>, price: f64) -> bool {
    balance.map_or(true, |b| price <= b)
}

/// Бот на обычном лимите: новые лоты не берёт (если только лот не исключительный, см. ниже).
/// `slots` — слотов АХ у бота сейчас (5, с арендой 10).
fn over_normal_cap(held: usize, inv_now: usize, slots: usize) -> bool {
    held >= slots + HELD_SPARE || inv_now >= LOCAL_INVENTORY_CAP
}

/// Жёсткий потолок даже для исключительных лотов — дальше не берём ничего.
fn over_hard_cap(held: usize, inv_now: usize, slots: usize) -> bool {
    held >= slots + HELD_SPARE + HELD_EXCEPTIONAL_EXTRA
        || inv_now >= LOCAL_INVENTORY_CAP + HELD_EXCEPTIONAL_EXTRA
}

/// Исключительный лот сверх лимита: оценка лота ≥ EXCEPTIONAL_SCORE_MULT × эталон флота сейчас (`score_ref`,
/// медиана оценок свежих покупок по категории, считает Go) — сравнение с тем, что рынок даёт прямо сейчас.
/// Эталона нет (Go ещё не накопил) — запасное правило по марже: ≥ EXCEPTIONAL_MARGIN_FRAC от цены лота.
fn exceptional_lot(score: f64, score_ref: f64, sell_price: f64, price: f64) -> bool {
    if score_ref > 0.0 {
        return score >= EXCEPTIONAL_SCORE_MULT * score_ref;
    }
    price > 0.0 && sell_price - price >= price * EXCEPTIONAL_MARGIN_FRAC
}

/// Оценка лота: маржа / √(сколько уже на руках + 1). Чем больше товара уже лежит, тем дольше новый лот займёт
/// слот (очередь продаж), но не пропорционально: по данным (5 тыс. закрытых лотов, проверка на отложенной части)
/// это лучше предсказывает реальную прибыль на слот-час, чем маржа / (held+1)/скорость (корреляция 0,33 против 0,19):
/// скорость продаж за короткое окно слишком шумная. Сравнивать оценки имеет смысл между собой и с эталоном флота
/// сейчас (`score_ref`), а не с фиксированным числом.
fn lot_score(margin: f64, held: u32) -> f64 {
    margin / (f64::from(held) + 1.0).sqrt()
}

/// Когда бот собирается кликнуть: базовая задержка по ряду + случайные 0–600 мс, отсчёт от `seen_at`
/// (момента, когда лот увидели). Решается ДО заявки в Go, чтобы Go знал, когда именно бот купит.
fn plan_click_delay(slot: usize) -> Duration {
    delay::ah_buy_delay(slot) + Duration::from_millis(rand::thread_rng().gen_range(0..=600u64))
}

/// Возвращает слот, подпись для лога и полную задержку до клика (от `seen_at`).
fn best_buy_slot(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    seen_at: Instant,
) -> Option<(usize, String, Duration)> {
    if safe_inv::try_id(bot)? == 0 {
        return None;
    }

    let contents = safe_inv::try_contents(bot)?;

    let (catalog_all, buying, username, wear_need) = {
        let s = state.lock();

        (
            s.catalog_all.clone(),
            s.items_buying.clone(),
            boot.username.clone(),
            s.wear_missing.clone(),
        )
    };

    // Своя позорная броня (wear.rs): слот брони не занят незеритом, а лот это позорная броня
    // такого типа. Первый попавшийся, цены и книга не важны, от Go не зависит
    // (набор зашит в wear.rs), в Go не отправляется.
    if !wear_need.is_empty() {
        let wear_balance = state.lock().balance;
        for slot in FIRST_AH..=LAST_BUYABLE {
            let Some(data) = contents.get(slot).and_then(present) else {
                continue;
            };
            let Some(price) = ah_price(data) else {
                continue;
            };
            // Как и в обычной покупке: не берём лот, на который не хватит денег (иначе «не хватает монет» по кругу).
            if !can_afford(wear_balance, price) {
                continue;
            }
            let Some(armor_slot) = crate::wear::wear_slot_for_lot(
                &crate::item_parse::item_minecraft_name(data),
                &all_enchant_labels(bot, data),
                &wear_need,
            ) else {
                continue;
            };
            let own = ah_seller_nick(data).eq_ignore_ascii_case(&username);
            let taken = auction_uuid(data)
                .is_some_and(|u| state::uuid_blocked_by_other(&buying, &u, &username));
            if own || taken {
                continue;
            }
            let label = crate::wear::begin_purchase(state, armor_slot, price);
            return Some((slot, label, plan_click_delay(slot)));
        }
    }

    if catalog_all.is_empty() {
        return None;
    }

    // Сколько у бота уже накоплено (лоты на АХ + инвентарь): уходит в Go вместе с заявкой на лот,
    // при нескольких заявках на один лот Go отдаёт его тому, у кого накоплено меньше (lot_claims.go).
    let inv_now = crate::sell::count_our_items(bot, boot, state) as usize;
    // Универсальный потолок на боте, не зависит от Go и связи: с таким инвентарём новые лоты не берём
    // (сначала выставить/разгрузить). Порог выше любого порога Go, это страховка.
    let on_ah = state.lock().listings.all_metas().len();
    let held = on_ah + inv_now;
    // АХ полон и запас в инвентаре уже есть: чужим ботам с пустым АХ этот лот нужнее
    // Сверх обычного лимита (но не выше жёсткого) разрешаем только исключительные лоты.
    // слотов АХ у бота сейчас: 5, а пока действует аренда — 10
    let lot_slots = crate::rent::slots(state);
    let over_cap = over_normal_cap(held, inv_now, lot_slots);
    if over_hard_cap(held, inv_now, lot_slots) {
        tracing::debug!("АХ → накоплено {held} шт (АХ {on_ah} + инвентарь {inv_now}), новые лоты не беру");
        return None;
    }
    let ah_full = on_ah >= lot_slots;

    let mut no_price: Vec<String> = Vec::new();
    let mut cands: Vec<(usize, f64, f64, crate::item_parse::SlotInfo)> = Vec::new();
    let balance = state.lock().balance;

    for slot in FIRST_AH..=LAST_BUYABLE {
        let Some(data) =
            contents.get(slot).and_then(present)
        else {
            continue;
        };

        let price = ah_price(data);

        let kind = crate::item_parse::get_slot_kind(
            bot,
            data,
            &catalog_all,
            &boot.go_type,
        );

        let info = kind.as_trade().cloned();

        let Some(price) = price else {
            no_price.push(format!(
                "slot{slot} {:?}",
                data.kind
            ));

            continue;
        };

        let Some(info) = info else {
            continue;
        };

        if price >= info.buy_price {
            continue;
        }

        let sell_cap =
            (info.sell_price * 0.90).floor();

        if sell_cap > 0.0 && price >= sell_cap {
            continue;
        }

        // Не пытаемся купить то, на что не хватит денег (сервер ответит «не хватает монет», а попытка стоит времени).
        if !can_afford(balance, price) {
            continue;
        }

        // Приоритет — прибыль на слот, а не порядок слотов (lot_score).
        let score = lot_score(info.sell_price - price, info.held);

        // На обычном лимите берём только исключительные лоты: оценка заметно выше типичной у флота сейчас.
        if over_cap && !exceptional_lot(score, info.score_ref, info.sell_price, price) {
            continue;
        }

        if let Some(uuid) = auction_uuid(data) {
            if state::uuid_blocked_by_other(
                &buying,
                &uuid,
                &username,
            ) {
                continue;
            }
        }

        cands.push((slot, score, price, info));
    }

    // берём первый подходящий лот окна (кандидаты набраны в порядке слотов)
    if let Some((slot, score, price, info)) = cands.into_iter().next() {
        let Some(data) = contents.get(slot).and_then(present) else {
            return None;
        };
        tracing::debug!("АХ → слот {slot}: оценка {:.0}/ч ({})", score, info.id);
        let plan = plan_click_delay(slot);
        let click_in = plan.saturating_sub(seen_at.elapsed());
    if let Some(uuid) = auction_uuid(data) {
        emit(&WorkerMsg {
            name: "buying".into(),
            username: Some(username.clone()),
            data: Some(json!({
                "uuid": uuid,
                "id": info.id,
                "price": price,
                "held": held,
                "ah_full": ah_full,
                // через сколько мс бот кликнет: Go отдаёт лот другому боту, только если тот кликнет
                // не позже (иначе потеряли бы время и лот)
                "click_in_ms": click_in.as_millis() as u64
            })),
            ..crate::empty_msg()
        });
    }

    // buy в Go — только после чата
    // «Вы успешно купили»
    let enchants: Value = json!(
        all_enchant_labels(bot, data)
            .into_iter()
            .map(|(n, l)| json!({
                "name": n,
                "lvl": l
            }))
            .collect::<Vec<_>>()
    );

    let dur = durability_percent(data);

    {
        let mut s = state.lock();

        s.buying_item = state::BuyingItem {
            id: info.id.clone(),
            price,
            buy_price: info.buy_price,
            nacenka: info.nacenka,
            enchants,
            durability: Some(dur),
            wear: None,
        };
    }

    let dur_pct =
        (dur * 100.0).round() as i32;

    let label = ipc::pretty_log::ah_buy_line(
        "покупаю",
        &info.id,
        &crate::money::money(price),
        Some(dur_pct),
    );

    return Some((slot, label, plan));
    }

    if !no_price.is_empty() {
        let show = no_price.len().min(3);

        warn!(
            "АХ → цена не найдена у {} лот(ов): {}",
            no_price.len(),
            no_price[..show].join("; ")
        );
    }

    None
}

fn flush_ah_book(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) {
    let Some(contents) =
        safe_inv::try_contents(bot)
    else {
        return;
    };

    let catalog = state.lock().catalog_all.clone();

    if catalog.is_empty() {
        return;
    }

    let mut lots = Vec::new();

    let mut skip_uuid = 0u32;
    let mut skip_match = 0u32;
    let mut skip_other = 0u32;

    for slot in contents.iter().take(45) {
        let Some(data) = present(slot) else {
            continue;
        };

        {
            let kind = get_slot_kind(bot, data, &catalog, &boot.go_type);
            let (match_kind, match_id) = match &kind {
                crate::item_parse::SlotKind::Trade(info) => ("trade", info.id.as_str()),
                crate::item_parse::SlotKind::Foreign { id, .. } => ("foreign", id.as_str()),
                crate::item_parse::SlotKind::Trash => ("trash", ""),
            };
            maybe_dump_ah_nbt(
                bot,
                data,
                match_kind,
                match_id,
                ah_price(data),
                auction_uuid(data).as_deref().unwrap_or(""),
                &ah_seller_nick(data),
                &boot.username,
            );
        }

        if skip_ah_book_crusher(bot, data) {
            skip_other += 1;
            continue;
        }

        // броня с починкой — отдельные SKU
        // (*-починка-1.21), в книгу шлём.
        let dur = durability_percent(data);

        // как JS AH_BOOK_MIN_DURABILITY = 0.9
        if dur < 0.9 {
            skip_other += 1;
            continue;
        }

        let Some(price) = ah_price(data) else {
            continue;
        };

        // JS AH_FAKE_SLOT_PRICE_MAX = 150000
        if price <= 150_000.0 {
            skip_other += 1;
            continue;
        }

        let Some(uuid) = auction_uuid(data) else {
            skip_uuid += 1;
            continue;
        };

        if state.lock().ah_book_sent_uuids.contains(&uuid) {
            skip_other += 1;
            continue;
        }

        let seller = ah_seller_nick(data);

        // как JS collectAhBookLots:
        // свои лоты в книгу не шлём.
        if !seller.is_empty()
            && seller.eq_ignore_ascii_case(&boot.username)
        {
            skip_other += 1;
            continue;
        }

        // Trade = наш тип,
        // Foreign = чужой из каталога.
        // Оба в книгу (как JS catalogAll).
        let (item_id, go_type) =
            match crate::item_parse::get_slot_kind(
                bot,
                data,
                &catalog,
                &boot.go_type,
            ) {
                crate::item_parse::SlotKind::Trade(info) => {
                    (info.id, boot.go_type.clone())
                }

                crate::item_parse::SlotKind::Foreign {
                    id,
                    foreign_type,
                } => (id, foreign_type),

                crate::item_parse::SlotKind::Trash => {
                    skip_match += 1;
                    continue;
                }
            };

        {
            let mut s = state.lock();

            if s.ah_book_sent_uuids.len() >= 40_000 {
                s.ah_book_sent_uuids.clear();
            }

            s.ah_book_sent_uuids.insert(uuid.clone());
        }

        let enchants: Value = json!(
            all_enchant_labels(bot, data)
                .into_iter()
                .map(|(n, l)| json!({
                    "name": n,
                    "lvl": l
                }))
                .collect::<Vec<_>>()
        );

        let price_i = price.round() as i64;

        lots.push(json!({
            "uuid": uuid,
            "go_type": go_type,
            "item_id": item_id,
            "price": price_i,
            "durability": (dur * 1000.0).round() / 1000.0,
            "seller": seller,
            "enchants": enchants,
            "anarchy": boot.anarchy,
            "seen_by": boot.username,
        }));
    }

    if lots.is_empty() {
        if skip_uuid + skip_match > 0 {
            warn!(
                "книга АХ пусто: uuid∅={skip_uuid} trash={skip_match} other={skip_other}"
            );
        }

        return;
    }

    info!(
        "книга АХ → {} лотов (uuid∅={skip_uuid} trash={skip_match})",
        lots.len()
    );

    emit(&WorkerMsg {
        name: "ah_lots".into(),
        username: Some(boot.username.clone()),
        lots: Some(json!(lots)),
        ..crate::empty_msg()
    });
}

fn window_content_key(bot: &Client) -> String {
    let Some(contents) =
        safe_inv::try_contents(bot)
    else {
        return String::new();
    };

    let mut parts = Vec::new();

    for (i, slot) in contents.iter().enumerate().take(45) {
        if let Some(d) = present(slot) {
            let p =
                ah_price(d).unwrap_or(0.0) as i64;

            let u =
                auction_uuid(d).unwrap_or_default();

            parts.push(format!(
                "{i}:{:?}:{p}:{u}",
                d.kind
            ));
        } else {
            parts.push(format!("{i}:_"));
        }
    }

    parts.join("|")
}

// Дожать паузу после клика до полного budget
// (темп между действиями).
/// Ждём, пока с момента `since` пройдёт `total`. Опрос раз в 100 мс; `false` — окно
/// закрылось/сменилось на не-АХ, бот останавливается или забанен (клик делать нельзя).
async fn wait_before_click(
    bot: &Client,
    state: &Arc<Mutex<BotState>>,
    since: Instant,
    total: Duration,
    last_storage: Instant,
) -> bool {
    loop {
        if state.lock().shutdown || state.lock().banned {
            return false;
        }
        // подошёл таймер сброса витрины — не досиживаем паузу, иначе заход в хранилище опаздывает на целую итерацию
        if last_storage.elapsed() > state::RESET_RETRY && state.lock().reset_soon_now() {
            return false;
        }
        match safe_inv::try_id(bot) {
            Some(id) if id != 0 => {}
            _ => return false,
        }
        if !is_analysis_ah_title(&state.lock().last_window_title) {
            return false;
        }
        let left = total.saturating_sub(since.elapsed());
        if left.is_zero() {
            return true;
        }
        tokio::time::sleep(left.min(Duration::from_millis(100))).await;
    }
}

/// Лот, который выбрали к покупке (запоминаем в момент выбора).
struct PickedLot {
    uuid: Option<String>,
    price: f64,
    /// id товара из каталога (`None` для своей брони)
    trade_id: Option<String>,
    wear: bool,
    /// Когда выбрали лот: первые секунды чужая заявка не отказ (Go может отдать лот нам, если мы свободнее).
    picked_at: Instant,
}

fn picked_lot(bot: &Client, state: &Arc<Mutex<BotState>>, slot: usize) -> Option<PickedLot> {
    let contents = safe_inv::try_contents(bot)?;
    let data = present(contents.get(slot)?)?;
    let price = ah_price(data)?;
    let s = state.lock();
    let wear = s.buying_item.wear.is_some();
    Some(PickedLot {
        uuid: auction_uuid(data),
        price,
        trade_id: if wear { None } else { Some(s.buying_item.id.clone()) },
        wear,
        picked_at: Instant::now(),
    })
}

/// Лот выгоден по тем же правилам, что в `best_buy_slot` (цена Go могла измениться).
fn trade_price_ok(info: &crate::item_parse::SlotInfo, price: f64) -> bool {
    if price >= info.buy_price {
        return false;
    }
    let sell_cap = (info.sell_price * 0.90).floor();
    if sell_cap > 0.0 && price >= sell_cap {
        return false;
    }
    true
}

/// Заявка на лот записана за нами.
fn claim_is_mine(items_buying: &Value, uuid: &str, username: &str) -> bool {
    items_buying.as_array().is_some_and(|arr| {
        arr.iter().any(|raw| {
            raw.get("uuid").and_then(|v| v.as_str()) == Some(uuid)
                && raw.get("username").and_then(|v| v.as_str()) == Some(username)
        })
    })
}

/// Все проверки лота перед кликом. `Err` — причина отказа. `Ok(true)` — заявка на лот записана
/// за нами, `Ok(false)` — её в списке нет (ещё не вернулась от Go или список обновился); чужой
/// владелец — это уже `Err`.
/// Столько после выбора лота терпим чужого владельца в заявке (окно выбора в Go — 1 с + дорога).
const OUTBID_GRACE: Duration = Duration::from_millis(1400);

fn check_lot(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    slot: usize,
    p: &PickedLot,
) -> Result<bool, &'static str> {
    {
        let s = state.lock();
        if s.shutdown || s.banned {
            return Err("бот останавливается");
        }
    }
    match safe_inv::try_id(bot) {
        Some(id) if id != 0 => {}
        _ => return Err("окно закрылось"),
    }
    if !is_analysis_ah_title(&state.lock().last_window_title) {
        return Err("окно сменилось");
    }
    let contents = safe_inv::try_contents(bot).ok_or("нет содержимого окна")?;
    let data = contents
        .get(slot)
        .and_then(present)
        .ok_or("слот опустел")?;
    if auction_uuid(data) != p.uuid {
        return Err("в слоте другой лот");
    }
    if ah_price(data) != Some(p.price) {
        return Err("цена лота изменилась");
    }
    let mut mine = true;
    if let Some(u) = &p.uuid {
        let s = state.lock();
        if state::uuid_blocked_by_other(&s.items_buying, u, &boot.username) {
            // Go в течение ~1 с после первой заявки отдаёт лот тому, у кого накоплено меньше: ждём
            // его решения, прежде чем считать лот чужим.
            if p.picked_at.elapsed() < OUTBID_GRACE {
                return Ok(false);
            }
            return Err("лот занял другой бот");
        }
        mine = p.wear || claim_is_mine(&s.items_buying, u, &boot.username);
    }
    let (catalog_all, wear_missing) = {
        let s = state.lock();
        (s.catalog_all.clone(), s.wear_missing.clone())
    };
    if p.wear {
        if crate::wear::wear_slot_for_lot(
            &crate::item_parse::item_minecraft_name(data),
            &all_enchant_labels(bot, data),
            &wear_missing,
        )
        .is_none()
        {
            return Err("эта броня уже не нужна");
        }
    } else {
        let kind = crate::item_parse::get_slot_kind(bot, data, &catalog_all, &boot.go_type);
        let Some(info) = kind.as_trade() else {
            return Err("лота больше нет в каталоге");
        };
        if Some(&info.id) != p.trade_id.as_ref() {
            return Err("в слоте другой товар");
        }
        if !trade_price_ok(info, p.price) {
            return Err("цена Go изменилась, лот невыгоден");
        }
    }
    Ok(mine)
}

/// Ждём задержку перед покупкой, проверяя лот каждые 80–150 мс, и один раз ещё прямо перед кликом.
/// Задержка = базовая из `ah_buy_delay` плюс случайный добавок 0–600 мс, чтобы боты не кликали
/// в один момент.
async fn await_buy_clearance(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
    seen_at: Instant,
    slot: usize,
    picked: &PickedLot,
    total: Duration,
) -> Result<(), &'static str> {
    // Заявка может на миг пропасть из общего списка, когда Go присылает обновление, в котором
    // ещё нет нашей записи. Это терпим; чужой владелец — отказ сразу (его вернёт check_lot),
    // а если нашей заявки нет дольше MAX_CLAIM_GAP подряд, что-то не так со связью — тоже отказ.
    const MAX_CLAIM_GAP: Duration = Duration::from_millis(1500);
    let deadline = total;
    let mut missing_since: Option<Instant> = None;
    loop {
        if check_lot(bot, boot, state, slot, picked)? {
            missing_since = None;
        } else {
            let since = *missing_since.get_or_insert_with(Instant::now);
            if since.elapsed() > MAX_CLAIM_GAP {
                return Err("заявка на лот не подтверждается");
            }
        }
        let left = deadline.saturating_sub(seen_at.elapsed());
        if left.is_zero() {
            break;
        }
        let step = rand::thread_rng().gen_range(80..=150u64);
        tokio::time::sleep(left.min(Duration::from_millis(step))).await;
    }
    // последняя проверка уже после ожидания, как можно ближе к клику: только чужой владелец
    // или изменившийся лот останавливают
    check_lot(bot, boot, state, slot, picked).map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuiSettle {
    // слоты сменились in-place —
    // можно не ждать полный WINDOW_DELAY
    Changed,

    // другое меню (подтверждение / хранилище / …)
    OtherMenu,

    NoWindow,

    Timeout,
}

fn is_analysis_ah_title(title: &str) -> bool {
    if title.trim().is_empty() {
        return true;
    }

    // подтверждение покупки, хранилище и RTP — не окно просмотра АХ
    !(titles::is_confirm(title) || titles::is_storage(title) || titles::is_rtp(title))
}

// Как old waitAhGuiSettle:
// поллим слоты, ранний выход при смене / другом меню.
async fn wait_ah_gui_settle(
    bot: &Client,
    state: &Arc<Mutex<BotState>>,
    content_before: &str,
) -> GuiSettle {
    const MIN_SETTLE_MS: u64 = 250;
    const POLL_MS: u64 = 100;

    let max =
        delay::duration("WINDOW_DELAY");

    let t0 = Instant::now();

    loop {
        if state.lock().shutdown
            || state.lock().banned
        {
            return GuiSettle::Timeout;
        }

        let elapsed = t0.elapsed();

        if elapsed >= max {
            return GuiSettle::Timeout;
        }

        let left =
            max.saturating_sub(elapsed);

        let chunk =
            Duration::from_millis(POLL_MS)
                .min(left);

        if !chunk.is_zero() {
            tokio::time::sleep(chunk).await;
        }

        match safe_inv::try_id(bot) {
            Some(0) | None => {
                return GuiSettle::NoWindow;
            }

            Some(_) => {}
        }

        let title =
            state.lock().last_window_title.clone();

        if !is_analysis_ah_title(&title) {
            return GuiSettle::OtherMenu;
        }

        if t0.elapsed()
            >= Duration::from_millis(MIN_SETTLE_MS)
            && window_content_key(bot)
                != content_before
        {
            return GuiSettle::Changed;
        }
    }
}

fn report_presence(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) {
    // Как JS: items (витрина 0–4) шлём только из хранилища.
    // Здесь при обходе АХ только inventory — иначе затираем AH-id
    // кривым {go_type:count}.

    let ids =
        sell::inventory_catalog_ids(
            bot,
            boot,
            state,
        );

    emit(&WorkerMsg {
        name: "inventory".into(),
        username: Some(boot.username.clone()),
        data: Some(json!(ids)),
        ..crate::empty_msg()
    });
}
#[cfg(test)]
mod lot_score_tests {
    use super::*;

    #[test]
    fn bigger_stock_lowers_score() {
        assert!(lot_score(500_000.0, 0) > lot_score(500_000.0, 6));
    }
    #[test]
    fn score_grows_with_margin() {
        assert!(lot_score(900_000.0, 3) > lot_score(300_000.0, 3));
    }
    #[test]
    fn sqrt_not_linear_in_held() {
        // held 3 → √4 = 2, held 8 → √9 = 3
        assert!((lot_score(600_000.0, 3) - 300_000.0).abs() < 1.0);
        assert!((lot_score(600_000.0, 8) - 200_000.0).abs() < 1.0);
    }
}

#[cfg(test)]
mod outbid_timing_tests {
    use super::*;

    /// Окно, в которое Go отдаёт лот более свободному боту (lot_claims.go, `lotClaimWindow`).
    const GO_OUTBID_WINDOW_MS: u64 = 1000;

    #[test]
    fn click_comes_after_the_owner_is_final() {
        let min_click = u64::from(delay::AH_BUY_DELAY_MIN_MS);
        // окно + запас на дорогу бот→orch→Go→orch→бот
        assert!(min_click >= GO_OUTBID_WINDOW_MS + 700, "клик раньше, чем закроется окно выбора");
        // терпимость к чужому владельцу не дольше, чем бот вообще ждёт до клика
        assert!(OUTBID_GRACE.as_millis() as u64 <= min_click);
        assert!(OUTBID_GRACE.as_millis() as u64 > GO_OUTBID_WINDOW_MS);
    }
}

#[cfg(test)]
mod prebuy_tests {
    use super::*;

    fn info(buy: f64, sell: f64) -> crate::item_parse::SlotInfo {
        crate::item_parse::SlotInfo {
            id: "x".into(),
            sell_price: sell,
            buy_price: buy,
            nacenka: 0.0,
            book_floor: 0.0,
            book_mid: 0.0,
            rate_per_hour: 0.0,
            held: 0,
            score_ref: 0.0,
        }
    }

    #[test]
    fn price_rules_match_best_buy_slot() {
        let i = info(1_000_000.0, 1_500_000.0);
        assert!(trade_price_ok(&i, 900_000.0));
        assert!(!trade_price_ok(&i, 1_000_000.0)); // не ниже лимита покупки
        assert!(!trade_price_ok(&info(2_000_000.0, 1_000_000.0), 950_000.0)); // ≥ 90% продажи
    }

    #[test]
    fn claim_must_be_ours() {
        let b = json!([{"uuid": "u1", "username": "me"}, {"uuid": "u2", "username": "other"}]);
        assert!(claim_is_mine(&b, "u1", "me"));
        assert!(!claim_is_mine(&b, "u2", "me"));
        assert!(!claim_is_mine(&b, "u3", "me"));
        assert!(!claim_is_mine(&json!([]), "u1", "me"));
    }

    #[test]
    fn caps_and_exceptional_lots() {
        // обычный лимит: 5 на АХ + 3 запас = 8
        assert!(!over_normal_cap(7, 2, 5));
        assert!(over_normal_cap(8, 3, 5));
        assert!(over_normal_cap(5, 8, 5));
        // с арендой (10 слотов) лимит выше: 10 + 3 = 13
        assert!(!over_normal_cap(12, 2, 10));
        assert!(over_normal_cap(13, 3, 10));
        // жёсткий потолок: 12 шт или 12 в инвентаре
        assert!(!over_hard_cap(11, 6, 5));
        assert!(over_hard_cap(12, 6, 5));
        assert!(over_hard_cap(9, 12, 5));
        assert!(!over_hard_cap(16, 6, 10));
        assert!(over_hard_cap(17, 6, 10));
        // исключительный лот: эталон есть — оценка ≥ 1,5 × эталон
        assert!(exceptional_lot(300_000.0, 200_000.0, 0.0, 0.0));
        assert!(!exceptional_lot(299_000.0, 200_000.0, 0.0, 0.0));
        // эталона нет — запасное правило: маржа ≥ 40% цены лота
        assert!(exceptional_lot(0.0, 0.0, 1_500_000.0, 1_000_000.0));
        assert!(exceptional_lot(0.0, 0.0, 1_400_000.0, 1_000_000.0));
        assert!(!exceptional_lot(0.0, 0.0, 1_350_000.0, 1_000_000.0));
        assert!(!exceptional_lot(0.0, 0.0, 1_000_000.0, 0.0));
    }
}

#[cfg(test)]
mod sell_timing_tests {
    use super::*;

    #[test]
    fn timed_sell_is_due_after_the_interval() {
        assert!(!sell_due(Duration::from_secs(0)));
        assert!(!sell_due(Duration::from_secs(54)));
        assert!(sell_due(Duration::from_secs(55)));
        assert!(sell_due(Duration::from_secs(300)));
    }
}

#[cfg(test)]
mod afford_tests {
    use super::*;

    #[test]
    fn affordability() {
        assert!(can_afford(None, 9e9)); // баланс неизвестен — не блокируем
        assert!(can_afford(Some(2_000_000.0), 2_000_000.0)); // ровно хватает
        assert!(!can_afford(Some(2_000_000.0), 2_000_001.0));
        assert!(can_afford(Some(0.0), 0.0));
    }
}
