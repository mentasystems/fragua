package script

import (
	"sort"
	"strings"
)

// VerbHelp documents one script verb (or a small family sharing a usage line).
type VerbHelp struct {
	Name     string   // primary verb
	Aliases  []string // alternate spellings, plus sibling verbs sharing the usage line
	Usage    string   // usage line as shown in the full reference (no leading indent)
	Notes    []string // extra reference lines, verbatim including their indentation
	Describe string   // prose; per-verb help only
	Examples []string // per-verb help only
}

// Verbs is the script reference, in the order the full help prints it.
var Verbs = []VerbHelp{
	{
		Name:     "outline",
		Usage:    "outline W H [radius=R]",
		Describe: "Set a rectangular board outline W x H mm, optionally with rounded corners of radius R mm. Do this first: placement, routing and pours all need an outline.",
		Examples: []string{"outline 30 20", "outline 80 30 radius=2"},
	},
	{
		Name:     "outline-poly",
		Aliases:  []string{"outline_poly"},
		Usage:    "outline-poly x1 y1 x2 y2 ...",
		Describe: "Set an arbitrary polygonal outline from a list of mm vertices. The polygon closes automatically; give at least three points.",
		Examples: []string{"outline-poly 0 0 40 0 40 25 20 32 0 25"},
	},
	{
		Name:     "cutout",
		Aliases:  []string{"clear-cutouts", "clear_cutouts"},
		Usage:    "cutout x1 y1 ... [label=NAME] | clear-cutouts",
		Describe: "Add a milled interior cutout as a polygon of mm vertices. `clear-cutouts` removes them all.",
		Examples: []string{"cutout 10 10 20 10 20 18 10 18 label=window", "clear-cutouts"},
	},
	{
		Name:     "hole",
		Aliases:  []string{"clear-holes", "clear_holes"},
		Usage:    "hole X Y D [label=NAME] | clear-holes",
		Describe: "Add a non-plated mounting hole of diameter D mm at X,Y. `clear-holes` removes them all.",
		Examples: []string{"hole 3 3 3.2 label=M3", "clear-holes"},
	},
	{
		Name:     "keepout",
		Usage:    "keepout X1 Y1 X2 Y2 [no_copper=true] [no_place=true]",
		Describe: "Mark a rectangle the router and/or placer must avoid. `no_copper` blocks traces, vias and pours; `no_place` blocks footprints.",
		Examples: []string{"keepout 0 0 6 6 no_place=true", "keepout 20 0 30 5 no_copper=true no_place=true"},
	},
	{
		Name:     "lib",
		Usage:    "lib KEY … + indented pad NUMBER X Y W H",
		Describe: "Define a custom footprint under KEY. Follow it with indented `pad NUMBER X Y W H` lines (mm, relative to the footprint origin). Use the built-in palette first — see `list-lib` — and only define a footprint the library lacks. `lcsc=`/`mpn=` name ONE part: on a passive they only reach the BOM when the entry also sets `value=` and the symbol's value agrees (put per-value ids on the `sym` line instead). `model=` is an optional 3D override (a KiCad `Library.3dshapes/Name.wrl` path or a local .wrl/.obj) used by `fragua render`.",
		Examples: []string{"lib my_conn\n  pad 1 -1.27 0 1.0 1.8\n  pad 2 1.27 0 1.0 1.8"},
	},
	{
		Name:     "sym",
		Usage:    "sym REF KIND … + indented pin for generic_ic",
		Describe: "Add a schematic symbol. KIND is one of ic/resistor/capacitor/inductor/led/diode. For a generic ic, follow with indented `pin NUMBER SIDE NAME [role=...]` lines. Roles: passive, input, output, bidir, power_in, power_out, nc.",
		Examples: []string{
			"sym R1 resistor key=r_0603 lcsc=C25804",
			"sym U1 ic key=esp32_s3_zero\n  pin 1 L 3V3 role=power_in\n  pin 2 L GND role=power_in\n  pin 3 R IO1 role=bidir",
		},
	},
	{
		Name:  "part",
		Usage: "part LCSC:C2040 [as=REF] [key=KEY] [value=V] [rot=DEG] [refresh=true]",
		Notes: []string{
			"        (real part: footprint + named pins from EasyEDA, cached in the library;",
			"         \"part C2040\" works too; FRAGUA_OFFLINE=1 = cache only)",
			"  part kicad:Library:Footprint [as=REF] [sym=Library:Symbol]",
			"        (e.g. part kicad:Package_TO_SOT_SMD:SOT-23 as=Q1; FRAGUA_KICAD_LIBS)",
		},
		Describe: "Pull a real part into the library and, with `as=REF`, spawn its symbol and bind the footprint so `place`/`net` work on the next line. `LCSC:Cnnnn` (or bare `Cnnnn`) fetches footprint, named pins, datasheet and JLCPCB class from EasyEDA and caches it under ~/.pcb-library; `kicad:Library:Footprint` reads a stock or FRAGUA_KICAD_LIBS KiCad footprint (add `sym=` for the matching .kicad_sym). Without `as=` the next free reference for the part's prefix is used. `refresh=true` bypasses the cache.",
		Examples: []string{"part C2040", "part LCSC:C25804 as=R1 value=10k", "part kicad:Package_TO_SOT_SMD:SOT-23 as=Q1 sym=Regulator_Linear:AP1117-15"},
	},
	{
		Name:  "lib-gen",
		Usage: "lib-gen NAME family=F … [density=N|L|M] [as=REF] [kind=r|c|l|led|d|ic]",
		Notes: []string{
			"        chip size=0201|0402|0603|0805|1206|1210|2512",
			"        sot23 | sot23-5 | sot23-6 | sot223 | sot89",
			"        soic|tssop|ssop|msop pins=N [pitch=P] [body=W]",
			"        qfn|dfn pins=N pitch=P body=W [body_len=L] [ep=S]",
			"        wlp pins=N [pitch=0.4] [body=W] [body_len=L] [pad=0.24] [cols=2]",
			"        qfp|lqfp pins=N pitch=P body=W",
			"        dip pins=N [pitch=2.54] [spacing=7.62]",
			"        header rows=1|2 pins=N [pitch=2.54|2.0|1.27]",
		},
		Describe: "Generate an IPC-7351 land pattern offline (courtyard, silk and pin-1 marker included) and store it under NAME. `density` picks the IPC nominal/least/most fillet; `as=REF` also spawns the symbol and binds the footprint; `kind` sets the symbol kind (default from the family).",
		Examples: []string{"lib-gen R0603 family=chip size=0603 as=R1 kind=r", "lib-gen U2 family=qfn pins=32 pitch=0.5 body=5 ep=3.2", "lib-gen max17220_wlp6 family=wlp pins=6 pitch=0.4 body=0.89 body_len=1.42 pad=0.24", "lib-gen J1x04 family=header rows=1 pins=4 as=J1"},
	},
	{
		Name:     "lib-import",
		Aliases:  []string{"lib_import"},
		Usage:    "lib-import kicad FILE|DIR [key=KEY] [as=REF]   (.kicad_mod, .pretty, .kicad_sym)",
		Describe: "Import KiCad library files from disk: a single .kicad_mod, a whole .pretty directory, or a .kicad_sym (needs `key=` to say which entry receives the pins). `as=REF` spawns the symbol and binds the footprint.",
		Examples: []string{"lib-import kicad ~/libs/MyParts.pretty", "lib-import kicad ~/libs/usb_c.kicad_mod as=J1", "lib-import kicad ~/libs/MyParts.kicad_sym key=usb_c"},
	},
	{
		Name:     "list-parts",
		Aliases:  []string{"list_parts"},
		Usage:    "list-parts [lcsc|kicad|ipc|SUBSTRING]",
		Describe: "List library entries that came from `part`, `lib-gen` or `lib-import`, with source, pin count and LCSC id. Filter by source or by a substring of the key.",
		Examples: []string{"list-parts", "list-parts lcsc", "list-parts sot"},
	},
	{
		Name:     "net",
		Usage:    "net NAME REF.PIN …            (REF.NAME works too when pins are named)",
		Describe: "Create or extend a net by listing the pins on it. Repeat the verb to add more pins later. `class=NAME` assigns a net class inline. Pins can be addressed by number or, for parts with named pins, by name.",
		Examples: []string{"net GND U1.GND C1.2 R1.2 class=ground", "net +3V3 U1.3V3 C1.1 class=power"},
	},
	{
		Name:     "class",
		Aliases:  []string{"net-class", "net_class"},
		Usage:    "class NAME [clearance=N] [width=N] [impedance=Z]",
		Notes:    []string{"  net-class NET CLASS"},
		Describe: "Define a net class (trace width, clearance, target impedance in ohms, `pour=both|top|bottom`). `net-class NET CLASS` assigns an existing class to a net.",
		Examples: []string{"class ground pour=both", "class power width=0.4", "class rf impedance=50", "net-class USB_DP rf"},
	},
	{
		Name:     "palette",
		Usage:    "palette REF KEY | palette list",
		Describe: "Bind a placed reference to a built-in footprint key (this is how a symbol gets a footprint). `palette list` prints the available keys. `model=` overrides the 3D body for `fragua render` (KiCad `Library.3dshapes/Name.wrl`, or a local .wrl/.obj).",
		Examples: []string{"palette list", "palette C1 c_0603 value=100nF", "palette U1 esp32_s3_zero"},
	},
	{
		Name:     "list-lib",
		Aliases:  []string{"lib-list", "list_lib", "lib_list"},
		Usage:    "list-lib",
		Describe: "List every footprint currently known: built-in palette entries plus any defined with `lib`.",
		Examples: []string{"list-lib"},
	},
	{
		Name:  "place",
		Usage: "place REF x y [rot=DEG]",
		Describe: "Place a footprint at x,y mm (its origin), optionally rotated. Use this to anchor the parts whose position matters, then let `auto-place` arrange the rest. " +
			"A placed part is pinned: a bare `auto-place` leaves it alone, and only `auto-place REF` moves it again.",
		Examples: []string{"place U1 25 15", "place J1 0 10 rot=90"},
	},
	{
		Name:     "place-legal",
		Aliases:  []string{"place_legal"},
		Usage:    "place-legal REF [tries=N] [rot=DEG]",
		Describe: "Place a footprint at the first legal spot found by search, instead of at coordinates you pick. Useful when you do not care where a part lands.",
		Examples: []string{"place-legal C3", "place-legal C3 tries=200"},
	},
	{
		Name:    "edge-place",
		Aliases: []string{"edge_place"},
		Usage:   "edge-place REF left|right|top|bottom [along=N]",
		Describe: "Snap a connector to a board edge, oriented outward. `along` is the distance in mm along that edge. " +
			"What gets snapped depends on the part: a surface-mount one (castellated module, edge-launch SMA) puts its PADS flush with the edge and is allowed to overhang, " +
			"which is what leaves a module's USB-C reachable by a cable. A through-hole one puts its BODY on the board instead — the library courtyard when it declares one, so a screw terminal's wire mouth ends at the edge with the block inboard — " +
			"and is then pulled back until copper and drill barrels clear the routed edge by the fab minimum.",
		Examples: []string{"edge-place J1 left", "edge-place J2 bottom along=12"},
	},
	{
		Name:     "edge-plan",
		Aliases:  []string{"edge_plan"},
		Usage:    "edge-plan REF [REF...]",
		Describe: "Distribute several connectors around the board edges in one pass, choosing edges and offsets for you.",
		Examples: []string{"edge-plan J1 J2 SW1"},
	},
	{
		Name:     "move",
		Aliases:  []string{"rotate"},
		Usage:    "move REF X Y | rotate REF DEG",
		Describe: "Move an already-placed footprint to X,Y mm, or rotate it to an absolute angle in degrees.",
		Examples: []string{"move C1 35 15", "rotate U1 90"},
	},
	{
		Name:     "unplace",
		Aliases:  []string{"delete", "clear-board", "clear_board"},
		Usage:    "unplace REF | delete REF | clear-board",
		Describe: "`unplace` lifts a footprint off the board but keeps the symbol. `delete` removes the part entirely. `clear-board` empties the board layout.",
		Examples: []string{"unplace C1", "delete R9", "clear-board"},
	},
	{
		Name:    "auto-place",
		Aliases: []string{"auto_place"},
		Usage:   "auto-place [REF...] [seed=N] [iters=N]",
		Describe: "ePlace global placement (Poisson/DCT + Nesterov) plus simulated-annealing legalisation over the listed parts (all movable parts if none listed). " +
			"Parts bound by `palette` / `part` / `lib-gen` but never placed are seated on the board first (reported as `seated N new`), so you do not have to `place` them by hand. " +
			"Anything you `place`d or `edge-place`d stays put and is routed around; name it as a REF to move it anyway. Pass a `seed` to make the result reproducible. Needs an outline.",
		Examples: []string{"auto-place", "auto-place R1 C1 C2 seed=42", "auto-place seed=7 iters=4000"},
	},
	{
		Name:  "route",
		Usage: "route [max_seconds=N] [clearance=MM] [organic=true] [teardrop=true] [engine=grid|topo]",
		Notes: []string{"        (max_seconds: default 600, max 3600; clearance: extra air, fab min is the floor; engine=topo is experimental)"},
		Describe: "Auto-route every unrouted net: Theta* any-angle search with rip-up-and-reroute, fanout escapes and pour stitching. " +
			"`engine=topo` opts into the Delaunay homotopy / rubber-band engine (clears existing copper and re-routes the board). " +
			"Default is the grid engine — keep it for the agent path until you have a reason to try topo. " +
			"Report lines say how many nets routed and which failed. Re-running the grid engine is cheap and only attacks what is still open.",
		Examples: []string{"route", "route max_seconds=120", "route max_seconds=300 organic=true teardrop=true", "route engine=topo max_seconds=60"},
	},
	{
		Name:     "clear-route",
		Aliases:  []string{"clear_route", "clear-net", "clear_net", "delete-trace", "delete_trace", "delete-via", "delete_via"},
		Usage:    "clear-route | clear-net NET | delete-trace ID | delete-via ID",
		Describe: "Undo routing: everything, one net, or a single trace/via by id. Use `clear-net` when one net routed badly and you want to retry just that one.",
		Examples: []string{"clear-route", "clear-net +3V3", "delete-trace t17"},
	},
	{
		Name:     "trace",
		Usage:    "trace NET x1 y1 x2 y2 [layer=Top] [width=0.15]",
		Describe: "Draw one trace segment by hand on a net. Rarely needed — prefer `route` — but useful to force a specific path.",
		Examples: []string{"trace GND 10 10 20 10", "trace +3V3 5 5 5 20 layer=Bottom width=0.4"},
	},
	{
		Name:     "via",
		Usage:    "via NET x y [drill=0.3] [dia=0.6]",
		Describe: "Place a via on a net at x,y mm. Defaults match the JLCPCB minimum; widen for high-current nets.",
		Examples: []string{"via GND 12 8", "via +3V3 12 8 drill=0.4 dia=0.8"},
	},
	{
		Name:     "pour",
		Usage:    "pour NET [layer=Top] [relief=spokes4|solid] [stitch=true] [pitch=N]",
		Describe: "Flood a copper pour for a net on one layer. `relief=spokes4` gives thermal reliefs on pads (solderable); `solid` connects directly.",
		Examples: []string{"pour GND layer=Bottom", "pour GND layer=Top relief=spokes4 stitch=true"},
	},
	{
		Name:     "auto-pour",
		Aliases:  []string{"auto_pour"},
		Usage:    "auto-pour [NET...]          (default GND, both layers)",
		Describe: "Pour the listed nets on every copper layer at once. With no arguments it pours GND on both sides — the usual move after routing.",
		Examples: []string{"auto-pour", "auto-pour GND +3V3"},
	},
	{
		Name:     "clear-pour",
		Aliases:  []string{"clear_pour"},
		Usage:    "clear-pour [NET]",
		Describe: "Delete pours, all of them or just one net's. Do this before re-routing, then pour again afterwards.",
		Examples: []string{"clear-pour", "clear-pour GND"},
	},
	{
		Name:     "stitch",
		Aliases:  []string{"stitch-isolated-pads", "stitch_isolated_pads"},
		Usage:    "stitch                      (grid + pad vias that tie pour islands)",
		Describe: "Add stitching vias so isolated pour islands and stranded pads reconnect to their net. Run after `auto-pour`.",
		Examples: []string{"stitch"},
	},
	{
		Name:     "nc",
		Usage:    "nc REF.PIN [REF.PIN...]     (mark unused MCU pins; no floating_pin)",
		Describe: "Mark pins as intentionally unconnected so ERC stops reporting them as floating. The usual fix for unused MCU GPIOs.",
		Examples: []string{"nc U1.IO9 U1.IO10 U1.IO11"},
	},
	{
		Name:     "fiducial",
		Usage:    "fiducial X Y [ref=FID1]",
		Describe: "Add a fiducial marker for pick-and-place alignment. Assemblers usually want three, spread asymmetrically.",
		Examples: []string{"fiducial 2 2", "fiducial 2 18 ref=FID2"},
	},
	{
		Name:     "diff",
		Aliases:  []string{"diff-pair", "diff_pair"},
		Usage:    "diff NETA NETB              (diff-pair data; single-ended Z only)",
		Describe: "Declare two nets a differential pair so `si-check` can report their skew. Impedance is still computed single-ended.",
		Examples: []string{"diff USB_DP USB_DM"},
	},
	{
		Name:     "impedance",
		Usage:    "impedance [NET]             (closed-form microstrip/stripline; not FEM)",
		Describe: "Report the computed characteristic impedance per net from the stackup and trace geometry. Closed-form approximation, not a field solver.",
		Examples: []string{"impedance", "impedance USB_DP"},
	},
	{
		Name:     "si-check",
		Aliases:  []string{"si_check", "sicheck"},
		Usage:    "si-check [NET...] [tol=0.10] [max_vias=N]",
		Notes:    []string{"                              (impedance, return path, diff skew, via budget)"},
		Describe: "Signal-integrity audit: impedance deviation beyond `tol`, plane gaps under a trace's return path, diff-pair skew, and vias per net over `max_vias`.",
		Examples: []string{"si-check", "si-check USB_DP USB_DM tol=0.05 max_vias=2"},
	},
	{
		Name:     "teardrop",
		Aliases:  []string{"teardrops"},
		Usage:    "teardrop on|off             (copper fillets at pad/via junctions)",
		Describe: "Toggle teardrop fillets where traces meet pads and vias. Improves manufacturing yield; turn on once the board routes clean.",
		Examples: []string{"teardrop on"},
	},
	{
		Name:     "silk-line",
		Aliases:  []string{"silk_line", "silk-text", "silk_text"},
		Usage:    "silk-line X1 Y1 X2 Y2 | silk-text X Y TEXT [size=1]",
		Describe: "Draw silkscreen lines and text (mm; `size` is cap height in mm). Reference designators are drawn for you — use these for labels, logos and polarity marks.",
		Examples: []string{"silk-text 4 18 FRAGUA size=1.5", "silk-line 0 12 30 12"},
	},
	{
		Name:     "rule-area",
		Aliases:  []string{"rule_area"},
		Usage:    "rule-area NAME x1 y1 x2 y2 [clearance=N] …",
		Describe: "Apply local design rules inside a rectangle — a wider clearance under a high-voltage part, for instance — without changing the global rules.",
		Examples: []string{"rule-area hv 20 0 30 20 clearance=0.5"},
	},
	{
		Name:     "fab-rules",
		Aliases:  []string{"fab_rules"},
		Usage:    "fab-rules jlcpcb|jlcpcb-2l-via02|jlcpcb-4l|clear|list",
		Describe: "Load a fabricator's minimum rule set. These become the floor DRC and the router will not go below them. Set this before routing so you never route something the fab rejects.",
		Examples: []string{"fab-rules list", "fab-rules jlcpcb", "fab-rules jlcpcb-4l"},
	},
	{
		Name:     "escape",
		Usage:    "escape via-in-pad REF.PAD | via-in-pad-stranded [on|off] | list",
		Describe: "Fine-pitch escape strategy: drop a via directly in a pad (QFN/BGA thermal and inner pads), or let the router do it automatically for pads it cannot otherwise reach.",
		Examples: []string{"escape list", "escape via-in-pad U1.EP", "escape via-in-pad-stranded on"},
	},
	{
		Name:     "layer",
		Usage:    "layer list|add|remove|rename|dielectric",
		Describe: "Inspect and edit the stackup. Default is 2-layer (Top/Bottom); `layer add` grows it, `layer dielectric` sets thickness and Er, which feed the impedance model.",
		Examples: []string{"layer list", "layer add GND plane", "layer dielectric 1 0.2 4.3"},
	},
	{
		Name:     "drc",
		Aliases:  []string{"erc"},
		Usage:    "drc / erc",
		Describe: "`erc` checks the schematic (floating pins, unconnected nets, power conflicts); `drc` checks the geometry (clearance, shorts, annular ring, board edge). Both print violations; both must be clean before `pack`.",
		Examples: []string{"erc", "drc"},
	},
	{
		Name:  "compact",
		Usage: "compact [step=1] [seed=N] [allow_failed=0] [route_seconds=20] [max_seconds=600] [aspect=keep|free]",
		Describe: "Iteratively shrink an already-clean, already-routed board, re-placing and re-routing at each step and rolling back any step that breaks DRC or connectivity. " +
			"Optional and slow — run it only once the board is clean.",
		Examples: []string{"compact allow_failed=0 route_seconds=90", "compact step=0.5 aspect=free max_seconds=300"},
	},
	{
		Name:    "pack",
		Aliases: []string{"export"},
		Usage:   "pack [fab=jlcpcb] [out=DIR] [teardrop=true] | export DIR   (fails on ERC errors)",
		Notes: []string{
			"  pack fab=kicad [out=board.kicad_pcb] [zones=filled|outline] [grid=MM]",
			"                              (KiCad 9 .kicad_pcb; zones ship pre-filled)",
		},
		Describe: "Write the manufacturing bundle — Gerbers, Excellon drill, BOM, CPL, a README and the board as .kicad_pcb — as <project>-<fab>.zip in DIR. Fabs: jlcpcb, pcbway, generic. `fab=kicad` writes only the KiCad 9 board file (zones pre-filled so KiCad shows copper without refilling). Refuses to run while ERC reports errors.",
		Examples: []string{"pack fab=jlcpcb out=/tmp", "pack fab=pcbway out=./out teardrop=true", "pack fab=kicad out=/tmp/board.kicad_pcb"},
	},
	{
		Name:     "screenshot",
		Usage:    "screenshot PATH",
		Describe: "Write the current board rendering to PATH as SVG. The same image is served live at GET /screenshot.",
		Examples: []string{"screenshot /tmp/board.svg"},
	},
	{
		Name:     "save",
		Aliases:  []string{"view", "status", "reset", "help"},
		Usage:    "save [PATH] | view | status | reset | help",
		Describe: "`status` (alias `view`) prints a one-shot summary: outline, part and net counts, routed/unrouted, DRC state. `save` writes the .fragua project. `reset` empties the project. `help` points at the full reference.",
		Examples: []string{"status", "save /tmp/board.fragua", "reset"},
	},
}

