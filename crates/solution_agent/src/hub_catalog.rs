//! The hub owns harness discovery, branding and provider-specific capabilities.
use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, AsyncApp, Task};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Request};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ui::IconName;

use crate::adapter::{AdapterRegistry, AgentBrand, HubAdapter};
use crate::store::SolutionAgentStore;

#[derive(Clone, Serialize, Deserialize)]
struct Catalog {
    version: u32,
    default_harness: String,
    harnesses: Vec<Harness>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Harness {
    id: String,
    name: String,
    vendor: String,
    models: String,
    color: u32,
    icon: String,
    icon_sha256: String,
    instruction_file: String,
    resume: bool,
    #[serde(default)]
    permissions: bool,
    #[serde(default)]
    context_usage_current: bool,
    #[serde(default)]
    effort_options: Vec<String>,
    #[serde(default)]
    legacy_actions: Vec<String>,
    #[serde(default)]
    local_icon: Option<PathBuf>,
}

fn endpoint() -> String {
    std::env::var("SPK_AI_HUB_URL").unwrap_or_else(|_| "http://127.0.0.1:5195".into())
}
pub(crate) fn validate_endpoint(value: &str) -> Result<String> {
    let invalid = "SPK AI Hub requires a root HTTP URL on a numeric loopback address with a valid port";
    let authority = value.strip_prefix("http://").context(invalid)?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').context(invalid)?;
        let port = if suffix.is_empty() { None } else { Some(suffix.strip_prefix(':').context(invalid)?) };
        (host, port)
    } else {
        match authority.split_once(':') { Some((host,port)) => (host,Some(port)), None => (authority,None) }
    };
    ensure!(host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()), "{invalid}");
    if let Some(port) = port {
        ensure!(!port.is_empty() && port.bytes().all(|b|b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p|p>0), "{invalid}");
    }
    Ok(format!("http://{authority}"))
}

static STATUS: std::sync::LazyLock<std::sync::RwLock<Option<String>>> = std::sync::LazyLock::new(||std::sync::RwLock::new(Some("Connecting to SPK AI Hub…".into())));
pub(crate) fn status() -> Option<String> { STATUS.read().unwrap_or_else(|e|e.into_inner()).clone() }
fn set_status(value: Option<String>, cx: &mut App) {
    let mut status = STATUS.write().unwrap_or_else(|e|e.into_inner());
    if *status != value { *status = value; drop(status); cx.refresh_windows(); }
}

fn cache() -> PathBuf {
    paths::base_dir().join("cache").join("ai-hub")
}

