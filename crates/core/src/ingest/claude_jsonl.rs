//! Histórico do Claude Code: `~/.claude/projects/**/*.jsonl`.
//!
//! Cada mensagem do assistente carrega `usage` completo e o `cwd` da sessão,
//! o que dá consumo por projeto sem depender de nenhuma API.

use super::{read_new_lines, walk_jsonl, IngestStats, ProjectResolver};
use crate::collect::home_dir;
use crate::model::Provider;
use crate::store::{Store, UsageEvent};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "requestId", default)]
    request_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Os quatro contadores são disjuntos — somá-los dá o total de entrada.
#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
}

pub fn projects_dir() -> Result<PathBuf> {
    let dir = home_dir()
        .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
        .join(".claude")
        .join("projects");
    if !dir.exists() {
        return Err(anyhow!("histórico do Claude não encontrado ({})", dir.display()));
    }
    Ok(dir)
}

/// Converte uma linha do JSONL num evento. `None` para tudo que não é
/// consumo (mensagens do usuário, resultados de ferramenta, etc).
fn parse_line(raw: &str, projects: &mut ProjectResolver) -> Option<UsageEvent> {
    let line: Line = serde_json::from_str(raw).ok()?;
    if line.kind.as_deref() != Some("assistant") {
        return None;
    }
    let message = line.message?;
    let usage = message.usage?;

    // `requestId` identifica a chamada à API; é a chave natural. Sem ele
    // caímos no uuid da mensagem, que também é único.
    let uid = line
        .request_id
        .or(line.uuid)
        .map(|id| format!("claude:{id}"))?;

    let ts: DateTime<Utc> = line
        .timestamp
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))?;

    Some(UsageEvent {
        uid,
        ts,
        provider: Provider::Claude,
        model: message.model,
        project: line.cwd.as_deref().map(|c| projects.resolve(c)),
        session_id: line.session_id,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_tokens: usage.cache_creation_input_tokens + usage.cache_read_input_tokens,
        cents: None,
    })
}

/// Varre todo o histórico, lendo de cada arquivo só o que ainda não foi lido.
pub fn ingest(store: &Store) -> Result<IngestStats> {
    let root = projects_dir()?;
    let mut stats = IngestStats::default();
    let mut projects = ProjectResolver::new();

    for path in walk_jsonl(&root) {
        let tail = match read_new_lines(store, &path) {
            Ok(t) => t,
            // Um arquivo ilegível (em escrita, permissão) não pode abortar a
            // varredura dos outros 142.
            Err(_) => continue,
        };
        stats.files_scanned += 1;
        if tail.lines.is_empty() {
            continue;
        }
        stats.bytes_read += tail.lines.iter().map(|l| l.len() as u64 + 1).sum::<u64>();

        let events: Vec<UsageEvent> = tail.lines.iter().filter_map(|l| parse_line(l, &mut projects)).collect();
        stats.events_inserted += store.insert_events(&events)?;
        // Só agora: se a inserção falhar, relemos na próxima vez.
        store.set_tail_cursor(&tail.cursor)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linha real capturada de `~/.claude/projects/.../*.jsonl`.
    const REAL: &str = r#"{"parentUuid":"179f335d","isSidechain":false,
      "message":{"model":"claude-opus-5","id":"msg_1","type":"message","role":"assistant",
        "content":[],"usage":{"input_tokens":2,"cache_creation_input_tokens":2292,
        "cache_read_input_tokens":73412,"output_tokens":593,
        "output_tokens_details":{"thinking_tokens":147},"service_tier":"standard"}},
      "requestId":"req_011CecjtPrGa4Ftf1E8YxpBB","type":"assistant",
      "uuid":"7ac7c553","timestamp":"2026-09-01T12:44:15.310Z","session_id":"5c7592dc",
      "sessionId":"5c7592dc","cwd":"E:\\ia-monitor","version":"2.1.246"}"#;

    #[test]
    fn extrai_consumo_de_mensagem_do_assistente() {
        let e = parse_line(REAL, &mut ProjectResolver::new()).unwrap();
        assert_eq!(e.uid, "claude:req_011CecjtPrGa4Ftf1E8YxpBB");
        assert_eq!(e.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(e.project.as_deref(), Some(r"E:\ia-monitor"));
        assert_eq!(e.input_tokens, 2);
        assert_eq!(e.output_tokens, 593);
        // cache_creation + cache_read somados: os contadores são disjuntos.
        assert_eq!(e.cache_tokens, 2292 + 73412);
    }

    /// O JSONL é majoritariamente linhas de usuário e de ferramenta. Contar
    /// qualquer uma delas como consumo inflaria tudo.
    #[test]
    fn ignora_linhas_que_nao_sao_consumo() {
        let mut p = ProjectResolver::new();
        assert!(parse_line(r#"{"type":"user","message":{"content":"oi"}}"#, &mut p).is_none());
        assert!(parse_line(r#"{"type":"assistant","message":{"model":"x"}}"#, &mut p).is_none());
        assert!(parse_line("lixo não-json", &mut p).is_none());
    }

    /// Sem timestamp o evento não tem lugar na linha do tempo.
    #[test]
    fn descarta_evento_sem_timestamp() {
        let json = r#"{"type":"assistant","requestId":"r1",
          "message":{"usage":{"input_tokens":1,"output_tokens":1}}}"#;
        assert!(parse_line(json, &mut ProjectResolver::new()).is_none());
    }
}
