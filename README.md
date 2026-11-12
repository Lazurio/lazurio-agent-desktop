# Lazurio Agent Desktop

Lazurio Agent Desktop lets the agents of a Lazurio Environment use the apps on a
person's computer (macOS or Windows 11), for example desktop Outlook or
PowerPoint, while the owner of the computer allows it.

- **The computer opens the only connection.** Nothing on the computer listens on
  the network; the Environment never connects to the computer.
- **The owner's switch is the consent.** Each paired Environment has its own
  switch in the menu bar (macOS) or notification area (Windows). Off means no
  connection at all. **Stop** ends an agent's work at once.
- **History without content.** The app keeps a local record of connections and
  of the tool names agents used, never screen contents, text or passwords.

This is a first, small utility on the way to Lazurio App. It proves one
mechanism: agents in a hosted Environment working on a person's computer over a
channel that the computer opens. What holds up moves into Lazurio App, and the
utility is uninstalled once Lazurio App ships. The core (`crates/core`) is
written to run later as a helper process of Lazurio App; the tray interface is
temporary.

> Status: version 0, for internal tests. The builds are **not signed** yet.

## Návod (česky)

Bez terminálu, pro Mac i Windows. Počítač musí být připojený k síti vašeho
Prostředí přes Tailscale a Admin vaší Organizace ho musí mít schválený.

1. **Instalace.** Stáhněte instalační soubor od Admina nebo z buildu: na Macu
   `.dmg` (otevřete ho a přetáhněte Lazurio Agent Desktop do Aplikací), na
   Windows `.exe` (instaluje se jen pro vás, bez práv správce) nebo `.msi`.
   Dokud aplikace není podepsaná, Mac při prvním otevření ohlásí neznámého
   vývojáře: otevřete Nastavení systému → Soukromí a zabezpečení a klikněte na
   **Přesto otevřít**. Windows může ukázat SmartScreen: **Další informace →
   Přesto spustit**.
2. **Povolení na Macu.** Při prvním spuštění vás panel provede povoleními
   **Zpřístupnění** a **Záznam obrazovky**. Po povolení Záznamu obrazovky
   aplikaci ukončete a znovu otevřete. Windows nic podobného nepotřebuje.
3. **Přidání Prostředí.** V panelu vložte adresu Lazuria svého Prostředí (tu,
   kterou otevíráte v prohlížeči) a klikněte na **Přidat**. Otevře se stránka
   Lazuria. Přihlaste se, zkontrolujte jméno počítače a otisk klíče a potvrďte
   **Povolit agentům tento počítač**. Vypínač se tím zapne.
4. **Běžné používání.** Ikona v liště ukazuje stav: bez tečky agenti počítač
   nepoužívají, s tečkou je připojený, s tečkou a kurzorem právě pracuje agent
   (ve Windows je tečka zelená a oranžová). Klikněte na ikonu a v panelu uvidíte,
   který nástroj a ve které aplikaci agent používá.
5. **Vypnutí.** Vypínač u Prostředí vypne ovládání úplně. **Zastavit** ukončí
   práci agenta hned; vypínač zůstane zapnutý a počítač se za chvíli připojí znovu.
6. **Odebrání.** **Odebrat** smaže klíč z tohoto počítače. **Spravovat
   v Lazuriu** otevře stránku Lazuria, kde počítač odeberete i na straně Prostředí.

Aplikace se spouští po přihlášení do systému; vypnete to v panelu.

## Security

**What an agent can do.** While the switch is on and the computer is connected,
one agent of that Environment at a time can see the screen and click and type
in any app under the signed-in user's account, like Computer Use. Agents follow
the Environment's rules: they prepare drafts, and sending or publishing waits
for the person's approval. The content of documents is data for the agent, not
instructions.

**Where the key is.** Pairing creates one ed25519 key per Environment. Only its
32-byte seed is stored, in the macOS Keychain or in the Windows Credential
Manager (protected by DPAPI for the signed-in user), under the service name
`io.lazurio.agent-desktop`. The private key never leaves the computer. For each
connection the app writes it to a file only the user can read, hands it to the
system OpenSSH client and deletes it as soon as the Environment answers (at the
latest after 30 seconds, and on every start of the app).

