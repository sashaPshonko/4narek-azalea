//! Режим «инвиз»: бот не торгует бронёй/мечами по каталогу Go, а перепродаёт зелье невидимости.
//!
//! Схема (записана с живого клиента 10.11.2026, `bot-view/packets.sell.jsonl`):
//!   * покупка — `/ah search инвиз` (окно 9×6), shift-клик по лоту-стаку; сервер сразу пишет
//!     «Вы успешно купили Зелье Невидимости x64 за $…»;
//!   * продажа — `/ah sell <цена>`: **продаёт только когда в руке одна штука**. Стак делится по хотбару
//!     (берём стак курсором, правым кликом по одной штуке в пустые слоты хотбара, остаток кладём на место), потом
//!     для каждой штуки: выбрать слот хотбара, `/ah sell 44999`. Ответ: «Предмет Зелье Невидимости выставлен на
//!     продажу за $…»; проданный лот: «У Вас купили Зелье Невидимости за $… на /ah»; слоты полны: «Не удалось
//!     выставить Зелье Невидимости, освободите хранилище или арендуйте больше слотов на /ah rent!»;
//!   * снятие — `/ah` → слот 46 (хранилище) → клик по слоту 0 (после каждого клика окно открывается заново,
//!     «Вы убрали предмет с продажи!»).
//!
//! Включается файлом `invis.json` в рабочей папке бота (корень `4narek-azalea`): `{"enabled": true,
//! "anarchies": [509], ...}`. Остальные поля — см. `InvisConfig`. Всё, что делает бот, пишется в
//! `invis-log/<ник>.jsonl`.
//!
//! Ритм: бот продаёт по одной штуке, пока есть свободные слоты АХ, паузами как у человека (`AH_CMD`).
//! В АХ заходит только:
//!   * купить новый стак — когда зелий в инвентаре не осталось совсем;
//!   * раз в `relist_sec` (60 с + разброс), пока на витрине есть лоты: `/ah search` → слот 46 (хранилище) →
//!     слот 52 «перевыставить» (подтверждение в чате «Предметы успешно перевыставлены!»);
//!   * снять зависшие лоты («снятие»): если `stale_unlist_sec` нет продаж, в хранилище кликаем по слоту 0, пока
//!     лоты не кончатся, и выставляем заново.
//!
//! Цена пока статическая (`price_start`): ценообразование решим, когда накопится книга. Книга пишется в
//! `invis-log/<ник>.jsonl` (события `book`: все лоты зелья на странице поиска при каждом заходе на
//! перевыставление, `list`, `sold`, `buy`, `unlist`, `relist`).
//!
//! Число занятых слотов АХ считается по чату.

use std::{
    collections::HashMap,
    io::Write,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use azalea::prelude::*;
use ipc::{emit, BotBoot, WorkerMsg};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::{afk, anarchy, cmd, delay, item_parse, safe_inv, state::BotState};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InvisConfig {
    pub enabled: bool,
    pub anarchies: Vec<u16>,
    /// Что вводить в `/ah search`.
    pub query: String,
    /// Покупаем только стаки не меньше этого (у нас 64).
    pub min_stack: i32,
    /// Максимальная цена за штуку при покупке стака.
    pub max_unit_cost: f64,
    /// Цена одиночки (статическая) и границы (пол не ниже 2 × себестоимость).
    pub price_start: f64,
    pub price_floor: f64,
    pub price_ceil: f64,
    pub price_step: f64,
    /// Слотов АХ у бота.
    pub ah_slots: usize,
    /// Сколько денег оставлять после покупки стака.
    pub reserve: f64,
    /// Если стака нет, повторяем поиск через столько секунд (покупаем только при пустом инвентаре).
    pub buy_retry_sec: u64,
    /// Период перевыставления (кнопка в хранилище), секунд; к нему добавляется разброс 0,5–3 с.
    pub relist_sec: u64,
    /// Нет продаж столько секунд при занятых слотах — снимаем лоты и выставляем заново.
    pub stale_unlist_sec: u64,
    /// Способ выставления: `ah_sell` (по одной штуке из хотбара командой `/ah sell`) или `sellgui` (окно
    /// `/ah sellgui`, до 5 штук за заход). Для сравнения можно задать способ отдельным ботам в `bot_modes`.
    pub sell_mode: String,
    pub bot_modes: HashMap<String, String>,
    /// Как часто чистить мусор из инвентаря и хотбара, секунд (при старте — сразу).
    pub clean_every_sec: u64,
    /// Своя броня: что искать в АХ, чтобы собрать позорные куски (как у обычных ботов — «netherite»).
    pub armor_query: String,
    /// Сколько раз подряд обновляем («роллим») поиск в поисках нужного позорного куска за один заход.
    pub armor_rolls: u32,
    /// Если за заход не нашли, следующий заход через столько секунд (бот пока торгует зельями).
    pub armor_retry_sec: u64,
    /// Меньше этого баланса стак не ищем (стак ≈0.6–1 кк + `reserve`): без денег поиск — пустые /ah search.
    pub min_balance_buy: f64,
    /// Меньше этого баланса броню не ищем (позорный кусок ≈0.25–0.7 кк).
    pub min_balance_wear: f64,
}

impl Default for InvisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            anarchies: vec![],
            query: "инвиз".into(),
            min_stack: 64,
            max_unit_cost: 15_625.0,
            price_start: 45_000.0,
            price_floor: 30_000.0,
            price_ceil: 79_000.0,
            price_step: 1_000.0,
            ah_slots: 5,
            reserve: 2_000_000.0,
            buy_retry_sec: 20,
            relist_sec: 60,
            stale_unlist_sec: 0,
            sell_mode: "ah_sell".into(),
            bot_modes: HashMap::new(),
            clean_every_sec: 600,
            armor_query: "netherite armour".into(),
            armor_rolls: 40,
            armor_retry_sec: 300,
            min_balance_buy: 3_000_000.0,
            min_balance_wear: 1_000_000.0,
        }
    }
}

