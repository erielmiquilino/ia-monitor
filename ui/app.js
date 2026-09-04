// UI do IA Monitor. Sem framework e sem bundler: a tela é pequena e o custo
// de um runtime extra apareceria justamente no que se quer barato aqui —
// startup e memória de uma janela que fica aberta o dia inteiro.
//
// Nada de timer no webview: o backend emite `snapshot` quando o dado muda.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const el = {
  pill: document.getElementById("pill"),
  pillGauges: document.getElementById("pill-gauges"),
  card: document.getElementById("card"),
  providers: document.getElementById("providers"),
  projects: document.getElementById("projects"),
  projectsSection: document.getElementById("projects-section"),
  updated: document.getElementById("updated"),
  status: document.getElementById("status"),
  collapse: document.getElementById("collapse"),
};

let expanded = false;
let latest = null;

const SEV_CLASS = {
  normal: "sev-normal",
  warn: "sev-warn",
  critical: "sev-critical",
  unknown: "sev-unknown",
};

const SHORT = { Claude: "CLD", Cursor: "CUR", Codex: "CDX" };

function pct(fraction) {
  if (fraction === null || fraction === undefined) return 0;
  return Math.max(0, Math.min(100, fraction * 100));
}

function text(node, value) {
  node.textContent = value ?? "";
}

/** Marcador de "onde eu deveria estar": a fração da janela já decorrida.
 *
 *  Só aparece quando o backend conhece o tamanho da janela. Barra sem
 *  marcador é informação legítima (saldo de crédito não reseta); marcador
 *  chutado seria uma referência falsa.
 */
function addMarker(bar, gauge) {
  if (gauge.expected === null || gauge.expected === undefined) return;
  const mark = document.createElement("div");
  mark.className = "bar-marker";
  mark.style.left = `${pct(gauge.expected)}%`;
  const alvo = Math.round(gauge.expected * 100);
  mark.title = `no ritmo do relógio você estaria em ${alvo}%`;
  bar.appendChild(mark);
}

/** O medidor que representa o provedor na pílula.
 *
 *  Quem decide é o backend (`primaryGaugeId`), porque qual limite representa
 *  cada provedor é conhecimento do domínio. A heurística anterior pegava a
 *  maior fração entre os ativos e, quando a janela de 5h do Claude ficava
 *  inativa, mostrava o limite por modelo — um número que ninguém procurava.
 */
function headlineGauge(sample) {
  const escolhido = sample.gauges.find((g) => g.id === sample.primaryGaugeId);
  if (escolhido) return escolhido;
  return sample.gauges.find((g) => g.fraction !== null && g.fraction !== undefined) ?? null;
}

function renderPill(snapshot) {
  el.pillGauges.replaceChildren();
  // Todos desligados e um estado valido; dizer isso e melhor que uma pilula
  // vazia, que parece defeito.
  if (snapshot.samples.length === 0) {
    const vazio = document.createElement("span");
    vazio.className = "empty-note";
    vazio.setAttribute("data-tauri-drag-region", "");
    vazio.textContent = "nenhum provedor ativo";
    el.pillGauges.appendChild(vazio);
    return;
  }
  for (const sample of snapshot.samples) {
    const item = document.createElement("div");
    item.className = "pill-item";
    item.setAttribute("data-tauri-drag-region", "");

    const tag = document.createElement("span");
    tag.className = "pill-tag";
    tag.textContent = SHORT[sample.providerLabel] ?? sample.providerLabel.slice(0, 3);

    const bar = document.createElement("div");
    bar.className = "pill-bar";
    const fill = document.createElement("div");
    const g = headlineGauge(sample);
    const semDado = !g || g.fraction === null || g.fraction === undefined;
    fill.className = `pill-fill ${SEV_CLASS[semDado ? "unknown" : g.severity]}`;
    fill.style.width = `${semDado ? 100 : pct(g.fraction)}%`;
    bar.appendChild(fill);
    if (g) addMarker(bar, g);

    const value = document.createElement("span");
    value.className = "pill-value";
    value.textContent = semDado ? "?" : g.headline;
    // Dado antigo fica atenuado em vez de sumir: o número ainda vale.
    if (sample.error) item.classList.add("stale-item");

    item.append(tag, bar, value);
    item.title = [
      `${sample.providerLabel} · ${g?.label ?? ""} ${g?.headline ?? ""}`.trim(),
      sample.error,
      sample.ageText,
    ]
      .filter(Boolean)
      .join("\n");
    el.pillGauges.appendChild(item);
  }
}

function gaugeRow(gauge, burn) {
  const row = document.createElement("div");
  row.className = `gauge${gauge.active ? "" : " inactive"}`;

  const label = document.createElement("span");
  label.className = "gauge-label";
  label.textContent = gauge.label;
  label.title = gauge.label;

  const bar = document.createElement("div");
  bar.className = "gauge-bar";
  const fill = document.createElement("div");
  fill.className = `gauge-fill ${SEV_CLASS[gauge.severity]}`;
  fill.style.width = `${pct(gauge.fraction)}%`;
  bar.appendChild(fill);
  addMarker(bar, gauge);

  const value = document.createElement("span");
  value.className = "gauge-value";
  if (gauge.severity === "warn") value.classList.add("txt-warn");
  if (gauge.severity === "critical") value.classList.add("txt-critical");
  value.textContent = gauge.headline;

  row.append(label, bar, value);

  // Subtítulo e projeção compartilham a linha de nota; a projeção só
  // aparece quando o backend teve amostras suficientes para calculá-la.
  const notes = [gauge.subtitle, burn].filter(Boolean);
  if (notes.length === 0) return [row];

  const note = document.createElement("div");
  note.className = "gauge-note";
  note.textContent = notes.join(" · ");
  return [row, note];
}

