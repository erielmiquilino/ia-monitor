//! `probe` — entregável da Fase 1. Imprime os medidores dos três provedores
//! no terminal para conferir contra as fontes de verdade (o `/usage` do
//! Claude Code, a tela Plan & Usage da IDE do Cursor, e o rodapé do Codex).

use chrono::Utc;
use ia_monitor_core::model::{humanize_until, ProviderSample, Severity};

const BAR_WIDTH: usize = 10;

/// Barra com o marcador de ritmo sobreposto: `|` mostra onde o consumo
/// estaria se fosse uniforme ao longo da janela.
fn bar(fraction: Option<f64>, expected: Option<f64>) -> String {
    let mut cells: Vec<char> = match fraction {
        None => vec!['─'; BAR_WIDTH],
        Some(f) => {
            let filled = ((f * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
            (0..BAR_WIDTH)
                .map(|i| if i < filled { '█' } else { '░' })
                .collect()
        }
    };
    if let Some(e) = expected {
        let at = ((e * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH - 1);
        cells[at] = '|';
    }
    cells.into_iter().collect()
}

fn mark(sev: Severity) -> &'static str {
    match sev {
        Severity::Normal => "  ",
        Severity::Warn => " !",
        Severity::Critical => " !!",
        Severity::Unknown => " ?",
    }
}

fn render(sample: &ProviderSample) {
    let plan = sample.plan.as_deref().unwrap_or("—");
    println!("\n{} · {}", sample.provider.label(), plan);

    if let Some(err) = &sample.error {
        println!("  indisponível: {err}");
        return;
    }

    for g in &sample.gauges {
        // Um limite inativo continua real; marcamos que ele não é o que
        // morde primeiro em vez de escondê-lo.
        let label = if g.active { g.label.clone() } else { format!("{} ~", g.label) };
        println!(
            "  {:<16} {} {:>10}{}",
            label,
            bar(g.fraction, g.expected),
            g.headline,
            mark(g.severity)
        );
        if let Some(sub) = &g.subtitle {
            println!("  {:<16} {:<10} {sub}", "", "");
        }
        // A diferença em pontos percentuais é o que o marcador mostra.
        if let Some(delta) = ia_monitor_core::model::pace_delta(g) {
            if delta.abs() >= 5.0 {
                let lado = if delta > 0.0 { "acima" } else { "abaixo" };
                println!(
                    "  {:<16} {:<10} {:.0} pts {lado} do ritmo",
                    "", "", delta.abs()
                );
            }
        }
    }

    // A idade do dado é parte do dado: o Codex só atualiza quando roda.
    if let Some(src) = sample.source_at {
        let age = (sample.observed_at - src).num_seconds();
        if age > 90 {
            println!(
                "  {:<16} lido de um evento de {} atrás",
                "",
                humanize_until(sample.observed_at, src)
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let started = std::time::Instant::now();
    let samples = ia_monitor_core::sample_all(None).await;

    println!("IA Monitor · probe · {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
    for s in &samples {
        render(s);
    }

    let failed = samples.iter().filter(|s| s.error.is_some()).count();
    println!("\n{} de {} provedores OK em {:?}", samples.len() - failed, samples.len(), started.elapsed());

    if failed == samples.len() {
        std::process::exit(1);
    }
}