/// Конфиг режима для этой анархии, если он включён.
pub fn config_for(boot: &BotBoot) -> Option<InvisConfig> {
    let raw = std::fs::read_to_string("invis.json").ok()?;
    let cfg: InvisConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            warn!("invis.json: не разобрался: {e}");
            return None;
        }
    };
    if cfg.enabled && cfg.anarchies.contains(&boot.anarchy) {
        Some(cfg)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
struct Lot {
    slot: usize,
    count: i32,
    price: f64,
}

impl Lot {
    fn unit(&self) -> f64 {
        self.price / self.count.max(1) as f64
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Запись в `invis-log/<ник>.jsonl`.
fn log_event(user: &str, kind: &str, data: serde_json::Value) {
    let _ = std::fs::create_dir_all("invis-log");
    let line = json!({ "t": now_ms(), "kind": kind, "data": data }).to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("invis-log/{user}.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Чьё это зелье по компоненту `potion_contents` (как `itemMatchesPotion` в oluh-bot): `Some(true)` — невидимость
/// (базовое зелье `invisibility`/`long_invisibility` или кастомный эффект invisibility), `Some(false)` —
/// другое зелье, `None` — по компонентам не определить (тогда не отбрасываем: поиск идёт по слову «инвиз»).
fn potion_is_invis(d: &azalea_inventory::ItemStackData) -> Option<bool> {
    use azalea_inventory::components::PotionContents;
    let pc = d.get_component::<PotionContents>()?;
    let base = pc.potion.as_ref().map(|p| format!("{p:?}").to_lowercase());
    let eff: Vec<String> = pc.custom_effects.iter().map(|e| format!("{:?}", e.id).to_lowercase()).collect();
    let by_name = item_parse::lore_strings(d).iter().any(|l| l.to_lowercase().contains("невидим"));
    if base.as_deref().map_or(false, |b| b.contains("invisib")) || eff.iter().any(|e| e.contains("invisib")) || by_name {
        return Some(true);
    }
    if base.is_none() && eff.is_empty() {
        return None;
    }
    Some(false)
}

/// Зелье невидимости (или неопознанное зелье): число штук в стаке.
fn is_potion(item: &azalea_inventory::ItemStack) -> Option<i32> {
    let d = item_parse::present(item)?;
    if item_parse::item_minecraft_name(d) != "potion" {
        return None;
    }
    if potion_is_invis(d) == Some(false) {
        return None;
    }
    Some(d.count)
}

/// Сколько зелий у бота в инвентаре (меню игрока) плюс на курсоре. `None` — сейчас не посчитать: открыто окно
/// (меню игрока в azalea обновляется только при закрытии окна) или инвентарь не прочитался. Раньше в таких
/// случаях возвращался 0, и бот докупал стак, хотя зелья ещё были (у fitosikosir 192 куплено, 93 продано, 101 на руках).
fn potions_in_inventory(bot: &Client) -> Option<i32> {
    if safe_inv::try_id(bot).map_or(true, |id| id != 0) {
        return None;
    }
    let slots: i32 = safe_inv::try_player_slots(bot)?.iter().filter_map(is_potion).sum();
    let carried = safe_inv::try_carried(bot).as_ref().and_then(is_potion).unwrap_or(0);
    Some(slots + carried)
}

/// Лоты зелья в открытом окне поиска.
fn scan_lots(bot: &Client) -> Vec<Lot> {
    let Some(contents) = safe_inv::try_contents(bot) else {
        return vec![];
    };
    contents
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let d = item_parse::present(it)?;
            is_potion(it)?;
            let price = item_parse::ah_price(d)?;
            Some(Lot { slot: i, count: d.count, price })
        })
        .collect()
}

#[derive(Default)]
struct Tally {
    bought: Option<(i32, f64)>,
    listed: usize,
    sold: usize,
    relisted: bool,
    sell_full: bool,
}

fn parse_money(s: &str) -> Option<f64> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

fn after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.find(key).map(|i| &text[i + key.len()..])
}

struct Run {
    cfg: InvisConfig,
    user: String,
    state: Arc<Mutex<BotState>>,
    /// Занятых слотов АХ (по чату).
    active: usize,
    /// Текущая цена одиночки.
    price: f64,
    /// Когда последний раз что-то продалось / выставили партию / перевыставили.
    last_sale: Instant,
    last_list: Instant,
    last_relist: Instant,
    /// Себестоимость штуки (последней купленной партии).
    unit_cost: f64,
    last_buy_try: Instant,
    sales_total: usize,
    listed_total: usize,
    /// Способ выставления для этого бота.
    mode: String,
}

impl Run {
    /// `try-sell` в Go (панель и цены считают «попытки выставить»): по одному событию на лот.
    fn try_sell(&self, n: usize, price: f64) {
        for _ in 0..n {
            emit(&WorkerMsg {
                name: "try-sell".into(),
                username: Some(self.user.clone()),
                id: Some(PANEL_ITEM.into()),
                price: Some(price),
                ..crate::empty_msg()
            });
        }
    }

    fn floor(&self) -> f64 {
        self.cfg.price_floor.max(self.unit_cost * 2.0)
    }

    fn clamp_price(&mut self) {
        let lo = self.floor();
        self.price = self.price.clamp(lo, self.cfg.price_ceil.max(lo));
    }

    /// Разбираем новые строки чата.
    fn poll(&mut self) -> Tally {
        let lines = std::mem::take(&mut self.state.lock().invis_chat);
        let mut t = Tally::default();
        for (_, text) in lines {
            let low = text.to_lowercase();
            if low.contains("успешно купили") && low.contains("невидим") {
                // «Вы успешно купили Зелье Невидимости x64 за $999,999!»
                let n = after(&text, " x")
                    .and_then(|r| r.split_whitespace().next().and_then(|w| w.parse::<i32>().ok()))
                    .unwrap_or(1);
                let p = after(&text, " за $").and_then(parse_money).unwrap_or(0.0);
                t.bought = Some((n, p));
                // в Go (и в панель): покупку стака разбиваем на штуки — по `buy` на каждую, чтобы «куплено» и «продано»
                // считались в одних единицах (продаём по одной). Сумма сохраняется: остаток от деления
                // раскидываем по первым штукам.
                let cnt = n.max(1) as i64;
                let total = p.round() as i64;
                let (base, rem) = (total / cnt, total % cnt);
                for i in 0..cnt {
                    emit(&WorkerMsg {
                        name: "buy".into(),
                        username: Some(self.user.clone()),
                        id: Some(PANEL_ITEM.into()),
                        price: Some((base + if i < rem { 1 } else { 0 }) as f64),
                        ..crate::empty_msg()
                    });
                }
            } else if low.contains("на продажу успешно выставлено") {
                let n = after(&low, "выставлено")
                    .and_then(|r| r.split_whitespace().next().and_then(|w| w.parse::<usize>().ok()))
                    .unwrap_or(1);
                t.listed += n;
                // «…выставлено 5 предметов по $44,999!»
                let p = after(&text, " по $").and_then(parse_money).unwrap_or(self.price);
                self.try_sell(n, p);
            } else if low.contains("выставлен на продажу") && low.contains("невидим") {
                t.listed += 1;
                // «Предмет Зелье Невидимости выставлен на продажу за $44,999!»
                let p = after(&text, " за $").and_then(parse_money).unwrap_or(self.price);
                self.try_sell(1, p);
            } else if low.contains("у вас купили") && low.contains("невидим") {
                t.sold += 1;
                let p = after(&text, " за $").and_then(parse_money).unwrap_or(0.0);
                log_event(&self.user, "sold", json!({ "price": p }));
                emit(&WorkerMsg {
                    name: "sell".into(),
                    username: Some(self.user.clone()),
                    id: Some(PANEL_ITEM.into()),
                    price: Some(p),
                    ..crate::empty_msg()
                });
            } else if low.contains("не удалось выставить") {
                t.sell_full = true;
            } else if low.contains("перевыставлен") {
                t.relisted = true;
                // перевыставление = новая попытка продать каждый лот на витрине (как storage reset у обычных ботов)
                self.try_sell(self.active, okprice(self.price, self.cfg.price_step));
            }
        }
        if t.listed > 0 {
            self.active = (self.active + t.listed).min(self.cfg.ah_slots);
            self.listed_total += t.listed;
        }
        if t.sell_full {
            self.active = self.cfg.ah_slots;
        }
        if t.sold > 0 {
            self.active = self.active.saturating_sub(t.sold);
            self.sales_total += t.sold;
            self.last_sale = Instant::now();
        }
        if t.relisted {
            self.last_relist = Instant::now();
        }
        t
    }
}

fn okprice(p: f64, step: f64) -> f64 {
    // цена вида …999: так лоты не склеиваются с чужими по одинаковой цене
    ((p / step).round() * step - 1.0).max(1.0)
}

async fn wait_window(bot: &Client, old_id: i32, secs: u64) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(id) = safe_inv::try_id(bot) {
            if id != 0 && id != old_id {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return true;
            }
        }
        delay::rnd("POLL").await;
    }
    false
}