async fn get(client: &Arc<dyn HttpClient>, url: String, limit: u64) -> Result<Vec<u8>> {
    let request = Request::builder()
        .uri(url)
        .method("GET")
        .follow_redirects(http_client::RedirectPolicy::NoFollow)
        .timeout(Duration::from_secs(5))
        .body(AsyncBody::empty())?;
    let mut response = client
        .send(request)
        .await
        .context("Could not connect to SPK AI Hub")?;
    ensure!(
        response.status().is_success(),
        "SPK AI Hub rejected catalog request"
    );
    let mut data = Vec::new();
    response
        .body_mut()
        .take(limit + 1)
        .read_to_end(&mut data)
        .await?;
    ensure!(
        data.len() as u64 <= limit,
        "SPK AI Hub response exceeds limit"
    );
    Ok(data)
}
async fn fetch() -> Result<Catalog> {
    let base = validate_endpoint(&endpoint())?;
    let direct = reqwest::Client::builder()
        .no_proxy()
        .redirect_policy(reqwest::redirect::Policy::none())
        .build()?;
    let client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::from(direct));
    let mut catalog: Catalog = serde_json::from_slice(
        &get(
            &client,
            format!("{}/v1/agents/catalog", base.trim_end_matches('/')),
            1 << 20,
        )
        .await?,
    )?;
    ensure!(
        catalog.version == 1 && catalog.harnesses.len() <= 128,
        "Unsupported hub catalog"
    );
    let root = cache();
    std::fs::create_dir_all(&root)?;
    let mut ids = std::collections::HashSet::new();
    for h in &mut catalog.harnesses {
        ensure!(
            !h.id.is_empty() && ids.insert(h.id.clone()),
            "Invalid or duplicate hub harness ID"
        );
        ensure!(
            h.icon == format!("/v1/agents/icons/{}.svg", h.id)
                && h.icon_sha256.len() == 64
                && h.icon_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid hub icon descriptor"
        );
        let path = root.join(format!("{}.svg", h.icon_sha256));
        let valid = std::fs::read(&path)
            .is_ok_and(|b| format!("{:x}", Sha256::digest(&b)) == h.icon_sha256);
        if !valid {
            let icon = get(
                &client,
                format!("{}{}", base.trim_end_matches('/'), h.icon),
                128 << 10,
            )
            .await?;
            ensure!(
                format!("{:x}", Sha256::digest(&icon)) == h.icon_sha256,
                "Hub icon checksum mismatch"
            );
            std::fs::write(&path, icon)?;
        }
        h.local_icon = Some(path);
    }
    std::fs::write(root.join("catalog.json"), serde_json::to_vec(&catalog)?)?;
    Ok(catalog)
}
fn apply(catalog: Catalog, cx: &mut App) {
    let mut registry = AdapterRegistry::new();
    let mut brands = Vec::new();
    let key = paths::config_dir().join("ai-hub-client.key");
    SolutionAgentStore::global(cx).update(cx, |store, cx| {
        for h in catalog.harnesses {
            let brand = Arc::new(AgentBrand {
                name: h.name,
                vendor: h.vendor,
                models: h.models,
                permissions: h.permissions,
                context_usage_current: h.context_usage_current,
                effort_options: h.effort_options,
                color: h.color,
                logo: IconName::Sparkle,
                icon_path: h
                    .local_icon
                    .map(|p| p.to_string_lossy().into_owned().into()),
            });
            registry.register(Arc::new(HubAdapter {
                id: h.id.clone().into(),
                brand: brand.clone(),
                instruction_file: h.instruction_file,
                resume: h.resume,
            }));
            store.register_agent_server(
                h.id.clone().into(),
                std::rc::Rc::new(agent_servers::HubAgentServer::new(
                    project::AgentId(h.id.clone().into()),
                    h.id.clone(),
                    key.clone(),
                )),
            );
            brands.push((h.id, brand, h.legacy_actions));
        }
        crate::adapter::install_brands(brands, catalog.default_harness);
        store.adapters = Arc::new(registry);
        cx.notify();
    });
    cx.refresh_windows();
}
pub fn init(cx: &mut App) {
    if let Ok(data) = std::fs::read(cache().join("catalog.json"))
        && let Ok(catalog) = serde_json::from_slice::<Catalog>(&data)
    {
        if catalog.version == 1 {
            apply(catalog, cx)
        }
    }
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            let task: Task<Result<Catalog>> = cx.background_spawn(fetch());
            match task.await {
                Ok(catalog) => {
                    cx.update(|cx| { apply(catalog, cx); set_status(None,cx); });
                }
                Err(error) => {
                    log::debug!("AI Hub catalog unavailable: {error:#}");
                    let detail: String = error.chain().last().unwrap_or(error.as_ref()).to_string().chars().take(96).collect();
                    cx.update(|cx| set_status(Some(format!("AI Hub unavailable: {detail}")),cx));
                }
            }
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::validate_endpoint;
    #[test]
    fn hub_endpoint_requires_a_numeric_loopback_root() {
        for valid in ["http://127.0.0.1:5195", "http://127.0.0.2/", "http://[::1]:5195/"] {
            assert!(validate_endpoint(valid).is_ok(), "{valid}");
        }
        for invalid in ["https://127.0.0.1", "http://localhost", "http://127.1", "http://2130706433", "http://10.0.0.1", "http://127.0.0.1:0", "http://127.0.0.1:65536", "http://127.0.0.1:", "http://secret@127.0.0.1", "http://127.0.0.1/path", "http://127.0.0.1?secret=x", "http://127.0.0.1?", "http://127.0.0.1#secret"] {
            let error = validate_endpoint(invalid).unwrap_err();
            assert!(!format!("{error:#}").contains("secret"));
        }
    }
}
