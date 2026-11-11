//! Listing memory 0–4 (порт items/listing-memory.mjs) — локально в воркере.
//! Орк зеркалит на диск через IPC `listing` / `listing_res`.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use ipc::{emit, WorkerMsg};

/// Максимум номеров лотов (последняя цифра цены): 5 без аренды, 10 с арендой.
pub const LISTING_ID_MAX: usize = 10;

#[derive(Debug, Clone)]
pub struct ListingMeta {
    pub listing_id: u8,
    pub catalog_id: String,
    pub enchants: Vec<Value>,
    pub durability: Option<f64>,
    pub price: f64,
}

#[derive(Default)]
pub struct ListingMemory {
    memory: HashMap<u8, ListingMeta>,
    pending: Option<ListingMeta>,
    occupied_from_scan: HashSet<u8>,
}

impl ListingMemory {
    pub fn sync_from_storage_prices(&mut self, prices: &[f64]) {
        self.occupied_from_scan.clear();
        for p in prices {
            if p.is_finite() && *p > 0.0 {
                self.occupied_from_scan.insert((*p as i64 % 10) as u8);
            }
        }
        let keep: HashSet<u8> = self.occupied_from_scan.clone();
        self.memory.retain(|id, _| keep.contains(id));
    }

    /// Хранилище — единственный источник правды: память лотов целиком заменяется тем, что реально лежит в слотах
    /// (каталожный id, чары, прочность, цена), а занятые номера берутся по ценам всех лотов окна.
    /// Ожидающая запись (`pending`, лот в процессе выставления) не трогается.
    pub fn rebuild_from_storage(&mut self, rows: Vec<ListingMeta>, all_prices: &[f64]) {
        self.occupied_from_scan.clear();
        for p in all_prices {
            if p.is_finite() && *p > 0.0 {
                self.occupied_from_scan.insert((*p as i64 % 10) as u8);
            }
        }
        self.memory.clear();
        for row in rows {
            self.memory.insert(row.listing_id, row);
        }
    }

    fn is_busy(&self, id: u8) -> bool {
        self.occupied_from_scan.contains(&id)
            || self.memory.contains_key(&id)
            || self.pending.as_ref().map(|p| p.listing_id) == Some(id)
    }

    /// Свободный номер лота среди первых `cap` (5 или 10, по аренде).
    pub fn alloc_id(&self, cap: usize) -> Option<u8> {
        (0..cap.min(LISTING_ID_MAX) as u8).find(|&id| !self.is_busy(id))
    }

    pub fn price_with_listing_id(base: f64, listing_id: u8) -> Option<f64> {
        let base = base.floor();
        if !base.is_finite() || base <= 0.0 {
            return None;
        }
        Some((base / 10.0).floor() * 10.0 + f64::from(listing_id % 10))
    }

    pub fn prices_match(ah: f64, catalog: f64) -> bool {
        if !ah.is_finite() || !catalog.is_finite() {
            return false;
        }
        (ah / 10.0).floor() == (catalog / 10.0).floor()
    }

    pub fn prices_aligned(ah: f64, catalog: f64) -> bool {
        if Self::prices_match(ah, catalog) {
            return true;
        }
        if !ah.is_finite() || !catalog.is_finite() {
            return false;
        }
        let a = (ah / 10.0).floor() * 10.0;
        let e = (catalog / 10.0).floor() * 10.0;
        let tol = (a.max(e) * 0.01).round().max(2_000.0);
        (a - e).abs() <= tol
    }

    pub fn set_pending(&mut self, meta: ListingMeta) {
        self.pending = Some(meta);
    }

    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    pub fn confirm_pending(&mut self, price: f64) -> Option<ListingMeta> {
        let pending = self.pending.take()?;
        if !price.is_finite() || (price as i64 % 10) as u8 != pending.listing_id {
            return None;
        }
        let row = ListingMeta {
            price,
            ..pending
        };
        self.memory.insert(row.listing_id, row.clone());
        Some(row)
    }

