//! Alertas por limiar.
//!
//! A regra que importa: **notifica na subida, uma vez por travessia**. Um
//! medidor oscilando em torno de 80% não pode gerar uma notificação por
//! ciclo — isso treina o usuário a ignorar o alerta, que é o oposto do
//! objetivo. O estado só rearma quando o valor cai abaixo do limiar menos
//! uma histerese.

use ia_monitor_core::model::ProviderSample;
use std::collections::HashMap;

/// Margem de rearme. Sem ela, ruído de 0,5% em volta do limiar dispara sem
/// parar.
const HYSTERESIS: f64 = 0.03;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Threshold {
    pub fraction: f64,
    pub label: &'static str,
}

/// Dois degraus: um aviso com tempo de reagir e um alerta de que acabou.
pub const DEFAULT_THRESHOLDS: [Threshold; 2] = [
    Threshold { fraction: 0.80, label: "80%" },
    Threshold { fraction: 0.95, label: "95%" },
];

#[derive(Debug, Clone)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// Guarda qual degrau já foi anunciado para cada medidor.
#[derive(Default)]
pub struct AlertState {
    armed: HashMap<String, usize>,
}

impl AlertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Avalia a amostra e devolve só as notificações inéditas.
    pub fn evaluate(&mut self, samples: &[ProviderSample]) -> Vec<Notice> {
        self.evaluate_with(samples, &DEFAULT_THRESHOLDS)
    }

    pub fn evaluate_with(
        &mut self,
        samples: &[ProviderSample],
        thresholds: &[Threshold],
    ) -> Vec<Notice> {
        let mut out = Vec::new();

        for sample in samples {
            // Provedor indisponível não gera alerta de cota: o número que
            // temos é velho, e alertar sobre ele seria inventar um fato.
            if sample.error.is_some() {
                continue;
            }
            for gauge in &sample.gauges {
                let Some(fraction) = gauge.fraction else {
                    continue;
                };
                let key = gauge.id.clone();

                // Degrau mais alto ultrapassado (0 = nenhum).
                let reached = thresholds
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| fraction >= t.fraction)
                    .map(|(i, _)| i + 1)
                    .max()
                    .unwrap_or(0);

                let previous = *self.armed.get(&key).unwrap_or(&0);

                if reached > previous {
                    let t = thresholds[reached - 1];
                    out.push(Notice {
                        title: format!("{} · {}", sample.provider.label(), gauge.label),
                        body: match &gauge.subtitle {
                            Some(s) => format!("{} de {} — {s}", gauge.headline, t.label),
                            None => format!("{} de {}", gauge.headline, t.label),
                        },
                    });
                    self.armed.insert(key, reached);
                } else if reached < previous {
                    // Só rearma depois de cair com folga: evita pingue-pongue
                    // no limiar. Um reset de janela cai muito abaixo e rearma.
                    let floor = thresholds
                        .get(previous - 1)
                        .map(|t| t.fraction - HYSTERESIS)
                        .unwrap_or(0.0);
                    if fraction < floor {
                        self.armed.insert(key, reached);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ia_monitor_core::model::{Gauge, Provider, Severity};

    fn sample(fraction: f64) -> Vec<ProviderSample> {
        vec![ProviderSample {
            provider: Provider::Claude,
            plan: None,
            gauges: vec![Gauge {
                id: "claude.session".into(),
                label: "Sessão 5h".into(),
                fraction: Some(fraction),
                headline: format!("{}%", (fraction * 100.0).round() as i64),
                subtitle: Some("reseta em 1h".into()),
                severity: Severity::Warn,
                resets_at: None,
                active: true,
                expected: None,
            }],
            observed_at: Utc::now(),
            source_at: Some(Utc::now()),
            error: None,
            retry_after: None,
        }]
    }

    #[test]
    fn abaixo_do_limiar_nao_notifica() {
        let mut s = AlertState::new();
        assert!(s.evaluate(&sample(0.5)).is_empty());
    }

    #[test]
    fn notifica_ao_cruzar_o_limiar() {
        let mut s = AlertState::new();
        let n = s.evaluate(&sample(0.82));
        assert_eq!(n.len(), 1);
        assert!(n[0].title.contains("Sessão 5h"));
        assert!(n[0].body.contains("82%"));
    }

    /// O comportamento que decide se o alerta é útil ou vira ruído.
    #[test]
    fn nao_repete_o_alerta_a_cada_ciclo() {
        let mut s = AlertState::new();
        assert_eq!(s.evaluate(&sample(0.82)).len(), 1);
        assert!(s.evaluate(&sample(0.83)).is_empty());
        assert!(s.evaluate(&sample(0.85)).is_empty());
    }

    #[test]
    fn segundo_degrau_gera_novo_alerta() {
        let mut s = AlertState::new();
        s.evaluate(&sample(0.82));
        let n = s.evaluate(&sample(0.96));
        assert_eq!(n.len(), 1, "95% é um fato novo, não repetição");
        assert!(n[0].body.contains("95%"));
    }

    /// Oscilar em volta do limiar não pode rearmar o alerta.
    #[test]
    fn histerese_impede_pingue_pongue() {
        let mut s = AlertState::new();
        s.evaluate(&sample(0.81));
        assert!(s.evaluate(&sample(0.79)).is_empty(), "queda mínima não rearma");
        assert!(s.evaluate(&sample(0.82)).is_empty(), "e não redispara");
    }

    /// Depois de um reset de janela o alerta precisa voltar a funcionar.
    #[test]
    fn reset_da_janela_rearma_o_alerta() {
        let mut s = AlertState::new();
        s.evaluate(&sample(0.82));
        s.evaluate(&sample(0.05)); // janela zerou
        assert_eq!(s.evaluate(&sample(0.85)).len(), 1, "novo ciclo, novo alerta");
    }

    /// Alertar com base num número velho seria afirmar algo que não sabemos.
    #[test]
    fn provedor_indisponivel_nao_gera_alerta() {
        let mut s = AlertState::new();
        let falha = vec![ProviderSample::failed(Provider::Cursor, "offline")];
        assert!(s.evaluate(&falha).is_empty());
    }

    /// Medidor sem teto (créditos ilimitados) não tem limiar que faça sentido.
    #[test]
    fn medidor_sem_fracao_e_ignorado() {
        let mut s = AlertState::new();
        let mut amostra = sample(0.9);
        amostra[0].gauges[0].fraction = None;
        assert!(s.evaluate(&amostra).is_empty());
    }
}
