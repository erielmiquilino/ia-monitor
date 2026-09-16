//! Claude Desktop como fonte de credencial do Claude.
//!
//! O Desktop guarda o token OAuth cifrado pelo `safeStorage` do Electron —
//! ver `win_secret`. Ele importa porque é a fonte que **se mantém fresca
//! sozinha**: quem usa o Desktop todo dia tem token válido todo dia, enquanto
//! o do Claude Code envelhece em poucas horas se o CLI não for aberto.
//!
//! Instalado pela Store o app é um pacote MSIX, e aí o `%APPDATA%` dele é
//! redirecionado para dentro do container — daí a varredura de `Packages`.
//!
//! O formato do `oauth:tokenCache*` não é documentado e pode mudar sem aviso.
//! Por isso a extração procura o token **por nome de campo**, sem exigir
//! esquema: um envelope novo em volta dos mesmos campos continua funcionando.

use crate::collect::win_secret::{decrypt_v10, dpapi_unprotect, strip_dpapi_prefix};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Chaves do `config.json` que guardam o token, da mais nova para a mais
/// antiga. A V2 veio depois e convive com a V1 em instalação atualizada.
const TOKEN_KEYS: [&str; 2] = ["oauth:tokenCacheV2", "oauth:tokenCache"];

/// Piso de plausibilidade para uma expiração: 2000-01-01 em ms.
///
/// É o que impede um `expiresIn` de 3600 (segundos de duração, não instante)
/// de virar "expirou em 1970" e derrubar a fonte inteira.
const EPOCH_MIN_MS: i64 = 946_684_800_000;
/// Teto correspondente: 2100-01-01 em ms.
const EPOCH_MAX_MS: i64 = 4_102_444_800_000;

pub struct DesktopToken {
    pub access_token: Zeroizing<String>,
    /// Epoch em milissegundos, como no `.credentials.json` do CLI.
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

/// Candidatos ao diretório de dados do Desktop, em ordem de preferência.
///
/// Os pacotes MSIX vêm primeiro, e entre eles o de `config.json` mais recente
/// — reinstalar deixa containers antigos para trás, e o velho tem um token que
/// não vale mais.
pub fn user_data_dirs() -> Vec<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let mut out = Vec::new();

    if let Some(local) = &local {
        let mut msix: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        if let Ok(entradas) = std::fs::read_dir(local.join("Packages")) {
            for e in entradas.flatten() {
                if !e.file_name().to_string_lossy().starts_with("Claude_") {
                    continue;
                }
                let dir = e.path().join("LocalCache").join("Roaming").join("Claude");
                let quando = std::fs::metadata(dir.join("config.json"))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                msix.push((quando, dir));
            }
        }
        msix.sort_by_key(|(quando, _)| std::cmp::Reverse(*quando));
        out.extend(msix.into_iter().map(|(_, d)| d));
    }

    // Instalação fora da Store: o Electron usa o `%APPDATA%` normal.
    if let Some(appdata) = &appdata {
        out.push(appdata.join("Claude"));
    }
    if let Some(local) = &local {
        out.push(local.join("AnthropicClaude"));
    }
    out
}

/// Um diretório só serve com os **dois** arquivos: sem o `Local State` não há
/// chave, e sem o `config.json` não há o que decifrar.
pub fn tem_os_dois_arquivos(dir: &Path) -> bool {
    dir.join("config.json").is_file() && dir.join("Local State").is_file()
}

pub fn user_data_dir() -> Option<PathBuf> {
    user_data_dirs().into_iter().find(|d| tem_os_dois_arquivos(d))
}

/// Chave mestra do `os_crypt`, lida do `Local State` e desenvelopada por DPAPI.
pub fn master_key(dir: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let caminho = dir.join("Local State");
    let raw = std::fs::read_to_string(&caminho)
        .with_context(|| format!("lendo {}", caminho.display()))?;
    let json: Value = serde_json::from_str(&raw).context("`Local State` não é JSON válido")?;
    let b64 = json
        .pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("`Local State` sem `os_crypt.encrypted_key`"))?;
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("`encrypted_key` não é base64")?;
    dpapi_unprotect(strip_dpapi_prefix(&envelope)?)
}