// verbIndex maps every name and alias to its entry.
var verbIndex = func() map[string]*VerbHelp {
	m := make(map[string]*VerbHelp, len(Verbs)*3)
	for i := range Verbs {
		v := &Verbs[i]
		m[v.Name] = v
		for _, a := range v.Aliases {
			m[a] = v
		}
	}
	return m
}()

// normVerb lowercases and folds underscores to dashes, matching the dispatcher.
func normVerb(name string) string {
	return strings.ReplaceAll(strings.ToLower(strings.TrimSpace(name)), "_", "-")
}

// LookupVerb finds a verb entry by name or alias.
func LookupVerb(name string) (*VerbHelp, bool) {
	if v, ok := verbIndex[strings.ToLower(strings.TrimSpace(name))]; ok {
		return v, true
	}
	want := normVerb(name)
	for i := range Verbs {
		v := &Verbs[i]
		if normVerb(v.Name) == want {
			return v, true
		}
		for _, a := range v.Aliases {
			if normVerb(a) == want {
				return v, true
			}
		}
	}
	return nil, false
}

// VerbNames lists every primary verb name, sorted.
func VerbNames() []string {
	names := make([]string, 0, len(Verbs))
	for i := range Verbs {
		names = append(names, Verbs[i].Name)
	}
	sort.Strings(names)
	return names
}

