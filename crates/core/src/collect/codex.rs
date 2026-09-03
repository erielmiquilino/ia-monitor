//! Codex — leitura passiva dos rollouts. O servidor devolve o estado da cota
//! a cada turno e o CLI grava isso em `~/.codex/sessions/AAAA/MM/DD/`.
//!
//! Por isso o dado é "última leitura conhecida", não tempo real: `source_at`
//! carrega o instante do evento para a UI dizer a verdade sobre a idade dele.
//!
//! Nunca usamos o `refresh_token` de `auth.json`: a rotação invalidaria o
//! token do CLI e quebraria o Codex do usuário.

use crate::collect::{home_dir, Collector};
use crate::model::{
    expected_fraction, format_pt_br, reset_label, Gauge, Provider, ProviderSample, Severity,
};
use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Quanto lemos do fim de cada arquivo antes de desistir. Um `token_count`
/// fica sempre perto do fim de uma sessão encerrada.
const TAIL_CHUNK: u64 = 256 * 1024;
const TAIL_MAX: u64 = 8 * 1024 * 1024;
/// Sessões mais recentes a inspecionar — uma sessão retomada atualiza o mtime
/// de um arquivo antigo, então não basta olhar só o primeiro.
const FILES_TO_SCAN: usize = 5;
/// Sem teto declarado pela API, a barra precisa de uma referência.
const DEFAULT_CREDIT_BASELINE: f64 = 1500.0;

#[derive(Deserialize, Debug)]
struct Window {
    used_percent: f64,
    window_minutes: i64,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Deserialize, Debug)]
struct Credits {
    #[serde(default)]
    unlimited: Option<bool>,
    #[serde(default)]
    balance: Option<String>,
}

