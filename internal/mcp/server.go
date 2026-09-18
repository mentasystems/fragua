package mcp

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	sdk "github.com/modelcontextprotocol/go-sdk/mcp"

	"github.com/mentasystems/fragua/internal/core"
	"github.com/mentasystems/fragua/internal/script"
)

// Version is reported to MCP clients during initialize.
const Version = core.Version

// text builds a plain-text tool result.
func text(s string) *sdk.CallToolResult {
	if strings.TrimSpace(s) == "" {
		s = "(no output)"
	}
	return &sdk.CallToolResult{Content: []sdk.Content{&sdk.TextContent{Text: s}}}
}

type scriptArgs struct {
	Script string `json:"script" jsonschema:"One or more fragua script lines separated by newlines, e.g. \"outline 30 20\\nstatus\". Indented pin/pad lines continue an open sym/lib block. Execution stops at the first failing line, so send related lines together and read every result."`
}

type helpArgs struct {
	Verb string `json:"verb,omitempty" jsonschema:"Optional script verb to explain, e.g. \"route\", \"auto-place\", \"pack\". Omit to get the full reference with every verb."`
}

type stateArgs struct {
	Section string `json:"section,omitempty" jsonschema:"Optional top-level section to return instead of the whole snapshot: \"board\", \"schematic\", \"palette\" or \"name\". Use it to keep the reply small; the full state of a real board is large."`
}

type saveArgs struct {
	Path string `json:"path,omitempty" jsonschema:"Absolute path to write the .fragua project to. Omit to reuse the path the session is already bound to; a memory-only session errors without one."`
}

type routeArgs struct {
	MaxSeconds float64 `json:"max_seconds,omitempty" jsonschema:"Wall-clock budget in seconds (default 600, max 3600). Always set one: routing a dense board can otherwise run for many minutes. 90-180 is a good first try."`
	Clearance  float64 `json:"clearance,omitempty" jsonschema:"Extra copper-to-copper air in mm on top of the design rules. The fab minimum is always the floor, so this can only widen."`
	Organic    bool    `json:"organic,omitempty" jsonschema:"Run the organic string-pull post-pass that smooths the routed traces."`
	Teardrop   bool    `json:"teardrop,omitempty" jsonschema:"Add teardrop fillets where traces meet pads and vias."`
	Engine     string  `json:"engine,omitempty" jsonschema:"Search engine: grid (default Theta*/RR&R) or topo (experimental Delaunay homotopy). Leave empty for the stable default."`
}

type emptyArgs struct{}

