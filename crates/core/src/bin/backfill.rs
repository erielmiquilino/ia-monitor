//! `backfill` — importa o histórico já existente em disco e na API.
//!
//! Rodar duas vezes seguidas é o teste do incremental: a segunda passada
//! deve ler ~0 bytes e inserir 0 eventos.

use chrono::{Duration, Utc};
use ia_monitor_core::model::{disambiguate_labels, format_pt_br};
use ia_monitor_core::store::{GroupBy, Store};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let store = Store::open_default()?;
    println!("banco: {}", Store::default_path()?.display());
    println!("eventos antes: {}", store.event_count()?);

    let client = ia_monitor_core::http_client();
    let (stats, errors) = ia_monitor_core::ingest_all(&store, &client).await;

    println!(
        "\nlidos {} arquivos/páginas, {:.1} MB, {} eventos novos em {:?}",
        stats.files_scanned,
        stats.bytes_read as f64 / 1_048_576.0,
        stats.events_inserted,
        started.elapsed()
    );
    for e in &errors {
        println!("  falha: {e}");
    }
    println!("eventos depois: {}", store.event_count()?);

    let since = Utc::now() - Duration::days(30);

    println!("\n=== consumo por provedor (30 dias) ===");
    for t in store.totals_by(GroupBy::Provider, since)? {
        println!(
            "  {:<10} {:>12} in {:>10} out {:>14} cache {:>6} reqs  US$ {}",
            t.key,
            format_pt_br(t.input_tokens as f64).replace(",00", ""),
            format_pt_br(t.output_tokens as f64).replace(",00", ""),
            format_pt_br(t.cache_tokens as f64).replace(",00", ""),
            t.events,
            format_pt_br(t.cents / 100.0)
        );
    }

    println!("\n=== top 10 projetos (30 dias) ===");
    let projetos: Vec<_> = store
        .totals_by(GroupBy::Project, since)?
        .into_iter()
        .take(10)
        .collect();
    let chaves: Vec<String> = projetos.iter().map(|t| t.key.clone()).collect();
    for (t, label) in projetos.iter().zip(disambiguate_labels(&chaves)) {
        println!(
            "  {:<42} {:>12} tokens {:>6} reqs",
            label,
            format_pt_br((t.input_tokens + t.output_tokens) as f64).replace(",00", ""),
            t.events
        );
    }

    println!("\n=== top 8 modelos (30 dias) ===");
    for t in store.totals_by(GroupBy::Model, since)?.into_iter().take(8) {
        println!(
            "  {:<32} {:>12} tokens {:>6} reqs",
            t.key,
            format_pt_br((t.input_tokens + t.output_tokens) as f64).replace(",00", ""),
            t.events
        );
    }

    Ok(())
}
