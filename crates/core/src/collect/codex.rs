//! Codex (ChatGPT) — cota ao vivo, com o rollout em disco como rede de segurança.
//!
//! Duas economias convivem sob o mesmo provedor, e é a licença que decide
//! qual delas governa:
//!
//! | Licença                | O que limita        | Reseta?              |
//! |------------------------|---------------------|----------------------|
//! | Plus / Pro (pessoal)   | janelas de 5h e 7d  | sim, por janela      |
//! | Business / Enterprise  | saldo de créditos   | não, recarga manual  |
//!
//! Por isso descobrir a licença é o primeiro passo, e ela está em
//! `~/.codex/auth.json`: o `id_token` carrega `chatgpt_plan_type` nas claims.
//! Local, instantâneo e sempre atual — trocar de plano reescreve o arquivo.
//!
//! Isso não é detalhe. O rollout em disco guarda o plano de **quando foi
//! gravado**. Quem migrou de business para plus tem, no último rollout, um
//! saldo de crédito que não governa mais nada; exibir aquilo seria mostrar a
//! cota da assinatura errada com cara de dado atual. Daí a regra: dado de
//! rollout só vale se o plano dele bater com o plano de agora.
//!
//! `GET /backend-api/codex/usage` é a fonte autoritativa e responde em tempo
//! real, o que tira do Codex a antiga condição de provedor "passivo". O
//! header `chatgpt-account-id` é obrigatório: sem ele a API devolve 403.
//!
//! Nunca usamos o `refresh_token` de `auth.json`: a rotação invalidaria o
//! token do CLI e quebraria o Codex do usuário.

use crate::collect::{home_dir, retry_after_seconds, Collector, RateLimited};
use crate::model::{
    expected_fraction, format_pt_br, reset_label, Gauge, Provider, ProviderSample, Severity,
};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/codex/usage";
const CLI_USER_AGENT: &str = "codex_cli_rs/0.147.0";
/// Onde as claims de licença moram dentro do JWT.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Quanto lemos do fim de cada arquivo antes de desistir. Um `token_count`
/// fica sempre perto do fim de uma sessão encerrada.
const TAIL_CHUNK: u64 = 256 * 1024;
const TAIL_MAX: u64 = 8 * 1024 * 1024;
/// Sessões mais recentes a inspecionar — uma sessão retomada atualiza o mtime
/// de um arquivo antigo, então não basta olhar só o primeiro.
const FILES_TO_SCAN: usize = 5;
/// Sem teto declarado pela API, a barra de crédito precisa de uma referência.
const DEFAULT_CREDIT_BASELINE: f64 = 1500.0;

// ---------------------------------------------------------------------------
// Licença: de quem é a cota
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    // `refresh_token` existe no arquivo e é deliberadamente ignorado.
}

#[derive(Deserialize, Default, Debug)]
struct AuthClaims {
    #[serde(default, rename = "chatgpt_plan_type")]
    plan_type: Option<String>,
    #[serde(default, rename = "chatgpt_account_id")]
    account_id: Option<String>,
    /// Fim do ciclo pago. Só aparece em assinatura pessoal.
    #[serde(default, rename = "chatgpt_subscription_active_until")]
    active_until: Option<String>,
}

/// O que o `auth.json` entrega. O token vive só o tempo da requisição.
struct CodexAuth {
    access_token: Zeroizing<String>,
    /// Vai no header `chatgpt-account-id`; sem ele a API devolve 403.
    account_id: String,
    plan: Option<String>,
    cycle_end: Option<DateTime<Utc>>,
}

/// Lê as claims sem validar assinatura — não estamos autenticando ninguém,
/// só perguntando ao token qual licença ele representa.
fn claims_from_jwt(jwt: &str) -> Option<AuthClaims> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    serde_json::from_value(value.get(AUTH_CLAIM)?.clone()).ok()
}

