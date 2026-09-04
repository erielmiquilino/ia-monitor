// A janela é sem decoração e transparente; sem isto o Windows abriria um
// console atrás dela em builds de release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alerts;
mod scheduler;
mod snapshot;
mod trayicon;
mod uicheck;

use alerts::AlertState;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use ia_monitor_core::model::{Provider, ProviderSample};
use ia_monitor_core::store::Store;
use snapshot::SnapshotView;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewWindow};
use tauri_plugin_notification::NotificationExt;

const PILL: (f64, f64) = (320.0, 34.0);
const CARD: (f64, f64) = (360.0, 420.0);
const POSITION_KEY: &str = "window.position";
/// Card fixado precisa sobreviver ao reinício: quem deixa aberto quer ver
/// os números sem clicar toda vez que liga a máquina.
const EXPANDED_KEY: &str = "window.expanded";

/// Onde a pílula estava antes de virar card, para o recolhimento devolvê-la
/// ao mesmo lugar. Sem isto o card sobe para caber na tela e, ao encolher, a
/// janela fica onde estava o topo do card — longe de onde o usuário clicou.
struct Anchor {
    pill: (f64, f64),
    /// Posição do card logo após expandir. Se ela mudar, o usuário arrastou
    /// a janela e a posição antiga da pílula deixou de valer.
    card: (f64, f64),
}

struct AppState {
    store: Arc<Store>,
    latest: Mutex<Option<SnapshotView>>,
    alerts: Mutex<AlertState>,
    paused: Mutex<bool>,
    anchor: Mutex<Option<Anchor>>,
    /// Acorda o laço fora do ritmo normal. Sem isto, ligar ou desligar um
    /// provedor só apareceria no próximo ciclo — até um minuto depois.
    wake: Arc<tokio::sync::Notify>,
}

fn provider_menu_id(p: Provider) -> String {
    format!("prov:{}", p.as_str())
}

/// Nova posição ao trocar de tamanho mantendo o canto **inferior direito**
/// fixo. É o que faz o card parecer brotar da pílula em vez de deslocá-la.
fn anchored_bottom_right(pos: (f64, f64), from: (f64, f64), to: (f64, f64)) -> (f64, f64) {
    (pos.0 + from.0 - to.0, pos.1 + from.1 - to.1)
}

fn same_spot(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 2.0 && (a.1 - b.1).abs() < 2.0
}

/// A pílula e o card são a MESMA janela em dois tamanhos. Um segundo webview
/// custaria dezenas de MB para mostrar o mesmo dado.
#[tauri::command]
fn set_expanded(
    window: WebviewWindow,
    state: tauri::State<'_, Arc<AppState>>,
    expanded: bool,
) -> Result<(), String> {
    // Ao recolher, o tamanho de origem é o que a janela tem AGORA, não o
    // piso `CARD`: o card cresceu para caber o conteúdo.
    let atual = logical_size(&window).unwrap_or(CARD);
    let (from, to) = if expanded { (PILL, CARD) } else { (atual, PILL) };
    let before = logical_position(&window);

    window
        .set_size(LogicalSize::new(to.0, to.1))
        .map_err(|e| e.to_string())?;

    if let Some(before) = before {
        let mut anchor = state.anchor.lock().unwrap();
        let target = if expanded {
            anchored_bottom_right(before, from, to)
        } else {
            // Volta exatamente para onde a pílula estava, a menos que o
            // usuário tenha arrastado o card — aí o canto inferior direito
            // do card é a referência honesta.
            match anchor.as_ref() {
                Some(a) if same_spot(before, a.card) => a.pill,
                _ => anchored_bottom_right(before, from, to),
            }
        };

        let placed = place(&window, target, to);
        *anchor = if expanded {
            Some(Anchor { pill: before, card: placed })
        } else {
            None
        };
    }

    let _ = state
        .store
        .config_set(EXPANDED_KEY, if expanded { "1" } else { "0" });
    Ok(())
}

