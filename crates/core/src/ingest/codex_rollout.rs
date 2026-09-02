//! Histórico do Codex: `~/.codex/sessions/AAAA/MM/DD/rollout-*.jsonl`.
//!
//! Diferente do Claude, o consumo não vem junto do contexto: `cwd` e modelo
//! chegam em linhas de cabeçalho (`session_meta`, `turn_context`) e valem
//! para os `token_count` seguintes. Como a leitura é incremental, esse
//! contexto precisa sobreviver entre passadas — fica persistido no `config`.

use super::{read_new_lines, walk_jsonl, IngestStats, ProjectResolver};
use crate::collect::home_dir;
use crate::model::Provider;
use crate::store::{Store, UsageEvent};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    ordinal: Option<i64>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    info: Option<Info>,
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    last_token_usage: Option<TokenUsage>,
}

/// Atenção: aqui `input_tokens` é o TOTAL de entrada e já inclui
/// `cached_input_tokens`. Sem separar, o cache seria contado duas vezes.
#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

/// Contexto da sessão que persiste entre leituras incrementais.
#[derive(Default, Serialize, Deserialize, Clone)]
struct SessionContext {
    session_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
}

pub fn sessions_dir() -> Result<PathBuf> {
    let dir = home_dir()
        .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
        .join(".codex")
        .join("sessions");
    if !dir.exists() {
        return Err(anyhow!("histórico do Codex não encontrado ({})", dir.display()));
    }
    Ok(dir)
}

fn context_key(path: &str) -> String {
    format!("codex.ctx:{path}")
}

/// Processa um lote de linhas mantendo o contexto corrente. Devolve os
/// eventos e o contexto final, para ser persistido.
fn scan(
    lines: &[String],
    mut ctx: SessionContext,
    projects: &mut ProjectResolver,
) -> (Vec<UsageEvent>, SessionContext) {
    let mut events = Vec::new();

    for raw in lines {
        let line: Line = match serde_json::from_str(raw) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let payload = match &line.payload {
            Some(p) => p,
            None => continue,
        };

        // Cabeçalhos atualizam o contexto e não geram consumo.
        let outer = line.kind.as_deref();
        let inner = payload.kind.as_deref();
        if outer == Some("session_meta") || inner == Some("session_meta") {
            if payload.session_id.is_some() {
                ctx.session_id = payload.session_id.clone();
            }
            if payload.cwd.is_some() {
                ctx.cwd = payload.cwd.clone();
            }
            continue;
        }
        if outer == Some("turn_context") || inner == Some("turn_context") {
            if payload.cwd.is_some() {
                ctx.cwd = payload.cwd.clone();
            }
            if payload.model.is_some() {
                ctx.model = payload.model.clone();
            }
            continue;
        }

        if inner != Some("token_count") {
            continue;
        }
        let usage = match payload.info.as_ref().and_then(|i| i.last_token_usage.as_ref()) {
            Some(u) => u,
            None => continue,
        };
        let ts: DateTime<Utc> = match line
            .timestamp
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            Some(d) => d.with_timezone(&Utc),
            None => continue,
        };

        // Turnos sem consumo novo aparecem no log; guardá-los só polui.
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            continue;
        }

        let session = ctx.session_id.clone().unwrap_or_else(|| "desconhecida".into());
        let ordinal = line.ordinal.unwrap_or_else(|| ts.timestamp_millis());

        events.push(UsageEvent {
            uid: format!("codex:{session}:{ordinal}"),
            ts,
            provider: Provider::Codex,
            model: ctx.model.clone(),
            project: ctx.cwd.as_deref().map(|c| projects.resolve(c)),
            session_id: ctx.session_id.clone(),
            // Deixa os contadores disjuntos, como no Claude.
            input_tokens: (usage.input_tokens - usage.cached_input_tokens).max(0),
            output_tokens: usage.output_tokens,
            cache_tokens: usage.cached_input_tokens,
            cents: None,
        });
    }

    (events, ctx)
}