**What the Environment can do with the key.** Launchpad writes the public key
to `authorized_keys` of the Environment's account with
`restrict,command="<home>/.local/bin/lazurio computer attach --device <id>"`, so
the key can only start that one command: no shell, no port forwarding, no agent
forwarding. The Environment pings the computer every 15 seconds, ends the
session when the key is removed there, and lets one agent at a time use the
computer; an agent's turn also ends after 2 minutes without a call.

**What the computer trusts.** The app connects only to the host and host key
that Launchpad returned during pairing (`StrictHostKeyChecking=yes`, its own
`known_hosts`, no user SSH configuration). A different host key stops the
connection.

**Turning it off.** Switch off (no connection), **Stop** (ends the current
session), **Remove** (deletes the key here), and removing the computer in
Lazurio (deletes the `authorized_keys` line). Quitting the app also ends every
connection.

**Local data.** `~/Library/Application Support/io.lazurio.agent-desktop`
(macOS) or `%LOCALAPPDATA%\io.lazurio.agent-desktop` (Windows):
`environments.json` (no secrets), `known_hosts/`, `history.jsonl` (no content).

## How it works

Contract v0 between the app and the Environment (the Environment side lives in
[Lazurio/LazurioPlatform](https://github.com/Lazurio/LazurioPlatform)):

1. **Pairing.** The app creates a key, listens once on `127.0.0.1:<random port>`
   with a random `state`, and opens
   `https://<launchpad>/connect/computer?name=…&os=…&pubkey=…&callback=http://127.0.0.1:<port>/cb&state=…`
   in the browser. After the owner approves, Launchpad redirects to the
   callback with the same `state` and `result` = base64url JSON
   `{ deviceId, ssh: { host, user, knownHosts } }`; Cancel on the page returns
   `error=cancelled` instead.
2. **Channel.** While the switch is on, the app starts the system OpenSSH client
   (`/usr/bin/ssh`, `%WINDIR%\System32\OpenSSH\ssh.exe`) to the Environment and
   the Agent Desktop MCP server (`agent-desktop mcp`), and joins their stdio line
   by line. The Environment's forced command speaks MCP to the computer, so the
   computer is the MCP server and its tools are the Agent Desktop `desktop_*`
   tools. When the session drops and the switch is on, the app reconnects with a
   growing delay (1 s up to 60 s).

The engine is [Lazurio/agent-desktop](https://github.com/Lazurio/agent-desktop),
a fork of Agent Desktop, built in CI at the commit pinned in
`engine/agent-desktop.ref` and bundled as a sidecar. On macOS the app is the
process that starts the engine, so macOS attributes the Accessibility and
Screen Recording permissions to Lazurio Agent Desktop. On Windows the app runs
in the signed-in user's session, where UI Automation works.

## Development

```sh
cargo test -p lazurio-agent-desktop-core -p lazurio-agent-desktop-testkit
```

The tests pair against a fake Launchpad and run the channel twice: with a fake
Environment in place of `ssh` (every platform) and over the real OpenSSH client
against a local `sshd` with the contract's `restrict,command=` line (Unix; CI
requires it on Linux with `LAD_REQUIRE_SSHD=1`).

The app (macOS or Windows):

```sh
scripts/build-engine.sh aarch64-apple-darwin      # or x86_64-apple-darwin, x86_64-pc-windows-msvc
npx @tauri-apps/cli@2.12.4 dev
npx @tauri-apps/cli@2.12.4 build --target aarch64-apple-darwin --bundles dmg
```

CI builds unsigned `.dmg` (Apple silicon and Intel) and `.msi`/`.exe` (Windows
x64) on every pull request.

### Signing

The CI steps switch on when these repository secrets exist:

- macOS (Developer ID and notarization): `APPLE_CERTIFICATE` (base64 `.p12`),
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, and for notarization
  `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_P8` (contents of the
  `.p8`). Without them the app is ad-hoc signed, and macOS asks for the
  permissions again after every update.
- Windows: `WINDOWS_CERTIFICATE` (base64 `.pfx`) and
  `WINDOWS_CERTIFICATE_PASSWORD`. A certificate kept in a cloud HSM (for
  example Azure Trusted Signing) needs `bundle.windows.signCommand` instead.

## License

Not decided yet. Until a license is added, no rights are granted beyond what
GitHub's terms allow. The bundled Agent Desktop engine is Apache-2.0, see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