/// Ajusta a altura do card ao conteúdo, dentro do que a tela comporta.
///
/// A altura fixa anterior fazia sobrar espaço morto com poucos medidores e
/// aparecer barra de rolagem quando as notas de ritmo e projeção entravam.
/// Devolve a altura aplicada: se veio menor que a pedida, o webview reativa
/// a rolagem em vez de cortar conteúdo.
#[tauri::command]
fn fit_card(window: WebviewWindow, height: f64) -> f64 {
    let disponivel = monitor_bounds(&window)
        .map(|(mpos, msize, _)| msize.1 - TASKBAR_RESERVE - EDGE_MARGIN * 2.0 - mpos.1.max(0.0))
        .unwrap_or(CARD.1);

    let alvo = height.clamp(CARD.1, disponivel.max(CARD.1));
    let _ = window.set_size(LogicalSize::new(CARD.0, alvo));
    keep_on_screen(&window, CARD.0, alvo);
    alvo
}

/// Estado inicial da janela, consultado pelo webview ao abrir.
#[tauri::command]
fn start_expanded(state: tauri::State<'_, Arc<AppState>>) -> bool {
    state
        .store
        .config_get(EXPANDED_KEY)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

#[tauri::command]
fn current_snapshot(state: tauri::State<'_, Arc<AppState>>) -> Option<SnapshotView> {
    state.latest.lock().ok()?.clone()
}

/// Persiste sempre a posição equivalente da **pílula**, em coordenadas
/// lógicas.
///
/// Dois cuidados aqui, ambos já custaram bug: gravar a posição física e
/// restaurá-la como lógica faz a janela escorregar a cada reinício num
/// monitor com escala; e gravar a posição do card faria a pílula reaparecer
/// no canto onde o card começava.
fn save_position(store: &Store, window: &WebviewWindow, expanded: bool) {
    // Minimizar dispara `Moved` com (-32000,-32000). Gravar isso deixaria a
    // posição salva impossível de restaurar.
    if window.is_minimized().unwrap_or(false) {
        return;
    }
    let Some(current) = logical_position(window) else {
        return;
    };
    if !on_some_monitor(window, current) {
        return;
    }
    let pill = if expanded {
        anchored_bottom_right(current, logical_size(window).unwrap_or(CARD), PILL)
    } else {
        current
    };
    let _ = store.config_set(POSITION_KEY, &format!("{},{}", pill.0, pill.1));
}

/// Altura reservada para a barra de tarefas. O monitor reporta a tela
/// inteira, então sem esta folga a janela fica parcialmente coberta.
const TASKBAR_RESERVE: f64 = 56.0;
const EDGE_MARGIN: f64 = 8.0;

/// Limita um retângulo à área útil do monitor. Puro, para poder ser testado.
fn clamp_to_bounds(
    pos: (f64, f64),
    size: (f64, f64),
    monitor_pos: (f64, f64),
    monitor_size: (f64, f64),
) -> (f64, f64) {
    let min_x = monitor_pos.0 + EDGE_MARGIN;
    let min_y = monitor_pos.1 + EDGE_MARGIN;
    let max_x = (monitor_pos.0 + monitor_size.0 - size.0 - EDGE_MARGIN).max(min_x);
    let max_y = (monitor_pos.1 + monitor_size.1 - size.1 - TASKBAR_RESERVE).max(min_y);
    (pos.0.clamp(min_x, max_x), pos.1.clamp(min_y, max_y))
}

/// Monitor da janela em coordenadas lógicas: (posição, tamanho, escala).
/// Janela ainda oculta não tem monitor "atual"; sem o fallback o clamp
/// simplesmente não acontece e o card nasce fora da tela.
fn monitor_bounds(window: &WebviewWindow) -> Option<((f64, f64), (f64, f64), f64)> {
    let monitor = match window.current_monitor() {
        Ok(Some(m)) => m,
        _ => window.primary_monitor().ok().flatten()?,
    };
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    Some(((pos.x, pos.y), (size.width, size.height), scale))
}

/// Tamanho atual da janela em coordenadas lógicas.
///
/// Desde que o card passou a se ajustar ao conteúdo, `CARD` é só um piso —
/// ancorar por ele deixaria a pílula deslocada pela diferença entre a altura
/// real e a mínima.
fn logical_size(window: &WebviewWindow) -> Option<(f64, f64)> {
    let (_, _, scale) = monitor_bounds(window)?;
    let s = window.outer_size().ok()?.to_logical::<f64>(scale);
    Some((s.width, s.height))
}

fn logical_position(window: &WebviewWindow) -> Option<(f64, f64)> {
    let (_, _, scale) = monitor_bounds(window)?;
    let p = window.outer_position().ok()?.to_logical::<f64>(scale);
    Some((p.x, p.y))
}

/// Move a janela para `target`, limitada à tela, e devolve onde ela ficou.
fn place(window: &WebviewWindow, target: (f64, f64), size: (f64, f64)) -> (f64, f64) {
    let Some((mpos, msize, _)) = monitor_bounds(window) else {
        return target;
    };
    let final_pos = clamp_to_bounds(target, size, mpos, msize);
    let _ = window.set_position(LogicalPosition::new(final_pos.0, final_pos.1));
    final_pos
}

/// Mantém a janela inteira dentro do monitor, sem movê-la à toa.
///
/// Sem isto, abrir já expandido perto da borda inferior joga o card para fora
/// da tela — e como a janela não aparece na barra de tarefas, não há como
/// trazê-la de volta.
fn keep_on_screen(window: &WebviewWindow, w: f64, h: f64) {
    let Some(current) = logical_position(window) else {
        return;
    };
    let Some((mpos, msize, _)) = monitor_bounds(window) else {
        return;
    };
    let clamped = clamp_to_bounds(current, (w, h), mpos, msize);
    if !same_spot(clamped, current) {
        let _ = window.set_position(LogicalPosition::new(clamped.0, clamped.1));
    }
}

/// Primeira execução: canto inferior direito, acima da barra de tarefas.
/// O padrão (0,0) cobre justamente a barra de abas do navegador, que é onde
/// o usuário mais clica.
fn default_position(window: &WebviewWindow) {
    let Ok(Some(monitor)) = window.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    let _ = window.set_position(LogicalPosition::new(
        pos.x + size.width - PILL.0 - 24.0,
        pos.y + size.height - PILL.1 - TASKBAR_RESERVE,
    ));
}

/// A posição cai sobre algum monitor conectado?
///
/// Serve para dois casos: monitor desconectado desde a última execução, e a
/// posição de parqueamento que o Windows dá a janelas minimizadas.
fn on_some_monitor(window: &WebviewWindow, pos: (f64, f64)) -> bool {
    window
        .available_monitors()
        .map(|monitors| {
            monitors.iter().any(|m| {
                let scale = m.scale_factor();
                let p = m.position().to_logical::<f64>(scale);
                let s = m.size().to_logical::<f64>(scale);
                pos.0 >= p.x - 50.0
                    && pos.1 >= p.y - 50.0
                    && pos.0 < p.x + s.width
                    && pos.1 < p.y + s.height
            })
        })
        .unwrap_or(false)
}

/// Posição salva da pílula, em coordenadas lógicas.
fn saved_pill_position(store: &Store) -> Option<(f64, f64)> {
    let raw = store.config_get(POSITION_KEY).ok().flatten()?;
    let (x, y) = raw.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Restaura a posição, já no tamanho do modo em que a janela vai abrir.
///
/// A posição guardada é sempre a da pílula; abrir direto no card exige
/// converter pela mesma âncora usada ao expandir, senão o card nasce
/// deslocado do lugar onde o usuário deixou a janela.
fn restore_position(store: &Store, window: &WebviewWindow, expanded: bool) {
    let size = if expanded { CARD } else { PILL };

    let Some(pill) = saved_pill_position(store) else {
        default_position(window);
        return;
    };

    let Some((mpos, msize, _)) = monitor_bounds(window) else {
        default_position(window);
        return;
    };

    // Monitor desconectado deixaria a janela fora da tela, e como ela não
    // aparece na barra de tarefas não haveria como trazê-la de volta.
    if !on_some_monitor(window, pill) {
        default_position(window);
        return;
    }

    let target = if expanded {
        anchored_bottom_right(pill, PILL, CARD)
    } else {
        pill
    };
    let clamped = clamp_to_bounds(target, size, mpos, msize);
    let _ = window.set_position(LogicalPosition::new(clamped.0, clamped.1));
}

/// Traz a janela de volta — inclusive de um estado minimizado.
///
/// `show()` só mexe em visibilidade e não desfaz minimização. "Mostrar
/// desktop" (Win+D) minimiza tudo, e o Windows parqueia a janela minimizada
/// em (-32000,-32000); sem `unminimize` o clique na bandeja não trazia nada
/// de volta, porque a janela já estava "visível" — só que fora da tela.
fn show_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_focus();

    // A posição parqueada da minimização não é uma posição real; devolve a
    // janela para onde ela estava antes.
    if let (Ok(store), Some(pos)) = (Store::open_default(), logical_position(&w)) {
        if !on_some_monitor(&w, pos) {
            let expandido = store
                .config_get(EXPANDED_KEY)
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            restore_position(&store, &w, expandido);
        }
    }
}

/// Estado de coleta de um provedor. Cada um tem seu próprio relógio: um
/// pode estar recuando por 429 enquanto os outros seguem normalmente.
struct PollState {
    due: DateTime<Utc>,
    failures: u32,
    /// Último dado bom. Uma falha temporária não deve apagar números reais
    /// da tela — mostramos o valor anterior com a idade dele.
    last_good: Option<ProviderSample>,
    last_error: Option<String>,
}

/// Espaçamento mínimo entre coletas de um mesmo provedor **entre execuções**.
///
/// Toda inicialização dispara uma coleta imediata. Sem esta trava, abrir e
/// fechar o app em sequência gera uma rajada de requisições que nenhuma
/// cadência interna evita — foi o que provavelmente rendeu o 429.
const RESTART_GAP_SECONDS: i64 = 30;

fn last_poll_key(p: Provider) -> String {
    format!("poll.last:{}", p.as_str())
}

/// O histórico do Cursor também custa rede e também dispara na
/// inicialização — precisa da mesma trava entre execuções que os medidores.
const CURSOR_HISTORY_KEY: &str = "poll.last:cursor-history";
const CURSOR_HISTORY_MINUTES: i64 = 30;

fn cursor_history_due(store: &Store, now: DateTime<Utc>) -> DateTime<Utc> {
    store
        .config_get(CURSOR_HISTORY_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|ts| chrono::TimeZone::timestamp_opt(&Utc, ts, 0).single())
        .map(|t| now.max(t + ChronoDuration::minutes(CURSOR_HISTORY_MINUTES)))
        .unwrap_or(now)
}

impl PollState {
    fn new(now: DateTime<Utc>) -> Self {
        Self { due: now, failures: 0, last_good: None, last_error: None }
    }

    /// Estado inicial que respeita a última coleta da execução anterior.
    fn restored(store: &Store, provider: Provider, now: DateTime<Utc>) -> Self {
        let anterior = store
            .config_get(&last_poll_key(provider))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|ts| chrono::TimeZone::timestamp_opt(&Utc, ts, 0).single());

        let due = match anterior {
            Some(t) => now.max(t + ChronoDuration::seconds(RESTART_GAP_SECONDS)),
            None => now,
        };
        Self { due, ..Self::new(now) }
    }
}

