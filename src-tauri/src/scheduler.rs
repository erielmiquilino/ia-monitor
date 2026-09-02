//! Ritmo de coleta, **por provedor**.
//!
//! A versão anterior tinha um recuo global disparado por "algum provedor
//! respondeu". Com isso, um provedor devolvendo 429 continuava sendo
//! consultado no ritmo normal enquanto os outros dois funcionassem — que é
//! exatamente o jeito de transformar um 429 pontual em permanente.
//!
//! Aqui cada provedor tem seu próprio relógio: um pode estar recuando de
//! hora em hora enquanto os demais seguem no ritmo normal.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use ia_monitor_core::model::{Provider, ProviderSample};
use std::time::Duration;

/// A partir daqui consideramos a sessão ociosa.
const IDLE_THRESHOLD_SECONDS: u64 = 180;
/// Folga depois do reset: o servidor não zera no milissegundo exato.
const AFTER_RESET_GRACE: i64 = 5;
/// Teto do recuo por falha comum (rede, 5xx).
const MAX_BACKOFF_SECONDS: i64 = 900;
/// Piso do recuo após um 429, mesmo sem `Retry-After`.
///
/// Precisa ser maior que o recuo de uma falha comum na mesma contagem
/// (180 x 2 = 360s), senão o primeiro 429 é tratado com a mesma leniência de
/// uma queda de rede — e ele é o problema mais grave dos dois.
const RATE_LIMIT_MIN_SECONDS: i64 = 600;
/// Teto do recuo por 429.
const RATE_LIMIT_MAX_SECONDS: i64 = 3600;

/// Intervalo base de um provedor, em segundos.
pub struct Cadence {
    pub active: i64,
    pub idle: i64,
}

/// Quanto vale consultar cada fonte.
///
/// Claude e Cursor custam requisição de rede e mudam na escala de minutos:
/// consultar a cada minuto gasta cota sem acrescentar informação — numa
/// janela de 5h, 3 minutos ainda dão resolução melhor que 1%. O Codex é
/// leitura de arquivo local, então é barato e pode ser mais frequente.
pub fn cadence(provider: Provider) -> Cadence {
    match provider {
        Provider::Claude | Provider::Cursor => Cadence { active: 180, idle: 900 },
        Provider::Codex => Cadence { active: 60, idle: 300 },
    }
}

/// Segundos desde a última interação do usuário.
///
/// Vai direto na API do Windows em vez de trazer uma dependência inteira
/// para duas chamadas.
#[cfg(windows)]
pub fn idle_seconds() -> u64 {
    #[repr(C)]
    struct LastInputInfo {
        cb_size: u32,
        dw_time: u32,
    }

    extern "system" {
        fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
        fn GetTickCount() -> u32;
    }

    let mut info = LastInputInfo {
        cb_size: std::mem::size_of::<LastInputInfo>() as u32,
        dw_time: 0,
    };
    // SAFETY: struct com tamanho declarado corretamente; a API só escreve
    // em `dw_time`.
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if ok == 0 {
        return 0;
    }
    let now = unsafe { GetTickCount() };
    // `GetTickCount` dá a volta em ~49 dias; `wrapping_sub` trata isso.
    (now.wrapping_sub(info.dw_time) / 1000) as u64
}

#[cfg(not(windows))]
pub fn idle_seconds() -> u64 {
    0
}

/// Quando este provedor deve ser consultado de novo.
///
/// `failures` é a contagem de falhas consecutivas dele; `retry_after` vem
/// preenchido só quando a última resposta foi 429.
pub fn next_due(
    provider: Provider,
    now: DateTime<Utc>,
    idle_secs: u64,
    failures: u32,
    retry_after: Option<i64>,
    next_reset: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    let base = {
        let c = cadence(provider);
        if idle_secs >= IDLE_THRESHOLD_SECONDS {
            c.idle
        } else {
            c.active
        }
    };

    // Recuo por falha comum: dobra a cada tentativa, com teto.
    let recuo_comum = if failures > 0 {
        base.saturating_mul(1i64 << failures.min(4))
            .min(MAX_BACKOFF_SECONDS)
    } else {
        base
    };

    // 429 manda em tudo: nem o alinhamento com o reset justifica insistir.
    if let Some(pedido) = retry_after {
        let escalado = RATE_LIMIT_MIN_SECONDS
            .saturating_mul(1i64 << failures.saturating_sub(1).min(4))
            .min(RATE_LIMIT_MAX_SECONDS)
            // Um 429 nunca pode esperar menos que uma falha de rede na mesma
            // contagem: é o problema mais grave dos dois.
            .max(recuo_comum);
        // O pedido do servidor é autoritativo e não é limitado pelo nosso
        // teto — ele sabe melhor que nós quando aceita voltar.
        let espera = pedido.max(escalado);
        return now + ChronoDuration::seconds(espera);
    }

    if failures > 0 {
        return now + ChronoDuration::seconds(recuo_comum);
    }

    let normal = now + ChronoDuration::seconds(base);

    // Se um reset acontece antes do próximo ciclo, acorda logo depois dele:
    // é quando o valor muda de verdade.
    if let Some(reset) = next_reset {
        let alvo = reset + ChronoDuration::seconds(AFTER_RESET_GRACE);
        if alvo > now && alvo < normal {
            return alvo;
        }
    }
    normal
}

