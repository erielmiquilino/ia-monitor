//! Modelo de visão enviado ao webview.
//!
//! A UI não faz conta nem formata número: recebe texto pronto. Isso mantém
//! uma única definição de "17%" e "reseta em 2h19m" — a do Rust, que é a
//! mesma usada pelos alertas e pelo ícone da bandeja.

use chrono::{DateTime, Duration, Utc};
use ia_monitor_core::analytics;
use ia_monitor_core::model::{
    disambiguate_labels, format_pt_br, humanize_until, pace_delta, Gauge, Provider,
    ProviderSample, Severity,
};
use ia_monitor_core::store::{GroupBy, Store};
use serde::Serialize;
use std::collections::HashMap;

/// Acima disso o dado deixa de ser "agora" e a UI precisa dizer a idade.
const STALE_AFTER_SECONDS: i64 = 120;
const TOP_PROJECTS: usize = 5;
const HISTORY_DAYS: i64 = 7;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SampleView {
    pub provider_label: String,
    pub plan: Option<String>,
    pub gauges: Vec<Gauge>,
    pub error: Option<String>,
    /// Preenchido só quando o dado é velho o bastante para importar.
    pub age_text: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub label: String,
    pub path: String,
    pub value: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotView {
    pub samples: Vec<SampleView>,
    /// id do medidor -> nota legível ("12 pts acima do ritmo · estoura em 2h13m").
    pub burn: HashMap<String, String>,
    pub top_projects: Vec<ProjectView>,
    pub updated_text: String,
    pub status_text: String,
}

/// Linha de idade e motivo. É o que impede a UI de apresentar um número
/// antigo como se fosse ao vivo.
fn age_text(sample: &ProviderSample) -> Option<String> {
    let mut partes: Vec<String> = Vec::new();

    if let Some(source) = sample.source_at {
        if (sample.observed_at - source).num_seconds() >= STALE_AFTER_SECONDS {
            partes.push(format!(
                "dado de {} atrás",
                humanize_until(sample.observed_at, source)
            ));
            // O Codex é passivo por natureza; os outros dois só ficam para
            // trás quando algo falha, e aí o motivo vem logo abaixo.
            if sample.provider == Provider::Codex {
                partes.push("o Codex só atualiza quando roda".to_string());
            }
        }
    }

    if let Some(espera) = sample.retry_after {
        partes.push(format!(
            "nova tentativa em {}",
            humanize_until(
                sample.observed_at + Duration::seconds(espera.max(0)),
                sample.observed_at
            )
        ));
    }

    if partes.is_empty() {
        None
    } else {
        Some(partes.join(" — "))
    }
}

/// Diferença mínima, em pontos percentuais, para valer a pena comentar o
/// ritmo. Abaixo disso o marcador na barra já diz tudo, e o texto viraria
/// ruído a cada ciclo.
const PACE_NOTICE_POINTS: f64 = 5.0;

/// Nota de cada medidor: o quanto está fora do ritmo e, quando a série
/// sustenta, a projeção de esgotamento.
///
/// Nada aqui é chute: a projeção só aparece com amostras suficientes, e o
/// ritmo só quando a janela é conhecida.
fn note_texts(
    store: &Store,
    samples: &[ProviderSample],
    now: DateTime<Utc>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let since = now - Duration::days(2);

    for sample in samples {
        for gauge in &sample.gauges {
            if gauge.fraction.is_none() {
                continue;
            }
            let mut parts: Vec<String> = Vec::new();

            if let Some(delta) = pace_delta(gauge) {
                if delta.abs() >= PACE_NOTICE_POINTS {
                    let lado = if delta > 0.0 { "acima" } else { "abaixo" };
                    parts.push(format!(
                        "{} pts {lado} do ritmo",
                        format_pt_br(delta.abs()).replace(",00", "")
                    ));
                }
            }

            if let Ok(series) = store.series(sample.provider, &gauge.id, since) {
                if let Some(burn) = analytics::burn(&series, now, gauge.resets_at) {
                    parts.push(analytics::describe(&burn, now));
                }
            }

            if !parts.is_empty() {
                out.insert(gauge.id.clone(), parts.join(" · "));
            }
        }
    }
    out
}

fn top_projects(store: &Store, now: DateTime<Utc>) -> Vec<ProjectView> {
    let since = now - Duration::days(HISTORY_DAYS);
    let totals = match store.totals_by(GroupBy::Project, since) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<_> = totals
        .into_iter()
        .filter(|t| !t.key.is_empty())
        .take(TOP_PROJECTS)
        .collect();
    let keys: Vec<String> = rows.iter().map(|t| t.key.clone()).collect();

    rows.iter()
        .zip(disambiguate_labels(&keys))
        .map(|(t, label)| ProjectView {
            label,
            path: t.key.clone(),
            value: compact_tokens(t.input_tokens + t.output_tokens),
        })
        .collect()
}

/// "19,9 M" cabe onde "19.935.157" não cabe.
fn compact_tokens(n: i64) -> String {
    let v = n as f64;
    if v >= 1_000_000.0 {
        format!("{} M", format_pt_br(v / 1_000_000.0))
    } else if v >= 1_000.0 {
        format!("{} k", format_pt_br(v / 1_000.0))
    } else {
        n.to_string()
    }
}

pub fn build(store: &Store, samples: &[ProviderSample], now: DateTime<Utc>) -> SnapshotView {
    let failed: Vec<&str> = samples
        .iter()
        .filter(|s| s.error.is_some())
        .map(|s| s.provider.label())
        .collect();

    let status_text = if failed.is_empty() {
        format!("{} provedores · dados locais", samples.len())
    } else {
        format!("indisponível: {}", failed.join(", "))
    };

    SnapshotView {
        samples: samples
            .iter()
            .map(|s| SampleView {
                provider_label: s.provider.label().to_string(),
                plan: s.plan.clone(),
                gauges: s.gauges.clone(),
                error: s.error.clone(),
                age_text: age_text(s),
            })
            .collect(),
        burn: note_texts(store, samples, now),
        top_projects: top_projects(store, now),
        updated_text: format!("{}", now.with_timezone(&chrono::Local).format("%H:%M")),
        status_text,
    }
}

/// Pior severidade entre todos os provedores — o que a bandeja precisa saber.
pub fn worst(samples: &[ProviderSample]) -> Severity {
    let rank = |s: Severity| match s {
        Severity::Normal => 0,
        Severity::Unknown => 1,
        Severity::Warn => 2,
        Severity::Critical => 3,
    };
    samples
        .iter()
        .map(|s| s.worst_severity())
        .max_by_key(|s| rank(*s))
        .unwrap_or(Severity::Unknown)
}

/// Fração representativa de cada provedor, na ordem fixa Claude/Cursor/Codex.
/// Alimenta o desenho do ícone da bandeja.
pub fn tray_fractions(samples: &[ProviderSample]) -> Vec<(Severity, f64)> {
    Provider::ALL
        .iter()
        .map(|p| {
            let sample = samples.iter().find(|s| s.provider == *p);
            match sample {
                None => (Severity::Unknown, 0.0),
                // Sem medidor nenhum não há o que desenhar; com medidor
                // antigo, desenhamos o que sabemos.
                Some(s) if s.gauges.is_empty() => (Severity::Unknown, 0.0),
                Some(s) => {
                    let g = s
                        .gauges
                        .iter()
                        .filter(|g| g.fraction.is_some())
                        .max_by(|a, b| {
                            a.fraction
                                .unwrap_or(0.0)
                                .partial_cmp(&b.fraction.unwrap_or(0.0))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                    match g {
                        Some(g) => (g.severity, g.fraction.unwrap_or(0.0)),
                        None => (Severity::Unknown, 0.0),
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauge(id: &str, f: f64, sev: Severity) -> Gauge {
        Gauge {
            id: id.into(),
            label: id.into(),
            fraction: Some(f),
            headline: format!("{}%", (f * 100.0) as i64),
            subtitle: None,
            severity: sev,
            resets_at: None,
            active: true,
            expected: None,
        }
    }

    fn sample(p: Provider, gauges: Vec<Gauge>) -> ProviderSample {
        ProviderSample {
            provider: p,
            plan: None,
            gauges,
            observed_at: Utc::now(),
            source_at: Some(Utc::now()),
            error: None,
            retry_after: None,
        }
    }

    #[test]
    fn compacta_tokens_para_caber_na_tela() {
        assert_eq!(compact_tokens(19_935_157), "19,94 M");
        assert_eq!(compact_tokens(1_500), "1,50 k");
        assert_eq!(compact_tokens(42), "42");
    }

    /// O dado do Codex pode ser antigo; esconder isso seria apresentá-lo
    /// como se fosse ao vivo.
    #[test]
    fn idade_aparece_apenas_quando_o_dado_e_velho() {
        let mut s = sample(Provider::Codex, vec![]);
        assert!(age_text(&s).is_none(), "dado fresco não precisa de aviso");

        s.source_at = Some(s.observed_at - Duration::minutes(40));
        let texto = age_text(&s).expect("dado velho precisa avisar");
        assert!(texto.contains("40m"), "{texto}");
    }

    #[test]
    fn bandeja_recebe_um_valor_por_provedor_em_ordem_fixa() {
        let samples = vec![
            sample(Provider::Claude, vec![gauge("a", 0.3, Severity::Normal)]),
            sample(Provider::Cursor, vec![gauge("b", 0.8, Severity::Warn)]),
        ];
        let f = tray_fractions(&samples);
        assert_eq!(f.len(), 3, "sempre três posições, mesmo faltando provedor");
        assert_eq!(f[0].1, 0.3);
        assert_eq!(f[1].0, Severity::Warn);
        // Codex ausente vira desconhecido em vez de zero enganoso.
        assert_eq!(f[2].0, Severity::Unknown);
    }

    /// Dentro de um provedor, o medidor mais alto é o que representa.
    #[test]
    fn bandeja_usa_o_medidor_mais_alto_do_provedor() {
        let samples = vec![sample(
            Provider::Claude,
            vec![
                gauge("sessao", 0.2, Severity::Normal),
                gauge("semana", 0.91, Severity::Critical),
            ],
        )];
        assert_eq!(tray_fractions(&samples)[0].0, Severity::Critical);
    }

    #[test]
    fn provedor_em_falha_nao_vira_barra_zerada() {
        let samples = vec![ProviderSample::failed(Provider::Cursor, "offline")];
        let f = tray_fractions(&samples);
        assert_eq!(f[1].0, Severity::Unknown);
        assert_eq!(worst(&samples), Severity::Unknown);
    }

    #[test]
    fn severidade_agregada_e_a_pior_entre_provedores() {
        let samples = vec![
            sample(Provider::Claude, vec![gauge("a", 0.1, Severity::Normal)]),
            sample(Provider::Codex, vec![gauge("b", 0.95, Severity::Critical)]),
        ];
        assert_eq!(worst(&samples), Severity::Critical);
    }
}