/// O que a UI recebe para um provedor: dado fresco, ou o último bom com a
/// idade e o motivo de não ter atualizado.
fn display_sample(provider: Provider, st: &PollState, now: DateTime<Utc>) -> ProviderSample {
    match (&st.last_good, &st.last_error) {
        (Some(good), erro) => {
            let mut s = good.clone();
            s.observed_at = now;
            // `source_at` fica intocado: é o que faz a UI mostrar a idade.
            s.error = erro.clone();
            s.retry_after = erro
                .as_ref()
                .map(|_| (st.due - now).num_seconds().max(0));
            s
        }
        (None, Some(msg)) => ProviderSample::failed(provider, msg),
        (None, None) => ProviderSample::failed(provider, "aguardando primeira coleta"),
    }
}

/// Laço principal: coleta, persiste, publica e alerta.
async fn run_loop(app: AppHandle, state: Arc<AppState>) {
    let client = ia_monitor_core::http_client();
    let mut poll: HashMap<Provider, PollState> = Provider::ALL
        .iter()
        .map(|p| (*p, PollState::restored(&state.store, *p, Utc::now())))
        .collect();
    // O histórico do Cursor custa rede e muda devagar; tem relógio próprio.
    let mut historico_due = cursor_history_due(&state.store, Utc::now());

    loop {
        if *state.paused.lock().unwrap() {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }

        let now = Utc::now();
        let idle = scheduler::idle_seconds();
        let mut fresh: Vec<ProviderSample> = Vec::new();

        // Provedor desligado não é consultado nem exibido. Descartar o estado
        // dele também evita que um dado velho reapareça ao religar.
        let ativos = state.store.enabled_providers();
        poll.retain(|p, _| ativos.contains(p));

        for provider in ativos.iter().copied() {
            poll.entry(provider)
                .or_insert_with(|| PollState::restored(&state.store, provider, now));
            let vencido = poll.get(&provider).map(|s| s.due <= now).unwrap_or(true);
            if !vencido {
                continue;
            }

            let amostra = ia_monitor_core::sample_one(provider, &client, None).await;
            // Registrado mesmo em falha: uma tentativa que estourou limite
            // conta tanto quanto uma que deu certo.
            let _ = state
                .store
                .config_set(&last_poll_key(provider), &now.timestamp().to_string());
            let st = poll.get_mut(&provider).expect("estado do provedor");

            if amostra.error.is_none() {
                st.failures = 0;
                st.last_error = None;
                st.due = scheduler::next_due(
                    provider,
                    now,
                    idle,
                    0,
                    None,
                    scheduler::nearest_reset(&amostra, now),
                );
                st.last_good = Some(amostra.clone());
                fresh.push(amostra);
            } else {
                // A contagem de falhas é DESTE provedor. Antes era global e
                // zerava se qualquer outro respondesse, o que fazia um 429
                // ser consultado de novo a cada minuto, indefinidamente.
                st.failures = st.failures.saturating_add(1);
                st.last_error = amostra.error.clone();
                st.due = scheduler::next_due(
                    provider,
                    now,
                    idle,
                    st.failures,
                    amostra.retry_after,
                    None,
                );
            }
        }

        // Só o que foi lido agora entra na série. Regravar um valor antigo
        // com carimbo novo faria o burn rate enxergar consumo parado.
        if !fresh.is_empty() {
            let store = state.store.clone();
            let to_record = fresh.clone();
            // SQLite e leitura de logs são bloqueantes; fora do executor async.
            let claude_on = ativos.contains(&Provider::Claude);
            let codex_on = ativos.contains(&Provider::Codex);
            let _ = tokio::task::spawn_blocking(move || {
                let _ = store.record_samples(&to_record);
                // Ler log de provedor desligado seria trabalho jogado fora.
                if claude_on {
                    let _ = ia_monitor_core::ingest::claude_jsonl::ingest(&store);
                }
                if codex_on {
                    let _ = ia_monitor_core::ingest::codex_rollout::ingest(&store);
                }
            })
            .await;
        }

        if historico_due <= now && ativos.contains(&Provider::Cursor) {
            let _ = ia_monitor_core::ingest::cursor_events::ingest(&state.store, &client).await;
            let _ = state
                .store
                .config_set(CURSOR_HISTORY_KEY, &now.timestamp().to_string());
            historico_due = now + ChronoDuration::minutes(CURSOR_HISTORY_MINUTES);
        }

        let samples: Vec<ProviderSample> = ativos
            .iter()
            .filter_map(|p| poll.get(p).map(|st| display_sample(*p, st, now)))
            .collect();
        let view = {
            let store = state.store.clone();
            let samples = samples.clone();
            tokio::task::spawn_blocking(move || snapshot::build(&store, &samples, now))
                .await
                .ok()
        };

        if let Some(view) = view {
            *state.latest.lock().unwrap() = Some(view.clone());
            let _ = app.emit("snapshot", view);
        }

        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_icon(Some(trayicon::draw(&snapshot::tray_fractions(&samples))));
            let _ = tray.set_tooltip(Some(trayicon::tooltip(&samples)));
        }

        let notices = state.alerts.lock().unwrap().evaluate(&samples);
        for n in notices {
            let _ = app
                .notification()
                .builder()
                .title(&n.title)
                .body(&n.body)
                .show();
        }

        // Dorme até o próximo provedor vencer, e não além disso: quem está
        // recuando por 429 não pode segurar quem está saudável. Ligar ou
        // desligar um provedor interrompe a espera.
        let proximo = poll.values().map(|s| s.due).min();
        let espera = scheduler::sleep_until(proximo, Utc::now());
        tokio::select! {
            _ = tokio::time::sleep(espera) => {}
            _ = state.wake.notified() => {}
        }
    }
}