// VerbUsage renders the help for a single verb, or a "did you mean" miss.
func VerbUsage(name string) (string, bool) {
	v, ok := LookupVerb(name)
	if !ok {
		var b strings.Builder
		b.WriteString("unknown verb " + strings.TrimSpace(name) + "\n\nKnown verbs:\n")
		b.WriteString(wrapList(VerbNames(), "  ", 72))
		b.WriteString("\nRun `fragua help` for the full reference.\n")
		return b.String(), false
	}
	var b strings.Builder
	b.WriteString(v.Name + " — fragua script verb\n\n")
	b.WriteString("Usage:\n  " + v.Usage + "\n")
	for _, n := range v.Notes {
		b.WriteString("  " + strings.TrimSpace(n) + "\n")
	}
	if len(v.Aliases) > 0 {
		b.WriteString("\nAliases:\n  " + strings.Join(v.Aliases, ", ") + "\n")
	}
	if v.Describe != "" {
		b.WriteString("\n" + wrapText(v.Describe, 76) + "\n")
	}
	if len(v.Examples) > 0 {
		b.WriteString("\nExamples:\n")
		for _, ex := range v.Examples {
			for _, line := range strings.Split(ex, "\n") {
				b.WriteString("  " + line + "\n")
			}
		}
	}
	b.WriteString("\nFull reference: `fragua help` or GET /help\n")
	return b.String(), true
}