fn normaliza(chave: &str) -> String {
    chave
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Um valor é o access token que procuramos?
///
/// O `refresh_token` é excluído de propósito, e não por precaução: usá-lo
/// rotacionaria o token do próprio Desktop e derrubaria a sessão do usuário.
/// O `id_token` também sai — é um JWT de identidade, não serve de bearer.
pub fn parece_token(chave: &str, valor: &str) -> bool {
    let k = normaliza(chave);
    if k.contains("refresh") || k.contains("idtoken") {
        return false;
    }
    if valor.starts_with("sk-ant-") {
        return true;
    }
    (k.contains("accesstoken") || k == "token") && valor.len() >= 20
}

/// Um campo é a expiração do token?
///
/// `expiresIn` fica de fora: é duração, não instante, e passaria pelo mesmo
/// filtro de nome.
pub fn parece_expiracao(chave: &str) -> bool {
    let k = normaliza(chave);
    if k.contains("refresh") || k.contains("expiresin") {
        return false;
    }
    k.contains("expiresat") || k.contains("expiry") || k.contains("expiration") || k == "expires"
}

fn parece_plano(chave: &str) -> bool {
    let k = normaliza(chave);
    k.contains("subscriptiontype") || k.contains("plantype") || k == "plan"
}

/// Normaliza uma expiração para epoch em milissegundos.
///
/// Aceita ms, segundos e RFC3339 porque as três formas circulam nos arquivos
/// da Anthropic — o `.credentials.json` usa ms, o `exp` de um JWT usa
/// segundos. Valores fora de 2000–2100 são descartados: quase sempre são uma
/// duração que caiu aqui por acidente.
pub fn coerce_epoch_ms(v: &Value) -> Option<i64> {
    let bruto = match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?,
        Value::String(s) => match s.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) => chrono::DateTime::parse_from_rfc3339(s.trim())
                .ok()?
                .timestamp_millis(),
        },
        _ => return None,
    };
    // Abaixo disso só cabe segundo: 10^10 ms é 1970, e 10^10 s é o ano 2286.
    let ms = if bruto.abs() < 10_000_000_000 {
        bruto.checked_mul(1000)?
    } else {
        bruto
    };
    (EPOCH_MIN_MS..EPOCH_MAX_MS).contains(&ms).then_some(ms)
}

/// Um token encontrado no cache, com a chave em que ele estava.
///
/// A chave importa: o Desktop indexa o cache por escopo, e o texto dela é
/// literalmente `acct:<conta>|<cliente>:<sessão>:<audiência>:<escopos>`.
pub struct Candidato {
    pub chave: String,
    pub token: Zeroizing<String>,
    pub expires_at: Option<i64>,
    pub plan: Option<String>,
}

/// Quanto um escopo serve para ler consumo.
///
/// Não é preferência estética: o cache guarda lado a lado o token da
/// integração com o Office e um de `user:profile` puro, que não leem consumo
/// nenhum. Escolher pela ordem do mapa pegaria qualquer um deles e renderia um
/// 401 com um token bom parado ao lado.
pub fn pontua_escopo(chave: &str) -> u8 {
    if chave.contains("user:sessions:claude_code") {
        3
    } else if chave.contains("user:inference") {
        2
    } else if chave.contains("user:profile") {
        1
    } else {
        0
    }
}

