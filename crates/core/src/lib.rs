//! Núcleo do IA Monitor: coleta e normalização do consumo de Claude, Cursor
//! e Codex. Sem UI e sem estado global — a camada Tauri só orquestra.

pub mod analytics;
pub mod collect;
pub mod ingest;
pub mod model;
pub mod store;

use collect::{claude::ClaudeCollector, codex::CodexCollector, cursor::CursorCollector, Collector};
use model::{Provider, ProviderSample};

/// Cliente HTTP compartilhado. Um só, para reaproveitar conexões TLS entre
/// os polls — abrir handshake novo a cada minuto seria desperdício.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("cliente HTTP")
}

/// Consulta um único provedor.
///
/// Cada provedor tem seu próprio ritmo e seu próprio recuo, então o laço
/// precisa poder consultar um sem arrastar os outros junto.
pub async fn sample_one(
    provider: Provider,
    client: &reqwest::Client,
    credit_baseline: Option<f64>,
) -> ProviderSample {
    match provider {
        Provider::Claude => ClaudeCollector::new(client.clone()).sample().await,
        Provider::Cursor => CursorCollector::new(client.clone()).sample().await,
        Provider::Codex => CodexCollector::new(credit_baseline).sample().await,
    }
}

/// Coleta os três provedores em paralelo. Nenhuma falha individual derruba
/// as demais — cada coletor devolve um `ProviderSample` com `error` populado.
pub async fn sample_all(credit_baseline: Option<f64>) -> Vec<ProviderSample> {
    let client = http_client();
    let claude = ClaudeCollector::new(client.clone());
    let cursor = CursorCollector::new(client);
    let codex = CodexCollector::new(credit_baseline);

    let (a, b, c) = tokio::join!(claude.sample(), cursor.sample(), codex.sample());
    vec![a, b, c]
}

/// Roda os três ingestores. O Cursor depende de rede; os outros dois leem
/// disco. Uma falha isolada não impede as demais fontes de avançarem.
pub async fn ingest_all(
    store: &store::Store,
    client: &reqwest::Client,
) -> (ingest::IngestStats, Vec<String>) {
    let mut stats = ingest::IngestStats::default();
    let mut errors = Vec::new();

    // Regras de interpretação mudaram? Reconstrói em vez de misturar
    // formatos antigos e novos no mesmo relatório.
    let stored = store
        .config_get("ingest.version")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u32>().ok());
    if stored != Some(ingest::INGEST_VERSION) {
        if let Err(e) = store.reset_derived() {
            errors.push(format!("reconstrução: {e}"));
        }
        let _ = store.config_set("ingest.version", &ingest::INGEST_VERSION.to_string());
    }

    match ingest::claude_jsonl::ingest(store) {
        Ok(s) => stats.merge(&s),
        Err(e) => errors.push(format!("Claude: {e}")),
    }
    match ingest::codex_rollout::ingest(store) {
        Ok(s) => stats.merge(&s),
        Err(e) => errors.push(format!("Codex: {e}")),
    }
    match ingest::cursor_events::ingest(store, client).await {
        Ok(s) => stats.merge(&s),
        Err(e) => errors.push(format!("Cursor: {e}")),
    }
    (stats, errors)
}
