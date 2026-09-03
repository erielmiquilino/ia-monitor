use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Estado de um medidor. Quando a fonte informa a severidade, ela vence;
/// caso contrário deriva-se da fração (ver `Severity::from_fraction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Normal,
    Warn,
    Critical,
    /// Sem dado — a UI mostra "?" em vez de fingir um valor.
    Unknown,
}

impl Severity {
    pub fn from_fraction(f: Option<f64>) -> Self {
        match f {
            None => Severity::Unknown,
            Some(v) if v >= 0.90 => Severity::Critical,
            Some(v) if v >= 0.75 => Severity::Warn,
            Some(_) => Severity::Normal,
        }
    }

    /// Severidade textual devolvida pela API do Claude em `limits[].severity`.
    pub fn from_api(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Severity::Normal),
            "warning" | "warn" => Some(Severity::Warn),
            "critical" | "exceeded" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// Um medidor normalizado. Claude e Cursor preenchem `fraction` com o número
/// que o próprio servidor calculou; o Codex deriva de uma baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gauge {
    /// Identificador estável, ex. "claude.session", "cursor.auto".
    pub id: String,
    pub label: String,
    /// 0.0..=1.0. `None` quando não há teto conhecido.
    pub fraction: Option<f64>,
    /// Número principal já formatado, ex. "17%" ou "1.131,46".
    pub headline: String,
    /// Linha de apoio, ex. "reseta em 2h19m".
    pub subtitle: Option<String>,
    pub severity: Severity,
    pub resets_at: Option<DateTime<Utc>>,
    /// Se este é o limite que está governando o consumo agora. Um limite
    /// inativo continua real e visível — só não é o que morde primeiro.
    pub active: bool,
    /// Fração da janela já decorrida: onde a barra estaria se o consumo
    /// fosse uniforme. É o marcador de "onde eu deveria estar".
    ///
    /// `None` quando a janela não é conhecida — e aí não há marcador, em vez
    /// de um palpite com cara de referência.
    pub expected: Option<f64>,
}

/// Quanto da janela já passou, entre 0 e 1.
///
/// Só faz sentido com o tamanho da janela conhecido. Chutar a duração daria
/// um marcador convincente e errado, que é pior do que marcador nenhum.
pub fn expected_fraction(
    resets_at: Option<DateTime<Utc>>,
    window_seconds: Option<i64>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let resets_at = resets_at?;
    let window = window_seconds?;
    if window <= 0 {
        return None;
    }
    let remaining = (resets_at - now).num_seconds() as f64;
    Some((1.0 - remaining / window as f64).clamp(0.0, 1.0))
}

/// Quanto o consumo está à frente do relógio, em pontos percentuais.
/// Positivo = gastando mais rápido que o tempo passa.
pub fn pace_delta(gauge: &Gauge) -> Option<f64> {
    Some((gauge.fraction? - gauge.expected?) * 100.0)
}

/// Formata número no padrão pt-BR: 1131.4 -> "1.131,40".
pub fn format_pt_br(value: f64) -> String {
    let s = format!("{value:.2}");
    let (int_part, dec_part) = s.split_once('.').unwrap_or((s.as_str(), "00"));
    let negative = int_part.starts_with('-');
    let digits = int_part.trim_start_matches('-');
    let mut grouped = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    let int_fmt: String = grouped.chars().rev().collect();
    format!("{}{int_fmt},{dec_part}", if negative { "-" } else { "" })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Cursor,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Cursor => "Cursor",
            Provider::Codex => "Codex",
        }
    }

    /// Chave estável usada no banco — nunca mude sem migração.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Cursor => "cursor",
            Provider::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "cursor" => Some(Provider::Cursor),
            "codex" => Some(Provider::Codex),
            _ => None,
        }
    }

    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Cursor, Provider::Codex];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSample {
    pub provider: Provider,
    /// Nome do plano conforme a fonte, ex. "max", "Team", "business".
    pub plan: Option<String>,
    pub gauges: Vec<Gauge>,
    /// Quando *nós* lemos o dado.
    pub observed_at: DateTime<Utc>,
    /// Quando a *fonte* produziu o dado. Para o Codex é a última execução do
    /// CLI, e é isso que impede a UI de fingir tempo real.
    pub source_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Segundos que a fonte pediu para esperar antes de tentar de novo.
    /// Só é preenchido em 429 — e é o que impede a ferramenta de insistir
    /// contra um limite que ela mesma acabou de estourar.
    pub retry_after: Option<i64>,
}

