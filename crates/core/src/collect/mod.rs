pub mod claude;
pub mod codex;
pub mod cursor;

use crate::model::ProviderSample;

/// Todo coletor devolve um `ProviderSample`, inclusive em caso de falha —
/// nenhum provedor indisponível pode derrubar os outros.
#[allow(async_fn_in_trait)]
pub trait Collector {
    fn provider(&self) -> crate::model::Provider;
    async fn sample(&self) -> ProviderSample;
}

/// Diretório home do usuário, sem depender de crates extras.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

/// Erro de excesso de requisições, com o tempo de espera pedido pela fonte.
///
/// Precisa ser um tipo próprio: o agendador trata 429 de forma diferente de
/// uma falha de rede. Insistir num 429 no ritmo normal alimenta o próprio
/// problema e pode estender a punição.
#[derive(Debug)]
pub struct RateLimited(pub Option<i64>);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "limite de requisições atingido")
    }
}

impl std::error::Error for RateLimited {}

/// Lê `Retry-After`. Aceita o formato em segundos, que é o que estas APIs
/// usam; data HTTP não é suportada e cai no recuo padrão.
pub fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|s| *s > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    fn com_retry(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn le_retry_after_em_segundos() {
        assert_eq!(retry_after_seconds(&com_retry("120")), Some(120));
        assert_eq!(retry_after_seconds(&com_retry(" 90 ")), Some(90));
    }

    /// Sem header, ou em formato de data HTTP, cai no recuo padrão em vez de
    /// virar espera zero.
    #[test]
    fn formato_nao_suportado_vira_recuo_padrao() {
        assert_eq!(retry_after_seconds(&HeaderMap::new()), None);
        assert_eq!(retry_after_seconds(&com_retry("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after_seconds(&com_retry("0")), None);
        assert_eq!(retry_after_seconds(&com_retry("-5")), None);
    }
}
