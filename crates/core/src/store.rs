//! Persistência local em SQLite.
//!
//! Duas granularidades convivem: `sample` guarda a série temporal dos
//! medidores (o que a barra mostrava naquele instante) e `event` guarda o
//! consumo requisição a requisição, que é o que permite quebrar por projeto
//! e por modelo.
//!
//! O banco fica em `%LOCALAPPDATA%\ia-monitor\ia-monitor.db` — nunca no
//! diretório do projeto, e nunca com token dentro.

use crate::model::{Provider, ProviderSample};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Um consumo individual já normalizado entre os três provedores.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    /// Chave natural da origem. É o que torna o backfill idempotente:
    /// reimportar o mesmo arquivo não duplica nada.
    pub uid: String,
    pub ts: DateTime<Utc>,
    pub provider: Provider,
    pub model: Option<String>,
    /// Repositório/pasta de trabalho, quando a origem informa.
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    /// Custo em centavos, quando a origem informa (só o Cursor hoje).
    pub cents: Option<f64>,
}

/// Posição de leitura de um arquivo de log, para nunca reler o que já foi
/// lido. `size` e `mtime` detectam truncamento ou rotação do arquivo.
#[derive(Debug, Clone)]
pub struct TailCursor {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub offset: u64,
}

fn provider_key(provider: Provider) -> String {
    format!("provider.enabled:{}", provider.as_str())
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn default_path() -> Result<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| crate::collect::home_dir().map(|h| h.join(".local/share")))
            .ok_or_else(|| anyhow!("não foi possível determinar o diretório de dados"))?;
        Ok(base.join("ia-monitor").join("ia-monitor.db"))
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&Self::default_path()?)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("criando {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("abrindo {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL para o backfill não bloquear leituras da UI.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sample (
                ts        INTEGER NOT NULL,
                provider  TEXT    NOT NULL,
                metric    TEXT    NOT NULL,
                value     REAL,
                headline  TEXT,
                severity  TEXT,
                PRIMARY KEY (ts, provider, metric)
            );
            CREATE INDEX IF NOT EXISTS sample_by_metric ON sample(provider, metric, ts);

            CREATE TABLE IF NOT EXISTS event (
                uid           TEXT PRIMARY KEY,
                ts            INTEGER NOT NULL,
                provider      TEXT    NOT NULL,
                model         TEXT,
                project       TEXT,
                session_id    TEXT,
                input_tokens  INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_tokens  INTEGER NOT NULL DEFAULT 0,
                cents         REAL
            );
            CREATE INDEX IF NOT EXISTS event_by_ts       ON event(ts);
            CREATE INDEX IF NOT EXISTS event_by_provider ON event(provider, ts);
            CREATE INDEX IF NOT EXISTS event_by_project  ON event(project, ts);

            CREATE TABLE IF NOT EXISTS tail_cursor (
                path   TEXT PRIMARY KEY,
                size   INTEGER NOT NULL,
                mtime  INTEGER NOT NULL,
                offset INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS config (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Grava um ponto da série para cada medidor. Reamostrar o mesmo segundo
    /// substitui em vez de duplicar.
    pub fn record_samples(&self, samples: &[ProviderSample]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for s in samples {
            if s.error.is_some() {
                continue;
            }
            let ts = s.observed_at.timestamp();
            for g in &s.gauges {
                tx.execute(
                    "INSERT OR REPLACE INTO sample (ts, provider, metric, value, headline, severity)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        ts,
                        s.provider.as_str(),
                        g.id,
                        g.fraction,
                        g.headline,
                        format!("{:?}", g.severity).to_lowercase()
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Insere eventos ignorando os já conhecidos. Devolve quantos entraram
    /// de fato — é assim que o backfill sabe quando chegou ao fim.
    pub fn insert_events(&self, events: &[UsageEvent]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO event
                 (uid, ts, provider, model, project, session_id,
                  input_tokens, output_tokens, cache_tokens, cents)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for e in events {
                inserted += stmt.execute(params![
                    e.uid,
                    e.ts.timestamp(),
                    e.provider.as_str(),
                    e.model,
                    e.project,
                    e.session_id,
                    e.input_tokens,
                    e.output_tokens,
                    e.cache_tokens,
                    e.cents,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn tail_cursor(&self, path: &str) -> Result<Option<TailCursor>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT size, mtime, offset FROM tail_cursor WHERE path = ?1",
                params![path],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional()?;
        Ok(row.map(|(size, mtime, offset)| TailCursor {
            path: path.to_string(),
            size: size as u64,
            mtime,
            offset: offset as u64,
        }))
    }

    pub fn set_tail_cursor(&self, c: &TailCursor) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tail_cursor (path, size, mtime, offset) VALUES (?1,?2,?3,?4)",
            params![c.path, c.size as i64, c.mtime, c.offset as i64],
        )?;
        Ok(())
    }

    /// Um provedor desligado some da UI **e para de ser consultado** — o
    /// ganho maior é esse: cota de requisição não gasta com quem não se usa.
    ///
    /// O padrão é ligado, então uma instalação nova mostra tudo e quem não
    /// tem uma das assinaturas desliga o que sobra.
    pub fn provider_enabled(&self, provider: Provider) -> bool {
        self.config_get(&provider_key(provider))
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    pub fn set_provider_enabled(&self, provider: Provider, enabled: bool) -> Result<()> {
        self.config_set(&provider_key(provider), if enabled { "1" } else { "0" })
    }

    /// Na ordem fixa de `Provider::ALL`, para a pílula não trocar de lugar.
    pub fn enabled_providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|p| self.provider_enabled(*p))
            .collect()
    }

    pub fn config_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM config WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Série de um medidor desde um instante — alimenta os gráficos.
    pub fn series(
        &self,
        provider: Provider,
        metric: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::analytics::Point>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, value FROM sample
             WHERE provider = ?1 AND metric = ?2 AND ts >= ?3 AND value IS NOT NULL
             ORDER BY ts",
        )?;
        let rows = stmt.query_map(
            params![provider.as_str(), metric, since.timestamp()],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (ts, value) = row?;
            if let Some(dt) = Utc.timestamp_opt(ts, 0).single() {
                out.push((dt, value));
            }
        }
        Ok(out)
    }

    /// Total consumido por chave (projeto ou modelo) numa janela.
    pub fn totals_by(
        &self,
        group: GroupBy,
        since: DateTime<Utc>,
    ) -> Result<Vec<UsageTotal>> {
        let column = match group {
            GroupBy::Project => "COALESCE(project, '(sem projeto)')",
            GroupBy::Model => "COALESCE(model, '(sem modelo)')",
            GroupBy::Provider => "provider",
        };
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {column} AS k,
                    SUM(input_tokens), SUM(output_tokens), SUM(cache_tokens),
                    SUM(COALESCE(cents,0)), COUNT(*)
             FROM event WHERE ts >= ?1
             GROUP BY k ORDER BY SUM(input_tokens + output_tokens) DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![since.timestamp()], |r| {
            Ok(UsageTotal {
                key: r.get(0)?,
                input_tokens: r.get(1)?,
                output_tokens: r.get(2)?,
                cache_tokens: r.get(3)?,
                cents: r.get(4)?,
                events: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Apaga tudo que é derivado dos logs. Usado quando a regra de
    /// interpretação muda (ex.: normalização de caminho) e os dados já
    /// gravados passariam a conviver com os novos em formatos diferentes.
    /// O histórico original continua em disco — reconstruir custa segundos.
    pub fn reset_derived(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM event;
             DELETE FROM tail_cursor;
             DELETE FROM config WHERE key LIKE 'codex.ctx:%';",
        )?;
        Ok(())
    }

    pub fn event_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?)
    }

    /// Instante do evento mais recente de um provedor — usado para decidir
    /// de onde o backfill incremental continua.
    pub fn latest_event_ts(&self, provider: Provider) -> Result<Option<DateTime<Utc>>> {
        let conn = self.conn.lock().unwrap();
        let ts: Option<i64> = conn.query_row(
            "SELECT MAX(ts) FROM event WHERE provider = ?1",
            params![provider.as_str()],
            |r| r.get(0),
        )?;
        Ok(ts.and_then(|t| Utc.timestamp_opt(t, 0).single()))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum GroupBy {
    Project,
    Model,
    Provider,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageTotal {
    pub key: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_tokens: i64,
    pub cents: f64,
    pub events: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(uid: &str, secs: i64, project: &str) -> UsageEvent {
        UsageEvent {
            uid: uid.into(),
            ts: Utc.timestamp_opt(secs, 0).unwrap(),
            provider: Provider::Claude,
            model: Some("claude-opus-5".into()),
            project: Some(project.into()),
            session_id: Some("s1".into()),
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 100,
            cents: None,
        }
    }

    /// O backfill roda de novo a cada inicialização; reimportar não pode
    /// inflar os números.
    /// Instalacao nova mostra tudo; quem nao tem uma das assinaturas desliga.
    #[test]
    fn provedores_comecam_todos_ligados() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.enabled_providers().len(), 3);
        assert!(s.provider_enabled(Provider::Codex));
    }

    #[test]
    fn desligar_um_provedor_o_tira_da_lista() {
        let s = Store::open_in_memory().unwrap();
        s.set_provider_enabled(Provider::Codex, false).unwrap();

        assert!(!s.provider_enabled(Provider::Codex));
        let ativos = s.enabled_providers();
        assert_eq!(ativos, vec![Provider::Claude, Provider::Cursor]);
        assert!(!ativos.contains(&Provider::Codex));
    }

    /// A ordem e fixa para a pilula nao trocar de lugar entre ciclos.
    #[test]
    fn ordem_dos_ativos_e_estavel() {
        let s = Store::open_in_memory().unwrap();
        s.set_provider_enabled(Provider::Claude, false).unwrap();
        assert_eq!(s.enabled_providers(), vec![Provider::Cursor, Provider::Codex]);
    }

    #[test]
    fn religar_devolve_o_provedor() {
        let s = Store::open_in_memory().unwrap();
        s.set_provider_enabled(Provider::Cursor, false).unwrap();
        s.set_provider_enabled(Provider::Cursor, true).unwrap();
        assert!(s.provider_enabled(Provider::Cursor));
    }

    /// Desligar todos e um estado valido: a UI avisa em vez de ficar vazia.
    #[test]
    fn desligar_todos_e_permitido() {
        let s = Store::open_in_memory().unwrap();
        for p in Provider::ALL {
            s.set_provider_enabled(p, false).unwrap();
        }
        assert!(s.enabled_providers().is_empty());
    }

    #[test]
    fn reimportar_o_mesmo_evento_nao_duplica() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.insert_events(&[ev("a", 100, "p")]).unwrap(), 1);
        assert_eq!(s.insert_events(&[ev("a", 100, "p")]).unwrap(), 0);
        assert_eq!(s.event_count().unwrap(), 1);
    }

    #[test]
    fn totais_agrupam_por_projeto() {
        let s = Store::open_in_memory().unwrap();
        s.insert_events(&[ev("a", 100, "alpha"), ev("b", 200, "alpha"), ev("c", 300, "beta")])
            .unwrap();
        let t = s.totals_by(GroupBy::Project, Utc.timestamp_opt(0, 0).unwrap()).unwrap();
        let alpha = t.iter().find(|x| x.key == "alpha").unwrap();
        assert_eq!(alpha.events, 2);
        assert_eq!(alpha.input_tokens, 20);
    }

    #[test]
    fn cursor_de_tailing_sobrevive_ao_reinicio() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.tail_cursor("x.jsonl").unwrap().is_none());
        s.set_tail_cursor(&TailCursor {
            path: "x.jsonl".into(),
            size: 500,
            mtime: 42,
            offset: 480,
        })
        .unwrap();
        let c = s.tail_cursor("x.jsonl").unwrap().unwrap();
        assert_eq!(c.offset, 480);
        assert_eq!(c.size, 500);
    }

    #[test]
    fn serie_preserva_ordem_temporal() {
        let s = Store::open_in_memory().unwrap();
        let sample = |ts: i64, v: f64| crate::model::ProviderSample {
            provider: Provider::Claude,
            plan: None,
            gauges: vec![crate::model::Gauge {
                id: "claude.session".into(),
                label: "Sessão".into(),
                fraction: Some(v),
                headline: format!("{}%", (v * 100.0) as i64),
                subtitle: None,
                severity: crate::model::Severity::Normal,
                resets_at: None,
                active: true,
                expected: None,
            }],
            observed_at: Utc.timestamp_opt(ts, 0).unwrap(),
            source_at: None,
            error: None,
            retry_after: None,
        };
        s.record_samples(&[sample(300, 0.3), sample(100, 0.1), sample(200, 0.2)])
            .unwrap();
        let serie = s
            .series(Provider::Claude, "claude.session", Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        let valores: Vec<f64> = serie.iter().map(|(_, v)| *v).collect();
        assert_eq!(valores, vec![0.1, 0.2, 0.3]);
    }

    /// Um provedor em falha não pode gravar ponto na série — isso criaria
    /// um buraco de zeros que estragaria o cálculo de burn rate.
    #[test]
    fn provedor_com_erro_nao_grava_amostra() {
        let s = Store::open_in_memory().unwrap();
        s.record_samples(&[ProviderSample::failed(Provider::Cursor, "offline")])
            .unwrap();
        let serie = s
            .series(Provider::Cursor, "cursor.auto", Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert!(serie.is_empty());
    }
}