pub fn ingest(store: &Store) -> Result<IngestStats> {
    let root = sessions_dir()?;
    let mut stats = IngestStats::default();
    let mut projects = ProjectResolver::new();

    for path in walk_jsonl(&root) {
        let key = path.to_string_lossy().to_string();
        let tail = match read_new_lines(store, &path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        stats.files_scanned += 1;
        if tail.lines.is_empty() {
            continue;
        }
        stats.bytes_read += tail.lines.iter().map(|l| l.len() as u64 + 1).sum::<u64>();

        // Numa leitura desde o início o cabeçalho vem junto; numa incremental
        // recuperamos o que foi visto antes.
        let ctx = if tail.from_start {
            SessionContext::default()
        } else {
            store
                .config_get(&context_key(&key))?
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default()
        };

        let (events, ctx) = scan(&tail.lines, ctx, &mut projects);
        stats.events_inserted += store.insert_events(&events)?;
        store.config_set(&context_key(&key), &serde_json::to_string(&ctx)?)?;
        store.set_tail_cursor(&tail.cursor)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const META: &str = r#"{"timestamp":"2026-09-01T13:44:59.571Z","ordinal":0,
      "type":"session_meta","payload":{"session_id":"01a05d36","id":"01a05d36",
      "cwd":"C:\\Users\\dev\\Documents\\Codex","originator":"Codex Desktop"}}"#;

    const TURN: &str = r#"{"timestamp":"2026-09-01T13:45:00.130Z","ordinal":5,
      "type":"turn_context","payload":{"turn_id":"t1","cwd":"E:\\InvoiceCore",
      "model":"gpt-5.6-luna","effort":"high"}}"#;

    const COUNT: &str = r#"{"timestamp":"2026-09-01T13:45:10.000Z","ordinal":6,
      "type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":
      {"input_tokens":29011,"cached_input_tokens":24320,"output_tokens":7544,
       "reasoning_output_tokens":6891,"total_tokens":36555}},
      "rate_limits":{"plan_type":"business"}}}"#;

    #[test]
    fn contexto_dos_cabecalhos_chega_ao_evento() {
        let (events, _) = scan(&lines(&[META, TURN, COUNT]), SessionContext::default(), &mut ProjectResolver::new());
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.session_id.as_deref(), Some("01a05d36"));
        // turn_context sobrescreve o cwd inicial da sessão.
        assert_eq!(e.project.as_deref(), Some(r"E:\InvoiceCore"));
        assert_eq!(e.model.as_deref(), Some("gpt-5.6-luna"));
    }

    /// `input_tokens` do Codex já inclui o cache. Somar os dois campos
    /// contaria o cache duas vezes.
    #[test]
    fn separa_cache_do_input_para_nao_contar_duas_vezes() {
        let (events, _) = scan(&lines(&[META, TURN, COUNT]), SessionContext::default(), &mut ProjectResolver::new());
        let e = &events[0];
        assert_eq!(e.input_tokens, 29011 - 24320);
        assert_eq!(e.cache_tokens, 24320);
        assert_eq!(e.output_tokens, 7544);
    }

    /// Leitura incremental não revê o cabeçalho: o contexto tem que vir do
    /// que foi persistido, senão o evento perde projeto e modelo.
    #[test]
    fn contexto_persistido_cobre_leitura_incremental() {
        let ctx = SessionContext {
            session_id: Some("s9".into()),
            cwd: Some(r"E:\Outro".into()),
            model: Some("gpt-5".into()),
        };
        let (events, _) = scan(&lines(&[COUNT]), ctx, &mut ProjectResolver::new());
        assert_eq!(events[0].project.as_deref(), Some(r"E:\Outro"));
        assert_eq!(events[0].session_id.as_deref(), Some("s9"));
    }

    #[test]
    fn turno_sem_consumo_nao_vira_evento() {
        let zero = r#"{"timestamp":"2026-09-01T13:45:10.000Z","ordinal":7,
          "payload":{"type":"token_count","info":{"last_token_usage":
          {"input_tokens":0,"cached_input_tokens":0,"output_tokens":0}}}}"#;
        let (events, _) = scan(&lines(&[META, zero]), SessionContext::default(), &mut ProjectResolver::new());
        assert!(events.is_empty());
    }

    /// O uid combina sessão e ordinal — reprocessar o arquivo é idempotente.
    #[test]
    fn uid_e_estavel_entre_execucoes() {
        let (a, _) = scan(&lines(&[META, TURN, COUNT]), SessionContext::default(), &mut ProjectResolver::new());
        let (b, _) = scan(&lines(&[META, TURN, COUNT]), SessionContext::default(), &mut ProjectResolver::new());
        assert_eq!(a[0].uid, b[0].uid);
        assert_eq!(a[0].uid, "codex:01a05d36:6");
    }
}
