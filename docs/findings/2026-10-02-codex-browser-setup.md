# Codex browser setup on this Linux host

The Mattermost session `pvyuwoje` failed with `Browser is not available: iab`.
Computer Use was installed and callable, but `cua.getState()` returned empty
apps and browsers. Chrome was running; the bundled diagnostics found neither
the OpenAI Chrome extension nor its native messaging host manifest.

For local web UI verification, the user chose Playwright MCP. A staged
`@playwright/mcp` 0.0.83 installation passed a real local-page navigation,
button click and screenshot check using the installed Google Chrome. An
isolated Codex app-server discovered all 25 MCP tools without an LLM request.

The launcher uses headless, isolated browser contexts and keeps temporary
profiles, caches and automatic artifacts in the active Solution's
`.agents/tmp/playwright`. It does not reuse the user's Chrome profile.

Installed with the user's explicit approval at
`/home/spk/.codex/tools/playwright-mcp`, registered globally as
`[mcp_servers.playwright]` in `/home/spk/.codex/config.toml`. The command uses
Node 22.22.2 and the pinned local package; startup does not download packages.
The installed location and actual global registration passed the same browser
smoke and isolated Codex discovery check (25 tools). The original configuration
was backed up inside the Solution's `.agents/tmp/playwright-setup`.

Existing Codex subprocesses may need Restart to reload the configuration.
The Mattermost session was running during final verification and was not
interrupted. Use Playwright tools for this workflow; installing Playwright does
not create the separate Computer Use `iab` browser surface.

References: [Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli),
[Playwright MCP](https://github.com/microsoft/playwright-mcp).
