//! Claude — API OAuth oficial. Fonte autoritativa, em tempo real.
//!
//! A cota lida aqui é **da conta**, não de um aplicativo: claude.ai, o
//! Claude Desktop e o Claude Code consomem a mesma janela de 5h e o mesmo
//! limite semanal. O token é só a chave de leitura — por isso duas fontes
//! servem, e por isso o número não muda conforme qual delas foi usada.
//!
//! São duas porque envelhecem de formas diferentes. O `.credentials.json`
//! do Claude Code vale poucas horas e só o próprio CLI o renova, então ele
//! seca em quem parou de usar o CLI. O Claude Desktop guarda o token
//! cifrado (ver `claude_desktop`) e o mantém fresco enquanto for aberto —
//! por isso ele vem primeiro.
//!
//! Nunca renovamos nada por conta própria: o refresh rotaciona o token e
//! quebraria o aplicativo de onde ele veio.

use crate::collect::{claude_desktop, home_dir, Collector};
use crate::model::{
    expected_fraction, local_moment, reset_label, Gauge, Provider, ProviderSample, Severity,
};
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

/// Onde a credencial foi lida. O Desktop vem primeiro porque é a fonte que
/// se mantém fresca sozinha; o CLI é a reserva de quem não usa o Desktop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CredSource {
    Desktop,
    Cli,
}

impl CredSource {
    fn nome(self) -> &'static str {
        match self {
            CredSource::Desktop => "Claude Desktop",
            CredSource::Cli => "Claude Code",
        }
    }
}

struct ClaudeCreds {
    token: Zeroizing<String>,
    /// Epoch em milissegundos.
    expires_at: Option<i64>,
    plan: Option<String>,
    fonte: CredSource,
}

impl ClaudeCreds {
    /// `Some(texto)` quando o token já venceu.
    ///
    /// O texto diz *quando*: o token vale poucas horas, então "expirado" é
    /// rotina e não indica problema — mas "expirado há dias" indica que
    /// aquele aplicativo não é mais usado nesta máquina, e é isso que o
    /// usuário precisa saber para escolher qual abrir.
    fn vencimento(&self, agora: DateTime<Utc>) -> Option<String> {
        let exp = self.expires_at?;
        if exp >= agora.timestamp_millis() {
            return None;
        }
        let quando = DateTime::from_timestamp_millis(exp)
            .map(|t| format!(" em {}", local_moment(t, agora)))
            .unwrap_or_default();
        Some(format!("token expirou{quando}"))
    }
}

pub struct ClaudeCollector {
    client: reqwest::Client,
    cli_version: String,
}

