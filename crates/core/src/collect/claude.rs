//! Claude Code — API OAuth oficial. Fonte autoritativa, em tempo real.
//!
//! O token sai de `~/.claude/.credentials.json`, escrito e renovado pelo
//! próprio CLI. Nunca renovamos por conta própria: o refresh rotaciona o
//! token e quebraria o Claude Code do usuário.

use crate::collect::{home_dir, Collector};
use crate::model::{expected_fraction, reset_label, Gauge, Provider, ProviderSample, Severity};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::collect::{retry_after_seconds, RateLimited};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OauthBlock>,
}

#[derive(Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Uma entrada de `limits[]`. O array é auto-descritivo: renderizar a partir
/// dele faz a UI absorver limites novos (Opus, Sonnet, ...) sem alteração.
#[derive(Deserialize, Debug)]
struct LimitEntry {
    kind: String,
    /// "session" ou "weekly" — é o que identifica a duração da janela.
    #[serde(default)]
    group: Option<String>,
    percent: f64,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    resets_at: Option<String>,
    /// Cuidado: NÃO significa "este limite existe". Significa "é o limite que
    /// está governando o consumo agora". Os inativos trazem percentuais reais
    /// e precisam aparecer na UI.
    #[serde(default)]
    is_active: Option<bool>,
    #[serde(default)]
    scope: Option<LimitScope>,
}

#[derive(Deserialize, Debug)]
struct LimitScope {
    #[serde(default)]
    model: Option<ScopedModel>,
}

#[derive(Deserialize, Debug)]
struct ScopedModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct UsageResponse {
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

pub struct ClaudeCollector {
    client: reqwest::Client,
    cli_version: String,
}

impl ClaudeCollector {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client, cli_version: "2.1.246".into() }
    }

    fn credentials() -> Result<(Zeroizing<String>, Option<i64>, Option<String>)> {
        let path = home_dir()
            .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
            .join(".claude")
            .join(".credentials.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("lendo {}", path.display()))?;
        let parsed: CredentialsFile = serde_json::from_str(&raw)?;
        let block = parsed
            .oauth
            .ok_or_else(|| anyhow!("claudeAiOauth ausente — Claude Code não está logado"))?;
        Ok((
            Zeroizing::new(block.access_token),
            block.expires_at,
            block.subscription_type,
        ))
    }

    /// Nomes amigáveis para os `kind` conhecidos; o resto passa direto para
    /// que um limite novo apareça na UI mesmo sem tradução. Quando a entrada
    /// tem escopo de modelo, ele entra no rótulo — é o que distingue dois
    /// limites semanais.
    fn label_for(entry: &LimitEntry) -> String {
        let base = match entry.kind.as_str() {
            "session" => "Sessão 5h".to_string(),
            "weekly_all" => "Semana".to_string(),
            "weekly_scoped" => "Semana".to_string(),
            other => other.replace('_', " "),
        };
        match Self::scoped_model(entry) {
            Some(model) => format!("{base} · {model}"),
            None => base,
        }
    }

    /// Duração da janela de cada limite.
    ///
    /// Não é chute: a própria resposta nomeia os campos `five_hour` e
    /// `seven_day`, e `group` distingue sessão de semana. Um `group`
    /// desconhecido devolve `None` — sem marcador é melhor que marcador
    /// errado.
    fn window_seconds(entry: &LimitEntry) -> Option<i64> {
        let key = entry.group.as_deref().unwrap_or(entry.kind.as_str());
        match key {
            "session" => Some(5 * 3600),
            "weekly" => Some(7 * 24 * 3600),
            _ => match entry.kind.as_str() {
                "session" => Some(5 * 3600),
                k if k.starts_with("weekly") => Some(7 * 24 * 3600),
                _ => None,
            },
        }
    }

    fn scoped_model(entry: &LimitEntry) -> Option<&str> {
        entry.scope.as_ref()?.model.as_ref()?.display_name.as_deref()
    }

    async fn fetch(&self) -> Result<ProviderSample> {
        let (token, expires_at, plan) = Self::credentials()?;

        if let Some(exp) = expires_at {
            if exp < Utc::now().timestamp_millis() {
                return Err(anyhow!(
                    "token expirado — rode o Claude Code uma vez para renovar"
                ));
            }
        }

        let resp = self
            .client
            .get(USAGE_URL)
            .bearer_auth(token.as_str())
            .header("anthropic-beta", OAUTH_BETA)
            .header(
                "User-Agent",
                format!("claude-cli/{} (external, cli)", self.cli_version),
            )
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "401 — token inválido; rode o Claude Code para renovar"
            ));
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(RateLimited(retry_after_seconds(resp.headers())).into());
        }
        let resp = resp.error_for_status()?;
        let body: UsageResponse = resp.json().await?;

        let now = Utc::now();
        let gauges = Self::build_gauges(&body.limits, now);

        if gauges.is_empty() {
            return Err(anyhow!("resposta sem limites"));
        }

        Ok(ProviderSample {
            provider: Provider::Claude,
            plan,
            gauges,
            observed_at: now,
            source_at: Some(now),
            error: None,
            retry_after: None,
        })
    }

    /// Puro: separado de `fetch` para poder ser testado contra respostas reais.
    fn build_gauges(limits: &[LimitEntry], now: DateTime<Utc>) -> Vec<Gauge> {
        let mut gauges = Vec::new();
        for entry in limits {
            let fraction = (entry.percent / 100.0).clamp(0.0, 1.0);
            let resets_at = entry
                .resets_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));

            let severity = entry
                .severity
                .as_deref()
                .and_then(Severity::from_api)
                .unwrap_or_else(|| Severity::from_fraction(Some(fraction)));

            // `kind` sozinho colide: dois limites semanais chegam como
            // weekly_all e weekly_scoped, e o escopo é o que os separa.
            let id = match Self::scoped_model(entry) {
                Some(model) => format!("claude.{}.{}", entry.kind, model.to_lowercase()),
                None => format!("claude.{}", entry.kind),
            };

            gauges.push(Gauge {
                id,
                label: Self::label_for(entry),
                fraction: Some(fraction),
                headline: format!("{}%", entry.percent.round() as i64),
                subtitle: resets_at.map(|r| reset_label(r, now)),
                severity,
                resets_at,
                active: entry.is_active.unwrap_or(true),
                expected: expected_fraction(resets_at, Self::window_seconds(entry), now),
            });
        }
        gauges
    }
}

