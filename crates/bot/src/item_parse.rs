//! Парсинг лота АХ: цена из лора, UUID, чары (упрощённый порт items/slotInfo.mjs).

use azalea::prelude::Client;
use azalea_chat::FormattedText;
use azalea_inventory::{
    ItemStack, ItemStackData,
    components::{
        CustomData, CustomName, Damage, Enchantments, Lore, MaxDamage, StoredEnchantments,
    },
};
use azalea_registry::{DataRegistry, builtin::ItemKind};
use regex::Regex;
use serde_json::Value;
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

pub fn price_with_durability(base: f64, dur: f64) -> f64 {
    if dur < 0.5 {
        return 0.0;
    }
    let marker = (base as i64) % 100;
    let price = ((base * dur).floor() as i64 / 100) * 100 + marker;
    price as f64
}

/// Чары: ваниль (id → имя как old/`minecraft-data` 1.21.11) + custom_data + лор.
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
        walk_custom_enchants(&data.nbt, &mut |n, l| push(n, l));
    }

    for s in lore_strings(item) {
        if let Some((name, lvl)) = parse_lore_enchant(&s) {
            push(name, lvl);
        }
    }
    by_name.into_iter().collect()
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

fn walk_custom_enchants(nbt: &Nbt, push: &mut dyn FnMut(String, i32)) {
    if matches!(nbt, Nbt::None) {
        return;
    }
    walk_custom_compound(nbt, push);
}

fn walk_custom_compound(c: &NbtCompound, push: &mut dyn FnMut(String, i32)) {
    for (k, v) in c.iter() {
        let key = k.to_string_lossy();
        if key.contains("custom-enchantments") || key.contains("custom_enchantments") {
            if let NbtTag::List(list) = v {
                if let NbtList::Compound(xs) = list {
                    for compound in xs {
                        let mut name = None;
                        let mut lvl = None;
                        for (kk, vv) in compound.iter() {
                            let ks = kk.to_string_lossy();
                            if ks.ends_with("type") || ks == "type" || ks == "id" {
                                if let NbtTag::String(s) = vv {
                                    name = Some(s.to_string_lossy().into_owned());
                                }
                            }
                            if ks.ends_with("level") || ks == "lvl" || ks == "level" {
                                if let NbtTag::Int(n) = vv {
                                    lvl = Some(*n);
                                } else if let NbtTag::Short(n) = vv {
                                    lvl = Some(*n as i32);
                                } else if let NbtTag::Byte(n) = vv {
                                    lvl = Some(*n as i32);
                                }
                            }
                        }
                        if let (Some(n), Some(l)) = (name, lvl) {
                            let mapped = map_custom_ru(&n).unwrap_or(n);
                            push(mapped, l);
                        }
                    }
                }
            }
        }
        match v {
            NbtTag::Compound(inner) => walk_custom_compound(inner, push),
            NbtTag::List(NbtList::Compound(xs)) => {
                for inner in xs {
                    walk_custom_compound(inner, push);
                }
            }
            _ => {}
        }
    }
}

