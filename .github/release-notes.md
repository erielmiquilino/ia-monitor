Monitor unificado de consumo de IA no Windows: **Claude**, **Cursor** e **ChatGPT Codex** numa pílula flutuante que expande em card.

## Qual arquivo baixar

| Arquivo | Quando usar |
|---|---|
| **`*-setup.exe`** | Instalação normal: cria atalho no menu Iniciar e desinstala pelo painel do Windows. |
| **`*-portable.exe`** | Só rodar, sem instalar nada. É o binário inteiro, por isso é maior. |

## Antes de rodar

**O Windows vai avisar.** O executável não é assinado, então aparece *"O Windows protegeu o seu PC"*. Clique em **Mais informações → Executar assim mesmo**. Assinar exigiria um certificado de code signing.

**Precisa do WebView2.** Já vem no Windows 11 e no Windows 10 atualizado. Se faltar, o [instalador da Microsoft](https://developer.microsoft.com/microsoft-edge/webview2/) resolve.

## O que ele lê da sua máquina

O app lê, **somente leitura e só na sua própria máquina**:

- `...\Packages\Claude_*\...\config.json` — o token do Claude Desktop,
  cifrado pelo Windows (DPAPI) e decifrado em memória a cada leitura
- `~/.claude/.credentials.json` — o token que o Claude Code já mantém
- `%APPDATA%\Cursor\...\state.vscdb` — o token de sessão do Cursor
- `~/.codex/auth.json` e `~/.codex/sessions/` — o token do Codex e os logs
  que servem de reserva quando a API não responde

As duas fontes do Claude existem porque envelhecem de formas diferentes: o
token do Claude Code vale poucas horas e só o CLI o renova, enquanto o do
Desktop se mantém fresco enquanto o app for usado. A cota lida é a mesma nos
dois casos — **é a da sua conta**, e claude.ai, Desktop e Claude Code
consomem a mesma janela.

Com esses tokens o app consulta o consumo **da sua conta** em
`api.anthropic.com`, `api2.cursor.sh`, `cursor.com` e `chatgpt.com`. Nenhum
token é gravado em disco pelo app, nenhum arquivo de outro programa é
alterado, não há telemetria, e nada é enviado para lugar nenhum além dessas
APIs oficiais. O histórico fica num SQLite local em
`%LOCALAPPDATA%\ia-monitor\`.

Você só vê o seu próprio consumo — não há visão de time.

## Como usar

- A pílula flutua sempre no topo. Clique para expandir o card, `Esc` para
  recolher. Expandir já pede uma leitura nova — quem abre o card quer o
  número de agora.
- O **⟳** no cabeçalho do card força a atualização a qualquer momento. Ele
  nunca insiste contra um limite de requisições já estourado: nesse caso o
  card continua dizendo quando será a próxima tentativa.
- Arraste para posicionar; a posição e o modo sobrevivem ao reinício.
- No ícone da bandeja: **Provedores** liga e desliga cada um. Quem não tem uma das assinaturas desliga e ela some da tela — e para de ser consultada.
- Ainda na bandeja: **Atualizar agora**, pausar a coleta, iniciar com o
  Windows, sair.

Detalhes de arquitetura e de onde vem cada número estão no [README](https://github.com/erielmiquilino/ia-monitor#readme).