async fn open_search(bot: &Client, state: &Arc<Mutex<BotState>>, query: &str) -> bool {
    if safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }
    afk::un_afk_quiet(bot).await;
    state.lock().afk = false;
    let old = safe_inv::try_id(bot).unwrap_or(0);
    delay::rnd("AH_CMD").await;
    cmd::say(bot, format!("/ah search {query}"));
    wait_window(bot, old, 6).await
}

/// Один раз за запуск пишем в лог, что за компоненты у зелья в окне поиска (проверить определение).
fn probe_potion(bot: &Client, user: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(c) = safe_inv::try_contents(bot) else { return };
    for it in c.iter() {
        if let Some(d) = item_parse::present(it) {
            if item_parse::item_minecraft_name(d) == "potion" {
                use azalea_inventory::components::PotionContents;
                let pc = d.get_component::<PotionContents>().map(|p| format!("{p:?}"));
                log_event(user, "potion_probe", json!({ "contents": pc, "ident": format!("{:?}", potion_is_invis(d)) }));
                break;
            }
        }
    }
}

/// Покупка лучшего стака (только 64 шт.). Возвращает, сколько штук купили.
async fn buy_stack(run: &mut Run, bot: &Client) -> i32 {
    run.last_buy_try = Instant::now();
    if !open_search(bot, &run.state, &run.cfg.query.clone()).await {
        warn!("инвиз → окно поиска не открылось");
        return 0;
    }
    let lots = scan_lots(bot);
    probe_potion(bot, &run.user);
    let balance = run.state.lock().balance.unwrap_or(f64::MAX);
    let best = lots
        .iter()
        .filter(|l| l.count >= run.cfg.min_stack)
        .filter(|l| l.unit() <= run.cfg.max_unit_cost)
        .filter(|l| l.price + run.cfg.reserve <= balance)
        .min_by(|a, b| a.unit().partial_cmp(&b.unit()).unwrap_or(std::cmp::Ordering::Equal))
        .cloned();
    let Some(lot) = best else {
        info!("инвиз → нет подходящего стака (лотов зелья {})", lots.len());
        log_event(&run.user, "scan", json!({ "lots": lots.len() }));
        let _ = safe_inv::try_close(bot);
        return 0;
    };
    info!(
        "инвиз → беру стак x{} за {} ({:.0} за шт), слот {}",
        lot.count,
        crate::money::money(lot.price),
        lot.unit(),
        lot.slot
    );
    let win = safe_inv::try_id(bot).unwrap_or(0);
    delay::rnd("GLASS").await;
    if !safe_inv::try_shift_click(bot, lot.slot) || !safe_inv::window_matches(safe_inv::try_id(bot).unwrap_or(0), win) {
        warn!("инвиз → окно исчезло до клика");
        return 0;
    }
    let start = Instant::now();
    let mut got = 0;
    while start.elapsed() < Duration::from_secs(6) {
        let t = run.poll();
        if let Some((n, p)) = t.bought {
            got = n;
            run.unit_cost = p / n.max(1) as f64;
            run.clamp_price();
            log_event(&run.user, "buy", json!({ "count": n, "price": p }));
            break;
        }
        delay::rnd("POLL").await;
    }
    let _ = safe_inv::try_close(bot);
    delay::rnd("WINDOW_DELAY").await;
    if got == 0 {
        warn!("инвиз → покупка не подтвердилась");
        log_event(&run.user, "buy_fail", json!({ "slot": lot.slot, "price": lot.price }));
    }
    got
}

