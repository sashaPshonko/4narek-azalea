//! Listing memory на стороне орка (диск: listings-state/<user>.json).

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use tracing::warn;

const LISTING_ID_COUNT: u8 = 5;

#[derive(Clone, Debug)]
struct ListingMeta {
    listing_id: u8,
    catalog_id: String,
    enchants: Value,
    durability: Option<f64>,
    price: f64,
}

struct Mem {
    memory: HashMap<u8, ListingMeta>,
    pending: Option<ListingMeta>,
    occupied: HashSet<u8>,
}

impl Default for Mem {
    fn default() -> Self {
        Self {
            memory: HashMap::new(),
            pending: None,
            occupied: HashSet::new(),
        }
    }
}

pub struct ListingStore {
    dir: PathBuf,
    by_user: HashMap<String, Mem>,
}

impl ListingStore {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            by_user: HashMap::new(),
        }
    }

    fn path(&self, username: &str) -> PathBuf {
        let safe: String = username
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '_' })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    fn load(&mut self, username: &str) -> &mut Mem {
        if !self.by_user.contains_key(username) {
            let mut mem = Mem::default();
            let p = self.path(username);
            if p.exists() {
                if let Ok(raw) = fs::read_to_string(&p) {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        import_state(&mut mem, &v);
                    }
                }
            }
            self.by_user.insert(username.to_string(), mem);
        }
        self.by_user.get_mut(username).unwrap()
    }

    fn save(&self, username: &str) {
        let Some(mem) = self.by_user.get(username) else {
            return;
        };
        let v = export_state(mem);
        if let Err(e) = fs::write(self.path(username), v.to_string()) {
            warn!("listing save {username}: {e}");
        }
    }

    pub fn export_for(&mut self, username: &str) -> Value {
        let mem = self.load(username);
        export_state(mem)
    }

    pub fn handle(&mut self, username: &str, data: &Value) -> Value {
        let op = data
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mem = self.load(username);
        let result = match op {
            "sync" => {
                let prices: Vec<f64> = data
                    .get("prices")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_f64())
                            .collect()
                    })
                    .unwrap_or_default();
                mem.occupied.clear();
                for p in prices {
                    if p > 0.0 {
                        mem.occupied.insert((p as i64 % 10) as u8);
                    }
                }
                let keep = mem.occupied.clone();
                mem.memory.retain(|id, _| keep.contains(id));
                json!({ "ok": true })
            }
            "alloc" => {
                // бот уже выбрал id локально — зеркалим pending
                if let (Some(id), Some(price)) = (
                    data.get("listingId").and_then(|v| v.as_u64()),
                    data.get("listPrice").and_then(|v| v.as_f64()),
                ) {
                    mem.pending = Some(ListingMeta {
                        listing_id: (id % 10) as u8,
                        catalog_id: data
                            .get("catalogId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        enchants: data.get("enchants").cloned().unwrap_or(json!([])),
                        durability: data.get("durability").and_then(|v| v.as_f64()),
                        price,
                    });
                    json!({ "listingId": id, "listPrice": price })
                } else {
                    // классический alloc
                    let id = (0..LISTING_ID_COUNT).find(|i| {
                        !mem.occupied.contains(i)
                            && !mem.memory.contains_key(i)
                            && mem.pending.as_ref().map(|p| p.listing_id) != Some(*i)
                    });
                    let Some(listing_id) = id else {
                        return json!({ "listingId": null, "listPrice": null });
                    };
                    let sell = data.get("sellPrice").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let list_price = (sell / 10.0).floor() * 10.0 + f64::from(listing_id);
                    mem.pending = Some(ListingMeta {
                        listing_id,
                        catalog_id: data
                            .get("catalogId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        enchants: data.get("enchants").cloned().unwrap_or(json!([])),
                        durability: data.get("durability").and_then(|v| v.as_f64()),
                        price: list_price,
                    });
                    json!({ "listingId": listing_id, "listPrice": list_price })
                }
            }
            "clearPending" => {
                mem.pending = None;
                json!({ "ok": true })
            }
            "confirm" => {
                let price = data.get("price").and_then(|v| v.as_f64());
                let out = if let (Some(price), Some(pending)) = (price, mem.pending.take()) {
                    if (price as i64 % 10) as u8 == pending.listing_id {
                        let row = ListingMeta { price, ..pending };
                        let v = json!({
                            "catalogId": row.catalog_id,
                            "listingId": row.listing_id,
                            "price": row.price,
                            "enchants": row.enchants,
                            "durability": row.durability,
                        });
                        mem.memory.insert(row.listing_id, row);
                        v
                    } else {
                        Value::Null
                    }
                } else {
                    Value::Null
                };
                out
            }
            "takeSold" => {
                let price = data.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let id = (price as i64 % 10) as u8;
                if let Some(row) = mem.memory.remove(&id) {
                    json!({
                        "catalogId": row.catalog_id,
                        "listingId": row.listing_id,
                        "price": row.price,
                        "enchants": row.enchants,
                        "durability": row.durability,
                    })
                } else {
                    Value::Null
                }
            }
            "exportState" => export_state(mem),
            _ => json!({ "error": format!("unknown_op:{op}") }),
        };
        self.save(username);
        result
    }
}

fn export_state(mem: &Mem) -> Value {
    let listings: Vec<Value> = mem
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
        "pending": mem.pending.as_ref().map(|p| json!({
            "listingId": p.listing_id,
            "catalogId": p.catalog_id,
            "enchants": p.enchants,
            "durability": p.durability,
            "price": p.price,
        })),
        "occupied": mem.occupied.iter().copied().collect::<Vec<_>>(),
        "listings": listings,
    })
}

fn import_state(mem: &mut Mem, raw: &Value) {
    mem.memory.clear();
    mem.pending = None;
    mem.occupied.clear();
    if let Some(arr) = raw.get("occupied").and_then(|v| v.as_array()) {
        for id in arr {
            if let Some(n) = id.as_u64() {
                mem.occupied.insert((n % 10) as u8);
            }
        }
    }
    if let Some(arr) = raw.get("listings").and_then(|v| v.as_array()) {
        for row in arr {
            let Some(id) = row.get("listingId").and_then(|v| v.as_u64()) else {
                continue;
            };
            let Some(cid) = row.get("catalogId").and_then(|v| v.as_str()) else {
                continue;
            };
            mem.memory.insert(
                (id % 10) as u8,
                ListingMeta {
                    listing_id: (id % 10) as u8,
                    catalog_id: cid.to_string(),
                    enchants: row.get("enchants").cloned().unwrap_or(json!([])),
                    durability: row.get("durability").and_then(|v| v.as_f64()),
                    price: row.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0),
                },
            );
        }
    }
}

pub fn default_dir(farm_root: &Path) -> PathBuf {
    farm_root.join("listings-state")
}
