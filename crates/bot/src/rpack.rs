//! Ресурспак сервера: отвечаем как ванильный клиент, а не как встроенный плагин azalea.
//!
//! azalea (`AcceptResourcePacksPlugin`) в ту же миллисекунду шлёт «принял» и «успешно загружен», ничего не
//! скачивая и без шага «скачан» (DOWNLOADED появился в 1.20.3, в `Action` azalea его вообще нет). Сервер видит
//! это сразу: живой клиент между «принял» и «загружен» тратит секунды и скачивает файл по ссылке.
//!
//! Ванильный порядок (1.21.11, `ServerboundResourcePackPacket.Action`):
//! `ACCEPTED(3)` → скачивание → `DOWNLOADED(4)` → перезагрузка ресурсов → `SUCCESSFULLY_LOADED(0)`.
//! Здесь так же: «принял» через 1–3,5 с (экран подтверждения), скачивание через прокси бота (тот же IP, что
//! и у игрового подключения), «скачан», затем «загружен» через 1,5–4 с. Уже скачанный пак (по хешу) повторно
//! не качаем, как и клиент со своим кэшем.
//!
//! Если скачать не вышло (прокси, таймаут), всё равно отвечаем «скачан» и «загружен»: «не удалось» на
//! обязательном паке кикнет бота. В лог пишем предупреждение.

use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use azalea::{
    app::{App, Plugin, Update},
    ecs::prelude::*,
    packet::{
        config::SendConfigPacketEvent,
        game::{ResourcePackEvent, SendGamePacketEvent},
    },
    protocol::packets::{config, game},
    InConfigState,
};
use parking_lot::Mutex;
use rand::Rng;
use tracing::{info, warn};
use uuid::Uuid;

/// Версия клиента, которую называет ванильный загрузчик паков.
const MC_VERSION: &str = "1.21.11";
/// Формат ресурспаков клиента 1.21.11 (заголовок X-Minecraft-Pack-Format).
const PACK_FORMAT: &str = "75";
/// Ванильный предел размера пака — 250 МиБ.
const MAX_PACK_BYTES: u64 = 250 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

pub struct VanillaResourcePackPlugin;
impl Plugin for VanillaResourcePackPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RpJobs>().add_systems(Update, (take_requests, drive_jobs).chain());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Accepted,
    Downloaded,
    Loaded,
}

#[derive(Default)]
struct DlState {
    done_at: Option<Instant>,
}

struct Job {
    entity: Entity,
    id: Uuid,
    accept_at: Instant,
    accepted: bool,
    downloaded: bool,
    loaded_at: Option<Instant>,
    dl: Arc<Mutex<DlState>>,
}

#[derive(Default)]
struct RpJobs(Vec<Job>);
impl Resource for RpJobs {}

/// Кэш паков клиента: хеши уже скачанных паков. Живёт на диске (`rp-cache/<ник>.txt`), как папка `downloads`
/// у игрока: после перезахода/рестарта воркера пак повторно не качается.
fn cache() -> &'static Mutex<HashSet<String>> {
    static C: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    C.get_or_init(|| {
        let set = std::fs::read_to_string(cache_file())
            .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        Mutex::new(set)
    })
}

fn cache_file() -> std::path::PathBuf {
    std::path::Path::new("rp-cache").join(format!("{}.txt", crate::boot::global().boot.username))
}

fn remember_pack(hash: String) {
    let mut c = cache().lock();
    if c.insert(hash) {
        let mut lines: Vec<&String> = c.iter().collect();
        lines.sort();
        let body: String = lines.iter().map(|h| format!("{h}\n")).collect();
        let _ = std::fs::create_dir_all("rp-cache");
        if let Err(e) = std::fs::write(cache_file(), body) {
            warn!("ресурспак: не записал кэш: {e}");
        }
    }
}

