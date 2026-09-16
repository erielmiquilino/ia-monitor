//! `desktop_probe` — diagnóstico da credencial do Claude Desktop.
//!
//! Responde a **uma** pergunta que nenhuma leitura de código responde: o token
//! que o Desktop guarda é aceito por `GET /api/oauth/usage`? Ele autentica por
//! sessão web, e o endpoint pode exigir o escopo `user:sessions:claude_code`
//! que só o CLI tem. 200 significa que o Desktop serve de fonte; 403 significa
//! que não serve, e aí quem só usa o Desktop precisa de outro caminho.
//!
//! Fica no repositório depois de respondida: quando o formato do Desktop
//! mudar, é esta ferramenta que diz onde quebrou.
//!
//! **Não imprime segredo.** Strings aparecem como prefixo de 7 caracteres mais
//! o comprimento; o corpo da resposta HTTP aparece só como tamanho.

use ia_monitor_core::collect::claude_desktop as desktop;
use serde_json::Value;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Esconde o hash do pacote MSIX: ele identifica a instalação e não acrescenta
/// nada ao diagnóstico.
fn caminho_curto(p: &std::path::Path) -> String {
    let texto = p.display().to_string();
    match texto.find("Packages\\") {
        Some(i) => {
            let resto = &texto[i + "Packages\\".len()..];
            let pacote = resto.split('\\').next().unwrap_or(resto);
            let curto = pacote.split('_').next().unwrap_or(pacote);
            format!("…\\Packages\\{curto}_…{}", &resto[pacote.len()..])
        }
        None => texto,
    }
}

/// Estrutura do JSON, nunca o conteúdo.
fn descreve(v: &Value, prefixo: &str, saida: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let caminho = if prefixo.is_empty() {
                    k.clone()
                } else {
                    format!("{prefixo}.{k}")
                };
                match val {
                    Value::Object(_) | Value::Array(_) => descreve(val, &caminho, saida),
                    Value::String(s) => saida.push(format!("  {caminho}: string {}", desktop::mascara(s))),
                    Value::Number(n) => saida.push(format!("  {caminho}: number {n}")),
                    Value::Bool(b) => saida.push(format!("  {caminho}: bool {b}")),
                    Value::Null => saida.push(format!("  {caminho}: null")),
                }
            }
        }
        Value::Array(itens) => {
            saida.push(format!("  {prefixo}: array[{}]", itens.len()));
            if let Some(primeiro) = itens.first() {
                descreve(primeiro, &format!("{prefixo}[0]"), saida);
            }
        }
        _ => descreve(&Value::Object(Default::default()), prefixo, saida),
    }
}

#[tokio::main]
async fn main() {
    println!("== Claude Desktop: onde está ==");
    let candidatos = desktop::user_data_dirs();
    if candidatos.is_empty() {
        println!("nenhum candidato — %LOCALAPPDATA% e %APPDATA% não resolveram");
        return;
    }
    for c in &candidatos {
        let marca = if desktop::tem_os_dois_arquivos(c) { "OK  " } else { "sem " };
        println!("  [{marca}] {}", caminho_curto(c));
    }

    let Some(dir) = desktop::user_data_dir() else {
        println!("\nnenhum candidato tem `config.json` + `Local State` — o Desktop está instalado e logado?");
        return;
    };
    println!("\nescolhido: {}", caminho_curto(&dir));

    println!("\n== Chave mestra (Local State → DPAPI) ==");
    match desktop::master_key(&dir) {
        Ok(k) => println!("  chave de {} bytes{}", k.len(), if k.len() == 32 { " (AES-256, como esperado)" } else { " — ESPERADO 32" }),
        Err(e) => {
            println!("  FALHOU: {e:#}");
            return;
        }
    }

    println!("\n== Token cifrado (config.json → v10/AES-GCM) ==");
    let token = match desktop::token_em(&dir) {
        Ok(t) => {
            println!("  token: {}", desktop::mascara(&t.access_token));
            match t.expires_at {
                Some(ms) => {
                    let quando = chrono::DateTime::from_timestamp_millis(ms);
                    let estado = if ms > chrono::Utc::now().timestamp_millis() {
                        "válido"
                    } else {
                        "EXPIRADO — abra o Desktop e rode de novo"
                    };
                    println!("  expira: {quando:?} ({estado})");
                }
                None => println!("  expira: não declarado no JSON"),
            }
            if let Some(p) = &t.plan {
                println!("  plano: {p}");
            }
            t
        }
        Err(e) => {
            println!("  FALHOU: {e:#}");
            return;
        }
    };

    // O shape completo ajuda a ajustar a extração quando o formato mudar.
    println!("\n== Formato do JSON decifrado ==");
    match desktop::master_key(&dir).and_then(|k| shape(&dir, &k)) {
        Ok(linhas) => linhas.iter().for_each(|l| println!("{l}")),
        Err(e) => println!("  (não foi possível descrever: {e:#})"),
    }

    println!("\n== A pergunta: /api/oauth/usage aceita este token? ==");
    let client = ia_monitor_core::http_client();
    let resp = client
        .get(USAGE_URL)
        .bearer_auth(token.access_token.as_str())
        .header("anthropic-beta", OAUTH_BETA)
        .header("User-Agent", "claude-cli/2.1.246 (external, cli)")
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let tamanho = r.bytes().await.map(|b| b.len()).unwrap_or(0);
            println!("  HTTP {status} · corpo de {tamanho} bytes");
            println!(
                "  →  {}",
                match status.as_u16() {
                    200 => "SERVE. O Desktop pode ser fonte de credencial.",
                    401 => "token recusado. Abra o Desktop para renovar e rode de novo; se persistir, não serve.",
                    403 => "escopo insuficiente — o Desktop NÃO serve para este endpoint.",
                    _ => "resposta inesperada; ver o status acima.",
                }
            );
        }
        Err(e) => println!("  falha de rede: {e:#}"),
    }
}

fn shape(dir: &std::path::Path, chave: &[u8]) -> anyhow::Result<Vec<String>> {
    use base64::Engine;
    let raw = std::fs::read_to_string(dir.join("config.json"))?;
    let cfg: Value = serde_json::from_str(&raw)?;
    let mut saida = Vec::new();
    for nome in ["oauth:tokenCacheV2", "oauth:tokenCache"] {
        let Some(b64) = cfg.get(nome).and_then(|v| v.as_str()) else {
            continue;
        };
        let blob = base64::engine::general_purpose::STANDARD.decode(b64)?;
        saida.push(format!(
            "{nome}: {} bytes cifrados, prefixo {:?}",
            blob.len(),
            String::from_utf8_lossy(&blob[..3.min(blob.len())])
        ));
        match ia_monitor_core::collect::win_secret::decrypt_v10(chave, &blob) {
            Ok(claro) => match serde_json::from_slice::<Value>(&claro) {
                Ok(json) => descreve(&json, "", &mut saida),
                Err(e) => saida.push(format!("  (decifrou {} bytes, mas não é JSON: {e})", claro.len())),
            },
            Err(e) => saida.push(format!("  (não decifrou: {e:#})")),
        }
    }
    Ok(saida)
}