/// O reset mais próximo no futuro entre os medidores de uma amostra.
pub fn nearest_reset(sample: &ProviderSample, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    sample
        .gauges
        .iter()
        .filter_map(|g| g.resets_at)
        .filter(|r| *r > now)
        .min()
}

/// Quanto o laço deve dormir até o próximo provedor vencer. Limitado para a
/// UI continuar atualizando os contadores de tempo, e com piso para o laço
/// não virar espera ocupada.
pub fn sleep_until(next: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
    const MIN: i64 = 5;
    const MAX: i64 = 60;
    let secs = next
        .map(|d| (d - now).num_seconds())
        .unwrap_or(MAX)
        .clamp(MIN, MAX);
    Duration::from_secs(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn agora() -> DateTime<Utc> {
        Utc.timestamp_opt(1_000_000, 0).unwrap()
    }

    fn daqui(d: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
        (d - now).num_seconds()
    }

    #[test]
    fn usuario_ativo_usa_o_ritmo_rapido() {
        let now = agora();
        assert_eq!(daqui(next_due(Provider::Claude, now, 5, 0, None, None), now), 180);
    }

    /// Máquina parada não gera consumo; consultar no mesmo ritmo é desperdício.
    #[test]
    fn maquina_ociosa_desacelera() {
        let now = agora();
        assert_eq!(daqui(next_due(Provider::Claude, now, 600, 0, None, None), now), 900);
    }

    /// O Codex lê arquivo local: não custa requisição e pode ser frequente.
    #[test]
    fn codex_e_mais_frequente_que_os_de_rede() {
        let now = agora();
        let codex = daqui(next_due(Provider::Codex, now, 0, 0, None, None), now);
        let claude = daqui(next_due(Provider::Claude, now, 0, 0, None, None), now);
        assert!(codex < claude, "codex={codex} claude={claude}");
    }

    /// O bug que motivou a reescrita: um 429 tem que gerar recuo longo, não
    /// o intervalo normal.
    #[test]
    fn resposta_429_recua_muito_mais_que_o_normal() {
        let now = agora();
        let normal = daqui(next_due(Provider::Claude, now, 0, 0, None, None), now);
        let limitado = daqui(next_due(Provider::Claude, now, 0, 1, Some(0), None), now);
        assert!(limitado >= RATE_LIMIT_MIN_SECONDS, "limitado={limitado}");
        assert!(limitado > normal, "recuo insuficiente: {limitado} vs {normal}");
    }

    /// 429 seguidos aumentam a espera, com teto.
    #[test]
    fn limites_consecutivos_escalam_ate_o_teto() {
        let now = agora();
        let esperas: Vec<i64> = (1..=8)
            .map(|f| daqui(next_due(Provider::Claude, now, 0, f, Some(0), None), now))
            .collect();
        assert!(esperas[0] < esperas[2], "{esperas:?}");
        assert_eq!(*esperas.last().unwrap(), RATE_LIMIT_MAX_SECONDS);
    }

    /// Quando a fonte diz quanto esperar, ela manda — desde que peça mais
    /// que o nosso piso.
    #[test]
    fn retry_after_da_fonte_e_respeitado() {
        let now = agora();
        assert_eq!(
            daqui(next_due(Provider::Claude, now, 0, 1, Some(1800), None), now),
            1800
        );
    }

    /// Um `Retry-After` curto não pode furar o piso: a API acabou de
    /// reclamar de volume.
    #[test]
    fn retry_after_curto_nao_fura_o_piso() {
        let now = agora();
        let d = daqui(next_due(Provider::Claude, now, 0, 1, Some(3), None), now);
        assert!(d >= RATE_LIMIT_MIN_SECONDS, "d={d}");
    }

    /// Regressão: com uma falha, o 429 chegava a esperar MENOS que um erro
    /// de rede — menos punição para o problema mais grave.
    #[test]
    fn limite_nunca_espera_menos_que_falha_comum() {
        let now = agora();
        for f in 1..=6 {
            let rede = daqui(next_due(Provider::Claude, now, 0, f, None, None), now);
            let limite = daqui(next_due(Provider::Claude, now, 0, f, Some(0), None), now);
            assert!(limite >= rede, "falhas={f}: rede={rede} limite={limite}");
        }
    }

    /// Se o servidor pede uma espera maior que o nosso teto, ele manda: sabe
    /// melhor que nós quando aceita voltar.
    #[test]
    fn pedido_longo_do_servidor_e_honrado_acima_do_teto() {
        let now = agora();
        let d = daqui(next_due(Provider::Claude, now, 0, 1, Some(7200), None), now);
        assert_eq!(d, 7200);
    }

    /// Alinhar com o reset é bom, mas não durante uma punição por volume.
    #[test]
    fn reset_proximo_nao_encurta_recuo_por_limite() {
        let now = agora();
        let reset = now + ChronoDuration::seconds(30);
        let d = next_due(Provider::Claude, now, 0, 1, Some(0), Some(reset));
        assert!(daqui(d, now) >= RATE_LIMIT_MIN_SECONDS);
    }

    /// Falha de rede também recua, só que bem menos.
    #[test]
    fn falha_comum_tambem_recua() {
        let now = agora();
        let rede = daqui(next_due(Provider::Claude, now, 0, 1, None, None), now);
        assert!(rede > 180, "precisa recuar de algum jeito: {rede}");
        assert!(rede < RATE_LIMIT_MIN_SECONDS, "mas menos que um 429: {rede}");
    }

    #[test]
    fn falhas_comuns_tem_teto() {
        let now = agora();
        assert_eq!(
            daqui(next_due(Provider::Claude, now, 0, 99, None, None), now),
            MAX_BACKOFF_SECONDS
        );
    }

    /// O instante em que o número muda de verdade merece um ciclo dedicado.
    #[test]
    fn acorda_logo_apos_o_reset() {
        let now = agora();
        let reset = now + ChronoDuration::seconds(20);
        assert_eq!(
            daqui(next_due(Provider::Claude, now, 0, 0, None, Some(reset)), now),
            25,
            "20s até o reset + folga"
        );
    }

    /// Reset já passado (relógio dessincronizado) não pode virar espera zero.
    #[test]
    fn reset_no_passado_nao_gera_laco_ocupado() {
        let now = agora();
        let reset = now - ChronoDuration::hours(1);
        assert_eq!(
            daqui(next_due(Provider::Claude, now, 0, 0, None, Some(reset)), now),
            180
        );
    }

    #[test]
    fn sono_do_laco_fica_entre_o_piso_e_o_teto() {
        let now = agora();
        assert_eq!(sleep_until(Some(now), now).as_secs(), 5, "nunca zero");
        assert_eq!(
            sleep_until(Some(now + ChronoDuration::hours(2)), now).as_secs(),
            60
        );
        assert_eq!(sleep_until(None, now).as_secs(), 60);
        assert_eq!(
            sleep_until(Some(now + ChronoDuration::seconds(20)), now).as_secs(),
            20
        );
    }

    #[test]
    fn encontra_o_reset_mais_proximo_do_provedor() {
        use ia_monitor_core::model::{Gauge, Severity};
        let now = agora();
        let g = |secs: i64| Gauge {
            id: "g".into(),
            label: "g".into(),
            fraction: Some(0.1),
            headline: "10%".into(),
            subtitle: None,
            severity: Severity::Normal,
            resets_at: Some(now + ChronoDuration::seconds(secs)),
            active: true,
            expected: None,
        };
        let sample = ProviderSample {
            provider: Provider::Claude,
            plan: None,
            // Inclui um reset no passado, que precisa ser ignorado.
            gauges: vec![g(-100), g(900), g(300)],
            observed_at: now,
            source_at: None,
            error: None,
            retry_after: None,
        };
        assert_eq!(
            nearest_reset(&sample, now),
            Some(now + ChronoDuration::seconds(300))
        );
    }
}