/// Rótulo da licença para a UI. Um plano desconhecido passa direto,
/// capitalizado: a tela não pode ficar muda porque a OpenAI criou um nome
/// novo.
pub fn plan_label(raw: &str) -> String {
    match raw {
        "free" => "Free".into(),
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        other => {
            let limpo = other.replace('_', " ");
            let mut chars = limpo.chars();
            match chars.next() {
                Some(p) => p.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// "Plus · até 08/10" — o fim do ciclo, quando a licença declara um.
/// Responde "assinei por um mês, quando acaba?" sem abrir o site.
fn plan_text(
    plan: Option<&str>,
    cycle_end: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<String> {
    let base = plan_label(plan?);
    match cycle_end {
        Some(fim) if fim > now => {
            let d = fim.with_timezone(&Local);
            Some(format!("{base} · até {:02}/{:02}", d.day(), d.month()))
        }
        _ => Some(base),
    }
}

// ---------------------------------------------------------------------------
// Cota normalizada: as duas fontes desembocam aqui
// ---------------------------------------------------------------------------

/// Janela normalizada. O endpoint fala em segundos, o rollout em minutos.
#[derive(Debug, Clone, Default)]
struct Window {
    used_percent: f64,
    seconds: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct Credits {
    #[serde(default)]
    has_credits: Option<bool>,
    #[serde(default)]
    unlimited: Option<bool>,
    #[serde(default)]
    balance: Option<String>,
}

impl Credits {
    fn balance_value(&self) -> Option<f64> {
        self.balance.as_deref().and_then(|b| b.parse::<f64>().ok())
    }

    /// O saldo de crédito só vira barra quando realmente governa o consumo.
    ///
    /// Numa licença de janela (Plus) o bloco vem zerado — é resquício da outra
    /// economia, não um limite estourado. Pintar 100% ali seria alarme falso,
    /// que é onde uma regra ingênua do tipo "tem campo, mostra" iria parar.
    fn govern(&self, reached: Option<&str>) -> bool {
        self.unlimited.unwrap_or(false)
            || self.has_credits.unwrap_or(false)
            || self.balance_value().is_some_and(|b| b > 0.0)
            || reached.is_some_and(|r| r.contains("credit"))
    }
}

/// O estado da cota, venha ele da API ou do rollout.
#[derive(Debug, Default)]
struct Quota {
    plan: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
    credits: Option<Credits>,
    reached: Option<String>,
}

// ---------------------------------------------------------------------------
// Fonte 1: a API (autoritativa, tempo real)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct HttpWindow {
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

impl From<&HttpWindow> for Window {
    fn from(w: &HttpWindow) -> Self {
        Window {
            used_percent: w.used_percent,
            seconds: w.limit_window_seconds,
            resets_at: w.reset_at,
        }
    }
}

#[derive(Deserialize, Debug, Default)]
struct HttpRateLimit {
    #[serde(default)]
    primary_window: Option<HttpWindow>,
    #[serde(default)]
    secondary_window: Option<HttpWindow>,
}

#[derive(Deserialize, Debug, Default)]
struct UsageResponse {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<HttpRateLimit>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

impl From<UsageResponse> for Quota {
    fn from(r: UsageResponse) -> Self {
        let limits = r.rate_limit.unwrap_or_default();
        Quota {
            plan: r.plan_type,
            primary: limits.primary_window.as_ref().map(Window::from),
            secondary: limits.secondary_window.as_ref().map(Window::from),
            credits: r.credits,
            reached: r.rate_limit_reached_type,
        }
    }
}

// ---------------------------------------------------------------------------
// Fonte 2: o rollout em disco (recuo, quando a rede falha)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
struct RolloutWindow {
    used_percent: f64,
    window_minutes: i64,
    #[serde(default)]
    resets_at: Option<i64>,
}

impl From<&RolloutWindow> for Window {
    fn from(w: &RolloutWindow) -> Self {
        Window {
            used_percent: w.used_percent,
            seconds: Some(w.window_minutes * 60),
            resets_at: w.resets_at,
        }
    }
}

#[derive(Deserialize, Debug)]
struct RateLimits {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    primary: Option<RolloutWindow>,
    #[serde(default)]
    secondary: Option<RolloutWindow>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

impl From<RateLimits> for Quota {
    fn from(r: RateLimits) -> Self {
        Quota {
            plan: r.plan_type,
            primary: r.primary.as_ref().map(Window::from),
            secondary: r.secondary.as_ref().map(Window::from),
            credits: r.credits,
            reached: r.rate_limit_reached_type,
        }
    }
}

#[derive(Deserialize, Debug)]
struct Payload {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize, Debug)]
struct RolloutLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

// ---------------------------------------------------------------------------

pub struct CodexCollector {
    client: reqwest::Client,
    credit_baseline: f64,
}

impl CodexCollector {
    pub fn new(client: reqwest::Client, credit_baseline: Option<f64>) -> Self {
        Self {
            client,
            credit_baseline: credit_baseline.unwrap_or(DEFAULT_CREDIT_BASELINE),
        }
    }

    fn auth() -> Result<CodexAuth> {
        let path = home_dir()
            .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
            .join(".codex")
            .join("auth.json");
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
        let parsed: AuthFile = serde_json::from_str(&raw)?;
        let tokens = parsed
            .tokens
            .ok_or_else(|| anyhow!("auth.json sem tokens — o Codex não está logado"))?;

        // O `id_token` traz o ciclo da assinatura; o `access_token` traz o
        // plano. Ler os dois cobre contas em que um deles vem enxuto.
        let do_id = tokens.id_token.as_deref().and_then(claims_from_jwt);
        let do_access = claims_from_jwt(&tokens.access_token);
        let plan = do_id
            .as_ref()
            .and_then(|c| c.plan_type.clone())
            .or_else(|| do_access.as_ref().and_then(|c| c.plan_type.clone()));
        let cycle_end = do_id
            .as_ref()
            .and_then(|c| c.active_until.as_deref())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let account_id = tokens
            .account_id
            .or_else(|| do_access.and_then(|c| c.account_id))
            .or_else(|| do_id.and_then(|c| c.account_id))
            .ok_or_else(|| anyhow!("auth.json sem account_id — refaça o login do Codex"))?;

        Ok(CodexAuth {
            access_token: Zeroizing::new(tokens.access_token),
            account_id,
            plan,
            cycle_end,
        })
    }

    async fn fetch_live(&self, auth: &CodexAuth) -> Result<Quota> {
        let resp = self
            .client
            .get(USAGE_URL)
            .bearer_auth(auth.access_token.as_str())
            // Obrigatório: sem ele a resposta é 403.
            .header("chatgpt-account-id", &auth.account_id)
            .header("User-Agent", CLI_USER_AGENT)
            .header("Accept", "application/json")
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!("401 — token expirado; abra o Codex para renovar"));
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(RateLimited(retry_after_seconds(resp.headers())).into());
        }
        let resp = resp.error_for_status()?;
        let body: UsageResponse = resp.json().await?;
        Ok(body.into())
    }

    fn sessions_dir() -> Result<PathBuf> {
        let dir = home_dir()
            .ok_or_else(|| anyhow!("home do usuário não encontrada"))?
            .join(".codex")
            .join("sessions");
        if !dir.exists() {
            return Err(anyhow!("Codex não encontrado ({})", dir.display()));
        }
        Ok(dir)
    }

    fn collect_rollouts(dir: &Path, out: &mut Vec<(std::time::SystemTime, PathBuf)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                Self::collect_rollouts(&path, out);
            } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Ok(mtime) = meta.modified() {
                    out.push((mtime, path));
                }
            }
        }
    }

    /// Lê o fim do arquivo em blocos até achar o último `token_count`.
    /// Evita carregar rollouts inteiros — o histórico soma centenas de MB.
    fn last_token_count(path: &Path) -> Option<(DateTime<Utc>, RateLimits)> {
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut read_back: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();

        while read_back < len && read_back < TAIL_MAX {
            read_back = (read_back + TAIL_CHUNK).min(len);
            let start = len - read_back;
            file.seek(SeekFrom::Start(start)).ok()?;
            buf.clear();
            buf.resize(read_back as usize, 0);
            file.read_exact(&mut buf).ok()?;

            let text = String::from_utf8_lossy(&buf);
            // De trás para frente: queremos o evento mais recente.
            for line in text.lines().rev() {
                if !line.contains("token_count") {
                    continue;
                }
                let parsed = match serde_json::from_str::<RolloutLine>(line) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let payload = match parsed.payload {
                    Some(p) => p,
                    None => continue,
                };
                if payload.kind.as_deref() != Some("token_count") {
                    continue;
                }
                let limits = match payload.rate_limits {
                    Some(l) => l,
                    None => continue,
                };
                let ts = parsed
                    .timestamp
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc));
                if let Some(ts) = ts {
                    return Some((ts, limits));
                }
            }
        }
        None
    }

    /// Última cota gravada em disco, **desde que ela pertença à licença de
    /// hoje**. Um rollout de business não descreve uma assinatura Plus.
    fn rollout_quota(plano_atual: Option<&str>) -> Option<(DateTime<Utc>, Quota)> {
        let dir = Self::sessions_dir().ok()?;
        let mut files = Vec::new();
        Self::collect_rollouts(&dir, &mut files);
        files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

        // O mais recente por mtime nem sempre tem o evento mais novo:
        // retomar uma sessão antiga atualiza o arquivo dela.
        let (source_at, limits) = files
            .iter()
            .take(FILES_TO_SCAN)
            .filter_map(|(_, p)| Self::last_token_count(p))
            .max_by_key(|(ts, _)| *ts)?;

        if !plano_confere(plano_atual, limits.plan_type.as_deref()) {
            return None;
        }
        Some((source_at, limits.into()))
    }

    fn window_label(seconds: Option<i64>) -> String {
        match seconds {
            Some(18_000) => "Sessão 5h".into(),
            Some(604_800) => "Semana".into(),
            Some(s) if s % 86_400 == 0 => format!("{} dias", s / 86_400),
            Some(s) if s % 3_600 == 0 => format!("{}h", s / 3_600),
            Some(s) => format!("{} min", s / 60),
            None => "Janela".into(),
        }
    }

    fn window_gauge(id: &str, w: &Window, now: DateTime<Utc>) -> Gauge {
        let fraction = (w.used_percent / 100.0).clamp(0.0, 1.0);
        let resets_at = w.resets_at.and_then(|s| Utc.timestamp_opt(s, 0).single());
        Gauge {
            id: id.into(),
            label: Self::window_label(w.seconds),
            fraction: Some(fraction),
            headline: format!("{}%", w.used_percent.round() as i64),
            subtitle: resets_at.map(|r| reset_label(r, now)),
            severity: Severity::from_fraction(Some(fraction)),
            resets_at,
            active: true,
            // A duração vem declarada na resposta: a janela é exata.
            expected: expected_fraction(resets_at, w.seconds, now),
        }
    }

    fn credit_gauge(&self, c: &Credits) -> Option<Gauge> {
        if c.unlimited.unwrap_or(false) {
            return Some(Gauge {
                id: "codex.credits".into(),
                label: "Créditos".into(),
                fraction: None,
                headline: "ilimitado".into(),
                subtitle: None,
                severity: Severity::Normal,
                resets_at: None,
                active: true,
                expected: None,
            });
        }
        let balance = c.balance_value()?;
        let baseline = self.credit_baseline.max(balance);
        let used = 1.0 - (balance / baseline).clamp(0.0, 1.0);
        Some(Gauge {
            id: "codex.credits".into(),
            label: "Créditos".into(),
            // A barra mede consumo, como as outras: 1 - restante.
            fraction: Some(used),
            headline: format_pt_br(balance),
            subtitle: Some(format!("de ~{}", format_pt_br(baseline))),
            severity: Severity::from_fraction(Some(used)),
            resets_at: None,
            active: true,
            // Saldo de crédito não reseta: não há "onde eu deveria estar"
            // nesta altura do mês.
            expected: None,
        })
    }

    async fn fetch(&self) -> Result<ProviderSample> {
        let auth = Self::auth()?;
        let now = Utc::now();

        let erro_live = match self.fetch_live(&auth).await {
            Ok(quota) => return self.build_sample(now, quota, auth.cycle_end, now),
            Err(e) if e.downcast_ref::<RateLimited>().is_some() => return Err(e),
            Err(e) => e,
        };

        // Sem rede ou com token vencido, o rollout ainda sabe o que o servidor
        // respondeu da última vez — e `source_at` conta a idade disso.
        match Self::rollout_quota(auth.plan.as_deref()) {
            Some((source_at, quota)) => self.build_sample(source_at, quota, auth.cycle_end, now),
            None => Err(erro_live),
        }
    }

    /// Puro: separado da rede e do disco para ser testado contra as respostas
    /// reais dos dois tipos de licença.
    fn build_sample(
        &self,
        source_at: DateTime<Utc>,
        quota: Quota,
        cycle_end: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<ProviderSample> {
        let mut gauges = Vec::new();

        // Licença de janela (Plus/Pro): o servidor já dá o percentual.
        if let Some(w) = &quota.primary {
            gauges.push(Self::window_gauge("codex.primary", w, now));
        }
        if let Some(w) = &quota.secondary {
            gauges.push(Self::window_gauge("codex.secondary", w, now));
        }

        // Licença de crédito (Business/Enterprise): sem teto declarado, a
        // fração sai de uma baseline — e a barra nunca estoura.
        if let Some(c) = &quota.credits {
            if c.govern(quota.reached.as_deref()) {
                gauges.extend(self.credit_gauge(c));
            }
        }

        if gauges.is_empty() {
            return Err(anyhow!("resposta de cota sem janelas nem créditos"));
        }

        // Limite atingido é estado de alarme. Ele marca o medidor que bateu na
        // parede — o de maior consumo — e não todos: uma janela semanal em 3%
        // não fica vermelha só porque a de 5h encheu.
        if let Some(reason) = &quota.reached {
            let alvo = gauges
                .iter()
                .enumerate()
                .filter(|(_, g)| g.fraction.is_some())
                .max_by(|a, b| {
                    a.1.fraction
                        .unwrap_or(0.0)
                        .partial_cmp(&b.1.fraction.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i);
            for (i, g) in gauges.iter_mut().enumerate() {
                if alvo.is_none() || alvo == Some(i) {
                    g.severity = Severity::Critical;
                    g.subtitle = Some(format!("limite atingido ({reason})"));
                }
            }
        }

        Ok(ProviderSample {
            provider: Provider::Codex,
            plan: plan_text(quota.plan.as_deref(), cycle_end, now),
            gauges,
            observed_at: now,
            source_at: Some(source_at),
            error: None,
            retry_after: None,
        })
    }
}

/// Só recusamos o rollout quando temos certeza da divergência. Sem saber o
/// plano de agora, ou sem o plano gravado, o dado antigo ainda é o melhor
/// disponível — e a UI já mostra a idade dele.
fn plano_confere(atual: Option<&str>, gravado: Option<&str>) -> bool {
    match (atual, gravado) {
        (Some(a), Some(g)) => a == g,
        _ => true,
    }
}

impl Collector for CodexCollector {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    async fn sample(&self) -> ProviderSample {
        match self.fetch().await {
            Ok(s) => s,
            Err(e) => match e.downcast_ref::<RateLimited>() {
                Some(rl) => ProviderSample::rate_limited(Provider::Codex, rl.0),
                None => ProviderSample::failed(Provider::Codex, e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coletor() -> CodexCollector {
        CodexCollector::new(reqwest::Client::new(), None)
    }

    fn quota_http(json: &str) -> Quota {
        serde_json::from_str::<UsageResponse>(json).unwrap().into()
    }

    fn quota_rollout(json: &str) -> Quota {
        serde_json::from_str::<RateLimits>(json).unwrap().into()
    }

    fn amostra(quota: Quota) -> ProviderSample {
        coletor()
            .build_sample(Utc::now(), quota, None, Utc::now())
            .unwrap()
    }

    /// Resposta real de `/backend-api/codex/usage` numa assinatura Plus.
    /// Repare no bloco `credits` zerado: ele existe, mas não governa nada.
    const PLUS_HTTP: &str = r#"{"plan_type":"plus","rate_limit":{"allowed":true,
      "limit_reached":false,
      "primary_window":{"used_percent":1,"limit_window_seconds":18000,
        "reset_after_seconds":17532,"reset_at":1788895096},
      "secondary_window":{"used_percent":0,"limit_window_seconds":604800,
        "reset_after_seconds":604332,"reset_at":1789481896}},
      "credits":{"has_credits":false,"unlimited":false,"overage_limit_reached":false,
        "balance":"0"},
      "spend_control":{"reached":false},"rate_limit_reached_type":null}"#;

    /// Evento real do plano business: sem janelas, só saldo de crédito.
    const BUSINESS_ROLLOUT: &str = r#"{"limit_id":"codex","limit_name":null,"primary":null,
      "secondary":null,"credits":{"has_credits":true,"unlimited":false,
      "balance":"1131.4605596065521"},"plan_type":"business",
      "rate_limit_reached_type":null}"#;

    /// Evento real de um plano plus antigo: janelas em minutos.
    const PLUS_ROLLOUT: &str = r#"{"limit_id":"codex","primary":{"used_percent":6.0,
      "window_minutes":300,"resets_at":1777054045},"secondary":{"used_percent":1.0,
      "window_minutes":10080,"resets_at":1777640845},"credits":null,
      "plan_type":"plus","rate_limit_reached_type":null}"#;

    // -- licença de janela (Plus) --------------------------------------------

    #[test]
    fn plus_usa_as_janelas_do_servidor() {
        let s = amostra(quota_http(PLUS_HTTP));
        let p = s.gauges.iter().find(|g| g.id == "codex.primary").unwrap();
        assert_eq!(p.label, "Sessão 5h");
        assert_eq!(p.headline, "1%");
        let sec = s.gauges.iter().find(|g| g.id == "codex.secondary").unwrap();
        assert_eq!(sec.label, "Semana");
        assert_eq!(s.plan.as_deref(), Some("Plus"));
    }

    /// O regressão que motivou tudo isto: no Plus o bloco `credits` vem
    /// zerado. Tratá-lo como medidor pintaria 100% de consumo e deixaria o
    /// ícone vermelho com a cota intacta.
    #[test]
    fn saldo_zerado_do_plus_nao_vira_barra() {
        let s = amostra(quota_http(PLUS_HTTP));
        assert!(
            !s.gauges.iter().any(|g| g.id == "codex.credits"),
            "credits do Plus não governa a cota e não pode virar barra"
        );
        assert!(s.gauges.iter().all(|g| g.severity == Severity::Normal));
    }

    #[test]
    fn janela_do_plus_gera_marcador_de_ritmo() {
        let s = amostra(quota_http(PLUS_HTTP));
        let p = s.gauges.iter().find(|g| g.id == "codex.primary").unwrap();
        assert!(p.expected.is_some(), "limit_window_seconds define a janela");
    }

    // -- licença de crédito (Business) ---------------------------------------

    #[test]
    fn business_mostra_o_saldo_de_credito() {
        let s = amostra(quota_rollout(BUSINESS_ROLLOUT));
        let g = s.gauges.iter().find(|g| g.id == "codex.credits").unwrap();
        assert_eq!(g.headline, "1.131,46");
        assert_eq!(s.plan.as_deref(), Some("Business"));
    }

    /// A barra mede CONSUMO, como as demais. Saldo alto = barra vazia.
    /// Inverter isso faria a UI gritar quando está tudo bem.
    #[test]
    fn barra_de_credito_mede_consumo_nao_saldo() {
        let s = amostra(quota_rollout(BUSINESS_ROLLOUT));
        let f = s.gauges[0].fraction.unwrap();
        // 1131 de 1500 => ~75% restante => ~25% consumido.
        assert!((f - 0.246).abs() < 0.01, "fração {f} deveria ser ~0.246");
        assert_eq!(s.gauges[0].severity, Severity::Normal);
    }

    #[test]
    fn saldo_acima_da_baseline_nao_estoura_a_barra() {
        let s = amostra(quota_rollout(
            r#"{"credits":{"has_credits":true,"balance":"9999"},"plan_type":"business"}"#,
        ));
        let f = s.gauges[0].fraction.unwrap();
        assert!((0.0..=1.0).contains(&f), "fração fora de 0..1: {f}");
    }

    #[test]
    fn plano_ilimitado_nao_inventa_fracao() {
        let s = amostra(quota_rollout(
            r#"{"credits":{"unlimited":true,"balance":null},"plan_type":"enterprise"}"#,
        ));
        assert_eq!(s.gauges[0].fraction, None);
        assert_eq!(s.gauges[0].headline, "ilimitado");
        assert_eq!(s.plan.as_deref(), Some("Enterprise"));
    }

    /// Estado real já visto no histórico: crédito esgotado. Aqui `has_credits`
    /// é falso, e o que mantém a barra viva é o motivo do bloqueio.
    #[test]
    fn credito_esgotado_vira_critico() {
        let s = amostra(quota_rollout(
            r#"{"limit_id":"premium","credits":{"has_credits":false,"unlimited":false,
             "balance":"0"},"plan_type":"business",
             "rate_limit_reached_type":"workspace_member_credits_depleted"}"#,
        ));
        let g = s.gauges.iter().find(|g| g.id == "codex.credits").unwrap();
        assert_eq!(g.severity, Severity::Critical);
    }

    #[test]
    fn credito_nao_tem_marcador_de_ritmo() {
        let s = amostra(quota_rollout(BUSINESS_ROLLOUT));
        assert!(s.gauges[0].expected.is_none());
    }

    // -- alarme --------------------------------------------------------------

    /// O alarme marca só quem bateu no teto. A janela semanal em 2% continua
    /// verde quando a de 5h enche — pintar as duas de vermelho esconderia
    /// justamente a informação de que ainda sobra semana.
    #[test]
    fn limite_atingido_marca_so_a_janela_que_encheu() {
        let s = amostra(quota_http(
            r#"{"plan_type":"plus","rate_limit":{
              "primary_window":{"used_percent":100,"limit_window_seconds":18000},
              "secondary_window":{"used_percent":2,"limit_window_seconds":604800}},
              "rate_limit_reached_type":"usage_limit_reached"}"#,
        ));
        let p = s.gauges.iter().find(|g| g.id == "codex.primary").unwrap();
        let sec = s.gauges.iter().find(|g| g.id == "codex.secondary").unwrap();
        assert_eq!(p.severity, Severity::Critical);
        assert_eq!(sec.severity, Severity::Normal);
    }

    // -- recuo para o rollout ------------------------------------------------

    /// O caso que quebraria a ferramenta na troca de assinatura: o último
    /// rollout é business, a licença de hoje é plus. Aceitar aquele saldo
    /// mostraria a cota da assinatura antiga como se fosse a atual.
    #[test]
    fn rollout_de_outra_licenca_e_recusado() {
        assert!(!plano_confere(Some("plus"), Some("business")));
        assert!(plano_confere(Some("plus"), Some("plus")));
    }

    /// Sem saber o plano atual, o dado antigo ainda é o melhor que existe —
    /// recusar tudo deixaria a UI vazia offline sem necessidade.
    #[test]
    fn rollout_passa_quando_o_plano_e_desconhecido() {
        assert!(plano_confere(None, Some("business")));
        assert!(plano_confere(Some("plus"), None));
    }

    /// O rollout fala em minutos e a API em segundos; os dois têm que produzir
    /// o mesmo rótulo, senão a janela muda de nome ao cair para o recuo.
    #[test]
    fn as_duas_fontes_rotulam_a_janela_igual() {
        let http = amostra(quota_http(PLUS_HTTP));
        let disco = amostra(quota_rollout(PLUS_ROLLOUT));
        let rot = |s: &ProviderSample, id: &str| {
            s.gauges.iter().find(|g| g.id == id).unwrap().label.clone()
        };
        assert_eq!(rot(&http, "codex.primary"), rot(&disco, "codex.primary"));
        assert_eq!(rot(&http, "codex.secondary"), rot(&disco, "codex.secondary"));
    }

    // -- licença --------------------------------------------------------------

    /// Um plano novo não pode deixar a UI muda.
    #[test]
    fn plano_desconhecido_ainda_ganha_rotulo() {
        assert_eq!(plan_label("plus"), "Plus");
        assert_eq!(plan_label("business"), "Business");
        assert_eq!(plan_label("edu_campus"), "Edu campus");
        assert_eq!(plan_label(""), "");
    }

    #[test]
    fn ciclo_da_assinatura_entra_no_rotulo() {
        let now = Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap();
        let fim = Utc.with_ymd_and_hms(2026, 10, 8, 14, 15, 56).unwrap();
        assert_eq!(
            plan_text(Some("plus"), Some(fim), now).as_deref(),
            Some("Plus · até 08/10")
        );
    }

    /// Ciclo vencido não vira rótulo: mostrar "até 08/09" no dia 20 seria
    /// informação errada com cara de precisa.
    #[test]
    fn ciclo_vencido_some_do_rotulo() {
        let now = Utc.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).unwrap();
        let fim = Utc.with_ymd_and_hms(2026, 9, 8, 14, 15, 56).unwrap();
        assert_eq!(plan_text(Some("plus"), Some(fim), now).as_deref(), Some("Plus"));
        assert_eq!(plan_text(None, Some(fim), now), None);
    }

    /// As claims ficam aninhadas sob a URL do emissor; ler o nível errado
    /// devolveria `None` e a licença viraria desconhecida.
    #[test]
    fn le_a_licenca_das_claims_do_jwt() {
        let corpo = serde_json::json!({
            "sub": "auth0|x",
            AUTH_CLAIM: {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": "conta-123",
                "chatgpt_subscription_active_until": "2026-10-08T14:15:56+00:00"
            }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&corpo).unwrap());
        let jwt = format!("cabecalho.{payload}.assinatura");

        let c = claims_from_jwt(&jwt).unwrap();
        assert_eq!(c.plan_type.as_deref(), Some("plus"));
        assert_eq!(c.account_id.as_deref(), Some("conta-123"));
        assert!(c.active_until.is_some());
    }

    #[test]
    fn jwt_invalido_nao_derruba_a_leitura() {
        assert!(claims_from_jwt("nao-e-jwt").is_none());
        assert!(claims_from_jwt("a.!!!.c").is_none());
    }

    // -- degradação ----------------------------------------------------------

    /// Sem cota nenhuma é erro — não uma barra vazia enganosa.
    #[test]
    fn resposta_sem_cota_falha() {
        let q = quota_http(r#"{"plan_type":"plus"}"#);
        assert!(coletor().build_sample(Utc::now(), q, None, Utc::now()).is_err());
    }
}