fn map_custom_ru(name: &str) -> Option<String> {
    let map = [
        ("Яд", "poison"),
        ("Вампиризм", "vampirism"),
        ("Детекция", "detection"),
        ("Тяжелый", "heavy"),
        ("Тяжёлый", "heavy"),
        ("Нестабильный", "unstable"),
        ("Бульдозер", "buldozing"),
        ("Магнит", "magnet"),
        ("Паутина", "web"),
        ("Авто-плавка", "smelting"),
        ("Опытный", "skilled"),
        ("Попрыгун", "jumping"),
        ("Снайпер", "sniper"),
        ("Окисление", "oxidation"),
    ];
    for (ru, en) in map {
        if name == ru || name.eq_ignore_ascii_case(en) {
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

fn parse_lore_enchant(s: &str) -> Option<(String, i32)> {
    let t = s.trim();
    if t.is_empty() || t.len() > 80 {
        return None;
    }
    // «Острота VII» / «Яд III» — имя + римская цифра в конце
    if let Some((name_raw, lvl)) = split_trailing_roman(t) {
        let mapped = map_lore_enchant_name(name_raw)?;
        return Some((mapped, lvl));
    }
    let map = [
        ("острота", "sharpness"),
        ("sharpness", "sharpness"),
        ("прочн", "unbreaking"),
        ("unbreaking", "unbreaking"),
        ("яд", "poison"),
        ("poison", "poison"),
        ("заговор огня", "fire_aspect"),
        ("огонь", "fire_aspect"),
        ("fire", "fire_aspect"),
        ("отдача", "knockback"),
        ("knockback", "knockback"),
        ("добыча", "looting"),
        ("looting", "looting"),
        ("детекц", "detection"),
        ("detection", "detection"),
        ("вампир", "vampirism"),
        ("тяж", "heavy"),
        ("heavy", "heavy"),
        ("нестабил", "unstable"),
    ];
    let low = t.to_lowercase();
    for (pat, canon) in map {
        if low.contains(pat) {
            let lvl = roman_or_digit(&low).unwrap_or(1);
            return Some((canon.to_string(), lvl));
        }
    }
    None
}

fn split_trailing_roman(s: &str) -> Option<(&str, i32)> {
    let s = s.trim();
    let (name, roman) = s.rsplit_once(' ')?;
    let roman = roman.trim();
    if !matches!(
        roman,
        "I" | "II" | "III" | "IV" | "V" | "VI" | "VII" | "VIII" | "IX" | "X"
            | "i" | "ii" | "iii" | "iv" | "v" | "vi" | "vii" | "viii" | "ix" | "x"
    ) {
        return None;
    }
    let lvl = roman_or_digit(&roman.to_lowercase())?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name, lvl))
}

fn map_lore_enchant_name(raw: &str) -> Option<String> {
    if let Some(m) = map_custom_ru(raw) {
        return Some(m);
    }
    let low = raw.to_lowercase();
    // FunTime RU display → как minecraft-data / каталог
    let map = [
        ("острота", "sharpness"),
        ("прочность", "unbreaking"),
        ("заговор огня", "fire_aspect"),
        ("огненный аспект", "fire_aspect"),
        ("аспект огня", "fire_aspect"),
        ("отдача", "knockback"),
        ("добыча", "looting"),
        ("починка", "mending"),
        ("эффективность", "efficiency"),
        ("удача", "fortune"),
        ("шелковое касание", "silk_touch"),
        ("щёлкающее касание", "silk_touch"),
        ("тягун", "knockback"),
        ("бич членистоногих", "bane_of_arthropods"),
        ("небесная кара", "smite"),
        ("охран", "protection"),
        ("шипы", "thorns"),
        ("подводник", "respiration"),
        ("родство с водой", "aqua_affinity"),
    ];
    for (pat, canon) in map {
        if low == pat || low.contains(pat) {
            return Some(canon.to_string());
        }
    }
    if raw.contains(':') || raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Some(raw.to_string());
    }
    None
}

fn roman_or_digit(s: &str) -> Option<i32> {
    for (r, n) in [
        ("viii", 8),
        ("vii", 7),
        ("vi", 6),
        ("iv", 4),
        ("v", 5),
        ("iii", 3),
        ("ii", 2),
        ("i", 1),
    ] {
        if s.contains(r) {
            return Some(n);
        }
    }
    for c in s.chars().rev() {
        if c.is_ascii_digit() {
            return c.to_digit(10).map(|d| d as i32);
        }
    }
    None
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
    let sell = price_with_durability(price_sell, durability_percent(item));
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
    if n.contains(':') {
        return n;
    }
    // ваниль → minecraft:*; кастом (poison, heavy) без префикса
    match n.as_str() {
        "poison" | "vampirism" | "heavy" | "unstable" | "detection" | "buldozing" | "magnet"
        | "web" | "smelting" | "skilled" | "jumping" | "sniper" | "oxidation" => n,
        _ => format!("minecraft:{n}"),
    }
}

fn enchant_names_match(a: &str, b: &str) -> bool {
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