function renderCard(snapshot) {
  el.providers.replaceChildren();

  if (snapshot.samples.length === 0) {
    const vazio = document.createElement("div");
    vazio.className = "empty-note";
    vazio.textContent = "Nenhum provedor ativo. Ligue um em Provedores, no menu da bandeja.";
    el.providers.appendChild(vazio);
  }

  for (const sample of snapshot.samples) {
    const block = document.createElement("div");
    block.className = "provider";

    const head = document.createElement("div");
    head.className = "provider-head";
    const name = document.createElement("span");
    name.className = "provider-name";
    name.textContent = sample.providerLabel;
    const plan = document.createElement("span");
    plan.className = "provider-plan";
    plan.textContent = sample.plan ?? "";
    head.append(name, plan);
    block.appendChild(head);

    // Uma falha temporária não apaga números reais: as barras continuam,
    // com o motivo e a idade logo abaixo.
    for (const gauge of sample.gauges) {
      block.append(...gaugeRow(gauge, snapshot.burn?.[gauge.id]));
    }
    if (sample.error) {
      const err = document.createElement("div");
      err.className = "provider-error";
      err.textContent = sample.error;
      block.appendChild(err);
    }
    if (sample.ageText) {
      const stale = document.createElement("div");
      stale.className = "stale";
      stale.textContent = sample.ageText;
      block.appendChild(stale);
    }
    el.providers.appendChild(block);
  }

  const projects = snapshot.topProjects ?? [];
  el.projectsSection.classList.toggle("hidden", projects.length === 0);
  el.projects.replaceChildren();
  for (const p of projects) {
    const row = document.createElement("div");
    row.className = "project";
    const n = document.createElement("span");
    n.className = "project-name";
    n.textContent = p.label;
    n.title = p.path;
    const v = document.createElement("span");
    v.className = "project-value";
    v.textContent = p.value;
    row.append(n, v);
    el.projects.appendChild(row);
  }

  text(el.updated, snapshot.updatedText);
  text(el.status, snapshot.statusText);
}


/** Altura real do conteúdo, medida sem a rolagem ativa.
 *
 *  Com `.scrolls` ligado o documento fica preso ao tamanho da janela, e medir
 *  nesse estado devolveria sempre a altura atual — a janela nunca encolheria.
 */
function alturaDoConteudo() {
  const rolando = document.body.classList.contains("scrolls");
  if (rolando) document.body.classList.remove("scrolls");
  const h = Math.ceil(document.documentElement.scrollHeight);
  if (rolando) document.body.classList.add("scrolls");
  return h;
}

/** Pede ao backend a altura que o conteúdo precisa. Se a tela não comportar,
 *  religa a rolagem em vez de cortar. */
async function ajustarCard() {
  if (!expanded) return;
  const desejada = alturaDoConteudo();
  try {
    const aplicada = await invoke("fit_card", { height: desejada });
    document.body.classList.toggle("scrolls", aplicada < desejada - 1);
  } catch {}
}

function render(snapshot) {
  latest = snapshot;
  if (expanded) {
    renderCard(snapshot);
    // Depois do layout assentar: medir antes disso devolve a altura antiga.
    requestAnimationFrame(ajustarCard);
  } else {
    renderPill(snapshot);
  }
}

async function setExpanded(next) {
  if (expanded === next) return;
  expanded = next;
  el.pill.classList.toggle("hidden", next);
  el.card.classList.toggle("hidden", !next);
  // O redimensionamento é do backend: é a mesma janela mudando de tamanho,
  // não um segundo webview.
  if (!next) document.body.classList.remove("scrolls");
  await invoke("set_expanded", { expanded: next });
  if (latest) render(latest);
}

el.pill.addEventListener("click", (e) => {
  // Arrastar não pode ser confundido com clique.
  if (e.detail === 0) return;
  setExpanded(true);
});
el.collapse.addEventListener("click", () => setExpanded(false));

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && expanded) setExpanded(false);
});

listen("snapshot", (event) => render(event.payload));

/** Aplica o modo sem chamar o backend: na abertura a janela já vem no
 *  tamanho certo, e redimensioná-la de novo causaria um piscar. */
function applyMode(next) {
  expanded = next;
  el.pill.classList.toggle("hidden", next);
  el.card.classList.toggle("hidden", !next);
}

// Ao abrir: restaura o modo salvo e pede o estado atual, em vez de esperar
// o próximo ciclo de coleta.
(async () => {
  try {
    applyMode(await invoke("start_expanded"));
  } catch {
    applyMode(false);
  }
  try {
    const s = await invoke("current_snapshot");
    if (s) render(s);
  } catch {}
})();
