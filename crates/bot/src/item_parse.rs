//! Парсинг лота АХ: цена из лора, UUID, чары (упрощённый порт items/slotInfo.mjs).

use azalea::prelude::Client;
use azalea_chat::FormattedText;
use azalea_inventory::{
    ItemStack, ItemStackData,
    components::{
        CustomData, CustomName, Damage, Enchantments, ItemName, Lore, MaxDamage,
        StoredEnchantments,
    },
};
use azalea_registry::{DataRegistry, builtin::ItemKind};
use regex::Regex;
use serde_json::{json, Value};
use simdnbt::owned::{Nbt, NbtCompound, NbtList, NbtTag};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct SlotInfo {
    pub id: String,
    pub sell_price: f64,
    pub buy_price: f64,
    pub nacenka: f64,
    /// Перцентиль пола книги из Go (0 = нет).
    pub book_floor: f64,
    /// Перцентиль потолка книги из Go (0 = нет).
    pub book_mid: f64,
    /// Скорость продаж товара в штуках в час (Go; 0 = нет данных).
    pub rate_per_hour: f64,
    /// Сколько товара уже на руках по флоту (лоты на АХ + инвентарь).
    pub held: u32,
    /// Типичная оценка лота у флота сейчас (медиана свежих покупок по категории, Go; 0 = нет данных).
    /// Лот «исключительный», если его оценка заметно выше этого эталона: сравнение с текущим рынком, а не абсолют.
    pub score_ref: f64,
}

/// Как old `getSlotInfo`.
#[derive(Debug, Clone)]
pub enum SlotKind {
    Trade(SlotInfo),
    Trash,
    Foreign {
        id: String,
        foreign_type: String,
    },
}

impl SlotKind {
    pub fn as_trade(&self) -> Option<&SlotInfo> {
        match self {
            SlotKind::Trade(i) => Some(i),
            _ => None,
        }
    }
    pub fn is_trash(&self) -> bool {
        matches!(self, SlotKind::Trash)
    }
    pub fn is_foreign(&self) -> bool {
        matches!(self, SlotKind::Foreign { .. })
    }
}

pub fn lore_strings(item: &ItemStackData) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(name) = item.get_component::<CustomName>() {
        push_formatted(&name.name, &mut out);
    }
    if let Some(lore) = item.get_component::<Lore>() {
        for line in &lore.lines {
            push_formatted(line, &mut out);
        }
    }
    out
}

fn push_formatted(t: &FormattedText, out: &mut Vec<String>) {
    let s = t.to_string();
    if !s.is_empty() {
        out.push(s);
    }
}

pub fn ah_price(item: &ItemStackData) -> Option<f64> {
    // Как JS getPriceFromAhItem: только lore-строки (не custom name), по одной линии,
    // лучший score; при равном score — большее число. Не трогаем «Продавец:».
    static COMMA_THOUSANDS: OnceLock<Regex> = OnceLock::new();
    let comma_re = COMMA_THOUSANDS.get_or_init(|| Regex::new(r"\d,\d{3}").unwrap());

    let lore = item.get_component::<Lore>()?;
    for line in &lore.lines {
        let mut strings = Vec::new();
        collect_formatted_texts(line, &mut strings);
        let joined = line.to_string();
        if !joined.is_empty() && !strings.iter().any(|s| s == &joined) {
            strings.push(joined);
        }
        if !strings
            .iter()
            .any(|s| lore_looks_like_price_label(s) || s.contains('$'))
        {
            continue;
        }
        let mut candidates: Vec<(f64, i32)> = Vec::new();
        for s in &strings {
            let Some(n) = parse_price_number(s) else {
                continue;
            };
            let score = (if s.contains('$') { 4 } else { 0 })
                + (if comma_re.is_match(s) { 2 } else { 0 })
                + (if n > 150_000.0 { 1 } else { 0 });
            candidates.push((n, score));
        }
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
        });
        return Some(candidates[0].0);
    }
    None
}

/// FunTime мешает латиницу в «Цена» (Ценa / Цeна) — как old `loreLooksLikePriceLabel`.
fn lore_looks_like_price_label(s: &str) -> bool {
    let n = normalize_funtime_cyr(s);
    n.contains("цен") || n.contains("стоимость")
}

fn lore_looks_like_seller_label(s: &str) -> bool {
    normalize_funtime_cyr(s).contains("продавец")
}