// wrapList renders names as indented comma-free columns within width.
func wrapList(names []string, indent string, width int) string {
	var b strings.Builder
	line := indent
	for _, n := range names {
		if len(line)+len(n)+1 > width && line != indent {
			b.WriteString(strings.TrimRight(line, " ") + "\n")
			line = indent
		}
		line += n + " "
	}
	if strings.TrimSpace(line) != "" {
		b.WriteString(strings.TrimRight(line, " ") + "\n")
	}
	return b.String()
}

// wrapText soft-wraps prose at width columns.
func wrapText(s string, width int) string {
	words := strings.Fields(s)
	var b strings.Builder
	line := ""
	for _, w := range words {
		if line != "" && len(line)+1+len(w) > width {
			b.WriteString(line + "\n")
			line = ""
		}
		if line == "" {
			line = w
		} else {
			line += " " + w
		}
	}
	if line != "" {
		b.WriteString(line)
	}
	return b.String()
}

// verbReference renders the "Script verbs" block of the full help.
func verbReference() string {
	var b strings.Builder
	for i := range Verbs {
		b.WriteString("  " + Verbs[i].Usage + "\n")
		for _, n := range Verbs[i].Notes {
			b.WriteString(n + "\n")
		}
	}
	return b.String()
}

const usageHeader = `fragua — AI-native PCB design tool

Usage:
  fragua                 print this help and exit
  fragua help [verb]     full reference, or help for one script verb
  fragua run [file]      start local HTTP API (default 127.0.0.1:7878)
  fragua mcp [file]      same host, plus an MCP server on stdio (for AI agents)
  fragua init [dir]      write agent onboarding files into a project directory
  fragua bench [dir]     run the reference boards (place → route → drc)
                         [--seed N] [--budget S] [--json f] [--md f] [--strict]
  fragua render FILE     3D product-shot PNG (also: fragua render3d)
                         [--3d] [-o out.png] [--width PX] [--height PX]
                         [--models kicad|easyeda|none] [--model-cache DIR] [--offline]

Environment:
  FRAGUA_API_ADDR        listen address (default 127.0.0.1:7878)
  FRAGUA_NO_BROWSER      if set, do not open a browser window
  FRAGUA_OFFLINE         if 1, part LCSC:… uses the library cache only
  FRAGUA_KICAD_LIBS      extra KiCad library roots (path-list separated)

HTTP API:
  GET  /  /help          this reference (?verb=route for one verb)
  GET  /health           ok
  GET  /screenshot       PNG/SVG of current board
  GET  /events           SSE project change stream
  GET  /state            JSON project snapshot (UI)
  POST /script           run multi-line script (text/plain body or JSON {"script":"..."})
  POST /save             save project {"path":"..."} optional

First 10 minutes:
  1. fragua run              (or fragua mcp — the UI stays at 127.0.0.1:7878/ui/)
  2. list-lib                see which footprints already exist
  3. sym / net               describe the circuit, then run erc until it is clean
  4. outline W H             give the board a size
  5. palette + place         bind footprints, anchor the parts whose position matters
  6. auto-place seed=42      seat and arrange the rest (hand-placed parts stay put)
  7. route max_seconds=120   auto-route; re-run to keep attacking what failed
  8. auto-pour then stitch   ground pours plus the vias that tie their islands
  9. drc                     fix violations, re-route, repeat until clean
 10. pack fab=jlcpcb out=/tmp    writes the upload-ready zip

Script verbs (line-oriented, agent-first):
`

const usageFooter = `
An agent can take a board from 0 to a JLCPCB pack with the verbs above.
Commercial floor: the same agent loop used on shipped boards:
auto-place (ePlace + SA + decouple + edge snap) → route (Theta* + fanout + stitch; engine=topo optional)
→ pour/stitch → drc/erc → pack.
`

// Usage returns the CLI / GET / help text.
func Usage() string {
	return usageHeader + verbReference() + usageFooter
}