/// Слоты хотбара в меню игрока.
const HOTBAR: std::ops::RangeInclusive<usize> = 36..=44;

/// Хотбар-слоты, где лежит ровно одна штука зелья.
fn hotbar_singles(bot: &Client) -> Vec<usize> {
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return vec![];
    };
    HOTBAR
        .filter(|i| slots.get(*i).and_then(is_potion) == Some(1))
        .collect()
}

/// Что лежит в хотбаре и инвентаре — для диагностики, когда зелья не удаётся разложить.
fn inventory_report(bot: &Client) -> String {
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return "меню игрока недоступно".into();
    };
    let item = |i: usize| -> String {
        match slots.get(i).and_then(item_parse::present) {
            None => "пусто".into(),
            Some(d) => format!("{} x{}", item_parse::item_minecraft_name(d), d.count),
        }
    };
    let hot: Vec<String> = HOTBAR.map(|i| format!("{i}={}", item(i))).collect();
    let pots: Vec<String> = (0..slots.len())
        .filter_map(|i| slots.get(i).and_then(is_potion).map(|c| format!("{i}:x{c}")))
        .collect();
    format!("хотбар [{}]; зелья в слотах [{}]", hot.join(", "), pots.join(", "))
}

/// Разложить зелья из инвентаря по пустым слотам хотбара по одной штуке (`/ah sell` берёт только одну из руки).
/// Стак ×N: берём курсором, правым кликом кладём по штуке в пустые слоты хотбара, остаток возвращаем на место.
/// Одиночки из основного инвентаря просто переносим. Возвращает, сколько штук положили в хотбар (0 — не вышло,
/// тогда в логе будет отчёт, что лежит в хотбаре и где зелья).
async fn fill_hotbar(bot: &Client) -> usize {
    let mut placed = 0;
    let mut why = String::new();
    let mut handle = None;
    for _ in 0..12 {
        let Some(slots) = safe_inv::try_player_slots(bot) else {
            why = "меню игрока недоступно".into();
            break;
        };
        let empty: Vec<usize> = HOTBAR.filter(|i| slots.get(*i).and_then(item_parse::present).is_none()).collect();
        if empty.is_empty() {
            why = "в хотбаре нет пустых слотов".into();
            break;
        }
        // источник: зелье вне хотбара (приоритет — самое большое), либо стак ≥2 в хотбаре
        let src = (9..=44usize)
            .filter(|i| {
                let c = slots.get(*i).and_then(is_potion).unwrap_or(0);
                if HOTBAR.contains(i) { c >= 2 } else { c >= 1 }
            })
            .max_by_key(|i| slots.get(*i).and_then(is_potion).unwrap_or(0));
        let Some(src) = src else {
            why = "нет зелья, которое можно переложить (вне хотбара или стак ≥2)".into();
            break;
        };
        let count = slots.get(src).and_then(is_potion).unwrap_or(0).max(1) as usize;

        // Инвентарь открываем один раз на всю раскладку и закрываем один раз в конце (как клиент: E → клики → Esc),
        // а не на каждый стак: лишние ContainerClose 0 между перекладываниями у живого игрока не бывает.
        if handle.is_none() {
            if safe_inv::try_close(bot) {
                delay::rnd("POLL").await;
            }
            let Some(h) = bot.open_inventory() else {
                why = "не открылся инвентарь игрока (open_inventory = None)".into();
                break;
            };
            delay::rnd("HOTBAR_DELAY").await;
            handle = Some(h);
        } else {
            delay::rnd("HOTBAR_DELAY").await; // пауза между перекладываниями
        }
        let Some(h) = handle.as_ref() else { break };
        h.left_click(src);
        delay::rnd("POLL").await;
        if count == 1 {
            h.left_click(empty[0]);
            placed += 1;
        } else {
            let n = empty.len().min(count);
            for t in empty.iter().take(n) {
                h.right_click(*t);
                placed += 1;
                tokio::time::sleep(Duration::from_millis(120 + (rand::random::<u64>() % 130))).await;
            }
            if count > n {
                delay::rnd("POLL").await;
                h.left_click(src); // остаток обратно на место
            }
        }
        delay::rnd("SELL_HOTBAR").await;
    }
    if let Some(h) = handle {
        // от последнего клика до закрытия у живого игрока ≈0,5 с (записи бот-вью)
        tokio::time::sleep(Duration::from_millis(350 + rand::random::<u64>() % 500)).await;
        drop(h); // ContainerClose 0 — один раз на всю раскладку
    }
    if placed == 0 {
        warn!("инвиз → не смог разложить зелья по хотбару: {why}; {}", inventory_report(bot));
    }
    placed
}

