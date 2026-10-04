//! Конфиг орка: `orch.json` рядом с репо / cwd.
//! Боты/прокси/овнер: `farm_root/bots/{an}b.json`, `ip.json`, `clan-owners.json`, `owner-ip.json`.

use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Clone, Deserialize)]
pub struct OrchConfig {
    /// Номер анки (как `502b.mjs`). Можно переопределить: `./orch502 503`.
    #[serde(default = "default_anarchy")]
    pub anarchy: u16,
    /// Корень с `bots/`, `ip.json`, `clan-owners.json`, `owner-ip.json`.
    #[serde(default = "default_farm_root")]
    pub farm_root: String,
    #[serde(default = "default_go_ws")]
    pub go_ws: String,
    /// Локально: clan-setup без ожидания Go `run_clan_setup`.
    #[serde(default = "default_true")]
    pub local_mode: bool,
}

fn default_anarchy() -> u16 {
    502
}
fn default_farm_root() -> String {
    ".".into()
}
fn default_go_ws() -> String {
    "ws://127.0.0.1:8080/ws".into()
}
fn default_true() -> bool {
    true
}

impl Default for OrchConfig {
    fn default() -> Self {
        Self {
            anarchy: default_anarchy(),
            farm_root: default_farm_root(),
            go_ws: default_go_ws(),
            local_mode: true,
        }
    }
}

pub struct LoadedOrch {
    pub cfg: OrchConfig,
    /// Абсолютный путь к farm_root.
    pub farm_root: PathBuf,
}

/// Ищем orch.json: cwd → рядом с бинарём → корень 4narek-azalea (рядом с crates/).
pub fn load() -> Result<LoadedOrch> {
    let mut cfg = OrchConfig::default();
    let mut config_dir = None;

    for cand in candidate_config_paths() {
        if cand.is_file() {
            let raw = std::fs::read_to_string(&cand)
                .with_context(|| format!("читать {}", cand.display()))?;
            cfg = serde_json::from_str(&raw)
                .with_context(|| format!("парсить {}", cand.display()))?;
            config_dir = cand.parent().map(|p| p.to_path_buf());
            info!("orch.json ← {}", cand.display());
            break;
        }
    }

    // CLI: ./orch502 503
    if let Some(arg) = env::args().nth(1) {
        if let Ok(an) = arg.parse::<u16>() {
            cfg.anarchy = an;
        } else if arg == "-h" || arg == "--help" {
            eprintln!(
                "usage: orch502 [anarchy]\n  конфиг: orch.json (anarchy, farm_root, go_ws, local_mode)\n  боты: {{farm_root}}/bots/{{an}}b.json + ip.json + clan-owners.json"
            );
            std::process::exit(0);
        }
    } else if config_dir.is_none() {
        // нет orch.json и нет argv — номер из имени бинаря orch502
        if let Some(an) = anarchy_from_bin_name() {
            cfg.anarchy = an;
        }
    }

    let farm_root = resolve_farm_root(&cfg.farm_root, config_dir.as_deref())?;
    let bots = farm_root.join(format!("bots/{}b.json", cfg.anarchy));
    if !bots.is_file() {
        bail!(
            "нет {} — поправь farm_root/anarchy в orch.json",
            bots.display()
        );
    }
    if !farm_root.join("ip.json").is_file() {
        bail!("нет {}/ip.json", farm_root.display());
    }

    if config_dir.is_none() {
        info!(
            "orch.json не найден — дефолты (an{}, farm={})",
            cfg.anarchy, cfg.farm_root
        );
    }
    info!(
        "анка {} · farm={} · go={} · local_mode={}",
        cfg.anarchy,
        farm_root.display(),
        cfg.go_ws,
        cfg.local_mode
    );

    Ok(LoadedOrch { cfg, farm_root })
}

fn candidate_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        out.push(cwd.join("orch.json"));
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("orch.json"));
            // target/debug → workspace
            if let Some(ws) = dir.parent().and_then(|p| p.parent()) {
                out.push(ws.join("orch.json"));
            }
        }
    }
    // crates/orch → 4narek-azalea
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    out.push(p.join("orch.json"));
    out
}

fn anarchy_from_bin_name() -> Option<u16> {
    let name = env::current_exe().ok()?;
    let stem = name.file_stem()?.to_str()?;
    // orch502 / bot502
    let digits: String = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

fn resolve_farm_root(raw: &str, config_dir: Option<&Path>) -> Result<PathBuf> {
    let p = PathBuf::from(raw);
    let abs = if p.is_absolute() {
        p
    } else if let Some(dir) = config_dir {
        dir.join(&p)
    } else {
        // относительно cwd, иначе относительно 4narek-azalea
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let via_cwd = cwd.join(&p);
        if via_cwd.join("bots").is_dir() {
            via_cwd
        } else {
            let mut ws = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            ws.pop();
            ws.pop();
            ws.join(&p)
        }
    };
    let canon = abs.canonicalize().unwrap_or(abs);
    Ok(canon)
}

pub fn bot_bin_path() -> Result<PathBuf> {
    // рядом с orch502 → bot502; иначе target/debug
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("bot502");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    let debug = p.join("target/debug/bot502");
    let release = p.join("target/release/bot502");
    if debug.exists() {
        Ok(debug)
    } else if release.exists() {
        Ok(release)
    } else {
        Ok(debug)
    }
}
