use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::{
    afk,
    anarchy,
    delay,
    item_parse::{
        ah_price, ah_seller_nick, all_enchant_labels, auction_uuid, durability_percent,
        present, skip_ah_book_crusher,
    },
    safe_inv,
    sell,
    state::{self, BotState},
    storage::{self, StorageResult},
    trash,
};

const FIRST_AH: usize = 0;
const LAST_BUYABLE: usize = 17;
const SLOT_STORAGE: usize = 46;

// Как JS AH_SLOT_RELOAD_MAIN / pickAhBrowseAction — всегда 50.
const SLOT_RELOAD_MAIN: usize = 50;

// подтверждение покупки — как slotGlass в 4narek-old
const SLOT_GLASS: usize = 0;

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

    let mut cycle: u64 = 0;
    let mut last_storage = Instant::now();

    loop {
        if state.lock().shutdown || state.lock().banned {
            break;
        }

        if state.lock().owner_ban_drain {
            info!("овнер бан → стоп АХ, казна");
            sell::sell_pass(&bot, &boot, &state).await;
            break;
        }

        if !state.lock().on_anarchy {
            warn!("слетели с анки — снова /an");

            if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
                break;
            }
        }

        cycle += 1;

        if state.lock().afk {
            info!("снимаю AFK");
            afk::un_afk(&bot).await;
            state.lock().afk = false;
        }

        trash::do_rtp_if_needed(&bot, &state).await;

        // Периодическая продажа остаётся.
        sell::sell_pass(&bot, &boot, &state).await;

        delay::rnd("BASE_DELAY").await;

        if state.lock().afk {
            afk::un_afk(&bot).await;
            state.lock().afk = false;
        }

        delay::rnd("AH_CMD").await;

        if !open_ah_search(&bot, &boot, &state).await {
            warn!("АХ не открылся — повтор");

            afk::un_afk(&bot).await;

            delay::rnd("CHECK_WINDOW").await;

            continue;
        }

        let mut stale = 0u32;

        loop {
            if state.lock().shutdown {
                return;
            }

            let Some(inv) = safe_inv::try_handle(&bot) else {
                warn!("АХ → нет Inventory mid-browse");
                break;
            };

            if inv.id() == 0 {
                warn!("АХ закрылся mid-browse");
                break;
            }

            let title = state.lock().last_window_title.to_lowercase();

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

            if title.contains("хранилищ")
                || title.contains("мои товар")
            {
                info!("хранилище → зашёл");

                last_storage = Instant::now();

                let res =
                    storage::handle_storage(&bot, &boot, &state).await;

                match res {
                    // После снятия одного предмета остаёмся
                    // в цикле и снова обрабатываем storage.
                    StorageResult::Unlisted
                    | StorageResult::Stay
                    | StorageResult::BackToAh => continue,

                    // storage сам выполнил продажу либо GUI закрылось.
                    StorageResult::Sold
                    | StorageResult::Closed => break,
                }
            }

            // ============================================================
            // NEED_SELL
            //
            // До сюда мы доходим только если сейчас НЕ storage.
            // Поэтому при очистке витрины этот блок её не прерывает.
            // ============================================================

            if state.lock().need_sell {
                info!("АХ → needSell → прогулка-продажа");

                let _ = safe_inv::try_close(&bot);

                delay::rnd("500MS").await;

                sell::sell_pass(&bot, &boot, &state).await;

                state.lock().need_sell = false;

                break;
            }

            // ============================================================
            // ПОДТВЕРЖДЕНИЕ ПОКУПКИ
            // ============================================================

            if title.contains("подтверж")
                || title.contains("подтвердить")
                || title.contains("accept")
            {
                info!("АХ → подтверждаю покупку");

                inv.left_click(SLOT_GLASS);

                delay::rnd("GLASS").await;

                continue;
            }

            // ============================================================
            // RTP
            // ============================================================

            if title.contains("rtp")
                || title.contains("ртп")
                || title.contains("случай")
            {
                info!("RTP → клик");

                inv.left_click(0usize);

                state.lock().need_rtp = false;
                state.lock().has_dangerous_trash = false;

                delay::rnd("BASE_DELAY").await;

                sell::sell_pass(&bot, &boot, &state).await;

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

                last_storage.elapsed() > Duration::from_secs(60)
                    || st.need_reset
                    || inventory_full
            };

            if open_storage {
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

                inv.left_click(SLOT_STORAGE);

                last_storage = Instant::now();

                delay::rnd("BASE_DELAY").await;

                let res =
                    storage::handle_storage(&bot, &boot, &state).await;

                match res {
                    StorageResult::Stay
                    | StorageResult::Unlisted
                    | StorageResult::BackToAh => continue,

                    StorageResult::Sold
                    | StorageResult::Closed => break,
                }
            }

            // ============================================================
            // ОБЫЧНЫЙ NEED_SELL
            //
            // Этот блок нужен для случаев, когда need_sell появился
            // не из-за очистки полной витрины.
            // ============================================================

            if state.lock().need_sell
                && sell::count_our_items(&bot, &boot, &state) > 0
            {
                info!("АХ → sellItems (needSell)");

                let _ = safe_inv::try_close(&bot);

                delay::rnd("500MS").await;

                sell::sell_pass(&bot, &boot, &state).await;

                state.lock().need_sell = false;

                break;
            }

            flush_ah_book(&bot, &boot, &state);

            report_presence(&bot, &boot, &state);

            let force_reload = state.lock().need_reload_ah;

            if force_reload {
                state.lock().need_reload_ah = false;
            }

            let content_before = window_content_key(&bot);

            // ============================================================
            // ПОКУПКА
            // ============================================================

            if !force_reload {
                if let Some((slot, label)) =
                    best_buy_slot(&bot, &boot, &state)
                {
                    info!("{label}");

                    let click_budget = delay::ah_buy_delay(slot);
                    let click_started = Instant::now();

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

                    finish_click_budget(
                        click_started,
                        click_budget,
                        "buy",
                    )
                    .await;

                    match settle {
                        GuiSettle::NoWindow => {
                            warn!("АХ → после buy GUI нет");
                            break;
                        }

                        GuiSettle::OtherMenu => {
                            if is_confirm_title(
                                &state.lock().last_window_title,
                            ) {
                                info!("АХ → подтверждаю покупку");

                                let _ =
                                    safe_inv::try_left_click(
                                        &bot,
                                        SLOT_GLASS,
                                    );

                                delay::rnd("GLASS").await;
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

            let click_budget = delay::duration("AH_BROWSE");
            let click_started = Instant::now();

            if !safe_inv::try_left_click(
                &bot,
                SLOT_RELOAD_MAIN,
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

            finish_click_budget(
                click_started,
                click_budget,
                "reload",
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

fn best_buy_slot(
    bot: &Client,
    boot: &BotBoot,
    state: &Arc<Mutex<BotState>>,
) -> Option<(usize, String)> {
    if safe_inv::try_id(bot)? == 0 {
        return None;
    }

    let contents = safe_inv::try_contents(bot)?;

    let (catalog_all, max_buy, buying, username) = {
        let s = state.lock();

        (
            s.catalog_all.clone(),
            s.max_buy_price,
            s.items_buying.clone(),
            boot.username.clone(),
        )
    };

    if catalog_all.is_empty() {
        return None;
    }

    let mut no_price: Vec<String> = Vec::new();

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

        if let Some(mb) = max_buy {
            if price > mb {
                continue;
            }
        }

        if let Some(uuid) = auction_uuid(data) {
            if state::uuid_blocked_by_other(
                &buying,
                &uuid,
                &username,
            ) {
                continue;
            }

            emit(&WorkerMsg {
                name: "buying".into(),
                username: Some(username.clone()),
                data: Some(json!({
                    "uuid": uuid,
                    "id": info.id,
                    "price": price
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

        return Some((slot, label));
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
async fn finish_click_budget(
    started: Instant,
    budget: Duration,
    label: &str,
) {
    let left =
        budget.saturating_sub(started.elapsed());

    if left.is_zero() {
        return;
    }

    info!(
        "АХ → остаток клика {label} {}мс",
        left.as_millis()
    );

    tokio::time::sleep(left).await;
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

fn is_confirm_title(title: &str) -> bool {
    let t = title.to_lowercase();

    t.contains("подтверж")
        || t.contains("подтвердить")
        || t.contains("accept")
}

fn is_analysis_ah_title(title: &str) -> bool {
    let t = title.to_lowercase();

    if t.is_empty() {
        return true;
    }

    if is_confirm_title(title) {
        return false;
    }

    if t.contains("хранилищ")
        || t.contains("мои товар")
    {
        return false;
    }

    if t.contains("rtp")
        || t.contains("ртп")
        || t.contains("случай")
    {
        return false;
    }

    true
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