/// Продать одну штуку: выбрать слот хотбара и `/ah sell <цена>`. Возвращает `true`, если «выставлен на продажу».
async fn sell_one(run: &mut Run, bot: &Client, hot_slot: usize) -> bool {
    let price = okprice(run.price, run.cfg.price_step);
    if safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }
    afk::un_afk_quiet(bot).await;
    run.state.lock().afk = false;
    bot.set_selected_hotbar_slot((hot_slot - 36) as u8);
    delay::rnd("AH_CMD").await;
    let _ = run.poll();
    cmd::say(bot, format!("/ah sell {}", price as i64));
    let start = Instant::now();
    let mut listed = 0;
    let mut full = false;
    while start.elapsed() < Duration::from_secs(4) {
        let t = run.poll();
        if t.listed > 0 {
            listed = t.listed;
            break;
        }
        if t.sell_full {
            full = true;
            break;
        }
        delay::rnd("POLL").await;
    }
    if listed > 0 {
        run.last_list = Instant::now();
        info!("инвиз → выставлено по {} (слотов занято {})", crate::money::money(price), run.active);
        log_event(&run.user, "list", json!({ "n": listed, "price": price, "mode": run.mode }));
    } else if full {
        warn!("инвиз → слоты АХ полны («не удалось выставить»)");
        log_event(&run.user, "list_full", json!({ "price": price, "active": run.active }));
    } else {
        warn!("инвиз → ответа на /ah sell нет за 4 с — считаю слоты полными, схожу в хранилище");
        run.active = run.cfg.ah_slots;
        log_event(&run.user, "list_fail", json!({ "price": price }));
    }
    listed > 0
}

/// Мусор в хотбаре: всё, кроме одиночного зелья невидимости. Сюда же попадают стаки зелий (≥2) — для `/ah sell`
/// в хотбаре нужны одиночки, а стак занимает слот.
fn hotbar_junk(slots: &[azalea_inventory::ItemStack]) -> Vec<usize> {
    HOTBAR
        .filter(|i| {
            let Some(it) = slots.get(*i) else { return false };
            if item_parse::present(it).is_none() {
                return false;
            }
            is_potion(it) != Some(1)
        })
        .collect()
}

/// Чистка хотбара: всё, что не одиночное зелье, переносим shift-кликом в основной инвентарь (слоты 9–35).
/// Выбрасывать не пытаемся: проверено по логу fitosikosir (64 «чистки» подряд, одни и те же слоты 9, 36–39, 42–44) —
/// выброшенное бот подбирает обратно или сервер выброс не принимает. Основному инвентарю мусор не мешает.
/// Возвращает, сколько слотов хотбара освободили (по факту, после перечитывания инвентаря).
async fn clean_junk(run: &Run, bot: &Client) -> usize {
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return 0;
    };
    let junk = hotbar_junk(&slots);
    if junk.is_empty() {
        return 0;
    }
    let main_free = (9..=35usize).filter(|i| slots.get(*i).and_then(item_parse::present).is_none()).count();
    if main_free == 0 {
        warn!("инвиз → хотбар занят мусором, а в основном инвентаре нет места; {}", inventory_report(bot));
        return 0;
    }
    if safe_inv::try_close(bot) {
        delay::rnd("POLL").await;
    }
    let Some(h) = bot.open_inventory() else {
        return 0;
    };
    delay::rnd("HOTBAR_DELAY").await;
    for slot in junk.iter().take(main_free) {
        h.shift_click(*slot);
        delay::rnd("SELL_HOTBAR").await;
    }
    drop(h);
    delay::rnd("500MS").await;
    let left = safe_inv::try_player_slots(bot).map(|s| hotbar_junk(&s)).unwrap_or_default();
    let freed = junk.len().saturating_sub(left.len());
    info!("инвиз → хотбар: убрал в инвентарь {freed} из {} слот(ов)", junk.len());
    log_event(&run.user, "junk", json!({ "slots": junk, "left": left }));
    freed
}

/// Выставить до `n` штук через `/ah sellgui <цена>` (окно 9×2, слоты 0–4 входные, слот 14 «Подтвердить»).
/// Возвращает, сколько штук выставлено.
async fn sell_batch(run: &mut Run, bot: &Client, n: usize) -> usize {
    let price = okprice(run.price, run.cfg.price_step);
    if safe_inv::try_close(bot) {
        delay::rnd("500MS").await;
    }
    afk::un_afk_quiet(bot).await;
    run.state.lock().afk = false;
    let old = safe_inv::try_id(bot).unwrap_or(0);
    delay::rnd("AH_CMD").await;
    cmd::say(bot, format!("/ah sellgui {}", price as i64));
    if !wait_window(bot, old, 6).await {
        warn!("инвиз → окно sellgui не открылось");
        return 0;
    }
    let win = safe_inv::try_id(bot).unwrap_or(0);
    let Some(menu) = safe_inv::try_menu(bot) else {
        return 0;
    };
    let cont_len = safe_inv::try_contents(bot).map(|c| c.len()).unwrap_or(18);
    let slots = menu.slots();
    let Some((src, stack)) = slots
        .iter()
        .enumerate()
        .skip(cont_len)
        .find_map(|(i, it)| is_potion(it).map(|c| (i, c)))
    else {
        warn!("инвиз → в окне продажи нет зелья в инвентаре");
        let _ = safe_inv::try_close(bot);
        return 0;
    };
    let n = n.min(stack as usize).min(5);
    if n == 0 {
        let _ = safe_inv::try_close(bot);
        return 0;
    }
    delay::rnd("WINDOW_DELAY").await;
    if !safe_inv::try_left_click_expect(bot, src, win) {
        return 0;
    }
    delay::rnd("POLL").await;
    let Some(h) = safe_inv::try_handle(bot) else {
        return 0;
    };
    for i in 0..n {
        h.right_click(i);
        tokio::time::sleep(Duration::from_millis(120 + (rand::random::<u64>() % 120))).await;
    }
    if (stack as usize) > n {
        delay::rnd("POLL").await;
        h.left_click(src);
    }
    delay::rnd("GLASS").await;
    let _ = run.poll();
    if !safe_inv::try_left_click_expect(bot, 14, win) {
        warn!("инвиз → окно sellgui исчезло до подтверждения");
        return 0;
    }
    let start = Instant::now();
    let mut listed = 0;
    while start.elapsed() < Duration::from_secs(6) {
        let t = run.poll();
        if t.listed > 0 {
            listed = t.listed;
            break;
        }
        if t.sell_full {
            break;
        }
        delay::rnd("POLL").await;
    }
    let _ = safe_inv::try_close(bot);
    if listed > 0 {
        run.last_list = Instant::now();
        info!("инвиз → sellgui: выставлено {listed} по {}", crate::money::money(price));
        log_event(&run.user, "list", json!({ "n": listed, "price": price, "mode": run.mode }));
    } else {
        warn!("инвиз → sellgui: «выставлено» в чате не пришло — считаю слоты полными, схожу в хранилище");
        run.active = run.cfg.ah_slots;
        log_event(&run.user, "list_fail", json!({ "n": n, "price": price, "mode": run.mode }));
    }
    listed
}