fn take_requests(mut events: MessageReader<ResourcePackEvent>, mut jobs: ResMut<RpJobs>) {
    for ev in events.read() {
        info!(
            "⚑ сервер: ресурспак id={} required={} prompt={} hash={} url={}",
            ev.id,
            ev.required,
            ev.prompt.as_ref().map(|p| p.to_string()).unwrap_or_default(),
            ev.hash,
            ev.url
        );
        if jobs.0.iter().any(|j| j.id == ev.id && j.entity == ev.entity) {
            continue;
        }
        let accept_ms = rand::thread_rng().gen_range(1000..=3500);
        let accept_at = Instant::now() + Duration::from_millis(accept_ms);
        let dl = Arc::new(Mutex::new(DlState::default()));
        start_download(ev.url.clone(), ev.hash.to_lowercase(), accept_at, dl.clone());
        jobs.0.push(Job { entity: ev.entity, id: ev.id, accept_at, accepted: false, downloaded: false, loaded_at: None, dl });
    }
}

fn drive_jobs(mut jobs: ResMut<RpJobs>, mut commands: Commands, phase: Query<Option<&InConfigState>>) {
    let now = Instant::now();
    jobs.0.retain_mut(|j| {
        let Ok(cfg) = phase.get(j.entity) else {
            return false; // клиент отключился
        };
        let in_config = cfg.is_some();
        if !j.accepted {
            if now < j.accept_at {
                return true;
            }
            send(&mut commands, j.entity, in_config, j.id, Step::Accepted);
            j.accepted = true;
        }
        if !j.downloaded {
            let Some(done) = j.dl.lock().done_at else {
                return true;
            };
            if now < done {
                return true;
            }
            send(&mut commands, j.entity, in_config, j.id, Step::Downloaded);
            j.downloaded = true;
            j.loaded_at = Some(now + Duration::from_millis(rand::thread_rng().gen_range(1500..=4000)));
        }
        if j.loaded_at.is_some_and(|t| now >= t) {
            send(&mut commands, j.entity, in_config, j.id, Step::Loaded);
            info!("ресурспак {} → принял / скачан / загружен", j.id);
            return false;
        }
        true
    });
}

fn send(commands: &mut Commands, entity: Entity, in_config: bool, id: Uuid, step: Step) {
    // В `Action` azalea нет DOWNLOADED; на проводе это код 4, который в azalea назван InvalidUrl.
    if in_config {
        use config::s_resource_pack::{Action, ServerboundResourcePack};
        let action = match step {
            Step::Accepted => Action::Accepted,
            Step::Downloaded => Action::InvalidUrl,
            Step::Loaded => Action::SuccessfullyLoaded,
        };
        commands.trigger(SendConfigPacketEvent::new(entity, ServerboundResourcePack { id, action }));
    } else {
        use game::s_resource_pack::{Action, ServerboundResourcePack};
        let action = match step {
            Step::Accepted => Action::Accepted,
            Step::Downloaded => Action::InvalidUrl,
            Step::Loaded => Action::SuccessfullyLoaded,
        };
        commands.trigger(SendGamePacketEvent::new(entity, ServerboundResourcePack { id, action }));
    }
}

/// Скачивание после «принял»: через прокси бота, с заголовками ванильного загрузчика.
fn start_download(url: String, hash: String, accept_at: Instant, dl: Arc<Mutex<DlState>>) {
    std::thread::spawn(move || {
        let wait = accept_at.saturating_duration_since(Instant::now());
        std::thread::sleep(wait);
        if !hash.is_empty() && cache().lock().contains(&hash) {
            // пак уже в кэше клиента: проверка файла на диске, без сети
            info!("ресурспак: уже в кэше, не качаю");
            let ms = rand::thread_rng().gen_range(500..=1200);
            dl.lock().done_at = Some(Instant::now() + Duration::from_millis(ms));
            return;
        }
        let started = Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("rpack rt");
        match rt.block_on(download(&url)) {
            Ok(bytes) => {
                let got = sha1_hex(&bytes);
                if !hash.is_empty() && got != hash {
                    warn!("ресурспак: хеш не совпал (ждали {hash}, получили {got}), {} байт", bytes.len());
                } else {
                    info!("ресурспак: скачан {} байт за {:.1} с", bytes.len(), started.elapsed().as_secs_f32());
                }
                // в кэш — только целый пак (как у клиента: битый файл он не сохраняет)
                if hash.is_empty() || got == hash {
                    remember_pack(if hash.is_empty() { got } else { hash });
                }
            }
            Err(e) => warn!("ресурспак: не скачал {url}: {e:#}"),
        }
        // Скачивание не бывает мгновенным: даже кэш или быстрая ошибка занимают не меньше полсекунды
        // между «принял» и «скачан».
        let floor = Duration::from_millis(rand::thread_rng().gen_range(500..=1500));
        dl.lock().done_at = Some(Instant::now().max(started + floor));
    });
}