fn main() {
    tauri::Builder::default()
        // Uma segunda instância só duplicaria o consumo e brigaria pela
        // bandeja; a janela existente vem para a frente.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![set_expanded, current_snapshot, start_expanded, fit_card])
        .setup(|app| {
            let store = Arc::new(Store::open_default()?);
            let state = Arc::new(AppState {
                store: store.clone(),
                latest: Mutex::new(None),
                alerts: Mutex::new(AlertState::new()),
                paused: Mutex::new(false),
                anchor: Mutex::new(None),
                wake: Arc::new(tokio::sync::Notify::new()),
            });
            app.manage(state.clone());

            let window = app
                .get_webview_window("main")
                .ok_or("janela principal não encontrada")?;
            let expandido = store
                .config_get(EXPANDED_KEY)
                .ok()
                .flatten()
                .map(|v| v == "1")
                .unwrap_or(false);
            let (w, h) = if expandido { CARD } else { PILL };
            window.set_size(LogicalSize::new(w, h))?;
            // Só depois de visível a janela reporta monitor e posição reais.
            window.show()?;
            restore_position(&store, &window, expandido);
            keep_on_screen(&window, w, h);

            // Arrastar a janela persiste a posição. O evento também dispara
            // nos nossos próprios `set_position`, e é por isso que
            // `save_position` sempre normaliza para a posição da pílula.
            {
                let store = store.clone();
                let state = state.clone();
                let w = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Moved(_) = event {
                        let expandido = state
                            .store
                            .config_get(EXPANDED_KEY)
                            .ok()
                            .flatten()
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        save_position(&store, &w, expandido);
                    }
                });
            }

            let abrir = MenuItemBuilder::with_id("abrir", "Mostrar").build(app)?;
            let pausar = CheckMenuItemBuilder::with_id("pausar", "Pausar coleta")
                .checked(false)
                .build(app)?;

            // Um item por provedor. Quem não tem uma das assinaturas desliga
            // e para de ver — e de consultar — o que não usa.
            let mut itens_provedor = Vec::new();
            for p in Provider::ALL {
                let item = CheckMenuItemBuilder::with_id(provider_menu_id(p), p.label())
                    .checked(store.provider_enabled(p))
                    .build(app)?;
                itens_provedor.push((p, item));
            }
            let mut provedores = SubmenuBuilder::new(app, "Provedores");
            for (_, item) in &itens_provedor {
                provedores = provedores.item(item);
            }
            let provedores = provedores.build()?;

            let autostart = CheckMenuItemBuilder::with_id("autostart", "Iniciar com o Windows")
                .checked({
                    use tauri_plugin_autostart::ManagerExt;
                    app.autolaunch().is_enabled().unwrap_or(false)
                })
                .build(app)?;
            let sair = MenuItemBuilder::with_id("sair", "Sair").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&abrir, &pausar])
                .item(&provedores)
                .items(&[&autostart])
                .separator()
                .items(&[&sair])
                .build()?;

            let tray_state = state.clone();
            TrayIconBuilder::with_id("main")
                .icon(trayicon::draw(&[]))
                .tooltip("IA Monitor — coletando…")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "abrir" => show_window(app),
                    "pausar" => {
                        let mut paused = tray_state.paused.lock().unwrap();
                        *paused = !*paused;
                    }
                    "autostart" => {
                        use tauri_plugin_autostart::ManagerExt;
                        let manager = app.autolaunch();
                        let _ = if manager.is_enabled().unwrap_or(false) {
                            manager.disable()
                        } else {
                            manager.enable()
                        };
                    }
                    "sair" => app.exit(0),
                    outro => {
                        if let Some((p, item)) =
                            itens_provedor.iter().find(|(p, _)| provider_menu_id(*p) == outro)
                        {
                            let novo = !tray_state.store.provider_enabled(*p);
                            let _ = tray_state.store.set_provider_enabled(*p, novo);
                            let _ = item.set_checked(novo);
                            // Acorda o laço: esperar até 60s para a mudança
                            // aparecer faria o clique parecer sem efeito.
                            tray_state.wake.notify_one();
                        }
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(run_loop(handle, state));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("falha ao iniciar o IA Monitor");
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPOS: (f64, f64) = (0.0, 0.0);
    const MSIZE: (f64, f64) = (1536.0, 864.0);

    /// Expandir e recolher tem que devolver a janela ao ponto de partida,
    /// mesmo quando o card precisa subir para caber na tela. Era o bug: a
    /// pílula reaparecia onde estava o topo do card.
    #[test]
    fn expandir_e_recolher_volta_ao_mesmo_lugar() {
        // Pílula no canto inferior direito, como no padrão.
        let pilula = (1192.0, 774.0);

        let alvo_card = anchored_bottom_right(pilula, PILL, CARD);
        let card = clamp_to_bounds(alvo_card, CARD, MPOS, MSIZE);
        assert!(card.1 < pilula.1, "o card precisa subir para caber");

        let alvo_pilula = anchored_bottom_right(card, CARD, PILL);
        let voltou = clamp_to_bounds(alvo_pilula, PILL, MPOS, MSIZE);
        assert!(
            same_spot(voltou, pilula),
            "voltou para {voltou:?}, esperado {pilula:?}"
        );
    }

    /// O card deve parecer brotar da pílula: mesma borda inferior direita.
    #[test]
    fn card_nasce_ancorado_na_pilula() {
        let pilula = (1192.0, 774.0);
        let card = anchored_bottom_right(pilula, PILL, CARD);
        assert_eq!(pilula.0 + PILL.0, card.0 + CARD.0, "borda direita fixa");
        assert_eq!(pilula.1 + PILL.1, card.1 + CARD.1, "borda inferior fixa");
    }

    /// Perto do topo o card não cabe acima; o clamp o segura na borda e o
    /// retorno usa a posição memorizada da pílula, não a geometria.
    #[test]
    fn perto_do_topo_o_card_e_contido_pela_borda() {
        let pilula = (40.0, 20.0);
        let card = clamp_to_bounds(anchored_bottom_right(pilula, PILL, CARD), CARD, MPOS, MSIZE);
        assert!(card.1 >= MPOS.1 + EDGE_MARGIN, "não pode sair pelo topo");
        // A geometria pura não devolveria a pílula ao lugar aqui — é por isso
        // que a posição original é memorizada em `Anchor`.
        let geometrico = clamp_to_bounds(
            anchored_bottom_right(card, CARD, PILL),
            PILL,
            MPOS,
            MSIZE,
        );
        assert!(!same_spot(geometrico, pilula));
    }

    #[test]
    fn a_janela_nunca_sai_da_tela() {
        for alvo in [(-500.0, -500.0), (5000.0, 5000.0), (1500.0, 850.0)] {
            let (x, y) = clamp_to_bounds(alvo, CARD, MPOS, MSIZE);
            assert!(x >= MPOS.0, "x={x}");
            assert!(y >= MPOS.1, "y={y}");
            assert!(x + CARD.0 <= MPOS.0 + MSIZE.0, "borda direita: {x}");
            assert!(y + CARD.1 <= MPOS.1 + MSIZE.1, "borda inferior: {y}");
        }
    }

    /// A barra de tarefas cobriria o rodapé do card.
    #[test]
    fn respeita_a_barra_de_tarefas() {
        let (_, y) = clamp_to_bounds((0.0, 9999.0), PILL, MPOS, MSIZE);
        assert!(y + PILL.1 <= MSIZE.1 - TASKBAR_RESERVE + 0.01, "y={y}");
    }

    /// Monitor secundário à esquerda tem coordenadas negativas.
    #[test]
    fn funciona_em_monitor_com_origem_negativa() {
        let mpos = (-1920.0, 0.0);
        let (x, y) = clamp_to_bounds((-5000.0, 0.0), PILL, mpos, (1920.0, 1080.0));
        assert!(x >= mpos.0 && x <= mpos.0 + 1920.0 - PILL.0, "x={x}");
        assert!(y >= 0.0);
    }

    /// A posição guardada é sempre a da pílula. Gravar a do card faria a
    /// pílula reaparecer no canto onde o card começava — o mesmo sintoma do
    /// bug de recolhimento, só que sobrevivendo ao reinício.
    #[test]
    fn posicao_salva_normaliza_para_a_pilula() {
        let pilula = (1192.0, 774.0);
        let card = anchored_bottom_right(pilula, PILL, CARD);
        // O que `save_position` grava estando expandido:
        let gravado = anchored_bottom_right(card, CARD, PILL);
        assert!(same_spot(gravado, pilula), "gravou {gravado:?}");
    }

    /// Abrir já expandido tem que colocar o card ancorado onde a pílula
    /// estava, não no ponto cru salvo.
    #[test]
    fn abrir_expandido_converte_da_pilula_para_o_card() {
        let pilula = (1192.0, 774.0);
        let card = clamp_to_bounds(
            anchored_bottom_right(pilula, PILL, CARD),
            CARD,
            MPOS,
            MSIZE,
        );
        assert_eq!(pilula.0 + PILL.0, card.0 + CARD.0, "borda direita preservada");
        assert!(card.1 + CARD.1 <= MSIZE.1 - TASKBAR_RESERVE + 0.01);
    }

    fn medidor() -> ia_monitor_core::model::Gauge {
        use ia_monitor_core::model::Severity;
        ia_monitor_core::model::Gauge {
            id: "claude.session".into(),
            label: "Sessão 5h".into(),
            fraction: Some(0.4),
            headline: "40%".into(),
            subtitle: None,
            severity: Severity::Normal,
            resets_at: None,
            active: true,
            expected: None,
        }
    }

    fn bom(em: DateTime<Utc>) -> ProviderSample {
        ProviderSample {
            provider: Provider::Claude,
            plan: Some("max".into()),
            gauges: vec![medidor()],
            observed_at: em,
            source_at: Some(em),
            error: None,
            retry_after: None,
        }
    }

    /// O que o usuário viu: um 429 apagou os números e deixou só o erro
    /// vermelho. Um dado de minutos atrás continua valendo mais que nada.
    #[test]
    fn falha_preserva_o_ultimo_dado_bom() {
        let agora = Utc::now();
        let st = PollState {
            due: agora + ChronoDuration::seconds(300),
            failures: 1,
            last_good: Some(bom(agora - ChronoDuration::minutes(4))),
            last_error: Some("limite de requisições atingido".into()),
        };
        let s = display_sample(Provider::Claude, &st, agora);

        assert_eq!(s.gauges.len(), 1, "as barras continuam");
        assert_eq!(s.gauges[0].headline, "40%");
        assert!(s.error.is_some(), "e o motivo aparece junto");
        // `source_at` intocado: é o que faz a UI mostrar a idade real.
        assert_eq!(s.source_at, Some(agora - ChronoDuration::minutes(4)));
        assert_eq!(s.retry_after, Some(300), "quanto falta para tentar de novo");
    }

    /// Sem nenhum dado anterior não há o que preservar — aí o erro é tudo
    /// que temos, e escondê-lo seria pior.
    #[test]
    fn falha_sem_dado_anterior_mostra_o_erro() {
        let agora = Utc::now();
        let st = PollState {
            due: agora,
            failures: 1,
            last_good: None,
            last_error: Some("sem rede".into()),
        };
        let s = display_sample(Provider::Claude, &st, agora);
        assert!(s.gauges.is_empty());
        assert_eq!(s.error.as_deref(), Some("sem rede"));
    }

    #[test]
    fn coleta_bem_sucedida_nao_marca_idade_nem_espera() {
        let agora = Utc::now();
        let st = PollState {
            due: agora + ChronoDuration::seconds(180),
            failures: 0,
            last_good: Some(bom(agora)),
            last_error: None,
        };
        let s = display_sample(Provider::Claude, &st, agora);
        assert!(s.error.is_none());
        assert!(s.retry_after.is_none());
    }

    /// Antes da primeira coleta o estado é "aguardando", não "quebrado".
    #[test]
    fn antes_da_primeira_coleta_o_estado_e_neutro() {
        let agora = Utc::now();
        let s = display_sample(Provider::Codex, &PollState::new(agora), agora);
        assert!(s.error.unwrap().contains("aguardando"));
    }

    /// Desde que o card se ajusta ao conteudo, ancorar pela constante `CARD`
    /// desloca a pilula pela diferenca entre a altura real e a minima.
    #[test]
    fn ancora_usa_a_altura_real_do_card() {
        let pilula = (1192.0, 774.0);
        // Card crescido para 525 (o piso e 420).
        let card_real = (CARD.0, 525.0);
        let card = anchored_bottom_right(pilula, PILL, card_real);
        let voltou = anchored_bottom_right(card, card_real, PILL);
        assert!(same_spot(voltou, pilula), "voltou para {voltou:?}");

        // Com a constante, o erro e exatamente a diferenca de altura.
        let errado = anchored_bottom_right(card, CARD, PILL);
        assert!(
            (errado.1 - pilula.1).abs() > 100.0,
            "o bug precisa ser visivel: {errado:?}"
        );
    }

    #[test]
    fn mesma_posicao_tolera_arredondamento() {
        assert!(same_spot((10.0, 10.0), (10.9, 9.2)));
        assert!(!same_spot((10.0, 10.0), (14.0, 10.0)));
    }
}
