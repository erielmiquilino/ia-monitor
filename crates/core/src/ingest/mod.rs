//! Ingestão de histórico. Backfill e leitura incremental usam o mesmo
//! caminho: a única diferença é o offset de onde a leitura começa.
//!
//! Regra que sustenta tudo: o cursor de leitura só avança **depois** que os
//! eventos foram commitados. Um crash no meio faz reler, não perder.

pub mod claude_jsonl;
pub mod codex_rollout;
pub mod cursor_events;

use crate::store::{Store, TailCursor};

/// Versão das regras de interpretação dos logs. Subir este número força a
/// reconstrução do histórico na próxima execução — necessário sempre que a
/// forma de extrair projeto, modelo ou tokens mudar.
pub const INGEST_VERSION: u32 = 3;
use anyhow::{Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestStats {
    pub files_scanned: usize,
    pub bytes_read: u64,
    pub events_inserted: usize,
}

impl IngestStats {
    pub fn merge(&mut self, other: &IngestStats) {
        self.files_scanned += other.files_scanned;
        self.bytes_read += other.bytes_read;
        self.events_inserted += other.events_inserted;
    }
}

pub struct TailResult {
    /// Só linhas completas. Uma linha parcial no fim do arquivo (o CLI ainda
    /// está escrevendo) fica para a próxima passada.
    pub lines: Vec<String>,
    pub cursor: TailCursor,
    /// Verdadeiro quando lemos desde o começo — é quando o cabeçalho da
    /// sessão (cwd, modelo) está disponível.
    pub from_start: bool,
}

/// Lê o que ainda não foi lido de um arquivo de log.
pub fn read_new_lines(store: &Store, path: &Path) -> Result<TailResult> {
    let key = path.to_string_lossy().to_string();
    let meta = std::fs::metadata(path).with_context(|| format!("stat {key}"))?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let previous = store.tail_cursor(&key)?;
    // Arquivo menor que o offset anterior significa truncamento ou rotação:
    // a única leitura segura é recomeçar.
    let start = match &previous {
        Some(c) if c.offset <= size => c.offset,
        _ => 0,
    };

    if start == size {
        return Ok(TailResult {
            lines: Vec::new(),
            cursor: TailCursor { path: key, size, mtime, offset: start },
            from_start: false,
        });
    }

    let mut file = std::fs::File::open(path).with_context(|| format!("abrindo {key}"))?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut buf)?;

    // Corta no último \n: o resto é linha incompleta.
    let last_newline = buf.iter().rposition(|b| *b == b'\n');
    let (complete, consumed) = match last_newline {
        Some(idx) => (&buf[..=idx], idx as u64 + 1),
        None => (&buf[..0], 0),
    };

    let lines = String::from_utf8_lossy(complete)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok(TailResult {
        lines,
        cursor: TailCursor { path: key, size, mtime, offset: start + consumed },
        from_start: start == 0,
    })
}

/// Caminho de projeto em forma canônica.
///
/// Sem isto o mesmo repositório vira duas linhas no relatório: o Claude grava
/// `E:\workspaces\InvoiceCore` e o Cursor grava a mesma pasta com a letra
/// do drive minúscula. No Windows são o mesmo lugar.
pub fn normalize_project(raw: &str) -> String {
    let unified = raw.replace('/', "\\");
    let trimmed = unified.trim_end_matches('\\');
    let mut chars = trimmed.chars();
    match (chars.next(), chars.next()) {
        // Só a letra do drive sobe de caixa; o resto do caminho preserva a
        // grafia original, que é o que o usuário reconhece.
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
            format!("{}:{}", drive.to_ascii_uppercase(), chars.as_str())
        }
        _ => trimmed.to_string(),
    }
}

/// Profundidade máxima da subida até a raiz do repositório. Um caminho
/// patológico não pode virar laço.
const MAX_ROOT_WALK: usize = 40;

/// Sobe do diretório de trabalho até a raiz do repositório.
///
/// Sem isto o relatório vira ruído: o `cwd` gravado é quase sempre uma
/// subpasta, e o mesmo repositório aparece dezenas de vezes
/// (`...\InvoiceCore`, `...\InvoiceCore\projects\core\server`, e por aí).
/// `is_root` é injetado para os testes não dependerem do disco.
pub fn repo_root_with<F: Fn(&Path) -> bool>(normalized: &str, is_root: F) -> String {
    let mut current = Path::new(normalized);
    for _ in 0..MAX_ROOT_WALK {
        if is_root(current) {
            return current.to_string_lossy().to_string();
        }
        match current.parent() {
            Some(parent) if parent != current && !parent.as_os_str().is_empty() => {
                current = parent
            }
            _ => break,
        }
    }
    // Fora de um repositório o próprio caminho é a melhor resposta —
    // devolver a raiz do drive juntaria coisas sem relação.
    normalized.to_string()
}

/// Resolve caminhos para a raiz do repositório, memorizando o resultado.
/// São centenas de eventos para poucas dezenas de pastas distintas.
#[derive(Default)]
pub struct ProjectResolver {
    cache: std::collections::HashMap<String, String>,
}

impl ProjectResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(&mut self, raw: &str) -> String {
        let normalized = normalize_project(raw);
        if let Some(hit) = self.cache.get(&normalized) {
            return hit.clone();
        }
        // `.git` é diretório num clone normal e arquivo numa worktree.
        let root = repo_root_with(&normalized, |p| p.join(".git").exists());
        self.cache.insert(normalized, root.clone());
        root
    }
}

