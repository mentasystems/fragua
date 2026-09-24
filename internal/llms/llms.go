// Package llms renders docs/llms.txt and docs/llms-full.txt (llmstxt.org) for
// fragua.cloud, so the published agent docs cannot drift from the binary.
package llms

import (
	"strings"

	"github.com/mentasystems/fragua/agent"
	"github.com/mentasystems/fragua/internal/core"
	"github.com/mentasystems/fragua/internal/script"
)

//go:generate go run ../../cmd/gen-llms ../../docs

const repo = "https://github.com/mentasystems/fragua"

// Index renders llms.txt: the short overview plus links.
func Index() string {
	var b strings.Builder
	b.WriteString("# fragua\n\n")
	b.WriteString("> AI-native PCB design tool. One static Go binary that hosts a script API for an AI\n")
	b.WriteString("> agent and a live browser UI for the human: schematic, footprint placement,\n")
	b.WriteString("> auto-routing, DRC/ERC, and JLCPCB/PCBWay-ready Gerber + BOM + CPL packs. No KiCad,\n")
	b.WriteString("> no FreeRouting, no shell-out to any external CAD tool.\n\n")

	b.WriteString("Start the tool, then drive it over MCP or plain HTTP:\n\n")
	b.WriteString("```sh\n")
	b.WriteString("fragua init              # AGENTS.md, a Claude Code skill, a Cursor rule, .mcp.json\n")
	b.WriteString("fragua mcp board.fragua  # MCP server on stdio + HTTP API + UI on 127.0.0.1:7878\n")
	b.WriteString("fragua run board.fragua  # HTTP API + UI only\n")
	b.WriteString("fragua render --3d board.fragua -o board.png  # static 3D product shot\n")
	b.WriteString("```\n\n")

	b.WriteString("## Docs\n\n")
	b.WriteString("- [llms-full.txt](https://fragua.cloud/llms-full.txt): the agent guide plus the complete script verb reference. Read this one.\n")
	b.WriteString("- [README](" + repo + "/blob/master/README.md): install, run, and drive it from an agent.\n")
	b.WriteString("- [AGENTS.md](" + repo + "/blob/master/agent/AGENTS.md): the onboarding guide `fragua init` writes into a project.\n")
	b.WriteString("- [VISION.md](" + repo + "/blob/master/VISION.md): what the product is and its non-negotiables.\n")
	b.WriteString("- [ARCHITECTURE.md](" + repo + "/blob/master/ARCHITECTURE.md): packages and data flow.\n")
	b.WriteString("- [rust-vs-go.md](" + repo + "/blob/master/docs/rust-vs-go.md): ePlace and topo are Go ports of rust 4e20ae0.\n")
	b.WriteString("- [CONTRIBUTING.md](" + repo + "/blob/master/CONTRIBUTING.md): scope and house style.\n\n")

	b.WriteString("## API\n\n")
	b.WriteString("- `POST /script`: run script lines, `{\"script\":\"outline 30 20\\nstatus\"}`, text/plain reply.\n")
	b.WriteString("- `GET /help`: full verb reference. `GET /help?verb=route` for one verb.\n")
	b.WriteString("- `GET /state`: JSON project snapshot. `GET /screenshot`: board SVG. `GET /events`: SSE.\n")
	b.WriteString("- `POST /save`: write the .fragua project.\n")
	b.WriteString("- MCP tools: `fragua_script`, `fragua_help`, `fragua_status`, `fragua_state`,\n")
	b.WriteString("  `fragua_screenshot`, `fragua_save`, `fragua_drc`, `fragua_route`.\n\n")

	b.WriteString("## Optional\n\n")
	b.WriteString("- [Landing](https://fragua.cloud)\n")
	b.WriteString("- [Releases](" + repo + "/releases/latest)\n")
	return b.String()
}

// Full renders llms-full.txt: the agent guide plus the full script reference.
func Full() string {
	var b strings.Builder
	b.WriteString("# fragua " + core.Version + " — full agent reference\n\n")
	b.WriteString("> AI-native PCB design tool. Generated from the fragua binary; do not edit by hand.\n")
	b.WriteString("> Regenerate with scripts/gen-llms.sh.\n\n")
	b.WriteString("---\n\n")
	b.WriteString("# Agent guide\n\n")
	// The shipped section is a level-2 heading; keep it as-is so the two copies match.
	b.WriteString(strings.TrimRight(agent.AGENTSMD, "\n"))
	b.WriteString("\n\n---\n\n")
	b.WriteString("# Script reference\n\n")
	b.WriteString("Output of `fragua help`:\n\n")
	b.WriteString("```text\n")
	b.WriteString(script.Usage())
	b.WriteString("```\n\n")
	b.WriteString("---\n\n")
	b.WriteString("# Per-verb reference\n\n")
	b.WriteString("Output of `fragua help <verb>` for every verb.\n\n")
	for i := range script.Verbs {
		v := &script.Verbs[i]
		out, ok := script.VerbUsage(v.Name)
		if !ok {
			continue
		}
		b.WriteString("## " + v.Name + "\n\n```text\n")
		b.WriteString(out)
		b.WriteString("```\n\n")
	}
	return b.String()
}