impl Collector for ClaudeCollector {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    async fn sample(&self) -> ProviderSample {
        match self.fetch().await {
            Ok(s) => s,
            Err(e) => match e.downcast_ref::<RateLimited>() {
                Some(rl) => ProviderSample::rate_limited(Provider::Claude, rl.0),
                None => ProviderSample::failed(Provider::Claude, e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resposta real capturada de `/api/oauth/usage` (conta Max 5x).
    const REAL_RESPONSE: &str = r#"{"limits":[
      {"kind":"session","group":"session","percent":30,"severity":"normal",
       "resets_at":"2026-09-01T15:40:00.252633+00:00","scope":null,"is_active":true},
      {"kind":"weekly_all","group":"weekly","percent":17,"severity":"normal",
       "resets_at":"2026-09-05T01:00:00.252656+00:00","scope":null,"is_active":false},
      {"kind":"weekly_scoped","group":"weekly","percent":15,"severity":"normal",
       "resets_at":"2026-09-05T01:00:00.252845+00:00",
       "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":false}
    ]}"#;

    fn gauges() -> Vec<Gauge> {
        let parsed: UsageResponse = serde_json::from_str(REAL_RESPONSE).unwrap();
        ClaudeCollector::build_gauges(&parsed.limits, Utc::now())
    }

    /// Regressão: `is_active:false` NÃO significa que o limite não existe.
    /// Filtrar por ele apagava as barras semanais (17% e 15%) da UI.
    #[test]
    fn limites_inativos_continuam_visiveis() {
        let g = gauges();
        assert_eq!(g.len(), 3, "os três limites devem virar medidores");
        assert!(g.iter().any(|x| x.label == "Semana" && !x.active));
        assert!(g.iter().any(|x| x.label == "Sessão 5h" && x.active));
    }

    /// weekly_all e weekly_scoped compartilham `group`; só o escopo os separa.
    #[test]
    fn limites_semanais_nao_colidem() {
        let g = gauges();
        let ids: std::collections::HashSet<_> = g.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids.len(), g.len(), "ids duplicados: {ids:?}");
        assert!(g.iter().any(|x| x.label == "Semana · Fable"));
    }

    #[test]
    fn fracao_vem_do_percentual_do_servidor() {
        let g = gauges();
        let sessao = g.iter().find(|x| x.label == "Sessão 5h").unwrap();
        assert_eq!(sessao.fraction, Some(0.30));
        assert_eq!(sessao.headline, "30%");
    }

    /// Um `kind` desconhecido não pode sumir da UI — é assim que um limite
    /// novo aparece sem precisarmos alterar código.
    #[test]
    fn kind_desconhecido_ainda_vira_medidor() {
        let json = r#"{"limits":[{"kind":"weekly_omelette","percent":42,"is_active":true}]}"#;
        let parsed: UsageResponse = serde_json::from_str(json).unwrap();
        let g = ClaudeCollector::build_gauges(&parsed.limits, Utc::now());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].label, "weekly omelette");
    }

    /// A janela sai do vocabulário da própria API: os campos do topo se
    /// chamam `five_hour` e `seven_day`, e `group` separa sessão de semana.
    #[test]
    fn janela_vem_do_grupo_do_limite() {
        let g = gauges();
        let sessao = g.iter().find(|x| x.label == "Sessão 5h").unwrap();
        let semana = g.iter().find(|x| x.label == "Semana").unwrap();
        assert!(sessao.expected.is_some(), "sessão precisa de marcador");
        assert!(semana.expected.is_some(), "semana precisa de marcador");
    }

    /// Um `group` desconhecido não pode gerar marcador inventado.
    #[test]
    fn grupo_desconhecido_nao_ganha_marcador() {
        let json = r#"{"limits":[{"kind":"nova_cota","group":"nova","percent":10,
          "resets_at":"2026-09-05T01:00:00+00:00","is_active":true}]}"#;
        let parsed: UsageResponse = serde_json::from_str(json).unwrap();
        let g = ClaudeCollector::build_gauges(&parsed.limits, Utc::now());
        assert_eq!(g.len(), 1, "o limite continua visível");
        assert!(g[0].expected.is_none(), "mas sem marcador de ritmo");
    }

    #[test]
    fn severidade_do_servidor_vence_a_derivada() {
        let json = r#"{"limits":[{"kind":"session","percent":10,"severity":"critical","is_active":true}]}"#;
        let parsed: UsageResponse = serde_json::from_str(json).unwrap();
        let g = ClaudeCollector::build_gauges(&parsed.limits, Utc::now());
        assert_eq!(g[0].severity, Severity::Critical);
    }
}
