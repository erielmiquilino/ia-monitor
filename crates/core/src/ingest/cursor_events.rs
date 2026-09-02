//! Histórico do Cursor. Diferente dos outros dois, não existe log local com
//! consumo (os registros de conversa no `state.vscdb` têm `tokenCount`
//! zerado). A API é o único caminho.
//!
//! Este endpoint usa auth por **cookie + header `Origin`**, não o Bearer do
//! connect-rpc — sem o `Origin` a resposta é 403.
//!
//! O projeto não vem na resposta: é reconstruído casando o `conversationId`
//! com o `workspaceIdentifier` guardado no `state.vscdb`.

use super::{IngestStats, ProjectResolver};
use crate::collect::cursor::CursorCollector;
use crate::model::Provider;
use crate::store::{Store, UsageEvent};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chrono::{Duration, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::collections::HashMap;
use zeroize::Zeroizing;

const EVENTS_URL: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const ME_URL: &str = "https://cursor.com/api/auth/me";
const PAGE_SIZE: u32 = 200;
/// Teto de segurança: o dashboard não guarda histórico ilimitado e não
/// queremos varrer o passado inteiro a cada inicialização.
const MAX_BACKFILL_DAYS: i64 = 120;

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(rename = "inputTokens", default)]
    input_tokens: i64,
    #[serde(rename = "outputTokens", default)]
    output_tokens: i64,
    #[serde(rename = "cacheReadTokens", default)]
    cache_read_tokens: i64,
}

#[derive(Deserialize)]
struct UsageEventRow {
    /// Epoch em milissegundos, como string.
    timestamp: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(rename = "tokenUsage", default)]
    token_usage: Option<TokenUsage>,
    #[serde(rename = "chargedCents", default)]
    charged_cents: Option<f64>,
    #[serde(rename = "conversationId", default)]
    conversation_id: Option<String>,
}

#[derive(Deserialize)]
struct EventsResponse {
    #[serde(rename = "usageEventsDisplay", default)]
    events: Vec<UsageEventRow>,
    #[serde(rename = "totalUsageEventsCount", default)]
    total: i64,
}

#[derive(Deserialize)]
struct Me {
    /// Id numérico interno, diferente do `sub` do WorkOS.
    id: i64,
}

#[derive(Deserialize)]
struct CachedTeam {
    #[serde(rename = "teamId")]
    team_id: i64,
}

/// Monta o cookie `WorkosCursorSessionToken=<sub>::<jwt>`. O `sub` sai do
/// payload do próprio JWT — não há outra fonte local para ele.
fn session_cookie() -> Result<Zeroizing<String>> {
    let token = CursorCollector::access_token()?;
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("token do Cursor não é um JWT"))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("decodificando payload do JWT")?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded)?;
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("JWT do Cursor sem `sub`"))?;

    Ok(Zeroizing::new(format!(
        "WorkosCursorSessionToken={}%3A%3A{}",
        urlencoding::encode(sub),
        token.as_str()
    )))
}

fn team_id() -> Result<i64> {
    let path = CursorCollector::state_db_path()?;
    let uri = format!("file:///{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let raw: String = conn.query_row(
        "SELECT value FROM ItemTable WHERE key = 'cursorAuth/cachedTeam'",
        [],
        |r| r.get(0),
    )?;
    let team: CachedTeam = serde_json::from_str(&raw)?;
    Ok(team.team_id)
}

