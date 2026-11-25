//! Imported configs: `<data>/configs/<id>.conf` (owner-only) + `prism.json`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub name: String,
    /// Server host as written in the config (shown under the name).
    pub host: String,
    /// Has its own AmneziaWG junk settings (otherwise Prism's profile is used).
    #[serde(default)]
    pub custom: bool,
    /// ISO country code of the server ("fi"), looked up once from its IP.
    #[serde(default)]
    pub country: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub items: Vec<Item>,
    pub selected: Option<String>,
    /// Appearance: "system", "light" or "dark".
    #[serde(default = "system")]
    pub theme: String,
    /// Smart routing: Russian sites go direct, the rest through the tunnel.
    #[serde(default = "yes")]
    pub smart: bool,
}

fn yes() -> bool {
    true
}

fn system() -> String {
    "system".into()
}

impl Default for Index {
    fn default() -> Self {
        Self { items: Vec::new(), selected: None, theme: system(), smart: true }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details {
    pub public_key: String,
    pub addresses: String,
    pub dns: String,
    pub mtu: u16,
    pub peer_key: String,
    pub preshared: bool,
    pub endpoint: String,
    pub allowed_ips: String,
    pub keepalive: Option<u16>,
}

pub struct Store {
    dir: PathBuf,
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Store {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(dir.join("configs")).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("prism.json")
    }

    fn conf_path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("bad config id");
        }
        Ok(self.dir.join("configs").join(format!("{id}.conf")))
    }

    pub fn index(&self) -> Index {
        std::fs::read(self.index_path())
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default()
    }

    fn save(&self, index: &Index) -> Result<()> {
        write_private(&self.index_path(), &serde_json::to_vec_pretty(index)?)
    }

    /// Validates and stores a config. Re-importing a file with the same name
    /// replaces the old one.
    pub fn import(&self, text: &str, file_name: Option<&str>) -> Result<Item> {
        let cfg = prism_engine::parse(text)?;
        let name = prism_engine::suggest_name(&cfg, file_name);
        let custom = text.lines().any(|l| l.trim_start().to_ascii_lowercase().starts_with("jc"));
        let mut index = self.index();
        let id = match index.items.iter().find(|i| i.name == name) {
            Some(existing) => existing.id.clone(),
            None => format!("{:016x}", rand::random::<u64>()),
        };
        write_private(&self.conf_path(&id)?, text.as_bytes())?;
        let item = Item { id: id.clone(), name, host: cfg.endpoint_host().to_string(), custom, country: None };
        match index.items.iter_mut().find(|i| i.id == id) {
            Some(slot) => *slot = item.clone(),
            None => index.items.push(item.clone()),
        }
        index.selected = Some(id);
        self.save(&index)?;
        Ok(item)
    }

    pub fn remove(&self, id: &str) -> Result<Index> {
        let mut index = self.index();
        index.items.retain(|i| i.id != id);
        let _ = std::fs::remove_file(self.conf_path(id)?);
        if index.selected.as_deref() == Some(id) {
            index.selected = index.items.first().map(|i| i.id.clone());
        }
        self.save(&index)?;
        Ok(index)
    }

    pub fn select(&self, id: &str) -> Result<Index> {
        let mut index = self.index();
        if !index.items.iter().any(|i| i.id == id) {
            bail!("this server no longer exists");
        }
        index.selected = Some(id.to_string());
        self.save(&index)?;
        Ok(index)
    }

    pub fn set_theme(&self, theme: &str) -> Result<Index> {
        if !matches!(theme, "system" | "light" | "dark") {
            bail!("unknown theme");
        }
        let mut index = self.index();
        index.theme = theme.to_string();
        self.save(&index)?;
        Ok(index)
    }

    /// What the details sheet shows. Never includes the private key.
    pub fn details(&self, id: &str) -> Result<Details> {
        let cfg = prism_engine::parse(&self.read(id)?)?;
        let join = |v: Vec<String>| v.join(", ");
        let b64 = |k: &[u8; 32]| base64::engine::general_purpose::STANDARD.encode(k);
        let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(cfg.private_key));
        Ok(Details {
            public_key: b64(public.as_bytes()),
            addresses: join(cfg.addresses.iter().map(ToString::to_string).collect()),
            dns: join(cfg.dns.iter().map(ToString::to_string).collect()),
            mtu: cfg.mtu.unwrap_or(1420),
            peer_key: b64(&cfg.peer.public_key),
            preshared: cfg.peer.preshared_key.is_some(),
            endpoint: cfg.peer.endpoint.clone(),
            allowed_ips: join(cfg.peer.allowed_ips.iter().map(ToString::to_string).collect()),
            keepalive: cfg.peer.keepalive,
        })
    }

    pub fn set_country(&self, id: &str, country: &str) -> Result<Index> {
        let cc = country.to_ascii_lowercase();
        if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_lowercase()) {
            bail!("bad country code");
        }
        let mut index = self.index();
        let item = index.items.iter_mut().find(|i| i.id == id).context("this server no longer exists")?;
        item.country = Some(cc);
        self.save(&index)?;
        Ok(index)
    }

    /// The server's address as the tunnel would dial it (IPv4 first).
    pub async fn server_ip(&self, id: &str) -> Result<std::net::IpAddr> {
        let cfg = prism_engine::parse(&self.read(id)?)?;
        let mut addrs: Vec<_> = tokio::net::lookup_host(cfg.peer.endpoint.as_str()).await?.map(|a| a.ip()).collect();
        addrs.sort_by_key(|a| !a.is_ipv4());
        addrs.into_iter().next().context("the server has no IP address")
    }

    pub fn set_smart(&self, on: bool) -> Result<Index> {
        let mut index = self.index();
        index.smart = on;
        self.save(&index)?;
        Ok(index)
    }

    pub fn read(&self, id: &str) -> Result<String> {
        std::fs::read_to_string(self.conf_path(id)?).context("couldn't read this server's config")
    }
}