impl ClaudeCollector {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client, cli_version: "2.1.246".into() }
    }

    /// Lê uma fonte, sem julgar validade — quem julga é `credentials`.
    fn ler(fonte: CredSource) -> Result<ClaudeCreds> {
        match fonte {
            CredSource::Desktop => {
                let t = claude_desktop::token()?;
                Ok(ClaudeCreds {
                    token: t.access_token,
                    expires_at: t.expires_at,
                    plan: t.plan,
                    fonte,
                })
            }
            CredSource::Cli => {
                let path = home_dir()
                    .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
                    .join(".claude")
                    .join(".credentials.json");
                if !path.exists() {
                    return Err(anyhow!("não instalado nesta máquina"));
                }
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("lendo {}", path.display()))?;
                let parsed: CredentialsFile = serde_json::from_str(&raw)?;
                let block = parsed
                    .oauth
                    .ok_or_else(|| anyhow!("`claudeAiOauth` ausente — não está logado"))?;
                Ok(ClaudeCreds {
                    token: Zeroizing::new(block.access_token),
                    expires_at: block.expires_at,
                    plan: block.subscription_type,
                    fonte,
                })
            }
        }
    }

    /// Primeira fonte com token vivo.
    ///
    /// Uma fonte ausente ou vencida não encerra a busca — é o caso comum de
    /// quem trocou de aplicativo. Só quando as duas falham é que vira erro,
    /// e aí ele diz o que houve com **cada uma**: mandar "rode o Claude
    /// Code" para quem nem tem o CLI instalado é pedir o impossível.
    fn credentials() -> Result<ClaudeCreds> {
        let lidas = [CredSource::Desktop, CredSource::Cli]
            .into_iter()
            .map(|f| (f, Self::ler(f)));
        Self::escolhe(lidas, Utc::now())
    }

    /// Puro: separado de `credentials` porque a leitura depende de disco,
    /// de DPAPI e de qual aplicativo está instalado — nada disso cabe num
    /// teste, mas a regra de precedência precisa caber.
    fn escolhe(
        lidas: impl Iterator<Item = (CredSource, Result<ClaudeCreds>)>,
        agora: DateTime<Utc>,
    ) -> Result<ClaudeCreds> {
        let mut motivos = Vec::new();
        for (fonte, lida) in lidas {
            match lida {
                Ok(cred) => match cred.vencimento(agora) {
                    None => return Ok(cred),
                    Some(texto) => motivos.push(format!("{}: {texto}", fonte.nome())),
                },
                Err(e) => motivos.push(format!("{}: {e:#}", fonte.nome())),
            }
        }

        Err(anyhow!(
            "nenhuma credencial do Claude serve ({}) — abra o Claude Desktop ou o Claude Code, ou desligue o Claude na bandeja",
            motivos.join("; ")
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
        // A validade já foi conferida na cadeia de fontes: o que chega aqui
        // é o primeiro token vivo.
        let cred = Self::credentials()?;

        let resp = self
            .client
            .get(USAGE_URL)
            .bearer_auth(cred.token.as_str())
            .header("anthropic-beta", OAUTH_BETA)
            .header(
                "User-Agent",
                format!("claude-cli/{} (external, cli)", self.cli_version),
            )
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "401 — o token do {} foi recusado; abra-o para renovar",
                cred.fonte.nome()
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
            plan: cred.plan,
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

    fn cred(fonte: CredSource, expira_em_ms: Option<i64>, agora: DateTime<Utc>) -> ClaudeCreds {
        ClaudeCreds {
            token: Zeroizing::new(format!("sk-ant-{}", fonte.nome())),
            expires_at: expira_em_ms.map(|d| agora.timestamp_millis() + d),
            plan: Some("max".into()),
            fonte,
        }
    }

    /// O Desktop vem primeiro porque é a fonte que se mantém fresca
    /// sozinha — quem usa o Desktop todo dia nunca vê a barra sumir.
    #[test]
    fn com_as_duas_vivas_vence_o_desktop() {
        let agora = Utc::now();
        let escolhida = ClaudeCollector::escolhe(
            [
                (CredSource::Desktop, Ok(cred(CredSource::Desktop, Some(3_600_000), agora))),
                (CredSource::Cli, Ok(cred(CredSource::Cli, Some(3_600_000), agora))),
            ]
            .into_iter(),
            agora,
        )
        .expect("credencial");
        assert_eq!(escolhida.fonte, CredSource::Desktop);
    }

    /// Fonte vencida não encerra a busca: é o caso de quem trocou de
    /// aplicativo, e abortar ali apagaria a barra com uma credencial boa
    /// parada ao lado.
    #[test]
    fn fonte_vencida_cai_para_a_seguinte() {
        let agora = Utc::now();
        let escolhida = ClaudeCollector::escolhe(
            [
                (CredSource::Desktop, Ok(cred(CredSource::Desktop, Some(-1), agora))),
                (CredSource::Cli, Ok(cred(CredSource::Cli, Some(3_600_000), agora))),
            ]
            .into_iter(),
            agora,
        )
        .expect("credencial");
        assert_eq!(escolhida.fonte, CredSource::Cli);
    }

    /// Fonte ausente também não encerra: a máquina pode ter só um dos dois.
    #[test]
    fn fonte_ausente_cai_para_a_seguinte() {
        let agora = Utc::now();
        let escolhida = ClaudeCollector::escolhe(
            [
                (CredSource::Desktop, Err(anyhow!("não encontrado"))),
                (CredSource::Cli, Ok(cred(CredSource::Cli, Some(60_000), agora))),
            ]
            .into_iter(),
            agora,
        )
        .expect("credencial");
        assert_eq!(escolhida.fonte, CredSource::Cli);
    }

    /// Com duas fontes, "token expirou" sem dizer qual manda o usuário
    /// adivinhar qual aplicativo abrir.
    #[test]
    fn falhando_as_duas_o_erro_cita_cada_uma() {
        let agora = Utc::now();
        let erro = ClaudeCollector::escolhe(
            [
                (CredSource::Desktop, Ok(cred(CredSource::Desktop, Some(-86_400_000), agora))),
                (CredSource::Cli, Err(anyhow!("não instalado nesta máquina"))),
            ]
            .into_iter(),
            agora,
        );
        // `unwrap_err` exigiria `Debug` em `ClaudeCreds`, e um `Debug`
        // derivado imprimiria o token.
        let erro = match erro {
            Ok(_) => panic!("não deveria haver credencial válida"),
            Err(e) => e.to_string(),
        };

        assert!(erro.contains("Claude Desktop"), "{erro}");
        assert!(erro.contains("expirou"), "{erro}");
        assert!(erro.contains("Claude Code"), "{erro}");
        assert!(erro.contains("não instalado"), "{erro}");
    }

    /// Sem expiração declarada não dá para provar que venceu — e um 401
    /// resolve depois, sem descartar a fonte antes de tentar.
    #[test]
    fn expiracao_ausente_conta_como_viva() {
        let agora = Utc::now();
        assert!(cred(CredSource::Desktop, None, agora).vencimento(agora).is_none());
    }

    #[test]
    fn vencimento_diz_quando_expirou() {
        let agora = Utc::now();
        let texto = cred(CredSource::Cli, Some(-7_200_000), agora)
            .vencimento(agora)
            .expect("vencido");
        assert!(texto.starts_with("token expirou em "), "{texto}");
    }
}
