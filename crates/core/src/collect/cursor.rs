//! Cursor — connect-rpc em `api2.cursor.sh`, a mesma fonte que alimenta a
//! tela "Plan & Usage" da IDE.
//!
//! O token vem do `state.vscdb` do Cursor. O arquivo tem centenas de MB e o
//! Cursor costuma estar rodando: abrimos em `mode=ro` via URI, sem copiar e
//! sem tomar lock (medido em ~0ms).
//!
//! Atenção: o Cursor usa DOIS esquemas de auth. O connect-rpc aqui quer
//! `Authorization: Bearer`; o histórico evento a evento em
//! `cursor.com/api/dashboard/*` quer cookie + header `Origin`.
//!
//! Desde o 3.27 o token não fica mais em claro: vem cifrado pelo
//! `safeStorage` do Electron, o mesmo esquema do Claude Desktop (ver
//! `win_secret`). Mandar o valor cru como bearer dá 401.

use crate::collect::win_secret::{decrypt_v10, master_key};
use crate::collect::{retry_after_seconds, Collector, RateLimited};
use crate::model::{
    expected_fraction, format_pt_br, reset_label, Gauge, Provider, ProviderSample, Severity,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use zeroize::Zeroizing;

const RPC_BASE: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService";
/// Envelope dos valores cifrados: `cursor-safe-storage:v1:` + base64 de um
/// blob `v10` do `os_crypt`.
const SAFE_STORAGE_PREFIX: &str = "cursor-safe-storage:";
const SAFE_STORAGE_V1: &str = "v1:";
/// O nome do plano muda raramente. Consultá-lo a cada ciclo dobrava o número
/// de requisições ao Cursor sem acrescentar nada.
const PLAN_CACHE_SECONDS: u64 = 6 * 3600;

/// Momento da gravação e o plano guardado.
type PlanoEmCache = Option<(std::time::Instant, Option<String>)>;

/// Cache de processo: o coletor é criado a cada ciclo, então guardar no
/// próprio struct não sobreviveria.
static PLAN_CACHE: std::sync::OnceLock<std::sync::Mutex<PlanoEmCache>> = std::sync::OnceLock::new();

fn cached_plan() -> Option<Option<String>> {
    let guard = PLAN_CACHE.get_or_init(Default::default).lock().ok()?;
    let (gravado_em, plano) = guard.as_ref()?;
    if gravado_em.elapsed().as_secs() < PLAN_CACHE_SECONDS {
        Some(plano.clone())
    } else {
        None
    }
}

fn cache_plan(plano: Option<String>) {
    if let Ok(mut guard) = PLAN_CACHE.get_or_init(Default::default).lock() {
        *guard = Some((std::time::Instant::now(), plano));
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    #[serde(default)]
    auto_percent_used: Option<f64>,
    #[serde(default)]
    api_percent_used: Option<f64>,
    #[serde(default)]
    total_spend: Option<f64>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CurrentPeriodUsage {
    #[serde(default)]
    plan_usage: Option<PlanUsage>,
    /// Epoch em milissegundos, serializado como string pelo protobuf JSON.
    #[serde(default)]
    billing_cycle_end: Option<String>,
    /// Com o início do ciclo a janela é exata, sem nenhuma convenção.
    #[serde(default)]
    billing_cycle_start: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlanInfo {
    #[serde(default)]
    plan_name: Option<String>,
    #[serde(default)]
    price: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PlanInfoResponse {
    #[serde(default)]
    plan_info: Option<PlanInfo>,
}

pub struct CursorCollector {
    client: reqwest::Client,
}

impl CursorCollector {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Diretório de dados do Electron: tem o `Local State` com a chave do
    /// `safeStorage` na raiz.
    fn user_data_dir() -> Result<std::path::PathBuf> {
        let appdata = std::env::var_os("APPDATA")
            .ok_or_else(|| anyhow!("APPDATA não definido"))?;
        Ok(std::path::PathBuf::from(appdata).join("Cursor"))
    }

    pub(crate) fn state_db_path() -> Result<std::path::PathBuf> {
        Ok(Self::user_data_dir()?
            .join("User")
            .join("globalStorage")
            .join("state.vscdb"))
    }

    /// Abre o valor guardado: decifra o envelope do `safeStorage` ou, em
    /// versões anteriores ao 3.27, devolve o JWT em claro como veio.
    ///
    /// A chave chega por closure para só tocar o DPAPI quando há o que
    /// decifrar — e para os testes não dependerem do `Local State` real.
    fn abre_token(
        guardado: &str,
        chave: impl FnOnce() -> Result<Zeroizing<Vec<u8>>>,
    ) -> Result<Zeroizing<String>> {
        let Some(envelope) = guardado.strip_prefix(SAFE_STORAGE_PREFIX) else {
            return Ok(Zeroizing::new(guardado.to_owned()));
        };
        // Versão nova é erro com nome, nunca o envelope mandado como bearer.
        let Some(b64) = envelope.strip_prefix(SAFE_STORAGE_V1) else {
            let versao = envelope.split(':').next().unwrap_or_default();
            bail!("token do Cursor em formato {versao:?} — esquema de cifra não suportado");
        };
        let blob = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("token cifrado do Cursor não é base64")?;
        let claro = decrypt_v10(&chave()?, &blob).context("decifrando o token do Cursor")?;
        let token = std::str::from_utf8(&claro)
            .map_err(|_| anyhow!("token decifrado do Cursor não é texto"))?;
        Ok(Zeroizing::new(token.to_owned()))
    }

    /// Lê o access token sem copiar o banco e sem bloquear o Cursor.
    pub(crate) fn access_token() -> Result<Zeroizing<String>> {
        let path = Self::state_db_path()?;
        if !path.exists() {
            return Err(anyhow!("Cursor não instalado ({})", path.display()));
        }
        let uri = format!(
            "file:///{}?mode=ro",
            path.to_string_lossy().replace('\\', "/")
        );
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| "abrindo state.vscdb somente-leitura")?;

        let guardado: Zeroizing<String> = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
                [],
                |row| row.get(0),
            )
            .map(Zeroizing::new)
            .map_err(|_| anyhow!("token não encontrado — faça login no Cursor"))?;

        if guardado.is_empty() {
            return Err(anyhow!("token vazio — faça login no Cursor"));
        }
        Self::abre_token(&guardado, || master_key(&Self::user_data_dir()?))
    }

    async fn rpc<T: for<'de> Deserialize<'de>>(
        &self,
        token: &str,
        method: &str,
    ) -> Result<T> {
        let resp = self
            .client
            .post(format!("{RPC_BASE}/{method}"))
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header("connect-protocol-version", "1")
            .body("{}")
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!("401 em {method} — refaça o login no Cursor"));
        }
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(RateLimited(retry_after_seconds(resp.headers())).into());
        }
        Ok(resp.error_for_status()?.json().await?)
    }

    fn epoch_ms(raw: &str) -> Option<DateTime<Utc>> {
        raw.parse::<i64>()
            .ok()
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
    }

    fn gauge(
        id: &str,
        label: &str,
        percent: f64,
        resets_at: Option<DateTime<Utc>>,
        window_seconds: Option<i64>,
        now: DateTime<Utc>,
    ) -> Gauge {
        let fraction = (percent / 100.0).clamp(0.0, 1.0);
        Gauge {
            id: id.into(),
            label: label.into(),
            fraction: Some(fraction),
            // A própria IDE arredonda para inteiro; espelhamos para bater na conferência.
            headline: format!("{}%", percent.round() as i64),
            subtitle: resets_at.map(|r| reset_label(r, now)),
            severity: Severity::from_fraction(Some(fraction)),
            resets_at,
            active: true,
            expected: expected_fraction(resets_at, window_seconds, now),
        }
    }

    async fn fetch(&self) -> Result<ProviderSample> {
        let token = Self::access_token()?;
        let usage: CurrentPeriodUsage = self.rpc(&token, "GetCurrentPeriodUsage").await?;

        // O plano é cosmético; se falhar, seguimos com os medidores.
        let plan = match cached_plan() {
            Some(hit) => hit,
            None => {
                let novo = self
                    .rpc::<PlanInfoResponse>(&token, "GetPlanInfo")
                    .await
                    .ok()
                    .and_then(|r| r.plan_info)
                    .map(|p| match p.price {
                        Some(price) => format!("{} ({price})", p.plan_name.unwrap_or_default()),
                        None => p.plan_name.unwrap_or_default(),
                    });
                cache_plan(novo.clone());
                novo
            }
        };

        let now = Utc::now();
        let gauges = Self::build_gauges(&usage, now)?;

        Ok(ProviderSample {
            provider: Provider::Cursor,
            plan,
            gauges,
            observed_at: now,
            source_at: Some(now),
            error: None,
            retry_after: None,
        })
    }

    /// Puro: separado de `fetch` para ser testado contra o JSON da API.
    fn build_gauges(usage: &CurrentPeriodUsage, now: DateTime<Utc>) -> Result<Vec<Gauge>> {
        let resets_at = usage.billing_cycle_end.as_deref().and_then(Self::epoch_ms);
        let starts_at = usage.billing_cycle_start.as_deref().and_then(Self::epoch_ms);
        let window_seconds = match (starts_at, resets_at) {
            (Some(i), Some(f)) => Some((f - i).num_seconds()),
            _ => None,
        };
        let pu = usage
            .plan_usage
            .as_ref()
            .ok_or_else(|| anyhow!("resposta sem planUsage"))?;

        let mut gauges = Vec::new();
        if let Some(p) = pu.auto_percent_used {
            gauges.push(Self::gauge(
                "cursor.auto",
                "Cursor Models",
                p,
                resets_at,
                window_seconds,
                now,
            ));
        }
        if let Some(p) = pu.api_percent_used {
            gauges.push(Self::gauge(
                "cursor.api",
                "Other Models",
                p,
                resets_at,
                window_seconds,
                now,
            ));
        }
        if gauges.is_empty() {
            return Err(anyhow!("resposta sem percentuais de uso"));
        }

        // Gasto do ciclo entra como contexto, não como barra: não há teto
        // declarado que torne uma fração honesta.
        if let Some(cents) = pu.total_spend {
            gauges.push(Gauge {
                id: "cursor.spend".into(),
                label: "Gasto no ciclo".into(),
                fraction: None,
                headline: format!("US$ {}", format_pt_br(cents / 100.0)),
                subtitle: None,
                severity: Severity::Normal,
                resets_at,
                active: true,
                // Gasto acumulado não tem teto; um marcador aqui sugeriria
                // um limite que não existe.
                expected: None,
            });
        }

        Ok(gauges)
    }
}

