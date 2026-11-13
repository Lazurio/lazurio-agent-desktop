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

**Kanál aktualizací** (stabilní nebo pilotní) změníte v panelu u položky
Aktualizace. **Aktualizace** se dějí samy: aplikace se na novou verzi podívá po spuštění
a pak každé 4 hodiny a nainstaluje ji ve chvíli, kdy na počítači zrovna
nepracuje agent. Potom se restartuje a znovu připojí. Když chcete hned, v panelu
se ukáže „Je k dispozici verze …“ s tlačítkem **Aktualizovat**. Aktualizaci
může spustit i agent nástrojem `lazurio_app_update`.

## Security

**What an agent can do.** While the switch is on and the computer is connected,
one agent at a time can see the screen and click and type in any app under the
signed-in user's account, like Computer Use. One agent on the whole computer:
while an agent from one Environment works (or worked less than 20 seconds ago),
tool calls from every other Environment are refused with "The computer is in
use by an agent from …", and the panel shows who works. Agents follow the
Environment's rules: they prepare drafts, and sending or publishing waits for
the person's approval. The content of documents is data for the agent, not
instructions.

**Where the key is.** Pairing creates one ed25519 key per Environment. Only its
32-byte seed is stored, in the macOS Keychain or in the Windows Credential
Manager (protected by DPAPI for the signed-in user), under the service name
`io.lazurio.agent-desktop`. The private key never leaves the computer and never
touches the disk: the app signs the Environment's challenge with it in memory.

**What the Environment can do with the key.** Launchpad keeps the public key in
the Environment's own list of computers. It only verifies the computer's
signature with it; the key opens no shell, account or port anywhere. The
Environment pings the computer every 15 seconds, refuses a second session of
the same computer, and ends the session when the computer is removed there.

**What the computer trusts.** The app connects only to
`wss://<Lazurio address>/.lazurio/computers/attach`, on the host of the Lazurio
address it was paired with, with ordinary certificate checks (the system's
trusted certificates). The network path stays the tailnet.

**Updates.** The app installs only update packages signed with the updater
key whose public half is in `src-tauri/tauri.conf.json`, only versions newer
than the running one, and only when the signature names the same version as
the release (`requireSignedVersion`). It reads `latest.json` of the newest
GitHub Release that is not a pre-release. The private updater key and its
password exist only outside this repository and as repository secrets for
the release workflow.

**Turning it off.** Switch off (no connection), **Stop** (ends the current
session), **Remove** (deletes the key here), and removing the computer in
Lazurio (the Environment forgets its key). Quitting the app also ends every
connection.

**Local data.** `~/Library/Application Support/io.lazurio.agent-desktop`
(macOS) or `%LOCALAPPDATA%\io.lazurio.agent-desktop` (Windows):
`environments.json` (no secrets) and `history.jsonl` (no content).

## How it works

Contract v1 between the app and the Environment (the Environment side lives in
[Lazurio/LazurioPlatform](https://github.com/Lazurio/LazurioPlatform)):

1. **Pairing.** The app creates a key, listens once on `127.0.0.1:<random port>`
   with a random `state`, and opens
   `https://<launchpad>/connect/computer?name=…&os=…&pubkey=…&callback=http://127.0.0.1:<port>/cb&state=…`
   in the browser. After the owner approves, Launchpad redirects to the
   callback with the same `state` and `result` = base64url JSON
   `{ deviceId, channel: { url }, environment: { name } }`; Cancel on the page
   returns `error=cancelled` instead.
2. **Channel.** While the switch is on, the app opens
   `wss://<launchpad host>/.lazurio/computers/attach?device=<deviceId>`. The
   Environment sends `{"type":"challenge","v":1,"environment","nonce","ts"}`;
   the app answers `{"type":"auth","v":1,"device","signature"}` with an Ed25519
   signature over `lazurio-computer-attach-v1\n<environment>\n<nonce>\n<ts>`.
   After `{"type":"ready"}` (or `denied` with a reason) the app starts the Agent
   Desktop MCP server (`agent-desktop mcp`) and every text frame is one MCP
   message: the computer is the MCP server and its tools are the Agent Desktop
   `desktop_*` tools. WebSocket ping every 15 s, disconnect after 45 s without
   an answer, reconnect after 1 s up to 60 s only while the switch is on.

A computer paired under contract v0 (SSH) gets its channel from the Lazurio
address it remembers; nothing about SSH is needed on the computer any more.

On that same session the app adds two tools of its own, `lazurio_app_status`
(version, system, update channel, available update) and `lazurio_app_update`
(downloads, verifies, installs, restarts, reconnects), answers their calls
itself and passes every other line to the engine unchanged. Its answer to
`initialize` carries the app's version and system under
`_meta["io.lazurio/agent-desktop"]`.

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

The tests pair against a fake Launchpad and run the channel against a fake
Environment that implements the attach endpoint of contract v1: challenge,
signature check, age of `ts`, one-time `nonce`, one session per computer,
`denied`, pings, and an agent that calls tools. With `LAD_ENGINE` pointing at a
build of the engine, one test runs the real engine over the channel.

The app (macOS or Windows):

```sh
scripts/build-engine.sh aarch64-apple-darwin      # or x86_64-apple-darwin, x86_64-pc-windows-msvc
npx @tauri-apps/cli@2.12.4 dev
npx @tauri-apps/cli@2.12.4 build --target aarch64-apple-darwin --bundles dmg
```

CI builds unsigned `.dmg` (Apple silicon and Intel) and `.msi`/`.exe` (Windows
x64) on every pull request.

### Releasing

Set the new version in `src-tauri/tauri.conf.json` and `Cargo.toml`
(`[workspace.package]`), merge, then push the tag `v<version>`. The release
workflow builds all three targets, signs the update packages, writes
`latest.json` and publishes the GitHub Release. A tag with a pre-release part
(`v0.2.0-rc.1`) becomes a pre-release, which the stable channel never sees.

**Pilot channel.** A tag `v<version>-pilot.<n>` (for example
`v0.1.0-pilot.2`) becomes a pre-release too, and its `latest.json` is also
uploaded to the rolling pre-release `pilot`. Apps on the pilot channel read
`releases/download/pilot/latest.json`; pilot builds start on that channel, and
anyone can switch channels in the panel. `0.1.0-pilot.N` sorts below `0.1.0`,
and the pilot channel also takes a newer stable release, so pilot installs move
on to stable by themselves. Pilot releases ship the NSIS installer on Windows
(MSI does not take a non-numeric pre-release) and may be unsigned by Apple and
Microsoft for a controlled internal pilot.

### Signing

Update packages are always signed with the updater key (secrets
`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`); pull
requests without access to them build without update packages. Code signing
switches on when these repository secrets exist:

- macOS (Developer ID and notarization): `APPLE_CERTIFICATE` (base64 `.p12`),
  `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, and for notarization
  `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_P8` (contents of the
  `.p8`). Without them the app is ad-hoc signed, and macOS asks for the
  permissions again after every update.
- Windows: `WINDOWS_CERTIFICATE` (base64 `.pfx`) and
  `WINDOWS_CERTIFICATE_PASSWORD`. A certificate kept in a cloud HSM (for
  example Azure Trusted Signing) needs `bundle.windows.signCommand` instead.

## License

Lazurio Agent Desktop is licensed under the [Elastic License 2.0](LICENSE)
(`Elastic-2.0`), like [Lazurio/LazurioPlatform](https://github.com/Lazurio/LazurioPlatform).
The bundled Agent Desktop engine stays under Apache-2.0, see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