/// Сначала одеться: если на каком-то слоте брони нет незерита и позорного куска в инвентаре, листаем
/// `/ah search netherite armour` («роллим» кнопкой «Обновить»), берём первый позорный лот нужного типа, потом надеваем
/// (`wear::equip_all`). Вся логика отбора и надевания — из wear.rs, она общая с обычными ботами.
/// Возвращает `true`, если слотов без брони не осталось.
async fn wear_phase(run: &mut Run, bot: &Client) -> bool {
    let _ = safe_inv::try_close(bot);
    delay::rnd("500MS").await;
    crate::wear::refresh_missing(bot, &run.state);
    // уже есть позорный кусок в инвентаре — просто надеваем
    crate::wear::equip_all(bot, &run.state).await;
    crate::wear::refresh_missing(bot, &run.state);
    if run.state.lock().wear_missing.is_empty() && !armor_slots_free(bot) {
        return true;
    }
    let need = run.state.lock().wear_missing.clone();
    if need.is_empty() {
        return !armor_slots_free(bot);
    }
    info!("инвиз → нет брони на слотах {need:?}, ищу позорную в АХ «{}»", run.cfg.armor_query);
    if !open_search(bot, &run.state, &run.cfg.armor_query.clone()).await {
        warn!("инвиз → АХ для брони не открылся");
        return false;
    }
    for roll in 0..run.cfg.armor_rolls {
        if run.state.lock().shutdown || run.state.lock().banned {
            return false;
        }
        let need = run.state.lock().wear_missing.clone();
        let balance = run.state.lock().balance;
        let win = safe_inv::try_id(bot).unwrap_or(0);
        let contents = safe_inv::try_contents(bot).unwrap_or_default();
        let mut pick: Option<(usize, usize, f64)> = None;
        for (slot, it) in contents.iter().enumerate().take(45) {
            let Some(d) = item_parse::present(it) else { continue };
            let Some(price) = item_parse::ah_price(d) else { continue };
            if balance.map_or(false, |b| price > b) {
                continue;
            }
            if let Some(armor_slot) = crate::wear::wear_slot_for_lot(
                &item_parse::item_minecraft_name(d),
                &item_parse::all_enchant_labels(bot, d),
                &need,
            ) {
                pick = Some((slot, armor_slot, price));
                break;
            }
        }
        if let Some((slot, armor_slot, price)) = pick {
            info!("{}", crate::wear::begin_purchase(&run.state, armor_slot, price));
            log_event(&run.user, "wear_buy", json!({ "armor_slot": armor_slot, "price": price, "roll": roll }));
            delay::rnd("GLASS").await;
            if !safe_inv::try_shift_click(bot, slot) {
                break;
            }
            // возможное окно подтверждения покупки
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                let (id, title) = {
                    let st = run.state.lock();
                    (st.last_window_id, st.last_window_title.clone())
                };
                if id != win && crate::titles::is_confirm(&title) {
                    delay::rnd("GLASS").await;
                    let _ = safe_inv::try_left_click(bot, 0);
                    break;
                }
                delay::rnd("POLL").await;
            }
            tokio::time::sleep(Duration::from_millis(2500)).await;
            let _ = safe_inv::try_close(bot);
            delay::rnd("WINDOW_DELAY").await;
            crate::wear::equip_all(bot, &run.state).await;
            crate::wear::refresh_missing(bot, &run.state);
            if run.state.lock().wear_missing.is_empty() {
                return true;
            }
            // остались ещё слоты: открываем поиск заново
            if !open_search(bot, &run.state, &run.cfg.armor_query.clone()).await {
                return false;
            }
            continue;
        }
        // нужного лота нет — «роллим»: кнопка «Обновить» (слот 50)
        delay::rnd("AH_CMD").await;
        if !safe_inv::try_left_click_expect(bot, 50, win) {
            if !open_search(bot, &run.state, &run.cfg.armor_query.clone()).await {
                return false;
            }
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    let _ = safe_inv::try_close(bot);
    info!("инвиз → позорной брони за {} обновлений не нашлось, вернусь через {} с", run.cfg.armor_rolls, run.cfg.armor_retry_sec);
    false
}

/// Есть ли ещё слот брони без незерита нужного типа (по меню игрока).
fn armor_slots_free(bot: &Client) -> bool {
    let Some(slots) = safe_inv::try_player_slots(bot) else {
        return false;
    };
    !crate::wear::free_armor_slots(|i| {
        slots
            .get(i)
            .and_then(item_parse::present)
            .map(item_parse::item_minecraft_name)
    })
    .is_empty()
}

/// Заход в хранилище (`/ah search` → слот 46): перевыставить (слот 52) и, если лоты зависли, снять их.
async fn relist_visit(run: &mut Run, bot: &Client) {
    if !open_search(bot, &run.state, &run.cfg.query.clone()).await {
        warn!("инвиз → для перевыставления не открылся АХ");
        run.last_relist = Instant::now() - Duration::from_secs(run.cfg.relist_sec / 2);
        return;
    }
    let ah_win = safe_inv::try_id(bot).unwrap_or(0);
    // книга: все лоты зелья на странице поиска (бесплатно — окно уже открыто ради хранилища)
    let book: Vec<serde_json::Value> = scan_lots(bot)
        .iter()
        .map(|l| json!({ "n": l.count, "p": l.price }))
        .collect();
    log_event(&run.user, "book", json!({ "lots": book, "my_price": run.price, "active": run.active }));
    delay::rnd("STORAGE_ENTER").await;
    if !safe_inv::try_left_click_expect(bot, SLOT_STORAGE, ah_win) {
        let _ = safe_inv::try_close(bot);
        return;
    }
    if !wait_window(bot, ah_win, 4).await {
        warn!("инвиз → хранилище не открылось");
        let _ = safe_inv::try_close(bot);
        return;
    }
    let win = safe_inv::try_id(bot).unwrap_or(0);

    // реальное число лотов на витрине (по слотам хранилища) — чинит счётчик после рестарта и пропущенных сообщений
    if let Some(c) = safe_inv::try_contents(bot) {
        let real = c.iter().take(STORAGE_LOT_SLOTS).filter(|it| is_potion(it).is_some()).count();
        if real != run.active {
            info!("инвиз → в хранилище лотов {real} (по счёту было {}), синхронизирую", run.active);
        }
        run.active = real;
    }

    // снятие нужно только для смены цены (снять и выставить по новой); при статичной цене хватает кнопки
    // «перевыставить», поэтому по умолчанию выключено (stale_unlist_sec = 0)
    let stale = run.cfg.stale_unlist_sec > 0
        && run.active > 0
        && run.last_sale.elapsed() > Duration::from_secs(run.cfg.stale_unlist_sec);
    if stale {
        // снятие: клик по каждому занятому слоту лотов — предмет возвращается в инвентарь
        let mut taken = 0;
        let mut win = win;
        for _ in 0..STORAGE_LOT_SLOTS {
            let has = safe_inv::try_contents(bot)
                .map(|c| c.iter().take(STORAGE_LOT_SLOTS).any(|it| is_potion(it).is_some()))
                .unwrap_or(false);
            if !has {
                break;
            }
            delay::rnd("STORAGE_UNLIST").await;
            // после каждого снятия сервер открывает окно хранилища заново (новый id), лоты сдвигаются в слот 0
            if !safe_inv::try_left_click_expect(bot, 0, win) {
                break;
            }
            taken += 1;
            if !wait_window(bot, win, 3).await {
                break;
            }
            win = safe_inv::try_id(bot).unwrap_or(0);
        }
        run.active = 0;
        run.last_sale = Instant::now();
        run.last_relist = Instant::now();
        info!("инвиз → сняты зависшие лоты ({taken}), выставлю заново по {}", crate::money::money(run.price));
        log_event(&run.user, "unlist", json!({ "taken": taken, "price": run.price }));
    } else if run.active == 0 {
        // на витрине пусто — перевыставлять нечего
        run.last_relist = Instant::now();
    } else {
        delay::rnd("GLASS").await;
        if safe_inv::try_left_click_expect(bot, SLOT_RELIST, win) {
            let start = Instant::now();
            let mut ok = false;
            while start.elapsed() < Duration::from_secs(5) {
                if run.poll().relisted {
                    ok = true;
                    break;
                }
                delay::rnd("POLL").await;
            }
            log_event(&run.user, "relist", json!({ "ok": ok, "active": run.active }));
            if !ok {
                warn!("инвиз → перевыставление: подтверждения в чате нет");
                run.last_relist = Instant::now() - Duration::from_secs(run.cfg.relist_sec / 2);
            }
        }
    }
    let _ = safe_inv::try_close(bot);
    delay::rnd("WINDOW_DELAY").await;
}

/// Id товара в Go: по нему покупки и продажи инвизов попадают в панель (см. `items_config.json`).
const PANEL_ITEM: &str = "инвиз-1.21";

/// Слоты окон (сняты с живого клиента).
const SLOT_STORAGE: usize = 46;
const SLOT_RELIST: usize = 52;
const STORAGE_LOT_SLOTS: usize = 10;

pub async fn run_loop(bot: Client, boot: Arc<BotBoot>, state: Arc<Mutex<BotState>>, cfg: InvisConfig) {
    if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
        warn!("инвиз: нет анки — стоп");
        return;
    }
    state.lock().invis_on = true;
    info!("инвиз · режим включён · {} · слотов {} · запрос «{}»", boot.username, cfg.ah_slots, cfg.query);

    let mut run = Run {
        user: boot.username.clone(),
        state: state.clone(),
        active: 0,
        price: cfg.price_start,
        last_sale: Instant::now(),
        last_list: Instant::now(),
        last_relist: Instant::now(),
        unit_cost: 15_000.0,
        last_buy_try: Instant::now() - Duration::from_secs(3600),
        sales_total: 0,
        listed_total: 0,
        mode: String::new(),
        cfg,
    };
    run.active = run.cfg.ah_slots; // после запуска неизвестно, что на витрине: первый заход в хранилище посчитает
    run.last_relist = Instant::now() - Duration::from_secs(3600);
    run.mode = run.cfg.bot_modes.get(&boot.username).cloned().unwrap_or_else(|| run.cfg.sell_mode.clone());
    info!("инвиз → способ выставления: {}", run.mode);
    log_event(&run.user, "start", json!({ "mode": run.mode, "price": run.price }));
    run.clamp_price();
    let mut last_balance = Instant::now() - Duration::from_secs(3600);
    let mut sell_backoff = Instant::now();
    let mut fill_backoff = Instant::now();
    let mut armor_retry_at = Instant::now();
    let mut last_clean = Instant::now() - Duration::from_secs(86400);
    let mut last_stat = Instant::now();
    let mut poor_logged = Instant::now() - Duration::from_secs(3600);
    let (mut stat_sold, mut stat_listed) = (0usize, 0usize);
    // случайный разброс считаем один раз на период, как у сброса витрины в storage.rs
    let mut relist_jitter = Duration::from_millis(500 + rand::random::<u64>() % 2500);

    loop {
        {
            let st = state.lock();
            if st.shutdown || st.banned {
                return;
            }
        }
        if !state.lock().on_anarchy {
            warn!("инвиз: слетели с анки — снова /an");
            if !anarchy::wait_until_on_anarchy(&bot, &boot, &state).await {
                return;
            }
        }
        let _ = run.poll();

        // баланс раз в 5 минут: main.rs сам отправит излишек казначею
        // сервер перевёл в AFK (бот долго стоял): команды не работают — шевелим мышью
        if state.lock().afk {
            info!("инвиз → снимаю AFK");
            afk::un_afk(&bot).await;
            state.lock().afk = false;
            delay::rnd("BASE_DELAY").await;
        }

        // без денег баланс проверяем раз в минуту, чтобы сразу заметить перевод от казначея / выравнивание
        let balance = state.lock().balance;
        let poor = balance.map_or(false, |b| b < run.cfg.min_balance_buy);
        let balance_every = if poor || balance.is_none() { 60 } else { 300 };
        if last_balance.elapsed() > Duration::from_secs(balance_every) {
            afk::un_afk_quiet(&bot).await;
            let asked = crate::request_balance(&bot, &state);
            last_balance = Instant::now();
            if asked {
                delay::rnd("BASE_DELAY").await;
            }
            let need = std::mem::take(&mut state.lock().need_sell);
            if need {
                crate::sell::treasurer_pass(&bot, &boot, &state).await;
            }
        }

        // чистка мусора: сразу при старте и потом раз в clean_every_sec (до одевания и торговли)
        if last_clean.elapsed() > Duration::from_secs(run.cfg.clean_every_sec) {
            last_clean = Instant::now();
            clean_junk(&run, &bot).await;
            continue;
        }

        // статистика раз в 5 минут для сравнения способов выставления
        if last_stat.elapsed() > Duration::from_secs(300) {
            log_event(&run.user, "stat", json!({
                "mode": run.mode,
                "sold_5m": run.sales_total - stat_sold,
                "listed_5m": run.listed_total - stat_listed,
                "price": run.price,
            }));
            stat_sold = run.sales_total;
            stat_listed = run.listed_total;
            last_stat = Instant::now();
        }

        // 0) сначала одеться (своя позорная броня), пока на слотах чего-то не хватает
        // (без денег не ищем: 40 обновлений поиска впустую — лишние действия, похожие на бота)
        let can_wear = balance.map_or(false, |b| b >= run.cfg.min_balance_wear);
        if can_wear && Instant::now() >= armor_retry_at && armor_slots_free(&bot) {
            if !wear_phase(&mut run, &bot).await {
                armor_retry_at = Instant::now() + Duration::from_secs(run.cfg.armor_retry_sec);
            }
            continue;
        }

        // окно могло остаться открытым (сервер открыл сам, клик не закрыл): тогда меню игрока не актуально
        if safe_inv::try_close(&bot) {
            delay::rnd("500MS").await;
            continue;
        }
        let Some(have) = potions_in_inventory(&bot) else {
            delay::rnd("POLL").await;
            continue;
        };
        let free = run.cfg.ah_slots.saturating_sub(run.active);

        // 1) перевыставить по таймеру (и снять зависшее), пока на витрине что-то есть
        if run.active > 0
            && run.last_relist.elapsed() >= Duration::from_secs(run.cfg.relist_sec) + relist_jitter
        {
            relist_visit(&mut run, &bot).await;
            relist_jitter = Duration::from_millis(500 + rand::random::<u64>() % 2500);
            continue;
        }

        if run.mode == "sellgui" {
            // 2) sellgui: выставляем, как только освободились слоты (штуки берёт прямо из инвентаря)
            let want = run.cfg.ah_slots.min(have.max(0) as usize).max(1);
            if have > 0 && free > 0 && Instant::now() >= sell_backoff
                && (free >= want || run.last_list.elapsed() > Duration::from_secs(4))
            {
                let n = free.min(have as usize);
                if sell_batch(&mut run, &bot, n).await == 0 {
                    sell_backoff = Instant::now() + Duration::from_secs(30);
                }
                continue;
            }
        } else {
            // 2) ah_sell: по одной штуке из хотбара, пока есть свободные слоты АХ
            if free > 0 && Instant::now() >= sell_backoff {
                if let Some(&hot) = hotbar_singles(&bot).first() {
                    if !sell_one(&mut run, &bot, hot).await {
                        sell_backoff = Instant::now() + Duration::from_secs(8);
                    }
                    continue;
                }
            }
            // 3) зелья есть, но в хотбаре нет одиночек — раскладываем по хотбару (при неудаче пауза 30 с, без спама)
            if have > 0 && Instant::now() >= fill_backoff && hotbar_singles(&bot).is_empty() {
                if fill_hotbar(&bot).await == 0 {
                    // обычно хотбар забит мусором или стаками — убираем их в инвентарь и сразу пробуем снова
                    if clean_junk(&run, &bot).await == 0 {
                        fill_backoff = Instant::now() + Duration::from_secs(30);
                    }
                }
                continue;
            }
        }

        // 4) покупаем стак только когда в инвентаре (и на курсоре) не осталось ни одного зелья.
        // Перепроверяем через секунду: одно чтение могло прийтись на рассинхрон после закрытия окна.
        // Без денег (или пока баланс неизвестен) не ищем: раньше бот с балансом 10k открывал поиск каждые 20 с.
        if have == 0 && run.last_buy_try.elapsed() > Duration::from_secs(run.cfg.buy_retry_sec) {
            if balance.is_none() || poor {
                if poor_logged.elapsed() > Duration::from_secs(600) {
                    info!(
                        "инвиз → баланс {} < {} — стак не ищу, жду денег от казначея",
                        balance.map(crate::money::money).unwrap_or_else(|| "?".into()),
                        crate::money::money(run.cfg.min_balance_buy)
                    );
                    log_event(&run.user, "poor", json!({ "balance": balance }));
                    poor_logged = Instant::now();
                }
                run.last_buy_try = Instant::now();
                continue;
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
            if potions_in_inventory(&bot) != Some(0) {
                continue;
            }
            if buy_stack(&mut run, &bot).await == 0 {
                // подходящего стака нет — следующий поиск не через 20 с, а через минуту
                run.last_buy_try = Instant::now() + Duration::from_secs(40);
            }
            continue;
        }

        delay::rnd("POLL").await;
    }
}