    pub fn take_sold(&mut self, price: f64) -> Option<ListingMeta> {
        if !price.is_finite() {
            return None;
        }
        let id = (price as i64 % 10) as u8;
        // Номер освободился и в «занятых по скану хранилища»: иначе is_busy() видел его занятым до следующего
        // захода в хранилище (до ~минуты) и продажа сразу после «У Вас купили» падала с «нет свободного listing id».
        self.occupied_from_scan.remove(&id);
        self.memory.remove(&id)
    }

    pub fn get_by_listing_id(&self, listing_id: u8) -> Option<&ListingMeta> {
        self.memory.get(&(listing_id % 10))
    }

    pub fn all_metas(&self) -> Vec<ListingMeta> {
        self.memory.values().cloned().collect()
    }

    pub fn export_state(&self) -> Value {
        let listings: Vec<Value> = self
            .memory
            .values()
            .map(|r| {
                json!({
                    "listingId": r.listing_id,
                    "catalogId": r.catalog_id,
                    "enchants": r.enchants,
                    "durability": r.durability,
                    "price": r.price,
                })
            })
            .collect();
        json!({
            "pending": self.pending.as_ref().map(|p| json!({
                "listingId": p.listing_id,
                "catalogId": p.catalog_id,
                "enchants": p.enchants,
                "durability": p.durability,
                "price": p.price,
            })),
            "occupied": self.occupied_from_scan.iter().copied().collect::<Vec<_>>(),
            "listings": listings,
        })
    }

    pub fn import_state(&mut self, raw: &Value) {
        self.memory.clear();
        self.pending = None;
        self.occupied_from_scan.clear();
        if let Some(arr) = raw.get("occupied").and_then(|v| v.as_array()) {
            for id in arr {
                if let Some(n) = id.as_u64() {
                    self.occupied_from_scan.insert((n % 10) as u8);
                }
            }
        }
        if let Some(arr) = raw.get("listings").and_then(|v| v.as_array()) {
            for row in arr {
                if let Some(meta) = meta_from_json(row) {
                    self.memory.insert(meta.listing_id, meta);
                }
            }
        }
        if let Some(p) = raw.get("pending") {
            if let Some(meta) = meta_from_json(p) {
                self.pending = Some(meta);
            }
        }
    }
}

fn meta_from_json(row: &Value) -> Option<ListingMeta> {
    let listing_id = row.get("listingId")?.as_u64()? as u8 % 10;
    let catalog_id = row.get("catalogId")?.as_str()?.to_string();
    if catalog_id.is_empty() {
        return None;
    }
    Some(ListingMeta {
        listing_id,
        catalog_id,
        enchants: row
            .get("enchants")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        durability: row.get("durability").and_then(|v| v.as_f64()),
        price: row.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0),
    })
}

pub fn expected_sell_from_meta(catalog_all: &[Value], meta: &ListingMeta) -> Option<f64> {
    let cfg = catalog_all.iter().find(|c| {
        c.get("id").and_then(|v| v.as_str()) == Some(meta.catalog_id.as_str())
    })?;
    let price_sell = cfg.get("priceSell")?.as_f64()?;
    let dur = meta.durability.unwrap_or(1.0);
    let mending = meta.enchants.iter().any(|e| {
        e.get("name")
            .and_then(|v| v.as_str())
            .map(|n| crate::item_parse::enchant_names_match(n, "minecraft:mending"))
            .unwrap_or(false)
    });
    let sell = crate::item_parse::price_with_durability_mending(price_sell, dur, mending);
    if sell > 0.0 {
        Some(sell)
    } else {
        None
    }
}

/// Ожидание ответа орка на listing RPC.
#[derive(Default)]
pub struct ListingWaiters {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
}

impl ListingWaiters {
    pub fn register(&self) -> (u64, oneshot::Receiver<Value>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        (id, rx)
    }

    pub fn resolve(&self, req_id: u64, result: Value) {
        if let Some(tx) = self.pending.lock().remove(&req_id) {
            let _ = tx.send(result);
        }
    }
}

