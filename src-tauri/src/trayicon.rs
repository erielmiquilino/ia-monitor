//! Ícone da bandeja desenhado em tempo de execução.
//!
//! Três barras verticais, uma por provedor, na mesma ordem da pílula. Ler o
//! ícone tem que bastar para saber se algo precisa de atenção — por isso a
//! altura é o consumo e a cor é a severidade.

use ia_monitor_core::model::Severity;
use tauri::image::Image;

const SIZE: u32 = 32;

fn color(sev: Severity) -> [u8; 4] {
    match sev {
        Severity::Normal => [75, 163, 255, 255],
        Severity::Warn => [240, 176, 60, 255],
        Severity::Critical => [242, 85, 90, 255],
        Severity::Unknown => [107, 107, 118, 255],
    }
}

/// Trilho apagado atrás de cada barra: sem ele, um provedor em 5% vira um
/// tracinho solto e o ícone parece quebrado.
const TRACK: [u8; 4] = [255, 255, 255, 46];

pub fn draw(bars: &[(Severity, f64)]) -> Image<'static> {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    if bars.is_empty() {
        return Image::new_owned(rgba, SIZE, SIZE);
    }

    let margin = 4u32;
    let usable = SIZE - margin * 2;
    let slot = usable / bars.len() as u32;
    let bar_w = slot.saturating_sub(2).max(1);

    for (i, (sev, fraction)) in bars.iter().enumerate() {
        let x0 = margin + i as u32 * slot;
        let filled = ((fraction.clamp(0.0, 1.0)) * usable as f64).round() as u32;
        let fill_color = color(*sev);

        for x in x0..(x0 + bar_w).min(SIZE) {
            for y in margin..(margin + usable) {
                let from_bottom = margin + usable - y;
                let px = ((y * SIZE + x) * 4) as usize;
                let c = if from_bottom <= filled { fill_color } else { TRACK };
                rgba[px..px + 4].copy_from_slice(&c);
            }
        }
    }

    Image::new_owned(rgba, SIZE, SIZE)
}

/// Texto do tooltip da bandeja: o resumo que cabe sem abrir nada.
pub fn tooltip(samples: &[ia_monitor_core::model::ProviderSample]) -> String {
    // Primeira linha responde "preciso olhar isso agora?" sem ler o resto.
    let estado = match crate::snapshot::worst(samples) {
        Severity::Normal => "IA Monitor — tudo folgado",
        Severity::Warn => "IA Monitor — atenção",
        Severity::Critical => "IA Monitor — no limite",
        Severity::Unknown => "IA Monitor — dados incompletos",
    };
    let mut linhas = vec![estado.to_string()];
    for s in samples {
        if let Some(err) = &s.error {
            linhas.push(format!("{}: indisponível ({err})", s.provider.label()));
            continue;
        }
        let resumo: Vec<String> = s
            .gauges
            .iter()
            .filter(|g| g.fraction.is_some())
            .map(|g| format!("{} {}", g.label, g.headline))
            .collect();
        linhas.push(format!("{}: {}", s.provider.label(), resumo.join(" · ")));
    }
    linhas.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(img: &Image) -> Vec<[u8; 4]> {
        img.rgba().as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn icone_tem_o_tamanho_esperado() {
        let img = draw(&[(Severity::Normal, 0.5)]);
        assert_eq!(img.width(), SIZE);
        assert_eq!(img.height(), SIZE);
        assert_eq!(img.rgba().len(), (SIZE * SIZE * 4) as usize);
    }

    /// Consumo maior tem que pintar mais pixels — é a única informação que
    /// o ícone carrega à distância.
    #[test]
    fn barra_cheia_pinta_mais_que_barra_vazia() {
        let conta = |f: f64| {
            pixels(&draw(&[(Severity::Normal, f)]))
                .iter()
                .filter(|p| **p == color(Severity::Normal))
                .count()
        };
        assert!(conta(0.9) > conta(0.1));
        assert_eq!(conta(0.0), 0, "sem consumo, nenhum pixel colorido");
    }

    /// Um provedor crítico precisa mudar a cor do ícone, não só a altura.
    #[test]
    fn severidade_muda_a_cor() {
        let critico = pixels(&draw(&[(Severity::Critical, 0.5)]));
        assert!(critico.iter().any(|p| *p == color(Severity::Critical)));
        assert!(!critico.iter().any(|p| *p == color(Severity::Normal)));
    }

    /// Mesmo em 0% o trilho aparece, senão o ícone parece quebrado.
    #[test]
    fn trilho_aparece_mesmo_sem_consumo() {
        let px = pixels(&draw(&[(Severity::Normal, 0.0)]));
        assert!(px.contains(&TRACK));
    }

    #[test]
    fn sem_provedores_nao_quebra() {
        let img = draw(&[]);
        assert_eq!(img.rgba().len(), (SIZE * SIZE * 4) as usize);
    }
}