/// Todos os `.jsonl` sob uma raiz, recursivamente.
pub fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_into(root, &mut out);
    out.sort();
    out
}

fn walk_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(m) if m.is_dir() => walk_into(&path, out),
            Ok(_) if path.extension().map(|e| e == "jsonl").unwrap_or(false) => out.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ia-monitor-test-{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    fn append(path: &Path, content: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// O ponto central da Fase 2: a segunda leitura não pode reler nada.
    #[test]
    fn segunda_leitura_nao_rele_o_que_ja_foi_lido() {
        let store = Store::open_in_memory().unwrap();
        let p = temp_file("incremental.jsonl", "a\nb\n");

        let first = read_new_lines(&store, &p).unwrap();
        assert_eq!(first.lines, vec!["a", "b"]);
        assert!(first.from_start);
        store.set_tail_cursor(&first.cursor).unwrap();

        let second = read_new_lines(&store, &p).unwrap();
        assert!(second.lines.is_empty(), "nada novo deveria ser lido");

        append(&p, "c\n");
        store.set_tail_cursor(&second.cursor).unwrap();
        let third = read_new_lines(&store, &p).unwrap();
        assert_eq!(third.lines, vec!["c"]);
        assert!(!third.from_start);

        std::fs::remove_file(&p).ok();
    }

    /// O CLI escreve enquanto lemos. Consumir meia linha corromperia o JSON.
    #[test]
    fn linha_incompleta_fica_para_a_proxima_passada() {
        let store = Store::open_in_memory().unwrap();
        let p = temp_file("parcial.jsonl", "completa\nparcial-sem-quebra");

        let first = read_new_lines(&store, &p).unwrap();
        assert_eq!(first.lines, vec!["completa"]);
        store.set_tail_cursor(&first.cursor).unwrap();

        // Agora a linha se completa.
        append(&p, "-agora-sim\n");
        let second = read_new_lines(&store, &p).unwrap();
        assert_eq!(second.lines, vec!["parcial-sem-quebra-agora-sim"]);

        std::fs::remove_file(&p).ok();
    }

    /// O caso que motiva o resolver: subpastas do mesmo repo viravam
    /// projetos distintos e enchiam o relatório de linhas repetidas.
    #[test]
    fn subpastas_sobem_para_a_raiz_do_repositorio() {
        let raiz = r"E:\proj\InvoiceCore";
        let is_root = |p: &Path| p.to_string_lossy() == raiz;

        for sub in [
            r"E:\proj\InvoiceCore",
            r"E:\proj\InvoiceCore\projects\core\server",
            r"E:\proj\InvoiceCore\projects\core\server\src\Invoice.Core.Tests",
        ] {
            assert_eq!(repo_root_with(sub, is_root), raiz, "falhou para {sub}");
        }
    }

    /// Fora de um repositório, devolver a raiz do drive juntaria pastas sem
    /// nenhuma relação entre si.
    #[test]
    fn caminho_fora_de_repositorio_fica_como_esta() {
        let caminho = r"E:\solto\pasta";
        assert_eq!(repo_root_with(caminho, |_| false), caminho);
    }

    /// Dois clones paralelos são pastas diferentes e continuam separados —
    /// juntá-los esconderia que são checkouts distintos.
    #[test]
    fn clones_paralelos_permanecem_distintos() {
        let a = r"E:\proj\InvoiceCore";
        let b = r"E:\proj\feature-tax\InvoiceCore";
        let is_root = |p: &Path| {
            let s = p.to_string_lossy().to_string();
            s == a || s == b
        };
        assert_ne!(
            repo_root_with(r"E:\proj\InvoiceCore\src", is_root),
            repo_root_with(r"E:\proj\feature-tax\InvoiceCore\src", is_root)
        );
    }

    #[test]
    fn resolver_memoriza_e_normaliza() {
        let mut r = ProjectResolver::new();
        // Sem repositório em disco, o resultado é o caminho normalizado.
        assert_eq!(r.resolve("e:/nao/existe/aqui"), r"E:\nao\existe\aqui");
        assert_eq!(r.resolve(r"E:\nao\existe\aqui"), r"E:\nao\existe\aqui");
    }

    /// O mesmo repositório visto por dois provedores tem que virar uma linha.
    #[test]
    fn caminhos_do_mesmo_projeto_convergem() {
        let claude = normalize_project(r"E:\workspaces\InvoiceCore");
        let cursor = normalize_project(r"e:\workspaces\InvoiceCore");
        assert_eq!(claude, cursor);
        assert_eq!(claude, r"E:\workspaces\InvoiceCore");
    }

    #[test]
    fn normaliza_separador_e_barra_final() {
        assert_eq!(
            normalize_project("e:/proj/app/"),
            normalize_project(r"E:\proj\app")
        );
    }

    /// Se o arquivo encolher, o offset antigo aponta para o lugar errado.
    #[test]
    fn arquivo_truncado_recomeca_do_zero() {
        let store = Store::open_in_memory().unwrap();
        let p = temp_file("truncado.jsonl", "linha1\nlinha2\nlinha3\n");
        let first = read_new_lines(&store, &p).unwrap();
        store.set_tail_cursor(&first.cursor).unwrap();

        std::fs::write(&p, "novo\n").unwrap();
        let second = read_new_lines(&store, &p).unwrap();
        assert_eq!(second.lines, vec!["novo"]);
        assert!(second.from_start, "truncamento deve reler do início");

        std::fs::remove_file(&p).ok();
    }
}