/// Локальный alloc (без ожидания орка) + зеркало в orch для диска.
pub fn alloc_local(
    mem: &mut ListingMemory,
    sell_price: f64,
    catalog_id: &str,
    enchants: Vec<Value>,
    durability: Option<f64>,
    username: &str,
    cap: usize,
) -> Option<(u8, f64)> {
    let listing_id = mem.alloc_id(cap)?;
    let list_price = ListingMemory::price_with_listing_id(sell_price, listing_id)?;
    mem.set_pending(ListingMeta {
        listing_id,
        catalog_id: catalog_id.to_string(),
        enchants: enchants.clone(),
        durability,
        price: list_price,
    });
    // зеркало
    emit(&WorkerMsg {
        name: "listing".into(),
        username: Some(username.to_string()),
        data: Some(json!({
            "op": "alloc",
            "sellPrice": sell_price,
            "catalogId": catalog_id,
            "enchants": enchants,
            "durability": durability,
            "listingId": listing_id,
            "listPrice": list_price,
        })),
        ..crate::empty_msg()
    });
    Some((listing_id, list_price))
}

pub fn mirror_op(username: &str, op: &str, payload: Value) {
    let mut data = payload;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("op".into(), json!(op));
    }
    emit(&WorkerMsg {
        name: "listing".into(),
        username: Some(username.to_string()),
        data: Some(data),
        ..crate::empty_msg()
    });
}

pub type SharedListings = Arc<Mutex<ListingMemory>>;
#[cfg(test)]
mod rebuild_tests {
    use super::*;

    fn meta(id: u8, catalog: &str, price: f64) -> ListingMeta {
        ListingMeta {
            listing_id: id,
            catalog_id: catalog.into(),
            enchants: vec![],
            durability: Some(1.0),
            price,
        }
    }

    #[test]
    fn rebuild_replaces_memory_with_storage_contents() {
        let mut m = ListingMemory::default();
        // в памяти устаревшая запись: лота с таким номером на витрине уже нет, а настоящий лот не записан
        m.memory.insert(2, meta(2, "старый-товар", 111.0));
        m.rebuild_from_storage(
            vec![meta(4, "pochti-megasword-1.21", 6_056_094.0), meta(0, "sword7-1.21", 1_200_000.0)],
            &[6_056_094.0, 1_200_000.0, 55_000.0],
        );
        let mut ids: Vec<u8> = m.all_metas().iter().map(|x| x.listing_id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 4], "память = ровно торгуемые лоты из слотов");
        assert_eq!(m.get_by_listing_id(4).unwrap().catalog_id, "pochti-megasword-1.21");
        assert!(m.get_by_listing_id(2).is_none(), "устаревшая запись удалена");
        // занятые номера идут по ценам ВСЕХ лотов окна (в том числе чужих/мусорных): номер 5 занят лотом за 55 000
        assert!(m.alloc_id(5).is_some());
        assert_eq!(m.alloc_id(5), Some(1), "свободен ближайший номер, не занятый ни одним лотом окна");
    }

    #[test]
    fn rebuild_keeps_pending_listing() {
        let mut m = ListingMemory::default();
        m.set_pending(meta(3, "sword7-1.21", 1_000_003.0));
        m.rebuild_from_storage(vec![], &[]);
        assert_eq!(m.alloc_id(5), Some(0));
        assert!(m.confirm_pending(1_000_003.0).is_some(), "лот в процессе выставления не теряется");
    }

    #[test]
    fn sold_listing_frees_its_id_even_after_storage_scan() {
        let mut m = ListingMemory::default();
        // витрина полна: 5 лотов, номера 0–4 заняты и по памяти, и по скану хранилища
        let rows: Vec<ListingMeta> = (0..5u8).map(|i| meta(i, "sword7-1.21", 1_000_000.0 + f64::from(i))).collect();
        let prices: Vec<f64> = (0..5u8).map(|i| 1_000_000.0 + f64::from(i)).collect();
        m.rebuild_from_storage(rows, &prices);
        assert_eq!(m.alloc_id(5), None, "все 5 заняты");
        // «У Вас купили … $1,000,004» → номер 4 свободен сразу, не после следующего захода в хранилище
        assert!(m.take_sold(1_000_004.0).is_some());
        assert_eq!(m.alloc_id(5), Some(4));
    }
}
