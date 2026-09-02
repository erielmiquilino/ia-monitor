//! Verificação de sintaxe do frontend.
//!
//! O projeto não usa bundler — o `ui/` vai direto para dentro do binário sem
//! ninguém olhar. Um erro de sintaxe em `app.js` não quebra o build do Rust:
//! ele só aparece em runtime, como uma janela vazia, sem erro visível
//! (`windows_subsystem = "windows"` descarta o stderr).
//!
//! Foi exatamente o que aconteceu: um `\n` que virou quebra de linha literal
//! dentro de uma string derrubou o módulo inteiro e a pílula ficou preta.
//! Estes testes são a rede que faltava.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    fn ui_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("raiz do workspace")
            .join("ui")
    }

    /// `node --check` parseia sem executar. Se o Node não estiver disponível
    /// o teste não falha — mas também não finge ter verificado.
    #[test]
    fn javascript_do_frontend_tem_sintaxe_valida() {
        let arquivo = ui_dir().join("app.js");
        assert!(arquivo.exists(), "app.js não encontrado em {arquivo:?}");

        let saida = match Command::new("node").arg("--check").arg(&arquivo).output() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("node indisponível — sintaxe do frontend NÃO verificada");
                return;
            }
        };

        assert!(
            saida.status.success(),
            "erro de sintaxe em app.js:\n{}",
            String::from_utf8_lossy(&saida.stderr)
        );
    }

    /// Os elementos que o `app.js` procura por id precisam existir no HTML.
    /// Um id renomeado só de um lado deixa a tela em branco silenciosamente.
    #[test]
    fn todos_os_ids_usados_pelo_js_existem_no_html() {
        let js = std::fs::read_to_string(ui_dir().join("app.js")).expect("app.js");
        let html = std::fs::read_to_string(ui_dir().join("index.html")).expect("index.html");

        let mut faltando = Vec::new();
        for trecho in js.split("getElementById(\"").skip(1) {
            if let Some(id) = trecho.split('"').next() {
                if !html.contains(&format!("id=\"{id}\"")) {
                    faltando.push(id.to_string());
                }
            }
        }
        assert!(faltando.is_empty(), "ids ausentes no HTML: {faltando:?}");
    }

    /// Classes que o JS aplica, extraídas do próprio arquivo.
    ///
    /// Uma lista fixa aqui envelhece mal: numa branch sem determinada feature
    /// ela acusa falta de uma classe que ninguém usa. Derivar do código faz o
    /// teste valer em qualquer branch.
    fn classes_aplicadas(js: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for (marcador, fim) in [
            ("className = \"", '"'),
            ("className = `", '`'),
            ("classList.add(\"", '"'),
        ] {
            for trecho in js.split(marcador).skip(1) {
                let literal = trecho.split(fim).next().unwrap_or("");
                // A interpolação encerra a parte literal do template.
                let literal = literal.split("${").next().unwrap_or("");
                out.extend(literal.split_whitespace().map(str::to_string));
            }
        }
        out
    }

    /// O CSS precisa definir as classes que o JS aplica; sem elas o elemento
    /// existe no DOM mas não aparece na tela.
    #[test]
    fn classes_aplicadas_pelo_js_existem_no_css() {
        let js = std::fs::read_to_string(ui_dir().join("app.js")).expect("app.js");
        let css = std::fs::read_to_string(ui_dir().join("style.css")).expect("style.css");

        let aplicadas = classes_aplicadas(&js);
        assert!(aplicadas.len() > 5, "extração falhou: {aplicadas:?}");

        let faltando: Vec<_> = aplicadas
            .iter()
            .filter(|c| !css.contains(&format!(".{c}")))
            .collect();
        assert!(faltando.is_empty(), "classes sem estilo: {faltando:?}");
    }

    /// As classes de severidade são contrato com o enum `Severity` do Rust:
    /// o JS as monta por interpolação, então não aparecem na extração acima.
    #[test]
    fn severidades_do_rust_tem_estilo_no_css() {
        let css = std::fs::read_to_string(ui_dir().join("style.css")).expect("style.css");
        for sev in ["normal", "warn", "critical", "unknown"] {
            assert!(css.contains(&format!(".sev-{sev}")), "faltando .sev-{sev}");
        }
    }
}