#[derive(Deserialize, Debug)]
struct RateLimits {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    primary: Option<Window>,
    #[serde(default)]
    secondary: Option<Window>,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
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

pub struct CodexCollector {
    credit_baseline: f64,
}

impl Default for CodexCollector {
    fn default() -> Self {
        Self { credit_baseline: DEFAULT_CREDIT_BASELINE }
    }
}

impl CodexCollector {
    pub fn new(credit_baseline: Option<f64>) -> Self {
        Self { credit_baseline: credit_baseline.unwrap_or(DEFAULT_CREDIT_BASELINE) }
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

    fn window_gauge(id: &str, w: &Window, now: DateTime<Utc>) -> Gauge {
        let fraction = (w.used_percent / 100.0).clamp(0.0, 1.0);
        let resets_at = w.resets_at.and_then(|s| Utc.timestamp_opt(s, 0).single());
        let label = match w.window_minutes {
            300 => "Sessão 5h".to_string(),
            10_080 => "Semana".to_string(),
            m if m % 1440 == 0 => format!("{} dias", m / 1440),
            m => format!("{m} min"),
        };
        Gauge {
            id: id.into(),
            label,
            fraction: Some(fraction),
            headline: format!("{}%", w.used_percent.round() as i64),
            subtitle: resets_at.map(|r| reset_label(r, now)),
            severity: Severity::from_fraction(Some(fraction)),
            resets_at,
            active: true,
            // `window_minutes` vem no evento: a janela é exata.
            expected: expected_fraction(resets_at, Some(w.window_minutes * 60), now),
        }
    }

    fn fetch(&self) -> Result<ProviderSample> {
        let dir = Self::sessions_dir()?;
        let mut files = Vec::new();
        Self::collect_rollouts(&dir, &mut files);
        if files.is_empty() {
            return Err(anyhow!("nenhum rollout encontrado — rode o Codex ao menos uma vez"));
        }
        files.sort_by(|a, b| b.0.cmp(&a.0));

        // O mais recente por mtime nem sempre tem o evento mais novo:
        // retomar uma sessão antiga atualiza o arquivo dela.
        let newest = files
            .iter()
            .take(FILES_TO_SCAN)
            .filter_map(|(_, p)| Self::last_token_count(p))
            .max_by_key(|(ts, _)| *ts);

        let (source_at, limits) =
            newest.ok_or_else(|| anyhow!("nenhum evento de cota nos rollouts recentes"))?;

        self.build_sample(source_at, &limits, Utc::now())
    }

    /// Puro: separado da varredura de arquivos para ser testado contra
    /// eventos reais dos dois formatos de plano (crédito e janela).
    fn build_sample(
        &self,
        source_at: DateTime<Utc>,
        limits: &RateLimits,
        now: DateTime<Utc>,
    ) -> Result<ProviderSample> {
        let mut gauges = Vec::new();

        // Planos com janela (plus): o servidor já dá o percentual.
        if let Some(w) = &limits.primary {
            gauges.push(Self::window_gauge("codex.primary", w, now));
        }
        if let Some(w) = &limits.secondary {
            gauges.push(Self::window_gauge("codex.secondary", w, now));
        }

        // Planos por crédito (business): não há teto, então a fração é
        // derivada de uma baseline — e nunca deixamos a barra estourar.
        if let Some(credits) = &limits.credits {
            if credits.unlimited.unwrap_or(false) {
                gauges.push(Gauge {
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
            } else if let Some(balance) =
                credits.balance.as_deref().and_then(|b| b.parse::<f64>().ok())
            {
                let baseline = self.credit_baseline.max(balance);
                let remaining = (balance / baseline).clamp(0.0, 1.0);
                let used = 1.0 - remaining;
                gauges.push(Gauge {
                    id: "codex.credits".into(),
                    label: "Créditos".into(),
                    // A barra mede consumo, como as outras: 1 - restante.
                    fraction: Some(used),
                    headline: format_pt_br(balance),
                    subtitle: Some(format!("de ~{}", format_pt_br(baseline))),
                    severity: Severity::from_fraction(Some(used)),
                    resets_at: None,
                    active: true,
                    // Saldo de crédito não reseta: não há "onde eu deveria
                    // estar" nesta altura do mês.
                    expected: None,
                });
            }
        }

        if gauges.is_empty() {
            return Err(anyhow!("evento de cota sem créditos nem janelas"));
        }

        // Saldo esgotado é estado de alarme, independente do que a barra diga.
        if let Some(reason) = &limits.rate_limit_reached_type {
            for g in &mut gauges {
                g.severity = Severity::Critical;
                g.subtitle = Some(format!("limite atingido ({reason})"));
            }
        }

        Ok(ProviderSample {
            provider: Provider::Codex,
            plan: limits.plan_type.clone(),
            gauges,
            observed_at: now,
            source_at: Some(source_at),
            error: None,
            retry_after: None,
        })
    }
}

impl Collector for CodexCollector {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    async fn sample(&self) -> ProviderSample {
        match self.fetch() {
            Ok(s) => s,
            Err(e) => ProviderSample::failed(Provider::Codex, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(json: &str) -> RateLimits {
        serde_json::from_str(json).unwrap()
    }

    fn sample(json: &str) -> ProviderSample {
        CodexCollector::default()
            .build_sample(Utc::now(), &limits(json), Utc::now())
            .unwrap()
    }

    /// Evento real do plano business: sem janelas, só saldo de crédito.
    const BUSINESS: &str = r#"{"limit_id":"codex","limit_name":null,"primary":null,
      "secondary":null,"credits":{"has_credits":true,"unlimited":false,
      "balance":"1131.4605596065521"},"plan_type":"business",
      "rate_limit_reached_type":null}"#;

    /// Evento real de um plano plus antigo: janelas de 5h e 7 dias.
    const PLUS: &str = r#"{"limit_id":"codex","primary":{"used_percent":6.0,
      "window_minutes":300,"resets_at":1777054045},"secondary":{"used_percent":1.0,
      "window_minutes":10080,"resets_at":1777640845},"credits":null,
      "plan_type":"plus","rate_limit_reached_type":null}"#;

    #[test]
    fn saldo_business_vira_medidor_de_credito() {
        let s = sample(BUSINESS);
        let g = s.gauges.iter().find(|x| x.id == "codex.credits").unwrap();
        assert_eq!(g.headline, "1.131,46");
        assert_eq!(s.plan.as_deref(), Some("business"));
    }

    /// A barra mede CONSUMO, como as demais. Saldo alto = barra vazia.
    /// Inverter isso faria a UI gritar quando está tudo bem.
    #[test]
    fn barra_de_credito_mede_consumo_nao_saldo() {
        let s = sample(BUSINESS);
        let g = s.gauges.iter().find(|x| x.id == "codex.credits").unwrap();
        let f = g.fraction.unwrap();
        // 1131 de 1500 => ~75% restante => ~25% consumido.
        assert!((f - 0.246).abs() < 0.01, "fração {f} deveria ser ~0.246");
        assert_eq!(g.severity, Severity::Normal);
    }

    /// Um saldo acima da baseline não pode gerar fração negativa.
    #[test]
    fn saldo_acima_da_baseline_nao_estoura_a_barra() {
        let s = sample(
            r#"{"credits":{"unlimited":false,"balance":"9999"},"plan_type":"business"}"#,
        );
        let f = s.gauges[0].fraction.unwrap();
        assert!((0.0..=1.0).contains(&f), "fração fora de 0..1: {f}");
    }

    /// Planos antigos ainda existem no histórico; o parser cobre os dois.
    #[test]
    fn plano_com_janelas_usa_percentual_do_servidor() {
        let s = sample(PLUS);
        let primary = s.gauges.iter().find(|x| x.id == "codex.primary").unwrap();
        assert_eq!(primary.label, "Sessão 5h");
        assert_eq!(primary.headline, "6%");
        let secondary = s.gauges.iter().find(|x| x.id == "codex.secondary").unwrap();
        assert_eq!(secondary.label, "Semana");
    }

    /// Estado real já visto no histórico deste usuário.
    #[test]
    fn credito_esgotado_vira_critico() {
        let s = sample(
            r#"{"limit_id":"premium","credits":{"has_credits":false,"unlimited":false,
             "balance":"0"},"plan_type":"business",
             "rate_limit_reached_type":"workspace_member_credits_depleted"}"#,
        );
        assert!(s.gauges.iter().all(|g| g.severity == Severity::Critical));
    }

    /// `window_minutes` vem no evento: a janela é exata, sem convenção.
    #[test]
    fn janela_do_plano_plus_gera_marcador() {
        let s = sample(PLUS);
        let primary = s.gauges.iter().find(|x| x.id == "codex.primary").unwrap();
        assert!(primary.expected.is_some());
    }

    /// Saldo de crédito não reseta: não existe "onde eu deveria estar".
    #[test]
    fn credito_nao_tem_marcador_de_ritmo() {
        let s = sample(BUSINESS);
        let c = s.gauges.iter().find(|x| x.id == "codex.credits").unwrap();
        assert!(c.expected.is_none());
    }

    #[test]
    fn plano_ilimitado_nao_inventa_fracao() {
        let s = sample(r#"{"credits":{"unlimited":true,"balance":null},"plan_type":"business"}"#);
        assert_eq!(s.gauges[0].fraction, None);
        assert_eq!(s.gauges[0].headline, "ilimitado");
    }

    /// Sem cota nenhuma no evento, é erro — não uma barra vazia enganosa.
    #[test]
    fn evento_sem_cota_falha() {
        let c = CodexCollector::default();
        let l = limits(r#"{"plan_type":"business"}"#);
        assert!(c.build_sample(Utc::now(), &l, Utc::now()).is_err());
    }
}