/// Todos os tokens da árvore, com a chave do objeto que os continha.
///
/// Coleta em vez de parar no primeiro justamente porque há vários: o critério
/// de escolha é explícito em `melhor_token`, não a ordem de iteração.
pub fn candidatos(v: &Value) -> Vec<Candidato> {
    fn visita(chave: &str, v: &Value, out: &mut Vec<Candidato>) {
        match v {
            Value::Object(map) => {
                let achado = map
                    .iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k, s)))
                    .filter(|(k, s)| parece_token(k, s))
                    // Um `sk-ant-` é certeza; o resto é heurística de nome.
                    .max_by_key(|(_, s)| s.starts_with("sk-ant-"))
                    .map(|(_, s)| s.to_string());

                if let Some(token) = achado {
                    out.push(Candidato {
                        chave: chave.to_string(),
                        token: Zeroizing::new(token),
                        expires_at: map
                            .iter()
                            .find(|(k, _)| parece_expiracao(k))
                            .and_then(|(_, val)| coerce_epoch_ms(val)),
                        plan: map
                            .iter()
                            .find(|(k, _)| parece_plano(k))
                            .and_then(|(_, val)| val.as_str())
                            .map(str::to_string),
                    });
                    // Um objeto que já deu token não esconde outro melhor
                    // dentro de si; descer alimentaria duplicata.
                    return;
                }
                for (k, val) in map {
                    visita(k, val, out);
                }
            }
            Value::Array(itens) => itens.iter().for_each(|x| visita(chave, x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    visita("", v, &mut out);
    out
}

/// O melhor token do cache para ler consumo.
///
/// Ordem: token vivo ganha de vencido, escopo mais capaz ganha de menos capaz,
/// e no empate vence o que dura mais. Expiração ausente conta como viva — não
/// dá para provar o contrário, e o 401 resolve se estiver errado.
pub fn melhor_token(v: &Value, agora_ms: i64) -> Option<DesktopToken> {
    candidatos(v)
        .into_iter()
        .max_by_key(|c| {
            (
                c.expires_at.map(|e| e > agora_ms).unwrap_or(true),
                pontua_escopo(&c.chave),
                c.expires_at.unwrap_or(0),
            )
        })
        .map(|c| DesktopToken {
            access_token: c.token,
            expires_at: c.expires_at,
            plan: c.plan,
        })
}

/// Prefixo curto mais o comprimento. É o **único** jeito autorizado de um
/// token aparecer em saída de diagnóstico.
pub fn mascara(s: &str) -> String {
    let inicio: String = s.chars().take(7).collect();
    format!("{inicio}… (len={})", s.chars().count())
}

/// Token do Claude Desktop, decifrado na hora e nunca persistido.
pub fn token() -> Result<DesktopToken> {
    let dir = user_data_dir()
        .ok_or_else(|| anyhow!("Claude Desktop não encontrado nesta máquina"))?;
    token_em(&dir)
}

/// A mesma leitura, com o diretório dado — é o que o `desktop_probe` usa para
/// relatar qual container foi escolhido.
pub fn token_em(dir: &Path) -> Result<DesktopToken> {
    let chave = master_key(dir)?;
    let caminho = dir.join("config.json");
    let raw = std::fs::read_to_string(&caminho)
        .with_context(|| format!("lendo {}", caminho.display()))?;
    let cfg: Value = serde_json::from_str(&raw).context("`config.json` não é JSON válido")?;

    let mut ultimo: Option<anyhow::Error> = None;
    for nome in TOKEN_KEYS {
        let Some(b64) = cfg.get(nome).and_then(|v| v.as_str()) else {
            continue;
        };
        let resultado = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("valor não é base64")
            .and_then(|blob| decrypt_v10(&chave, &blob))
            .and_then(|claro| {
                serde_json::from_slice::<Value>(&claro).context("o conteúdo decifrado não é JSON")
            })
            .and_then(|json| {
                melhor_token(&json, chrono::Utc::now().timestamp_millis())
                    .ok_or_else(|| anyhow!("decifrou, mas sem token reconhecível"))
            });

        match resultado {
            Ok(t) => return Ok(t),
            Err(e) => ultimo = Some(e.context(format!("`{nome}`"))),
        }
    }

    match ultimo {
        Some(e) => Err(e),
        None => bail!("`config.json` do Desktop não tem `oauth:tokenCache` — faça login no Claude Desktop"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Bem no futuro: os testes de escolha não podem virar vermelho pelo
    /// simples passar do tempo.
    const AGORA: i64 = 1_800_000_000_000;

    fn escolhe(v: &Value) -> Option<DesktopToken> {
        melhor_token(v, AGORA)
    }

    #[test]
    fn acha_o_token_aninhado_em_qualquer_envelope() {
        let v = json!({
            "contas": [
                { "uuid": "a", "dados": { "accessToken": "sk-ant-oat01-exemplo-de-token" } }
            ]
        });
        let t = escolhe(&v).expect("token");
        assert!(t.access_token.starts_with("sk-ant-"));
    }

    #[test]
    fn acha_o_token_por_nome_de_campo_sem_prefixo_conhecido() {
        let v = json!({ "access_token": "abcdefghijklmnopqrstuvwxyz0123" });
        assert!(escolhe(&v).is_some());
    }

    /// Usar o refresh token rotacionaria a credencial e derrubaria a sessão do
    /// Desktop — exatamente o estrago que esta ferramenta não pode causar.
    #[test]
    fn nunca_confunde_refresh_token_com_access_token() {
        let v = json!({ "refreshToken": "sk-ant-ort01-nao-e-para-usar" });
        assert!(escolhe(&v).is_none());
    }

    #[test]
    fn id_token_nao_serve_de_bearer() {
        let v = json!({ "id_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.x.y" });
        assert!(escolhe(&v).is_none());
    }

    /// Com refresh e access no mesmo objeto, sai o access.
    #[test]
    fn escolhe_o_access_token_entre_vizinhos() {
        let v = json!({
            "refreshToken": "sk-ant-ort01-nao",
            "token": "sk-ant-oat01-sim",
            "expiresAt": AGORA + 3_600_000i64
        });
        let t = escolhe(&v).expect("token");
        assert_eq!(&*t.access_token, "sk-ant-oat01-sim");
        assert_eq!(t.expires_at, Some(AGORA + 3_600_000));
    }

    /// O formato real do `oauth:tokenCacheV2`: um objeto por escopo, com os
    /// escopos escritos na própria chave. Capturado de uma instalação real.
    fn cache_real() -> Value {
        json!({
            "acct:C|a473d7bb:S:https://api.anthropic.com:user:profile": {
                "token": "sk-ant-so-perfil-nao-le-consumo",
                "refreshToken": "sk-ant-refresh-perfil",
                "expiresAt": AGORA + 9_000_000_000i64,
                "subscriptionType": "max"
            },
            "acct:C|89355bc3:S:https://api.anthropic.com:user:inference user:office": {
                "token": "sk-ant-integracao-do-office",
                "expiresAt": AGORA + 9_000_000_000i64,
                "subscriptionType": null
            },
            "acct:C|9d1c250a:S:https://api.anthropic.com:user:inference user:file_upload user:profile user:sessions:claude_code": {
                "token": "sk-ant-o-que-queremos",
                "refreshToken": "sk-ant-refresh-claude-code",
                "expiresAt": AGORA + 1_000_000i64,
                "subscriptionType": "max"
            }
        })
    }

    /// O cache real tem três tokens válidos ao mesmo tempo, e dois deles não
    /// leem consumo. Escolher pela ordem do mapa renderia 401 com o token bom
    /// parado ao lado.
    #[test]
    fn escolhe_pelo_escopo_e_nao_pela_ordem_do_mapa() {
        let t = escolhe(&cache_real()).expect("token");
        assert_eq!(&*t.access_token, "sk-ant-o-que-queremos");
        assert_eq!(t.plan.as_deref(), Some("max"));
    }

    /// Escopo bom mas vencido perde para escopo pior e vivo: um token expirado
    /// não lê nada, por melhor que seja o escopo.
    #[test]
    fn token_vivo_ganha_de_token_vencido_com_escopo_melhor() {
        let mut v = cache_real();
        v["acct:C|9d1c250a:S:https://api.anthropic.com:user:inference user:file_upload user:profile user:sessions:claude_code"]
            ["expiresAt"] = json!(AGORA - 1);
        let t = escolhe(&v).expect("token");
        assert_eq!(&*t.access_token, "sk-ant-integracao-do-office");
    }

    /// Mesmo escopo nos dois: fica o que dura mais.
    #[test]
    fn no_empate_de_escopo_vence_a_expiracao_mais_longa() {
        let v = json!({
            "a:user:inference": { "token": "sk-ant-curto", "expiresAt": AGORA + 1_000i64 },
            "b:user:inference": { "token": "sk-ant-longo", "expiresAt": AGORA + 90_000_000i64 }
        });
        assert_eq!(&*escolhe(&v).unwrap().access_token, "sk-ant-longo");
    }

    #[test]
    fn pontuacao_de_escopo_segue_a_capacidade() {
        assert!(pontua_escopo("x:user:sessions:claude_code") > pontua_escopo("x:user:inference"));
        assert!(pontua_escopo("x:user:inference") > pontua_escopo("x:user:profile"));
        assert_eq!(pontua_escopo("x:sem:escopo:conhecido"), 0);
    }

    #[test]
    fn expiracao_em_ms_segundos_e_rfc3339_chegam_no_mesmo_lugar() {
        assert_eq!(coerce_epoch_ms(&json!(1_800_000_000_000i64)), Some(1_800_000_000_000));
        assert_eq!(coerce_epoch_ms(&json!(1_800_000_000i64)), Some(1_800_000_000_000));
        assert_eq!(coerce_epoch_ms(&json!("1800000000000")), Some(1_800_000_000_000));
        let esperado = chrono::DateTime::parse_from_rfc3339("2027-01-15T10:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(coerce_epoch_ms(&json!("2027-01-15T10:00:00Z")), Some(esperado));
    }

    /// `expiresIn` é duração. Tratá-lo como instante faria o token parecer
    /// vencido desde 1970 e derrubaria a fonte inteira sem motivo.
    #[test]
    fn duracao_nao_vira_instante() {
        assert!(!parece_expiracao("expiresIn"));
        assert!(!parece_expiracao("expires_in"));
        assert!(parece_expiracao("expiresAt"));
        assert!(parece_expiracao("expires_at"));
        // Mesmo que passasse pelo nome, 3600 s cai fora da janela plausível.
        assert_eq!(coerce_epoch_ms(&json!(3600)), None);
    }

    #[test]
    fn expiracao_do_refresh_nao_e_a_do_access() {
        assert!(!parece_expiracao("refreshTokenExpiresAt"));
    }

    #[test]
    fn json_sem_token_devolve_nada() {
        assert!(escolhe(&json!({ "tema": "escuro", "janela": { "largura": 900 } })).is_none());
    }

    /// Um campo curto chamado `token` costuma ser id de UI, não credencial.
    #[test]
    fn string_curta_chamada_token_nao_vira_credencial() {
        assert!(escolhe(&json!({ "token": "abc" })).is_none());
    }

    #[test]
    fn mascara_nunca_revela_o_segredo() {
        let m = mascara("sk-ant-oat01-um-token-bem-comprido");
        assert_eq!(m, "sk-ant-… (len=34)");
        assert!(!m.contains("oat01"));
        assert_eq!(mascara(""), "… (len=0)");
    }

    #[test]
    fn diretorio_precisa_dos_dois_arquivos() {
        let base = std::env::temp_dir().join("ia-monitor-teste-desktop-dir");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(!tem_os_dois_arquivos(&base));
        std::fs::write(base.join("config.json"), "{}").unwrap();
        assert!(!tem_os_dois_arquivos(&base), "só o config não basta");
        std::fs::write(base.join("Local State"), "{}").unwrap();
        assert!(tem_os_dois_arquivos(&base));
        let _ = std::fs::remove_dir_all(&base);
    }
}