impl Collector for CursorCollector {
    fn provider(&self) -> Provider {
        Provider::Cursor
    }

    async fn sample(&self) -> ProviderSample {
        match self.fetch().await {
            Ok(s) => s,
            Err(e) => match e.downcast_ref::<RateLimited>() {
                Some(rl) => ProviderSample::rate_limited(Provider::Cursor, rl.0),
                None => ProviderSample::failed(Provider::Cursor, e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture no formato de `GetCurrentPeriodUsage` (plano Team).
    /// Os percentuais devem bater com o arredondamento da IDE.
    const SAMPLE_RESPONSE: &str = r#"{
      "billingCycleStart":"1786467815000","billingCycleEnd":"1789146215000",
      "planUsage":{"totalSpend":4200,"includedSpend":2000,"bonusSpend":2200,
                   "limit":2000,"autoPercentUsed":16.14666666666667,
                   "apiPercentUsed":0,"totalPercentUsed":13.5632},
      "spendLimitUsage":{"pooledUsed":0,"limitType":"team"},
      "displayThreshold":200}"#;

    fn gauges() -> Vec<Gauge> {
        let usage: CurrentPeriodUsage = serde_json::from_str(SAMPLE_RESPONSE).unwrap();
        CursorCollector::build_gauges(&usage, Utc::now()).unwrap()
    }

    /// As duas barras da IDE: "Cursor Models" e "Other Models".
    #[test]
    fn espelha_as_barras_da_ide() {
        let g = gauges();
        let auto = g.iter().find(|x| x.label == "Cursor Models").unwrap();
        let api = g.iter().find(|x| x.label == "Other Models").unwrap();
        assert_eq!(auto.headline, "16%", "a IDE arredonda 16.147 para 16%");
        assert_eq!(api.headline, "0%");
    }

    /// 0% é um valor legítimo — não pode ser tratado como ausente e sumir.
    #[test]
    fn percentual_zero_ainda_vira_barra() {
        assert!(gauges().iter().any(|x| x.label == "Other Models"));
    }

    /// O gasto não tem teto declarado; virar barra seria inventar uma fração.
    #[test]
    fn gasto_do_ciclo_e_contexto_nao_barra() {
        let g = gauges();
        let spend = g.iter().find(|x| x.id == "cursor.spend").unwrap();
        assert_eq!(spend.fraction, None);
        assert_eq!(spend.headline, "US$ 42,00");
    }

    #[test]
    fn fim_do_ciclo_vira_data_de_reset() {
        let g = gauges();
        let auto = g.iter().find(|x| x.id == "cursor.auto").unwrap();
        assert!(auto.resets_at.is_some(), "billingCycleEnd deve virar resets_at");
    }

    /// Uma conta sem os percentuais não pode gerar barras falsas.
    /// Aqui a janela é exata: a resposta traz início E fim do ciclo.
    #[test]
    fn marcador_usa_o_ciclo_completo_de_faturamento() {
        let g = gauges();
        let auto = g.iter().find(|x| x.id == "cursor.auto").unwrap();
        let e = auto.expected.expect("ciclo conhecido deve gerar marcador");
        assert!((0.0..=1.0).contains(&e), "e={e}");
    }

    /// Gasto acumulado não tem teto; marcá-lo sugeriria um limite inexistente.
    #[test]
    fn gasto_do_ciclo_nao_tem_marcador() {
        let g = gauges();
        let spend = g.iter().find(|x| x.id == "cursor.spend").unwrap();
        assert!(spend.expected.is_none());
    }

    /// Sem o início do ciclo a duração é desconhecida — sem marcador.
    #[test]
    fn sem_inicio_de_ciclo_nao_ha_marcador() {
        let json = r#"{"billingCycleEnd":"1789146215000",
          "planUsage":{"autoPercentUsed":16.0,"apiPercentUsed":0}}"#;
        let usage: CurrentPeriodUsage = serde_json::from_str(json).unwrap();
        let g = CursorCollector::build_gauges(&usage, Utc::now()).unwrap();
        assert!(g.iter().all(|x| x.expected.is_none()));
    }

    const CHAVE: [u8; 32] = [5u8; 32];
    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.assinatura";

    fn chave_de_teste() -> Result<Zeroizing<Vec<u8>>> {
        Ok(Zeroizing::new(CHAVE.to_vec()))
    }

    fn envelope(versao: &str, claro: &str) -> String {
        let blob = crate::collect::win_secret::tests::cifra(&CHAVE, [1u8; 12], claro.as_bytes());
        format!(
            "cursor-safe-storage:{versao}:{}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        )
    }

    /// Cursor 3.27+: o valor do `state.vscdb` é o envelope, e o bearer
    /// precisa ser o JWT de dentro dele.
    #[test]
    fn decifra_o_envelope_do_safe_storage() {
        let token = CursorCollector::abre_token(&envelope("v1", JWT), chave_de_teste).unwrap();
        assert_eq!(token.as_str(), JWT);
    }

    /// Antes do 3.27 o JWT fica em claro — e aí o DPAPI nem é tocado.
    #[test]
    fn token_em_claro_passa_sem_tocar_na_chave() {
        let token =
            CursorCollector::abre_token(JWT, || panic!("não deveria ler a chave")).unwrap();
        assert_eq!(token.as_str(), JWT);
    }

    /// Um `v2` futuro tem que falhar com nome, não virar bearer e dar 401.
    #[test]
    fn versao_desconhecida_do_envelope_falha_com_nome() {
        let erro = CursorCollector::abre_token(&envelope("v2", JWT), chave_de_teste)
            .unwrap_err()
            .to_string();
        assert!(erro.contains("v2"), "o erro precisa dizer o que viu: {erro}");
    }

    #[test]
    fn envelope_com_chave_errada_nao_decifra() {
        let erro = CursorCollector::abre_token(&envelope("v1", JWT), || {
            Ok(Zeroizing::new(vec![9u8; 32]))
        });
        assert!(erro.is_err());
    }

    #[test]
    fn resposta_sem_percentuais_falha_explicitamente() {
        let usage: CurrentPeriodUsage =
            serde_json::from_str(r#"{"planUsage":{"totalSpend":100}}"#).unwrap();
        assert!(CursorCollector::build_gauges(&usage, Utc::now()).is_err());
    }
}