// NewServer builds the fragua MCP server over b.
func NewServer(b Backend) *sdk.Server {
	s := sdk.NewServer(&sdk.Implementation{Name: "fragua", Version: Version}, nil)

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_script",
		Description: "Run fragua script lines against the live board. This is the main tool: every PCB action — outline, sym, net, " +
			"palette, place, auto-place, route, pour, drc, erc, pack — is a script verb. Returns one text line per script line, " +
			"\"ok <verb>: <result>\" or \"error line <n> <verb>: <reason>\". Execution stops at the first error, so anything after it did not run. " +
			"Call fragua_help first if you are unsure of a verb's arguments.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, in scriptArgs) (*sdk.CallToolResult, any, error) {
		if strings.TrimSpace(in.Script) == "" {
			return nil, nil, fmt.Errorf("script is empty")
		}
		out, err := b.Script(ctx, in.Script)
		if err != nil {
			return nil, nil, err
		}
		return text(out), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_help",
		Description: "Get the fragua script reference. With no argument returns the full reference: every verb, its arguments, and a " +
			"\"First 10 minutes\" recipe. With a verb argument returns that verb's usage, aliases, description and examples. " +
			"Read this before writing a script you are not sure about — the verb set is small but specific.",
	}, func(_ context.Context, _ *sdk.CallToolRequest, in helpArgs) (*sdk.CallToolResult, any, error) {
		if v := strings.TrimSpace(in.Verb); v != "" {
			out, ok := script.VerbUsage(v)
			if !ok {
				return nil, nil, fmt.Errorf("%s", strings.TrimSpace(out))
			}
			return text(out), nil, nil
		}
		return text(script.Usage()), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_status",
		Description: "One-shot summary of the board: outline size, part and net counts, how much is placed and routed, and the current " +
			"DRC state. Cheap — call it after every significant step to see where you are. Equivalent to the `status` script verb.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, _ emptyArgs) (*sdk.CallToolResult, any, error) {
		out, err := b.Script(ctx, "status")
		if err != nil {
			return nil, nil, err
		}
		return text(out + "\nAPI and browser UI: http://" + b.Addr() + "/ui/  (" + b.Mode() + ")\n"), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_state",
		Description: "Full project state as JSON: name, board (outline, footprints, traces, vias, pours, stackup), schematic (symbols, " +
			"pins, nets) and palette. Use the section argument to fetch just one part — a routed board's full state is very large. " +
			"Prefer fragua_status for a quick look; use this when you need exact coordinates or net membership.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, in stateArgs) (*sdk.CallToolResult, any, error) {
		raw, err := b.State(ctx)
		if err != nil {
			return nil, nil, err
		}
		sec := strings.TrimSpace(in.Section)
		if sec == "" {
			return text(string(raw)), nil, nil
		}
		var m map[string]json.RawMessage
		if err := json.Unmarshal(raw, &m); err != nil {
			return nil, nil, err
		}
		v, ok := m[sec]
		if !ok {
			keys := make([]string, 0, len(m))
			for k := range m {
				keys = append(keys, k)
			}
			return nil, nil, fmt.Errorf("no section %q; available: %s", sec, strings.Join(keys, ", "))
		}
		var pretty any
		if json.Unmarshal(v, &pretty) == nil {
			if b, err := json.MarshalIndent(pretty, "", "  "); err == nil {
				return text(string(b)), nil, nil
			}
		}
		return text(string(v)), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_screenshot",
		Description: "Render the current board as an SVG image and return its source. fragua renders vectors, not raster, so this comes " +
			"back as SVG text rather than an image attachment — read it, or hand the markup to the human. The same image is live in " +
			"the browser UI, which is usually the better place to look.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, _ emptyArgs) (*sdk.CallToolResult, any, error) {
		svg, err := b.ScreenshotSVG(ctx)
		if err != nil {
			return nil, nil, err
		}
		note := "Board render as SVG (fragua has no raster renderer; live view: http://" + b.Addr() + "/ui/).\n\n"
		return text(note + svg), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_save",
		Description: "Write the project to a .fragua file (atomic) and bind autosave to that path. A session started without a file is " +
			"memory-only and will lose everything on exit, so save early with an explicit absolute path.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, in saveArgs) (*sdk.CallToolResult, any, error) {
		out, err := b.Save(ctx, strings.TrimSpace(in.Path))
		if err != nil {
			return nil, nil, err
		}
		return text(out), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_drc",
		Description: "Run design-rule and electrical-rule checks together (the `drc` and `erc` verbs). ERC checks the schematic — floating " +
			"pins, unconnected nets, power conflicts. DRC checks the geometry — clearance, shorts, annular ring, board edge, split nets. " +
			"Both must report zero errors before pack will produce manufacturing files. Warnings do not block.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, _ emptyArgs) (*sdk.CallToolResult, any, error) {
		out, err := b.Script(ctx, "erc\ndrc")
		if err != nil {
			return nil, nil, err
		}
		return text(out), nil, nil
	})

	sdk.AddTool(s, &sdk.Tool{
		Name: "fragua_route",
		Description: "Auto-route every unrouted net: any-angle Theta* search with rip-up-and-reroute, pin escapes and pour stitching. " +
			"Place the parts first. Re-running is cheap and only attacks what is still open, so prefer several bounded calls over one " +
			"unbounded one. Read the reply: it says how many nets connected and which failed.",
	}, func(ctx context.Context, _ *sdk.CallToolRequest, in routeArgs) (*sdk.CallToolResult, any, error) {
		line := "route"
		if in.MaxSeconds > 0 {
			line += fmt.Sprintf(" max_seconds=%g", in.MaxSeconds)
		}
		if in.Clearance > 0 {
			line += fmt.Sprintf(" clearance=%g", in.Clearance)
		}
		if in.Organic {
			line += " organic=true"
		}
		if in.Teardrop {
			line += " teardrop=true"
		}
		if strings.EqualFold(strings.TrimSpace(in.Engine), "topo") {
			line += " engine=topo"
		}
		out, err := b.Script(ctx, line)
		if err != nil {
			return nil, nil, err
		}
		return text(line + "\n" + out), nil, nil
	})

	s.AddResource(&sdk.Resource{
		URI:         "fragua://help",
		Name:        "fragua script reference",
		Description: "The complete fragua script verb reference, same text as `fragua help` and GET /help.",
		MIMEType:    "text/plain",
	}, func(_ context.Context, req *sdk.ReadResourceRequest) (*sdk.ReadResourceResult, error) {
		return &sdk.ReadResourceResult{Contents: []*sdk.ResourceContents{{
			URI: req.Params.URI, MIMEType: "text/plain", Text: script.Usage(),
		}}}, nil
	})

	s.AddResource(&sdk.Resource{
		URI:         "fragua://state",
		Name:        "fragua project state",
		Description: "The live project as JSON: board, schematic, palette. Same payload as GET /state.",
		MIMEType:    "application/json",
	}, func(ctx context.Context, req *sdk.ReadResourceRequest) (*sdk.ReadResourceResult, error) {
		raw, err := b.State(ctx)
		if err != nil {
			return nil, err
		}
		return &sdk.ReadResourceResult{Contents: []*sdk.ResourceContents{{
			URI: req.Params.URI, MIMEType: "application/json", Text: string(raw),
		}}}, nil
	})

	return s
}

// Serve runs the MCP server on stdio until the client disconnects.
// stdout carries the protocol: never write anything else to it.
func Serve(ctx context.Context, b Backend) error {
	return NewServer(b).Run(ctx, &sdk.StdioTransport{})
}