/// conversationId -> caminho do workspace, lido do banco local do Cursor.
/// Um miss aqui não é erro: conversas em janela sem pasta existem.
pub fn workspace_map() -> Result<HashMap<String, String>> {
    let path = CursorCollector::state_db_path()?;
    let uri = format!("file:///{}?mode=ro", path.to_string_lossy().replace('\\', "/"));
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    let mut stmt = conn.prepare(
        "SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;

    let mut map = HashMap::new();
    for row in rows.flatten() {
        let (key, value) = row;
        let composer_id = match key.split_once(':') {
            Some((_, id)) => id.to_string(),
            None => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&value) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(fs_path) = parsed
            .get("workspaceIdentifier")
            .and_then(|w| w.get("uri"))
            .and_then(|u| u.get("fsPath"))
            .and_then(|p| p.as_str())
        {
            map.insert(composer_id, fs_path.to_string());
        }
    }
    Ok(map)
}

pub async fn ingest(store: &Store, client: &reqwest::Client) -> Result<IngestStats> {
    let cookie = session_cookie()?;
    let team = team_id()?;

    let me: Me = client
        .get(ME_URL)
        .header("Cookie", cookie.as_str())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Continua de onde parou; na primeira vez busca o passado recente.
    let floor = Utc::now() - Duration::days(MAX_BACKFILL_DAYS);
    let since = store
        .latest_event_ts(Provider::Cursor)?
        .map(|t| t.max(floor))
        // Um segundo de folga evita perder eventos do mesmo instante.
        .map(|t| t - Duration::seconds(1))
        .unwrap_or(floor);
    let until = Utc::now();

    let workspaces = workspace_map().unwrap_or_default();
    let mut projects = ProjectResolver::new();
    let mut stats = IngestStats::default();
    let mut page = 1u32;

    loop {
        let body = serde_json::json!({
            "teamId": team,
            "userId": me.id,
            "startDate": since.timestamp_millis().to_string(),
            "endDate": until.timestamp_millis().to_string(),
            "page": page,
            "pageSize": PAGE_SIZE,
        });

        let resp: EventsResponse = client
            .post(EVENTS_URL)
            .header("Cookie", cookie.as_str())
            // Sem este header a API responde 403.
            .header("Origin", "https://cursor.com")
            .header("Referer", "https://cursor.com/dashboard")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.events.is_empty() {
            break;
        }
        let received = resp.events.len();
        let events: Vec<UsageEvent> = resp
            .events
            .iter()
            .filter_map(|row| to_event(row, &workspaces, &mut projects))
            .collect();

        stats.events_inserted += store.insert_events(&events)?;
        stats.files_scanned += 1; // aqui "arquivo" é página da API

        if received < PAGE_SIZE as usize || (page as i64 * PAGE_SIZE as i64) >= resp.total {
            break;
        }
        page += 1;
    }

    Ok(stats)
}

fn to_event(
    row: &UsageEventRow,
    workspaces: &HashMap<String, String>,
    projects: &mut ProjectResolver,
) -> Option<UsageEvent> {
    let ms: i64 = row.timestamp.parse().ok()?;
    let ts = Utc.timestamp_millis_opt(ms).single()?;
    let usage = row.token_usage.as_ref();
    let model = row.model.clone().unwrap_or_else(|| "desconhecido".into());
    let conversation = row.conversation_id.clone();

    Some(UsageEvent {
        // Timestamp em ms + conversa + modelo: estável entre execuções, que
        // é o que torna o backfill idempotente.
        uid: format!(
            "cursor:{ms}:{}:{model}",
            conversation.as_deref().unwrap_or("-")
        ),
        ts,
        provider: Provider::Cursor,
        model: Some(model),
        project: conversation
            .as_ref()
            .and_then(|c| workspaces.get(c))
            .map(|p| projects.resolve(p)),
        session_id: conversation,
        input_tokens: usage.map(|u| u.input_tokens).unwrap_or(0),
        output_tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
        cache_tokens: usage.map(|u| u.cache_read_tokens).unwrap_or(0),
        cents: row.charged_cents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evento real de `get-filtered-usage-events`.
    const ROW: &str = r#"{"timestamp":"1788266388160","model":"cursor-grok-4.6-high-fast",
      "kind":"USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS","requestsCosts":91.5,
      "tokenUsage":{"inputTokens":191334,"outputTokens":23945,
                    "cacheReadTokens":2607424,"totalCents":366.01},
      "chargedCents":366.010009765625,"conversationId":"11e3a491","owningTeam":"000000"}"#;

    fn row() -> UsageEventRow {
        serde_json::from_str(ROW).unwrap()
    }

    #[test]
    fn converte_evento_da_api() {
        let e = to_event(&row(), &HashMap::new(), &mut ProjectResolver::new()).unwrap();
        assert_eq!(e.input_tokens, 191_334);
        assert_eq!(e.cache_tokens, 2_607_424);
        assert_eq!(e.cents, Some(366.010009765625));
        assert_eq!(e.session_id.as_deref(), Some("11e3a491"));
        assert_eq!(e.ts.timestamp_millis(), 1_788_266_388_160);
    }

    /// O projeto é reconstruído localmente; a API não o informa. O caminho
    /// sai normalizado para casar com o que o Claude e o Codex gravam — o
    /// Cursor guarda a letra do drive em minúscula.
    #[test]
    fn casa_conversa_com_workspace_local() {
        let mut ws = HashMap::new();
        ws.insert("11e3a491".to_string(), r"e:\workspaces\InvoiceCore".to_string());
        let e = to_event(&row(), &ws, &mut ProjectResolver::new()).unwrap();
        assert_eq!(e.project.as_deref(), Some(r"E:\workspaces\InvoiceCore"));
    }

    /// Conversa sem pasta (janela vazia) não pode virar erro nem projeto falso.
    #[test]
    fn conversa_sem_workspace_fica_sem_projeto() {
        let e = to_event(&row(), &HashMap::new(), &mut ProjectResolver::new()).unwrap();
        assert!(e.project.is_none());
    }

    #[test]
    fn uid_e_estavel_entre_execucoes() {
        let a = to_event(&row(), &HashMap::new(), &mut ProjectResolver::new()).unwrap();
        let b = to_event(&row(), &HashMap::new(), &mut ProjectResolver::new()).unwrap();
        assert_eq!(a.uid, b.uid);
    }
}