impl ProviderSample {
    pub fn failed(provider: Provider, err: impl std::fmt::Display) -> Self {
        Self {
            provider,
            plan: None,
            gauges: Vec::new(),
            observed_at: Utc::now(),
            source_at: None,
            error: Some(err.to_string()),
            retry_after: None,
        }
    }

    /// Falha por excesso de requisições. Separada das demais porque exige
    /// um recuo muito maior: insistir aqui piora o próprio problema.
    pub fn rate_limited(provider: Provider, retry_after: Option<i64>) -> Self {
        Self {
            error: Some("limite de requisições atingido".into()),
            retry_after: Some(retry_after.unwrap_or(0).max(0)),
            ..Self::failed(provider, "")
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        self.retry_after.is_some()
    }

    /// Medidor que representa o provedor na visão compacta.
    ///
    /// Escolha explícita, não heurística: antes a pílula pegava a maior
    /// fração entre os ativos, então quando a janela de 5h do Claude ficava
    /// inativa era o limite por modelo (Fable) que aparecia — um número que
    /// o usuário não estava procurando.
    pub fn primary_gauge(&self) -> Option<&Gauge> {
        let preferido = match self.provider {
            // A janela de 5h é a que morde no dia a dia; semanal e por
            // modelo são contexto, e só aparecem no card.
            Provider::Claude => "claude.session",
            Provider::Cursor => "cursor.auto",
            Provider::Codex => "codex.credits",
        };
        self.gauges
            .iter()
            .find(|g| g.id == preferido && g.fraction.is_some())
            // Plano sem o medidor preferido (Codex `plus` usa janelas): cai
            // no de maior consumo, que é o que mais importa saber.
            .or_else(|| {
                self.gauges
                    .iter()
                    .filter(|g| g.fraction.is_some())
                    .max_by(|a, b| {
                        a.fraction
                            .unwrap_or(0.0)
                            .partial_cmp(&b.fraction.unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
    }

    /// Severidade agregada — alimenta a cor do ícone da bandeja.
    ///
    /// Um provedor com dado antigo mas real continua colorido: pintar de
    /// cinza um valor de cinco minutos atrás esconde informação boa. A idade
    /// é comunicada no card e no tooltip, não apagando a cor.
    pub fn worst_severity(&self) -> Severity {
        if self.gauges.is_empty() {
            return Severity::Unknown;
        }
        self.gauges
            .iter()
            .map(|g| g.severity)
            .max_by_key(|s| match s {
                Severity::Normal => 0,
                Severity::Unknown => 1,
                Severity::Warn => 2,
                Severity::Critical => 3,
            })
            .unwrap_or(Severity::Unknown)
    }
}

/// "2h19m", "11 dias" — usado nos subtítulos de reset.
pub fn humanize_until(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (target - now).num_seconds();
    if secs <= 0 {
        return "agora".into();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d} dia{}", if d == 1 { "" } else { "s" })
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// Instante do reset no fuso local: "hoje às 22h", "sex 04/09 às 22h40".
///
/// Só o tempo restante não basta para planejar: "reseta em 1 dia" não diz se
/// isso cai antes ou depois do fim do expediente.
pub fn local_moment(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    use chrono::{Datelike, Local, Timelike};

    let alvo = target.with_timezone(&Local);
    let hoje = now.with_timezone(&Local).date_naive();
    let dia = alvo.date_naive();

    // Minuto zero é o caso comum nos resets semanais; "22h" lê melhor que
    // "22h00".
    let hora = if alvo.minute() == 0 {
        format!("{}h", alvo.hour())
    } else {
        format!("{}h{:02}", alvo.hour(), alvo.minute())
    };

    if dia == hoje {
        return format!("hoje às {hora}");
    }
    if Some(dia) == hoje.succ_opt() {
        return format!("amanhã às {hora}");
    }
    const DIAS: [&str; 7] = ["seg", "ter", "qua", "qui", "sex", "sáb", "dom"];
    let nome = DIAS[alvo.weekday().num_days_from_monday() as usize];
    format!("{nome} {:02}/{:02} às {hora}", alvo.day(), alvo.month())
}

/// "reseta em 2h19m (hoje às 22h)" — quanto falta e quando exatamente.
pub fn reset_label(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format!(
        "reseta em {} ({})",
        humanize_until(target, now),
        local_moment(target, now)
    )
}

/// Rótulos curtos e distinguíveis para uma lista de caminhos.
///
/// Mostrar só a última pasta é o que o usuário reconhece, mas checkouts
/// paralelos (`...\InvoiceCore` e `...\feature-tax\InvoiceCore`) viram
/// duas linhas idênticas. Aqui cada rótulo ganha segmentos do pai até ficar
/// único — e só os que precisam.
pub fn disambiguate_labels(paths: &[String]) -> Vec<String> {
    let split: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split(['\\', '/']).filter(|s| !s.is_empty()).collect())
        .collect();

    let mut depth: Vec<usize> = vec![1; paths.len()];
    // Cresce a profundidade só dos rótulos ainda ambíguos, repetidamente:
    // desempatar um par pode criar empate com um terceiro.
    for _ in 0..8 {
        let labels: Vec<String> = build_labels(&split, &depth);
        let mut seen: std::collections::HashMap<&String, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, l) in labels.iter().enumerate() {
            seen.entry(l).or_default().push(i);
        }
        let mut changed = false;
        for (_, idxs) in seen.iter().filter(|(_, v)| v.len() > 1) {
            for &i in idxs {
                if depth[i] < split[i].len() {
                    depth[i] += 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    build_labels(&split, &depth)
}

fn build_labels(split: &[Vec<&str>], depth: &[usize]) -> Vec<String> {
    split
        .iter()
        .zip(depth)
        .map(|(parts, d)| {
            if parts.is_empty() {
                return "(sem projeto)".to_string();
            }
            let take = (*d).min(parts.len());
            parts[parts.len() - take..].join("\\")
        })
        .collect()
}

#[cfg(test)]
mod model_tests {
    use super::*;

    /// Meio caminho da janela: o marcador fica no meio da barra.
    #[test]
    fn fracao_decorrida_da_janela() {
        let now = Utc::now();
        let reset = now + chrono::Duration::hours(2);
        // Janela de 4h com 2h restantes => metade decorrida.
        let e = expected_fraction(Some(reset), Some(4 * 3600), now).unwrap();
        assert!((e - 0.5).abs() < 1e-6, "e={e}");
    }

    /// Sem janela conhecida não há marcador — melhor nenhum do que um
    /// chutado, que pareceria uma referência de verdade.
    #[test]
    fn sem_janela_nao_ha_marcador() {
        let now = Utc::now();
        assert!(expected_fraction(Some(now), None, now).is_none());
        assert!(expected_fraction(None, Some(3600), now).is_none());
        assert!(expected_fraction(Some(now), Some(0), now).is_none());
    }

    /// Relógio dessincronizado não pode empurrar o marcador para fora da barra.
    #[test]
    fn marcador_fica_dentro_da_barra() {
        let now = Utc::now();
        let muito_futuro = expected_fraction(Some(now + chrono::Duration::hours(99)), Some(3600), now);
        let ja_passou = expected_fraction(Some(now - chrono::Duration::hours(99)), Some(3600), now);
        assert_eq!(muito_futuro, Some(0.0));
        assert_eq!(ja_passou, Some(1.0));
    }

    #[test]
    fn diferenca_de_ritmo_em_pontos_percentuais() {
        let mut g = Gauge {
            id: "x".into(),
            label: "x".into(),
            fraction: Some(0.55),
            headline: "55%".into(),
            subtitle: None,
            severity: Severity::Normal,
            resets_at: None,
            active: true,
            expected: Some(0.40),
        };
        assert!((pace_delta(&g).unwrap() - 15.0).abs() < 1e-6, "gastando à frente");

        g.fraction = Some(0.16);
        g.expected = Some(0.68);
        assert!((pace_delta(&g).unwrap() + 52.0).abs() < 1e-6, "folgado = negativo");

        g.expected = None;
        assert!(pace_delta(&g).is_none(), "sem marcador, sem comparação");
    }

    fn medidor(id: &str, f: f64, ativo: bool) -> Gauge {
        Gauge {
            id: id.into(),
            label: id.into(),
            fraction: Some(f),
            headline: format!("{}%", (f * 100.0) as i64),
            subtitle: None,
            severity: Severity::Normal,
            resets_at: None,
            active: ativo,
            expected: None,
        }
    }

    fn amostra(p: Provider, gauges: Vec<Gauge>) -> ProviderSample {
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

    /// O bug relatado: com a sessao de 5h inativa, a pilula passava a mostrar
    /// o limite por modelo, que tinha fracao maior.
    #[test]
    fn pilula_mostra_a_sessao_mesmo_inativa_e_menor() {
        let s = amostra(
            Provider::Claude,
            vec![
                medidor("claude.session", 0.06, false),
                medidor("claude.weekly_all", 0.21, true),
                medidor("claude.weekly_scoped.fable", 0.56, false),
            ],
        );
        assert_eq!(s.primary_gauge().unwrap().id, "claude.session");
    }

    #[test]
    fn cada_provedor_tem_seu_medidor_principal() {
        let cur = amostra(
            Provider::Cursor,
            vec![medidor("cursor.api", 0.9, true), medidor("cursor.auto", 0.1, true)],
        );
        assert_eq!(cur.primary_gauge().unwrap().id, "cursor.auto");

        let cdx = amostra(Provider::Codex, vec![medidor("codex.credits", 0.2, true)]);
        assert_eq!(cdx.primary_gauge().unwrap().id, "codex.credits");
    }

    /// Plano sem o medidor preferido (Codex `plus` usa janelas): cai no de
    /// maior consumo em vez de nao mostrar nada.
    #[test]
    fn sem_o_preferido_usa_o_de_maior_consumo() {
        let s = amostra(
            Provider::Codex,
            vec![medidor("codex.primary", 0.3, true), medidor("codex.secondary", 0.7, true)],
        );
        assert_eq!(s.primary_gauge().unwrap().id, "codex.secondary");
    }

    #[test]
    fn provedor_sem_medidores_nao_tem_principal() {
        assert!(amostra(Provider::Claude, vec![]).primary_gauge().is_none());
    }

    /// Datas construidas a partir do fuso local para o teste nao depender do
    /// fuso da maquina.
    fn no_local(dias: i64, hora: u32, minuto: u32) -> DateTime<Utc> {
        use chrono::{Local, TimeZone};
        let dia = Local::now().date_naive() + chrono::Duration::days(dias);
        Local
            .from_local_datetime(&dia.and_hms_opt(hora, minuto, 0).unwrap())
            .single()
            .expect("horario local valido")
            .with_timezone(&Utc)
    }

    #[test]
    fn momento_do_reset_usa_hoje_e_amanha() {
        let agora = no_local(0, 9, 0);
        assert_eq!(local_moment(no_local(0, 22, 0), agora), "hoje às 22h");
        assert_eq!(local_moment(no_local(1, 1, 0), agora), "amanhã às 1h");
    }

    /// Minuto zero e o caso comum nos resets semanais; "22h" le melhor.
    #[test]
    fn minuto_zero_e_omitido() {
        let agora = no_local(0, 9, 0);
        assert_eq!(local_moment(no_local(0, 22, 0), agora), "hoje às 22h");
        assert_eq!(local_moment(no_local(0, 22, 40), agora), "hoje às 22h40");
    }

    /// Alem de amanha entra dia da semana e data, que e o que permite
    /// planejar sem contar nos dedos.
    #[test]
    fn datas_distantes_ganham_dia_da_semana() {
        let agora = no_local(0, 9, 0);
        let texto = local_moment(no_local(4, 22, 0), agora);
        assert!(texto.contains('/'), "{texto}");
        assert!(texto.contains("às 22h"), "{texto}");
        assert!(!texto.contains("hoje") && !texto.contains("amanhã"), "{texto}");
    }

    /// O rotulo completo junta quanto falta e quando exatamente.
    #[test]
    fn rotulo_de_reset_traz_prazo_e_instante() {
        let agora = no_local(0, 9, 0);
        let texto = reset_label(no_local(0, 22, 0), agora);
        assert!(texto.starts_with("reseta em "), "{texto}");
        assert!(texto.contains("(hoje às 22h)"), "{texto}");
    }

    #[test]
    fn formata_numero_em_pt_br() {
        assert_eq!(format_pt_br(1131.4605), "1.131,46");
        assert_eq!(format_pt_br(0.5), "0,50");
        assert_eq!(format_pt_br(1_234_567.0), "1.234.567,00");
        assert_eq!(format_pt_br(-42.5), "-42,50");
    }

    /// Caminhos sem conflito ficam curtos; só os ambíguos crescem.
    #[test]
    fn so_os_ambiguos_ganham_o_caminho_do_pai() {
        let paths = vec![
            r"E:\workspaces\InvoiceCore".to_string(),
            r"E:\workspaces\feature-tax\InvoiceCore".to_string(),
            r"E:\workspaces\InvoicePay".to_string(),
        ];
        let labels = disambiguate_labels(&paths);
        assert_eq!(labels[2], "InvoicePay", "sem conflito, fica só o nome");
        assert_ne!(labels[0], labels[1], "checkouts paralelos precisam diferir");
        assert!(labels[1].contains("feature-tax"));
    }

    #[test]
    fn caminho_unico_fica_so_com_a_ultima_pasta() {
        let paths = vec![r"E:\a\b\MeuProjeto".to_string()];
        assert_eq!(disambiguate_labels(&paths), vec!["MeuProjeto"]);
    }

    /// Severidade agregada alimenta a cor do ícone da bandeja: o pior manda.
    #[test]
    fn pior_severidade_domina_o_provedor() {
        let g = |s: Severity| Gauge {
            id: "x".into(),
            label: "x".into(),
            fraction: Some(0.5),
            headline: "50%".into(),
            subtitle: None,
            severity: s,
            resets_at: None,
            active: true,
            expected: None,
        };
        let sample = ProviderSample {
            provider: Provider::Claude,
            plan: None,
            gauges: vec![g(Severity::Normal), g(Severity::Critical), g(Severity::Warn)],
            observed_at: Utc::now(),
            source_at: None,
            error: None,
            retry_after: None,
        };
        assert_eq!(sample.worst_severity(), Severity::Critical);
    }

    /// 429 precisa ser distinguível de uma falha qualquer: só ele justifica
    /// um recuo de minutos.
    #[test]
    fn limite_de_volume_e_distinguivel_de_falha_comum() {
        let limitado = ProviderSample::rate_limited(Provider::Claude, Some(120));
        assert!(limitado.is_rate_limited());
        assert_eq!(limitado.retry_after, Some(120));

        let comum = ProviderSample::failed(Provider::Claude, "sem rede");
        assert!(!comum.is_rate_limited());
        assert_eq!(comum.retry_after, None);
    }

    /// Sem `Retry-After` a amostra ainda marca o 429; quem decide a espera
    /// é o agendador.
    #[test]
    fn limite_sem_retry_after_ainda_e_reconhecido() {
        let s = ProviderSample::rate_limited(Provider::Cursor, None);
        assert!(s.is_rate_limited());
        assert_eq!(s.retry_after, Some(0));
    }

    /// Um provedor com dado antigo mas real continua colorido na bandeja:
    /// pintar de cinza um valor de minutos atrás esconde informação boa.
    #[test]
    fn dado_antigo_mantem_a_severidade_dos_medidores() {
        let g = |s: Severity| Gauge {
            id: "x".into(), label: "x".into(), fraction: Some(0.8),
            headline: "80%".into(), subtitle: None, severity: s,
            resets_at: None, active: true, expected: None,
        };
        let antigo = ProviderSample {
            provider: Provider::Claude,
            plan: None,
            gauges: vec![g(Severity::Warn)],
            observed_at: Utc::now(),
            source_at: Some(Utc::now()),
            error: Some("limite de requisições atingido".into()),
            retry_after: Some(300),
        };
        assert_eq!(antigo.worst_severity(), Severity::Warn);
    }

    #[test]
    fn provedor_em_falha_tem_severidade_desconhecida() {
        let s = ProviderSample::failed(Provider::Cursor, "offline");
        assert_eq!(s.worst_severity(), Severity::Unknown);
    }
}
