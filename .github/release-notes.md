Monitor unificado de consumo de IA no Windows: **Claude Code**, **Cursor** e **ChatGPT Codex** numa pílula flutuante que expande em card.

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

- `~/.claude/.credentials.json` — o token que o Claude Code já mantém
- `%APPDATA%\Cursor\...\state.vscdb` — o token de sessão do Cursor
- `~/.codex/sessions/` — os logs de uso do Codex

Com eles consulta o consumo **da sua conta** em `api.anthropic.com` e `cursor.sh`. Nenhum token é gravado em disco pelo app, não há telemetria, e nada é enviado para lugar nenhum além dessas APIs oficiais. O histórico fica num SQLite local em `%LOCALAPPDATA%\ia-monitor\`.

Você só vê o seu próprio consumo — não há visão de time.

## Como usar

- A pílula flutua sempre no topo. Clique para expandir o card, `Esc` para recolher.
- Arraste para posicionar; a posição e o modo sobrevivem ao reinício.
- No ícone da bandeja: **Provedores** liga e desliga cada um. Quem não tem uma das assinaturas desliga e ela some da tela — e para de ser consultada.
- Ainda na bandeja: pausar a coleta, iniciar com o Windows, sair.

Detalhes de arquitetura e de onde vem cada número estão no [README](https://github.com/erielmiquilino/ia-monitor#readme).