async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    let g = crate::boot::global();
    let boot = &g.boot;
    let mut b = reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT);
    if !boot.proxy.trim().is_empty() {
        b = b.proxy(reqwest::Proxy::all(socks5h(&boot.proxy))?);
    }
    let client = b.build()?;
    let uuid = azalea::Account::offline(&boot.username).uuid_or_offline().simple().to_string();
    let resp = client
        .get(url)
        .header("User-Agent", format!("Minecraft Java/{MC_VERSION}"))
        .header("X-Minecraft-Username", boot.username.as_str())
        .header("X-Minecraft-UUID", uuid)
        .header("X-Minecraft-Version", MC_VERSION)
        .header("X-Minecraft-Version-ID", MC_VERSION)
        .header("X-Minecraft-Pack-Format", PACK_FORMAT)
        .send()
        .await?
        .error_for_status()?;
    if resp.content_length().is_some_and(|n| n > MAX_PACK_BYTES) {
        anyhow::bail!("пак больше 250 МиБ");
    }
    let bytes = resp.bytes().await?;
    Ok(bytes.to_vec())
}

/// Журнал служебных запросов сервера в игре: всё, чем сервер может проверить клиента и на что ванильный
/// клиент реагирует (диалоги, таблички, cookies, плагин-каналы, книги). Плагин-канал — один раз на имя.
pub fn log_server_probe(p: &azalea::protocol::packets::game::ClientboundGamePacket) {
    use azalea::protocol::packets::game::ClientboundGamePacket as P;
    static CHANNELS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let line = match p {
        P::CustomPayload(x) => {
            let id = x.identifier.to_string();
            if !CHANNELS.get_or_init(Default::default).lock().insert(id.clone()) {
                return;
            }
            format!("плагин-канал {id}")
        }
        P::ShowDialog(x) => format!("диалог {}", azalea::short_debug(x)),
        P::ClearDialog(_) => "закрыть диалог".to_string(),
        P::OpenSignEditor(x) => format!("редактор таблички {}", azalea::short_debug(x)),
        P::CookieRequest(x) => format!("запрос cookie {}", x.key),
        P::StoreCookie(x) => format!("сохранить cookie {}", x.key),
        P::OpenBook(x) => format!("открыть книгу {}", azalea::short_debug(x)),
        P::ResourcePackPop(x) => format!("снять ресурспак {}", azalea::short_debug(x)),
        P::Transfer(x) => format!("transfer {}", azalea::short_debug(x)),
        P::CustomReportDetails(_) => "custom report details".to_string(),
        P::ServerLinks(_) => "ссылки сервера".to_string(),
        _ => return,
    };
    info!("⚑ сервер: {line}");
}

/// `socks5://` → `socks5h://`: DNS имени хоста пака резолвит прокси, а не наш сервер.
fn socks5h(proxy: &str) -> String {
    let p = proxy.trim();
    if let Some(rest) = p.strip_prefix("socks5://") {
        format!("socks5h://{rest}")
    } else if p.contains("://") {
        p.to_string()
    } else {
        format!("socks5h://{p}")
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_goes_through_remote_dns() {
        assert_eq!(socks5h("socks5://u:p@1.2.3.4:8000"), "socks5h://u:p@1.2.3.4:8000");
        assert_eq!(socks5h("1.2.3.4:8000"), "socks5h://1.2.3.4:8000");
        assert_eq!(socks5h("http://h:1"), "http://h:1");
    }

    #[test]
    fn downloaded_is_wire_code_4() {
        // ванильный DOWNLOADED = 4; в azalea этот код называется InvalidUrl
        assert_eq!(game::s_resource_pack::Action::InvalidUrl as u8, 4);
        assert_eq!(config::s_resource_pack::Action::InvalidUrl as u8, 4);
        assert_eq!(game::s_resource_pack::Action::Accepted as u8, 3);
        assert_eq!(game::s_resource_pack::Action::SuccessfullyLoaded as u8, 0);
    }

    #[test]
    fn sha1_matches_known_value() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
