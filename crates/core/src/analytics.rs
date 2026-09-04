//! Burn rate e projeção de esgotamento.
//!
//! O cuidado central aqui é o **reset**. Uma janela de 5h volta a zero, e uma
//! regressão ingênua atravessando esse degrau produz inclinação negativa —
//! diria "você está recuperando cota", que é falso. Toda análise começa
//! recortando a série no último reset observado.

use chrono::{DateTime, Duration, Utc};

/// Um ponto da série: instante e valor da fração naquele instante.
pub type Point = (DateTime<Utc>, f64);

/// Quanto a série precisa cair para ser considerada um reset, e não ruído.
const RESET_DROP: f64 = 0.02;
/// Abaixo disso a inclinação é chute: dois pontos colados não são tendência.
const MIN_SPAN_MINUTES: i64 = 10;
const MIN_POINTS: usize = 3;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Burn {
    /// Unidades da série consumidas por hora (fração 0..1 para medidores).
    pub per_hour: f64,
    /// Extensão real da amostra usada, em horas.
    pub window_hours: f64,
    /// Quando a série atinge 1.0 no ritmo atual. `None` se o ritmo for nulo
    /// ou negativo, ou se o esgotamento cair depois do próximo reset.
    pub exhausted_at: Option<DateTime<Utc>>,
    /// Instante do último reset observado, quando houve um.
    pub last_reset: Option<DateTime<Utc>>,
}

/// Recorta a série a partir do último reset. É o que separa o ciclo atual
/// do passado já zerado.
fn current_cycle(points: &[Point]) -> (&[Point], Option<DateTime<Utc>>) {
    let mut start = 0usize;
    let mut reset_at = None;
    for i in 1..points.len() {
        if points[i].1 < points[i - 1].1 - RESET_DROP {
            start = i;
            reset_at = Some(points[i].0);
        }
    }
    (&points[start..], reset_at)
}

/// Inclinação por mínimos quadrados, em unidades por hora. Menos sensível a
/// um ponto fora da curva do que comparar apenas o primeiro com o último.
fn slope_per_hour(points: &[Point]) -> Option<f64> {
    let n = points.len() as f64;
    let t0 = points[0].0;
    let xs: Vec<f64> = points
        .iter()
        .map(|(t, _)| (*t - t0).num_seconds() as f64 / 3600.0)
        .collect();
    let ys: Vec<f64> = points.iter().map(|(_, v)| *v).collect();

    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..points.len() {
        num += (xs[i] - mean_x) * (ys[i] - mean_y);
        den += (xs[i] - mean_x).powi(2);
    }
    if den.abs() < f64::EPSILON {
        return None;
    }
    Some(num / den)
}

/// Calcula o ritmo de consumo do ciclo atual.
pub fn burn(
    points: &[Point],
    now: DateTime<Utc>,
    resets_at: Option<DateTime<Utc>>,
) -> Option<Burn> {
    if points.len() < MIN_POINTS {
        return None;
    }
    let (cycle, last_reset) = current_cycle(points);
    if cycle.len() < MIN_POINTS {
        return None;
    }

    let span = cycle.last()?.0 - cycle.first()?.0;
    if span < Duration::minutes(MIN_SPAN_MINUTES) {
        return None;
    }

    let per_hour = slope_per_hour(cycle)?;
    let current = cycle.last()?.1;

    let exhausted_at = if per_hour > 1e-6 && current < 1.0 {
        let hours_left = (1.0 - current) / per_hour;
        let at = now + Duration::seconds((hours_left * 3600.0) as i64);
        // Estourar depois do reset é o mesmo que não estourar.
        match resets_at {
            Some(r) if at >= r => None,
            _ => Some(at),
        }
    } else {
        None
    };

    Some(Burn {
        per_hour,
        window_hours: span.num_seconds() as f64 / 3600.0,
        exhausted_at,
        last_reset,
    })
}

/// "estoura em 2h13m", "no ritmo atual não estoura antes do reset".
pub fn describe(burn: &Burn, now: DateTime<Utc>) -> String {
    match burn.exhausted_at {
        Some(at) => format!("estoura em {}", crate::model::humanize_until(at, now)),
        None if burn.per_hour <= 1e-6 => "sem consumo recente".to_string(),
        None => "não estoura antes do reset".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn series(values: &[(i64, f64)]) -> Vec<Point> {
        values
            .iter()
            .map(|(min, v)| (Utc.timestamp_opt(min * 60, 0).unwrap(), *v))
            .collect()
    }

    #[test]
    fn ritmo_constante_e_medido_corretamente() {
        // 10% por hora ao longo de 3h.
        let s = series(&[(0, 0.0), (60, 0.10), (120, 0.20), (180, 0.30)]);
        let now = Utc.timestamp_opt(180 * 60, 0).unwrap();
        let b = burn(&s, now, None).unwrap();
        assert!((b.per_hour - 0.10).abs() < 1e-6, "per_hour={}", b.per_hour);
    }

    /// O caso que motiva o módulo: sem recortar no reset, a inclinação sai
    /// negativa e a UI diria que a cota está aumentando sozinha.
    #[test]
    fn reset_nao_produz_ritmo_negativo() {
        let s = series(&[
            (0, 0.80), (60, 0.90), (120, 0.95), // janela cheia
            (180, 0.05), (240, 0.15), (300, 0.25), // reset e nova janela
        ]);
        let now = Utc.timestamp_opt(300 * 60, 0).unwrap();
        let b = burn(&s, now, None).unwrap();
        assert!(b.per_hour > 0.0, "ritmo deveria ser positivo, veio {}", b.per_hour);
        assert!((b.per_hour - 0.10).abs() < 1e-6);
        assert_eq!(b.last_reset, Some(Utc.timestamp_opt(180 * 60, 0).unwrap()));
    }

    #[test]
    fn projeta_o_esgotamento() {
        let s = series(&[(0, 0.0), (60, 0.25), (120, 0.50)]);
        let now = Utc.timestamp_opt(120 * 60, 0).unwrap();
        let b = burn(&s, now, None).unwrap();
        // 50% restantes a 25%/h => 2h.
        let at = b.exhausted_at.unwrap();
        assert_eq!((at - now).num_minutes(), 120);
    }

    /// Estourar depois do reset é o mesmo que não estourar — alertar seria
    /// gerar ansiedade por um limite que zera antes.
    #[test]
    fn esgotamento_apos_o_reset_e_ignorado() {
        let s = series(&[(0, 0.0), (60, 0.05), (120, 0.10)]);
        let now = Utc.timestamp_opt(120 * 60, 0).unwrap();
        let reset = now + Duration::hours(1);
        let b = burn(&s, now, Some(reset)).unwrap();
        assert!(b.exhausted_at.is_none());
        assert_eq!(describe(&b, now), "não estoura antes do reset");
    }

    #[test]
    fn poucos_pontos_nao_viram_tendencia() {
        assert!(burn(&series(&[(0, 0.1), (60, 0.2)]), Utc::now(), None).is_none());
    }

    /// Amostras coladas no tempo dariam uma inclinação enorme e falsa.
    #[test]
    fn janela_curta_demais_e_rejeitada() {
        let s = vec![
            (Utc.timestamp_opt(0, 0).unwrap(), 0.10),
            (Utc.timestamp_opt(60, 0).unwrap(), 0.20),
            (Utc.timestamp_opt(120, 0).unwrap(), 0.30),
        ];
        assert!(burn(&s, Utc::now(), None).is_none());
    }

}