fn looks_like_mc_nick(t: &str) -> bool {
    let t = t.trim();
    if t.len() < 3 || t.len() > 16 {
        return false;
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let low = t.to_lowercase();
    !matches!(
        low.as_str(),
        "string" | "compound" | "list" | "byte" | "int"
    )
}

fn first_mc_nick_in_text(s: &str) -> Option<String> {
    for part in s.split_whitespace() {
        let cleaned = part.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        if looks_like_mc_nick(cleaned) {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn nick_after_seller_label(s: &str) -> Option<String> {
    if !lore_looks_like_seller_label(s) {
        return None;
    }
    let after = s.rsplit_once(':').map(|(_, rest)| rest)?;
    first_mc_nick_in_text(after)
}

fn collect_formatted_texts(t: &FormattedText, out: &mut Vec<String>) {
    match t {
        FormattedText::Text(c) => {
            if !c.text.is_empty() {
                out.push(c.text.clone());
            }
            for sib in &c.base.siblings {
                collect_formatted_texts(sib, out);
            }
        }
        FormattedText::Translatable(c) => {
            for sib in &c.base.siblings {
                collect_formatted_texts(sib, out);
            }
        }
    }
}

fn seller_from_formatted(t: &FormattedText) -> Option<String> {
    match t {
        FormattedText::Text(c) => {
            if lore_looks_like_seller_label(&c.text) {
                if let Some(n) = nick_after_seller_label(&c.text) {
                    return Some(n);
                }
                for sib in &c.base.siblings {
                    let mut parts = Vec::new();
                    collect_formatted_texts(sib, &mut parts);
                    for p in parts {
                        if let Some(n) = first_mc_nick_in_text(&p) {
                            return Some(n);
                        }
                    }
                }
            }
            for sib in &c.base.siblings {
                if let Some(n) = seller_from_formatted(sib) {
                    return Some(n);
                }
            }
            None
        }
        FormattedText::Translatable(c) => {
            for sib in &c.base.siblings {
                if let Some(n) = seller_from_formatted(sib) {
                    return Some(n);
                }
            }
            None
        }
    }
}

/// Ник продавца с лота АХ («Продавец:» + extra), как JS `getAhSellerNick`.
pub fn ah_seller_nick(item: &ItemStackData) -> String {
    if let Some(lore) = item.get_component::<Lore>() {
        for line in &lore.lines {
            if let Some(n) = seller_from_formatted(line) {
                return n;
            }
        }
    }
    let strings = lore_strings(item);
    for (i, s) in strings.iter().enumerate() {
        if !lore_looks_like_seller_label(s) {
            continue;
        }
        if let Some(n) = nick_after_seller_label(s) {
            return n;
        }
        for cand in strings.iter().skip(i + 1).take(3) {
            if let Some(n) = first_mc_nick_in_text(cand) {
                return n;
            }
        }
    }
    String::new()
}

fn normalize_funtime_cyr(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'a' => 'а',
            'e' => 'е',
            'o' => 'о',
            'p' => 'р',
            'c' => 'с',
            'x' => 'х',
            'y' => 'у',
            other => other,
        })
        .collect()
}

fn parse_price_number(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == ' ' || *c == '.')
        .collect();
    let cleaned = cleaned.replace(' ', "").replace(',', "");
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse().ok()
}

pub fn auction_uuid(item: &ItemStackData) -> Option<String> {
    let data = item.get_component::<CustomData>()?;
    let mut found = None;
    walk_nbt_for_uuid(&data.nbt, &mut found);
    found
}

fn walk_nbt_for_uuid(nbt: &Nbt, found: &mut Option<String>) {
    // Nbt → BaseNbt → NbtCompound (Deref)
    if matches!(nbt, Nbt::None) {
        return;
    }
    walk_compound(nbt, found);
}

fn walk_compound(c: &NbtCompound, found: &mut Option<String>) {
    if found.is_some() {
        return;
    }
    for (k, v) in c.iter() {
        let key = k.to_string_lossy();
        if key.contains("if-uuid") || key.contains("auctions:if-uuid") {
            if let Some(s) = uuid_from_tag(v) {
                *found = Some(s);
                return;
            }
        }
        match v {
            NbtTag::Compound(inner) => walk_compound(inner, found),
            NbtTag::List(list) => walk_list(list, found),
            _ => {}
        }
        if found.is_some() {
            return;
        }
    }
}

fn walk_list(list: &NbtList, found: &mut Option<String>) {
    if found.is_some() {
        return;
    }
    if let Some(compounds) = list.compounds() {
        for inner in compounds {
            walk_compound(inner, found);
            if found.is_some() {
                return;
            }
        }
    }
}

fn uuid_from_tag(tag: &NbtTag) -> Option<String> {
    match tag {
        // FunTime: auctions:if-uuid = byteArray (mineflayer/JS → signed bytes " -86,79,...")
        NbtTag::ByteArray(arr) if !arr.is_empty() => Some(
            arr.iter()
                .map(|b| (*b as i8).to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        NbtTag::IntArray(arr) if !arr.is_empty() => Some(
            arr.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        NbtTag::List(list) => match list {
            NbtList::Byte(xs) if !xs.is_empty() => {
                Some(xs.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","))
            }
            NbtList::Int(xs) if !xs.is_empty() => {
                Some(xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
            }
            NbtList::Long(xs) if !xs.is_empty() => {
                Some(xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
            }
            NbtList::String(xs) if !xs.is_empty() => Some(
                xs.iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            _ => None,
        },
        NbtTag::String(s) => {
            let t = s.to_string_lossy();
            if t.is_empty() {
                None
            } else {
                Some(t.into_owned())
            }
        }
        _ => None,
    }
}

pub fn durability_percent(item: &ItemStackData) -> f64 {
    let max = item
        .get_component::<MaxDamage>()
        .map(|m| m.amount as f64)
        .unwrap_or(0.0);
    if max <= 0.0 {
        return 1.0;
    }
    let dmg = item
        .get_component::<Damage>()
        .map(|d| d.amount as f64)
        .unwrap_or(0.0);
    ((max - dmg) / max).clamp(0.0, 1.0)
}

/// `mending`: мягкая кривая прочности (пол 0.5), без порога trash — торгуется всегда.
/// Без mending: линейно, trash при < 0.5 (как раньше).
pub fn price_with_durability_mending(base: f64, dur: f64, mending: bool) -> f64 {
    let dur_eff = if mending { 0.5 + 0.5 * dur } else { dur };
    if !mending && dur_eff < 0.5 {
        return 0.0;
    }
    let marker = (base as i64) % 100;
    let price = ((base * dur_eff).floor() as i64 / 100) * 100 + marker;
    price as f64
}

/// Чары: ваниль из компонента + кастом.
/// На АХ FunTime кастом в NBT нет (только auctions:if-uuid) — чары в лоре
/// отдельными строками «Яд I». Custom name не читаем (скам переименованием).
pub fn all_enchant_labels(bot: &Client, item: &ItemStackData) -> Vec<(String, i32)> {
    let _ = bot; // API стабильный; ваниль — по protocol id как old, не Azalea registry
    let mut by_name: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    let mut push = |name: String, lvl: i32| {
        if name.is_empty() || lvl <= 0 {
            return;
        }
        let name = normalize_enchant_name(&name);
        let e = by_name.entry(name).or_insert(0);
        if lvl > *e {
            *e = lvl;
        }
    };

    if let Some(ench) = item.get_component::<Enchantments>() {
        for (id, lvl) in ench.levels.iter() {
            if let Some(name) = vanilla_enchant_name(id.protocol_id()) {
                push(name.to_string(), *lvl as i32);
            }
        }
    }
    if let Some(stored) = item.get_component::<StoredEnchantments>() {
        for (id, lvl) in stored.enchantments.iter() {
            if let Some(name) = vanilla_enchant_name(id.protocol_id()) {
                push(name.to_string(), *lvl as i32);
            }
        }
    }

    if let Some(data) = item.get_component::<CustomData>() {
        for (n, l) in extract_custom_enchants_from_nbt(&data.nbt) {
            push(n, l);
        }
    }

    for (n, l) in extract_custom_enchants_from_lore_lines(&lines_for_custom_enchant_parse(item))
    {
        push(n, l);
    }

    by_name.into_iter().collect()
}

/// Чары кастом на АХ: отдельные куски текста (лор + extra у имени).
/// Целиком переименованный custom name («Яд II») не берём — скам.
fn lines_for_custom_enchant_parse(item: &ItemStackData) -> Vec<String> {
    let mut out = Vec::new();
    let rename = plain_custom_name_rename(item);

    if let Some(name) = item.get_component::<CustomName>() {
        push_enchant_text_nodes(&name.name, rename.as_deref(), &mut out);
    }
    if let Some(iname) = item.get_component::<ItemName>() {
        push_enchant_text_nodes(&iname.name, None, &mut out);
    }
    if let Some(lore) = item.get_component::<Lore>() {
        for line in &lore.lines {
            push_enchant_text_nodes(line, None, &mut out);
        }
    }
    // то же, что в дампе (name+lore to_string) — если extra склеились в одну строку
    for s in lore_strings(item) {
        push_split_lines(&s, rename.as_deref(), &mut out);
    }
    out
}

fn plain_custom_name_rename(item: &ItemStackData) -> Option<String> {
    let name = item.get_component::<CustomName>()?;
    match &name.name {
        FormattedText::Text(c) if c.base.siblings.is_empty() => {
            let t = c.text.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

fn push_enchant_text_nodes(t: &FormattedText, skip_exact: Option<&str>, out: &mut Vec<String>) {
    let mut nodes = Vec::new();
    collect_formatted_texts(t, &mut nodes);
    let joined = t.to_string();
    if !joined.is_empty() && !nodes.iter().any(|s| s == &joined) {
        nodes.push(joined);
    }
    for n in nodes {
        push_split_lines(&n, skip_exact, out);
    }
}

fn push_split_lines(s: &str, skip_exact: Option<&str>, out: &mut Vec<String>) {
    for part in s.split(['\n', '\r']) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if skip_exact.is_some_and(|r| r == t) {
            continue;
        }
        out.push(t.to_string());
    }
}

/// Живой снимок custom_data → JSON (книга АХ). Лор отдельно, в матч не идёт.
pub fn custom_data_json(item: &ItemStackData) -> Value {
    match item.get_component::<CustomData>() {
        Some(data) => nbt_to_json(&data.nbt),
        None => Value::Null,
    }
}

pub fn nbt_to_json(nbt: &Nbt) -> Value {
    if matches!(nbt, Nbt::None) {
        return Value::Null;
    }
    compound_to_json(nbt)
}

fn compound_to_json(c: &NbtCompound) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in c.iter() {
        map.insert(k.to_string_lossy().into_owned(), tag_to_json(v));
    }
    Value::Object(map)
}

fn tag_to_json(tag: &NbtTag) -> Value {
    match tag {
        NbtTag::Byte(n) => json!({ "t": "byte", "v": n }),
        NbtTag::Short(n) => json!({ "t": "short", "v": n }),
        NbtTag::Int(n) => json!({ "t": "int", "v": n }),
        NbtTag::Long(n) => json!({ "t": "long", "v": n }),
        NbtTag::Float(n) => json!({ "t": "float", "v": n }),
        NbtTag::Double(n) => json!({ "t": "double", "v": n }),
        NbtTag::String(s) => json!({ "t": "string", "v": s.to_string_lossy() }),
        NbtTag::Compound(inner) => json!({ "t": "compound", "v": compound_to_json(inner) }),
        NbtTag::List(list) => json!({ "t": "list", "v": list_to_json(list) }),
        NbtTag::ByteArray(xs) => json!({
            "t": "byteArray",
            "v": xs.iter().map(|b| *b as i8).collect::<Vec<i8>>(),
        }),
        NbtTag::IntArray(xs) => json!({ "t": "intArray", "v": xs }),
        NbtTag::LongArray(xs) => json!({ "t": "longArray", "v": xs }),
    }
}

fn list_to_json(list: &NbtList) -> Value {
    match list {
        NbtList::Empty => json!([]),
        NbtList::Byte(xs) => json!(xs),
        NbtList::Short(xs) => json!(xs),
        NbtList::Int(xs) => json!(xs),
        NbtList::Long(xs) => json!(xs),
        NbtList::Float(xs) => json!(xs),
        NbtList::Double(xs) => json!(xs),
        NbtList::String(xs) => json!(xs.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>()),
        NbtList::Compound(xs) => json!(xs.iter().map(compound_to_json).collect::<Vec<_>>()),
        NbtList::List(xs) => json!(xs.iter().map(list_to_json).collect::<Vec<_>>()),
        NbtList::ByteArray(xs) => json!(xs),
        NbtList::IntArray(xs) => json!(xs),
        NbtList::LongArray(xs) => json!(xs),
    }
}

const AH_NBT_DUMP_MAX: u32 = 80;

pub fn maybe_dump_ah_nbt(
    bot: &Client,
    item: &ItemStackData,
    match_kind: &str,
    match_id: &str,
    price: Option<f64>,
    uuid: &str,
    seller: &str,
    nick: &str,
) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    static COUNT: AtomicU32 = AtomicU32::new(0);
    static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

    let name = item_minecraft_name(item);
    if name != "netherite_sword" && name != "netherite_pickaxe" {
        return;
    }

    let n = COUNT.load(Ordering::Relaxed);
    if n >= AH_NBT_DUMP_MAX {
        return;
    }

    let key = if uuid.is_empty() {
        format!("{nick}:{name}:{price:?}")
    } else {
        uuid.to_string()
    };
    {
        let seen = SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
        let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
        if !g.insert(key) {
            return;
        }
    }

    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n >= AH_NBT_DUMP_MAX {
        return;
    }

    let path = PATH.get_or_init(|| {
        let dir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("dumps");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("ah-nbt.jsonl");
        tracing::info!("NBT-дамп АХ → {}", p.display());
        p
    });

    let parsed: Vec<Value> = all_enchant_labels(bot, item)
        .into_iter()
        .map(|(n, l)| json!({ "name": n, "lvl": l }))
        .collect();

    let rec = json!({
        "ts": chrono_like_now(),
        "nick": nick,
        "kind": name,
        "match": match_kind,
        "id": match_id,
        "price": price,
        "uuid": uuid,
        "seller": seller,
        "parsed_enchants": parsed,
        "lore": lore_strings(item),
        "enchant_src": lines_for_custom_enchant_parse(item),
        "custom_data": custom_data_json(item),
    });

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{rec}");
    }

    if n + 1 == AH_NBT_DUMP_MAX {
        tracing::info!("NBT-дамп АХ: набрали {AH_NBT_DUMP_MAX} лотов, стоп");
    }
}

pub(crate) fn chrono_like_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            let secs = d.as_secs();
            format!("{secs}")
        })
        .unwrap_or_default()
}

/// Как `FUNTIME_ENCHANT_BY_ID` / minecraft-data 1.21.11 (funtime-enchants.mjs).
fn vanilla_enchant_name(protocol_id: u32) -> Option<&'static str> {
    Some(match protocol_id {
        0 => "aqua_affinity",
        1 => "bane_of_arthropods",
        2 => "binding_curse",
        3 => "blast_protection",
        4 => "breach",
        5 => "channeling",
        6 => "density",
        7 => "depth_strider",
        8 => "efficiency",
        9 => "feather_falling",
        10 => "fire_aspect",
        11 => "fire_protection",
        12 => "flame",
        13 => "fortune",
        14 => "frost_walker",
        15 => "impaling",
        16 => "infinity",
        17 => "knockback",
        18 => "looting",
        19 => "loyalty",
        20 => "luck_of_the_sea",
        21 => "lunge",
        22 => "lure",
        23 => "mending",
        24 => "multishot",
        25 => "piercing",
        26 => "power",
        27 => "projectile_protection",
        28 => "protection",
        29 => "punch",
        30 => "quick_charge",
        31 => "respiration",
        32 => "riptide",
        33 => "sharpness",
        34 => "silk_touch",
        35 => "smite",
        36 => "soul_speed",
        37 => "sweeping_edge",
        38 => "swift_sneak",
        39 => "thorns",
        40 => "unbreaking",
        41 => "vanishing_curse",
        42 => "wind_burst",
        _ => return None,
    })
}

/// FunTime кастом из PDC (`PublicBukkitValues` / `minecraft:custom-enchantments`).
/// Только белый список имён — ваниль и мусор лора («Продавец», «Острота») отбрасываем.
pub(crate) fn extract_custom_enchants_from_nbt(nbt: &Nbt) -> Vec<(String, i32)> {
    let mut by_name: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    if matches!(nbt, Nbt::None) {
        return Vec::new();
    }
    walk_custom_compound(nbt, &mut |raw, lvl| {
        if lvl <= 0 {
            return;
        }
        let Some(name) = canonicalize_custom_enchant(&raw) else {
            return;
        };
        let e = by_name.entry(name).or_insert(0);
        if lvl > *e {
            *e = lvl;
        }
    });
    by_name.into_iter().collect()
}

/// Строка лора целиком = имя из белого списка + римская/число.
/// Не подстрока: «Меч с Яд II» не матчится. АХ-хром (цена/продавец) пропускаем.
pub(crate) fn extract_custom_enchants_from_lore_lines(
    lines: &[String],
) -> Vec<(String, i32)> {
    let mut by_name: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for line in lines {
        if lore_line_is_ah_chrome(line) {
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with('[') {
            continue;
        }
        let (raw, lvl) = if let Some((n, l)) = split_name_level(t) {
            (n, l)
        } else {
            (t.to_string(), 1)
        };
        let Some(name) = canonicalize_custom_enchant(&raw) else {
            continue;
        };
        let e = by_name.entry(name).or_insert(0);
        if lvl > *e {
            *e = lvl;
        }
    }
    by_name.into_iter().collect()
}

fn lore_line_is_ah_chrome(s: &str) -> bool {
    let n = normalize_funtime_cyr(s);
    n.contains('ц') && (n.contains("цен") || n.contains("стоимость"))
        || n.contains("продавец")
        || n.contains("нажмите")
        || n.contains("истек")
        || n.contains("купить")
        || s.contains('$')
}

fn walk_custom_compound(c: &NbtCompound, push: &mut dyn FnMut(String, i32)) {
    if let Some((n, l)) = extract_enchant_pair(c) {
        push(n, l);
    }
    for (k, v) in c.iter() {
        let key = k.to_string_lossy().to_lowercase();
        match v {
            NbtTag::Compound(inner) => walk_custom_compound(inner, push),
            NbtTag::List(list) => walk_custom_list(&key, list, push),
            NbtTag::String(s) if key_looks_like_enchant(&key) => {
                parse_enchant_blob(&s.to_string_lossy(), push);
            }
            _ => {}
        }
    }
}

fn walk_custom_list(key: &str, list: &NbtList, push: &mut dyn FnMut(String, i32)) {
    if let Some(xs) = list.compounds() {
        for inner in xs {
            walk_custom_compound(inner, push);
        }
        return;
    }
    match list {
        NbtList::String(xs) if key_looks_like_enchant(key) => {
            for s in xs {
                parse_enchant_blob(&s.to_string_lossy(), push);
            }
        }
        NbtList::List(lists) => {
            for inner in lists {
                walk_custom_list(key, inner, push);
            }
        }
        _ => {}
    }
}

fn key_looks_like_enchant(key: &str) -> bool {
    key.contains("enchant") || key.contains("чар")
}

fn extract_enchant_pair(c: &NbtCompound) -> Option<(String, i32)> {
    let mut name = None;
    let mut lvl = None;
    for (k, v) in c.iter() {
        let ks = k.to_string_lossy().to_lowercase();
        let short = ks.rsplit(':').next().unwrap_or(&ks);
        if short == "type" || short == "id" {
            name = tag_name_string(v);
        } else if short == "level" || short == "lvl" {
            lvl = tag_level(v);
        }
    }
    Some((name?, lvl?))
}

fn tag_name_string(tag: &NbtTag) -> Option<String> {
    match tag {
        NbtTag::String(s) => {
            let t = s.to_string_lossy().into_owned();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        _ => None,
    }
}

fn tag_level(tag: &NbtTag) -> Option<i32> {
    match tag {
        NbtTag::Int(n) => Some(*n),
        NbtTag::Short(n) => Some(*n as i32),
        NbtTag::Byte(n) => Some(*n as i32),
        NbtTag::Long(n) => i32::try_from(*n).ok(),
        NbtTag::String(s) => parse_level_str(&s.to_string_lossy()),
        _ => None,
    }
}

fn parse_enchant_blob(s: &str, push: &mut dyn FnMut(String, i32)) {
    let t = s.trim();
    if t.is_empty() {
        return;
    }
    if let Some((n, l)) = split_name_level(t) {
        push(n, l);
        return;
    }
    for part in t.split(|c: char| c == ',' || c == ';' || c == '|' || c == '\n') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((n, l)) = split_name_level(part) {
            push(n, l);
        }
    }
}

fn split_name_level(s: &str) -> Option<(String, i32)> {
    let s = s.trim();
    if let Some((a, b)) = s.rsplit_once(':') {
        if let Some(lvl) = parse_level_str(b.trim()) {
            let name = a.trim();
            if !name.is_empty() {
                return Some((name.to_string(), lvl));
            }
        }
    }
    if let Some((a, b)) = s.rsplit_once(char::is_whitespace) {
        if let Some(lvl) = parse_level_str(b.trim()) {
            let name = a.trim();
            if !name.is_empty() {
                return Some((name.to_string(), lvl));
            }
        }
    }
    None
}

fn parse_level_str(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i32>() {
        return (n > 0).then_some(n);
    }
    roman_to_lvl(s)
}

fn roman_to_lvl(s: &str) -> Option<i32> {
    let u = s.trim().to_uppercase();
    let n = match u.as_str() {
        "I" => 1,
        "II" => 2,
        "III" => 3,
        "IV" => 4,
        "V" => 5,
        "VI" => 6,
        "VII" => 7,
        "VIII" => 8,
        "IX" => 9,
        "X" => 10,
        _ => return None,
    };
    Some(n)
}

fn known_custom_short(n: &str) -> bool {
    matches!(
        n,
        "poison"
            | "vampirism"
            | "detection"
            | "heavy"
            | "unstable"
            | "buldozing"
            | "mega_buldozing"
            | "magnet"
            | "web"
            | "smelting"
            | "skilled"
            | "jumping"
            | "sniper"
            | "oxidation"
            | "lava_walker"
            | "pinger"
            | "scout"
            | "stupor"
            | "pulling"
            | "returning"
            | "demolishing"
    )
}

fn canonicalize_custom_enchant(name: &str) -> Option<String> {
    if let Some(mapped) = map_custom_ru(name) {
        return Some(mapped);
    }
    let mut n = name.trim().to_lowercase();
    if let Some((_, short)) = n.rsplit_once(':') {
        n = short.to_string();
    }
    known_custom_short(&n).then_some(n)
}

fn map_custom_ru(name: &str) -> Option<String> {
    let low = name
        .trim()
        .to_lowercase()
        .replace('ё', "е")
        .replace('_', " ");
    let map = [
        ("яд", "poison"),
        ("вампиризм", "vampirism"),
        ("детекция", "detection"),
        ("тяжелый", "heavy"),
        ("нестабильный", "unstable"),
        ("бульдозер", "buldozing"),
        ("мега-бульдозер", "mega_buldozing"),
        ("мега бульдозер", "mega_buldozing"),
        ("магнит", "magnet"),
        ("паутина", "web"),
        ("авто-плавка", "smelting"),
        ("авто плавка", "smelting"),
        ("опытный", "skilled"),
        ("попрыгун", "jumping"),
        ("снайпер", "sniper"),
        ("окисление", "oxidation"),
        ("лаваход", "lava_walker"),
        ("пингер", "pinger"),
        ("скаут", "scout"),
        ("ступор", "stupor"),
        ("притяжение", "pulling"),
        ("возвращение", "returning"),
        ("подрывник", "demolishing"),
    ];
    for (ru, en) in map {
        if low == ru || low == en || low.replace(' ', "_") == en {
            return Some(en.to_string());
        }
    }
    None
}

/// Склейка custom_name+lore — крушитель.
pub fn flatten_item_chat_text(item: &ItemStackData) -> String {
    lore_strings(item)
        .into_iter()
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .collect::<Vec<_>>()
        .join("")
}

pub fn is_funtime_crusher_item(item: &ItemStackData) -> bool {
    flatten_item_chat_text(item).to_lowercase().contains("крушител")
}

fn crusher_kits() -> &'static Value {
    static KITS: OnceLock<Value> = OnceLock::new();
    KITS.get_or_init(|| {
        serde_json::from_str(include_str!("../funtime-crusher-kits.json")).unwrap_or(Value::Null)
    })
}

#[allow(dead_code)] // киты оставлены; книга АХ больше не режет крушитель
pub fn item_matches_crusher_kit(bot: &Client, item: &ItemStackData) -> bool {
    let Some(arr) = crusher_kits().as_array() else {
        return false;
    };
    let item_name = kind_id(item.kind);
    let have = all_enchant_labels(bot, item);
    for kit in arr {
        let match_name = kit
            .get("matchName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if match_name != item_name {
            continue;
        }
        let Some(enchants) = kit.get("enchants").and_then(|v| v.as_array()) else {
            continue;
        };
        if enchants.is_empty() {
            continue;
        }
        let ok = enchants.iter().all(|req| {
            let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let lvl = req.get("level").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
            have.iter()
                .any(|(n, l)| enchant_names_match(n, id) && *l >= lvl)
        });
        if ok {
            return true;
        }
    }
    false
}

/// Раньше кит крушителя выкидывали из книги АХ. Теперь торгуем SKU `*-крушитель-1.21` → шлём.
pub fn skip_ah_book_crusher(_bot: &Client, _item: &ItemStackData) -> bool {
    false
}

/// Как 4narek-old `getSlotInfo`.
pub fn get_slot_kind(
    bot: &Client,
    item: &ItemStackData,
    catalog: &[Value],
    go_type: &str,
) -> SlotKind {
    let item_name = kind_id(item.kind);
    let have = all_enchant_labels(bot, item);

    let mut candidates: Vec<&Value> = catalog
        .iter()
        .filter(|c| {
            c.get("id")
                .and_then(|v| v.as_str())
                .map(|id| id.ends_with("1.21"))
                .unwrap_or(false)
        })
        .collect();
    if candidates.is_empty() {
        candidates = catalog.iter().collect();
    }
    candidates.sort_by(|a, b| {
        let na = a.get("num").and_then(|v| v.as_i64()).unwrap_or(0);
        let nb = b.get("num").and_then(|v| v.as_i64()).unwrap_or(0);
        nb.cmp(&na)
    });

    let mut best: Option<&Value> = None;
    for cfg in candidates {
        if !item_matches_config_entry(item, &item_name, &have, cfg) {
            continue;
        }
        best = Some(cfg);
        break;
    }
    let Some(cfg) = best else {
        return SlotKind::Trash;
    };

    let cfg_ty = cfg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let cfg_name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if !go_type.is_empty() && !catalog_type_matches_go(cfg_ty, go_type, cfg_name) {
        return SlotKind::Foreign {
            id: cfg
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            foreign_type: cfg_ty.to_string(),
        };
    }

    let Some(id) = cfg.get("id").and_then(|v| v.as_str()) else {
        return SlotKind::Trash;
    };
    let Some(price_sell) = cfg.get("priceSell").and_then(|v| v.as_f64()) else {
        return SlotKind::Trash;
    };
    let nacenka = cfg.get("nacenka").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let book_floor = cfg
        .get("bookFloor")
        .and_then(|v| v.as_f64())
        .or_else(|| cfg.get("book_floor").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    let book_mid = cfg
        .get("bookMid")
        .and_then(|v| v.as_f64())
        .or_else(|| cfg.get("book_mid").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
        let mending = have
        .iter()
        .any(|(n, _)| enchant_names_match(n, "minecraft:mending"));
    let sell = price_with_durability_mending(price_sell, durability_percent(item), mending);
    if sell <= 0.0 {
        return SlotKind::Trash;
    }
    SlotKind::Trade(SlotInfo {
        id: id.to_string(),
        sell_price: sell,
        buy_price: sell - nacenka,
        nacenka,
        book_floor,
        book_mid,
        rate_per_hour: cfg
            .get("ratePerHour")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        held: cfg.get("held").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        score_ref: cfg.get("scoreRef").and_then(|v| v.as_f64()).unwrap_or(0.0),
    })
}

/// Только свой торгуемый лот (не trash / не foreign).
pub fn match_slot_info(
    bot: &Client,
    item: &ItemStackData,
    catalog: &[Value],
    go_type: &str,
) -> Option<SlotInfo> {
    get_slot_kind(bot, item, catalog, go_type).as_trade().cloned()
}

/// old `itemMatchesConfigEntry`
fn item_matches_config_entry(
    item: &ItemStackData,
    item_name: &str,
    have: &[(String, i32)],
    cfg: &Value,
) -> bool {
    let cfg_name = match cfg.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.is_empty() => n.trim().trim_start_matches("minecraft:"),
        _ => return false, // без name в каталоге — не матчим (old: item.name !== undefined)
    };
    // обязательно: item.name === configItem.name
    if item_name != cfg_name {
        return false;
    }

    if !lore_matches_config(item, cfg) {
        return false;
    }

    let required = cfg.get("effects").and_then(|v| v.as_array());
    let required = required.map(|a| a.as_slice()).unwrap_or(&[]);
    for req in required {
        let name = req
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if name.is_empty() {
            continue;
        }
        let lvl = req.get("lvl").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
        let canon = normalize_enchant_name(&name);
        let ok = have.iter().any(|(n, l)| {
            enchant_names_match(n, &canon) && *l >= lvl
        });
        if !ok {
            return false;
        }
    }

    if has_forbidden_enchant(have, cfg, required) {
        return false;
    }

    let exact = cfg
        .get("exact_effects")
        .or_else(|| cfg.get("exactEffects"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if exact {
        let required_names: Vec<String> = required
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
            .map(normalize_enchant_name)
            .filter(|s| !s.is_empty())
            .collect();
        if have
            .iter()
            .any(|(n, _)| !required_names.iter().any(|r| enchant_names_match(n, r)))
        {
            return false;
        }
    }
    true
}

fn lore_matches_config(item: &ItemStackData, cfg: &Value) -> bool {
    let needle = cfg
        .get("lore_match")
        .or_else(|| cfg.get("loreMatch"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if needle.is_empty() {
        return true;
    }
    lore_strings(item).iter().any(|s| s.contains(needle))
        || lore_strings(item).join("").contains(needle)
}

fn has_forbidden_enchant(
    have: &[(String, i32)],
    cfg: &Value,
    required: &[Value],
) -> bool {
    let cfg_name = cfg
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches("minecraft:");
    let mut forbidden: Vec<String> = forbidden_by_type(cfg_name)
        .iter()
        .map(|s| normalize_enchant_name(s))
        .collect();
    if let Some(extra) = cfg
        .get("forbidden_effects")
        .or_else(|| cfg.get("forbiddenEffects"))
        .and_then(|v| v.as_array())
    {
        for e in extra {
            if let Some(n) = e.get("name").and_then(|v| v.as_str()) {
                forbidden.push(normalize_enchant_name(n));
            }
        }
    }
    if forbidden.is_empty() {
        return false;
    }
    let allowed: Vec<String> = required
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
        .map(normalize_enchant_name)
        .collect();
    have.iter().any(|(n, _)| {
        let nn = normalize_enchant_name(n);
        if allowed.iter().any(|a| enchant_names_match(&nn, a)) {
            return false;
        }
        forbidden.iter().any(|f| enchant_names_match(&nn, f))
    })
}

fn forbidden_by_type(cfg_name: &str) -> &'static [&'static str] {
    match cfg_name {
        "netherite_sword" | "diamond_sword" | "netherite_pickaxe" => &["heavy", "unstable"],
        "netherite_helmet" | "netherite_chestplate" | "netherite_leggings" | "netherite_boots" => {
            &["minecraft:thorns", "thorns"]
        }
        _ => &[],
    }
}

/// old `catalogTypeMatchesGoType` (упрощённо + armor).
fn catalog_type_matches_go(catalog_type: &str, go_type: &str, catalog_name: &str) -> bool {
    if go_type.is_empty() {
        return true;
    }
    if catalog_type == go_type {
        return true;
    }
    const ARMOR: &str = "netherite_armor-1.21";
    const PIECES: &[(&str, &str)] = &[
        ("netherite_helmet-1.21", "netherite_helmet"),
        ("netherite_chestplate-1.21", "netherite_chestplate"),
        ("netherite_leggings-1.21", "netherite_leggings"),
        ("netherite_boots-1.21", "netherite_boots"),
    ];
    if go_type == ARMOR {
        if PIECES.iter().any(|(t, _)| *t == catalog_type) {
            return true;
        }
        if catalog_type == ARMOR {
            return catalog_name.is_empty()
                || PIECES.iter().any(|(_, n)| *n == catalog_name);
        }
        return false;
    }
    if catalog_type == ARMOR {
        return PIECES
            .iter()
            .any(|(t, n)| *t == go_type && *n == catalog_name);
    }
    // legacy prefix: netherite_sword-1.21 vs netherite_sword-1.21
    let a = catalog_type.split('-').next().unwrap_or(catalog_type);
    let b = go_type.split('-').next().unwrap_or(go_type);
    a == b
}

fn normalize_enchant_name(name: &str) -> String {
    let mut n = name.trim().to_lowercase();
    if n == "minecraft:sweeping" || n == "sweeping" {
        n = "minecraft:sweeping_edge".into();
    }
    if n == "sweeping_edge" {
        return "minecraft:sweeping_edge".into();
    }
    if let Some(mapped) = canonicalize_custom_enchant(&n) {
        return mapped;
    }
    if n.contains(':') {
        return n;
    }
    format!("minecraft:{n}")
}

pub fn enchant_names_match(a: &str, b: &str) -> bool {
    normalize_enchant_name(a) == normalize_enchant_name(b)
}

/// Minecraft id предмета (`netherite_sword`) — как `item.name` в mineflayer.
pub fn item_minecraft_name(item: &ItemStackData) -> String {
    kind_id(item.kind)
}

fn kind_id(kind: ItemKind) -> String {
    kind_id_from_debug(&format!("{kind:?}"))
}

fn kind_id_from_debug(s: &str) -> String {
    // NetheriteSword / Sharpness → netherite_sword / sharpness
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn present(item: &ItemStack) -> Option<&ItemStackData> {
    item.as_present()
}

#[cfg(test)]
mod custom_enchant_tests {
    use super::*;
    use simdnbt::owned::{Nbt, NbtCompound, NbtList};

    fn nbt_root(c: NbtCompound) -> Nbt {
        Nbt::new("".into(), c)
    }

    fn lvl(got: &[(String, i32)], name: &str) -> i32 {
        got.iter()
            .find(|(n, _)| n == name)
            .map(|(_, l)| *l)
            .unwrap_or(0)
    }

    #[test]
    fn bukkit_minecraft_type_ru() {
        let mut ench = NbtCompound::new();
        ench.insert("minecraft:type", "Яд");
        ench.insert("minecraft:level", 2i32);
        let mut bukkit = NbtCompound::new();
        bukkit.insert(
            "minecraft:custom-enchantments",
            NbtList::Compound(vec![ench]),
        );
        let mut root = NbtCompound::new();
        root.insert("PublicBukkitValues", NbtTag::Compound(bukkit));
        let got = extract_custom_enchants_from_nbt(&nbt_root(root));
        assert_eq!(lvl(&got, "poison"), 2);
    }

    #[test]
    fn namespaced_english_and_lowercase_ru() {
        let mut a = NbtCompound::new();
        a.insert("type", "minecraft:poison");
        a.insert("level", 2i32);
        let mut b = NbtCompound::new();
        b.insert("id", "яд");
        b.insert("lvl", "III");
        let mut c = NbtCompound::new();
        c.insert("minecraft:type", "Детекция");
        c.insert("minecraft:level", 1i16);
        let mut root = NbtCompound::new();
        root.insert("custom_enchantments", NbtList::Compound(vec![a, b, c]));
        let got = extract_custom_enchants_from_nbt(&nbt_root(root));
        assert_eq!(lvl(&got, "poison"), 3);
        assert_eq!(lvl(&got, "detection"), 1);
    }

    #[test]
    fn pair_without_enchant_parent_key() {
        let mut inner = NbtCompound::new();
        inner.insert("minecraft:type", "Бульдозер");
        inner.insert("minecraft:level", 2i32);
        let mut wrap = NbtCompound::new();
        wrap.insert("whatever", NbtTag::Compound(inner));
        let got = extract_custom_enchants_from_nbt(&nbt_root(wrap));
        assert_eq!(lvl(&got, "buldozing"), 2);
    }

    #[test]
    fn string_list_under_enchant_key() {
        let mut root = NbtCompound::new();
        root.insert(
            "custom-enchantments",
            NbtList::String(vec!["Яд II".into(), "detection:3".into()]),
        );
        let got = extract_custom_enchants_from_nbt(&nbt_root(root));
        assert_eq!(lvl(&got, "poison"), 2);
        assert_eq!(lvl(&got, "detection"), 3);
    }

    #[test]
    fn lore_garbage_and_vanilla_not_custom() {
        let mut junk = NbtCompound::new();
        junk.insert("minecraft:type", "Острота");
        junk.insert("minecraft:level", 7i32);
        let mut lore = NbtCompound::new();
        lore.insert("text", "Яд II");
        lore.insert("Продавец:", "Nick");
        let mut root = NbtCompound::new();
        root.insert("minecraft:custom-enchantments", NbtList::Compound(vec![junk]));
        root.insert("display", NbtTag::Compound(lore));
        let got = extract_custom_enchants_from_nbt(&nbt_root(root));
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn config_poison_matches_namespaced() {
        assert!(enchant_names_match("minecraft:poison", "poison"));
        assert!(enchant_names_match("Яд", "poison"));
        assert!(!enchant_names_match("minecraft:sharpness", "poison"));
    }

    #[test]
    fn nbt_json_keeps_bukkit_keys() {
        let mut ench = NbtCompound::new();
        ench.insert("minecraft:type", "Яд");
        ench.insert("minecraft:level", 2i32);
        let mut bukkit = NbtCompound::new();
        bukkit.insert(
            "minecraft:custom-enchantments",
            NbtList::Compound(vec![ench]),
        );
        let mut root = NbtCompound::new();
        root.insert("PublicBukkitValues", NbtTag::Compound(bukkit));
        let j = nbt_to_json(&nbt_root(root));
        let s = j.to_string();
        assert!(s.contains("PublicBukkitValues"), "{s}");
        assert!(s.contains("custom-enchantments"), "{s}");
        assert!(s.contains("Яд"), "{s}");
    }

    #[test]
    fn lore_dump_style_pochti() {
        let lines = [
            "Вампиризм I",
            "Окисление I",
            "Яд I",
            "Детекция I",
            "Опытный II",
            " ➥ Нажмите, чтобы купить",
            "$ Ценa: $4,499,000",
            "⭐ Продавец: someone",
        ]
        .map(str::to_string);
        let got = extract_custom_enchants_from_lore_lines(&lines);
        assert_eq!(lvl(&got, "poison"), 1);
        assert_eq!(lvl(&got, "detection"), 1);
        assert_eq!(lvl(&got, "vampirism"), 1);
        assert_eq!(lvl(&got, "oxidation"), 1);
        assert_eq!(lvl(&got, "skilled"), 2);
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn lore_scam_not_substring_or_name() {
        let lines = [
            "Меч с Яд II",
            "Острота VII",
            "Яд II и ещё текст",
        ]
        .map(str::to_string);
        let got = extract_custom_enchants_from_lore_lines(&lines);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn lore_skip_plain_rename_keeps_other_lines() {
        let rename = Some("Яд II");
        let mut out = Vec::new();
        push_split_lines("Яд II", rename, &mut out);
        push_split_lines("Окисление I", rename, &mut out);
        push_split_lines("Яд I", rename, &mut out);
        let got = extract_custom_enchants_from_lore_lines(&out);
        assert_eq!(lvl(&got, "poison"), 1);
        assert_eq!(lvl(&got, "oxidation"), 1);
        assert_ne!(lvl(&got, "poison"), 2);
    }

    #[test]
    fn lore_mega_bulldozer_no_level() {
        let lines = ["Магнит", "Мега-бульдозер"].map(str::to_string);
        let got = extract_custom_enchants_from_lore_lines(&lines);
        assert_eq!(lvl(&got, "magnet"), 1);
        assert_eq!(lvl(&got, "mega_buldozing"), 1);
    }
}