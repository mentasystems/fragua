package script

import (
	"bufio"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
	"github.com/mentasystems/fragua/internal/drc"
	"github.com/mentasystems/fragua/internal/erc"
	"github.com/mentasystems/fragua/internal/fab"
	"github.com/mentasystems/fragua/internal/placer"
	"github.com/mentasystems/fragua/internal/render"
	"github.com/mentasystems/fragua/internal/router"
)

// Result is one script line outcome.
type Result struct {
	Line   int    `json:"line"`
	Tool   string `json:"tool"`
	OK     bool   `json:"ok"`
	Result string `json:"result"`
}

// openBlock accumulates indented pin/pad lines under sym/lib.
type openBlock struct {
	verb string
	line int
	args string
	pins []core.SchPin
	pads []core.LibraryPad
}

// RunScript executes a multi-line script against project.
func RunScript(p *core.Project, script string) []Result {
	var results []Result
	sc := bufio.NewScanner(strings.NewReader(script))
	lineNo := 0
	var block *openBlock

	flushBlock := func() bool {
		if block == nil {
			return true
		}
		b := block
		block = nil
		msg, err := finishBlock(p, b)
		r := Result{Line: b.line, Tool: b.verb, OK: err == nil, Result: msg}
		if err != nil {
			r.Result = err.Error()
		}
		results = append(results, r)
		return err == nil
	}

	for sc.Scan() {
		lineNo++
		raw := sc.Text()
		// strip trailing whitespace; keep leading for indent detect
		raw = strings.TrimRight(raw, " \t\r")
		if strings.TrimSpace(raw) == "" {
			continue
		}
		trim := strings.TrimSpace(raw)
		if strings.HasPrefix(trim, "#") || strings.HasPrefix(trim, "//") {
			continue
		}
		// strip inline comments
		if i := strings.Index(trim, " #"); i >= 0 {
			trim = strings.TrimSpace(trim[:i])
		}
		indented := strings.HasPrefix(raw, " ") || strings.HasPrefix(raw, "\t")
		if indented {
			if block == nil {
				results = append(results, Result{
					Line: lineNo, Tool: "?", OK: false,
					Result: "indented line without open sym/lib block",
				})
				return results
			}
			if err := absorbContinuation(block, lineNo, trim); err != nil {
				results = append(results, Result{
					Line: lineNo, Tool: block.verb, OK: false, Result: err.Error(),
				})
				return results
			}
			continue
		}
		// non-indented: finish any open block first
		if !flushBlock() {
			return results
		}
		tool, args := splitVerb(trim)
		if tool == "sym" || tool == "lib" {
			block = &openBlock{verb: tool, line: lineNo, args: args}
			continue
		}
		msg, err := dispatch(p, tool, args)
		r := Result{Line: lineNo, Tool: tool, OK: err == nil, Result: msg}
		if err != nil {
			r.Result = err.Error()
		}
		results = append(results, r)
		if err != nil {
			return results
		}
	}
	_ = flushBlock()
	return results
}

// FormatResults renders results as text/plain for agents.
func FormatResults(rs []Result) string {
	var b strings.Builder
	for _, r := range rs {
		if r.OK {
			fmt.Fprintf(&b, "ok %s: %s\n", r.Tool, r.Result)
		} else {
			fmt.Fprintf(&b, "error line %d %s: %s\n", r.Line, r.Tool, r.Result)
		}
	}
	return b.String()
}

func splitVerb(line string) (tool string, args string) {
	parts := strings.SplitN(line, " ", 2)
	tool = strings.ToLower(parts[0])
	if len(parts) > 1 {
		args = strings.TrimSpace(parts[1])
	}
	return tool, args
}

func absorbContinuation(b *openBlock, line int, body string) error {
	fields := tokenize(body)
	if len(fields) == 0 {
		return nil
	}
	sub := strings.ToLower(fields[0])
	switch {
	case b.verb == "sym" && sub == "pin":
		// pin NUMBER SIDE [NAME] [name=] [role=]
		if len(fields) < 3 {
			return fmt.Errorf("pin needs: pin NUMBER SIDE [NAME] [role=ROLE]")
		}
		pin := core.SchPin{
			Number: fields[1],
			Side:   expandSide(fields[2]),
			Role:   core.PinPassive,
		}
		for _, t := range fields[3:] {
			if strings.HasPrefix(t, "name=") {
				pin.Name = strings.TrimPrefix(t, "name=")
			} else if strings.HasPrefix(t, "role=") {
				pin.Role = expandRole(strings.TrimPrefix(t, "role="))
				if pin.Role == core.PinNC {
					pin.NC = true
				}
			} else if strings.EqualFold(t, "nc") {
				pin.NC = true
				pin.Role = core.PinNC
			} else if !strings.Contains(t, "=") {
				pin.Name = t
			}
		}
		b.pins = append(b.pins, pin)
		return nil
	case b.verb == "lib" && sub == "pad":
		// pad NUMBER X Y W H [name=NAME] [drill=MM]
		if len(fields) < 6 {
			return fmt.Errorf("pad needs: pad NUMBER X Y W H [name=NAME] [drill=MM]")
		}
		x, err1 := strconv.ParseFloat(fields[2], 64)
		y, err2 := strconv.ParseFloat(fields[3], 64)
		w, err3 := strconv.ParseFloat(fields[4], 64)
		h, err4 := strconv.ParseFloat(fields[5], 64)
		if err1 != nil || err2 != nil || err3 != nil || err4 != nil {
			return fmt.Errorf("pad: bad numbers")
		}
		pad := core.LibraryPad{Number: fields[1], XMM: x, YMM: y, WMM: w, HMM: h}
		for _, t := range fields[6:] {
			if strings.HasPrefix(t, "name=") {
				pad.Name = strings.TrimPrefix(t, "name=")
			} else if strings.HasPrefix(t, "drill=") {
				d, err := strconv.ParseFloat(strings.TrimPrefix(t, "drill="), 64)
				if err != nil {
					return fmt.Errorf("pad drill: %w", err)
				}
				pad.DrillMM = &d
			}
		}
		b.pads = append(b.pads, pad)
		return nil
	default:
		return fmt.Errorf("`%s` block can't contain `%s`", b.verb, sub)
	}
}

func finishBlock(p *core.Project, b *openBlock) (string, error) {
	switch b.verb {
	case "sym":
		return addSymbol(p, b.args, b.pins)
	case "lib":
		return addLibrary(p, b.args, b.pads)
	default:
		return "", fmt.Errorf("unknown block %q", b.verb)
	}
}

func dispatch(p *core.Project, tool, args string) (string, error) {
	// Real-part verbs (part / lib-gen / lib-import / list-parts) live in
	// parts_verbs.go; ok=false falls through to the switch below.
	if msg, err, ok := partsVerb(p, tool, args); ok {
		return msg, err
	}
	switch tool {
	case "status", "view":
		return status(p), nil
	case "clear-route", "clear_route":
		p.MutateBoard(func(b *core.Board) { b.ClearRoute() })
		return "cleared traces and vias", nil
	case "drc":
		p.RLock()
		rep := drc.Check(p.Board(), p.Schematic(), drc.DefaultOptions())
		p.RUnlock()
		return rep.Detail(), nil
	case "erc":
		p.RLock()
		rep := erc.Check(p.Schematic(), p.Board(), erc.DefaultOptions())
		p.RUnlock()
		return rep.Detail(), nil
	case "route":
		opts := router.DefaultOptions()
		opts = router.ParseOptions(opts, args)
		var rep router.Report
		runOp(p, "route", func() {
			emit := progressEmitter(p, "route")
			opts.Progress = func(pr router.Progress) { emit(pr.Net, pr.Done, pr.Total) }
			opts.Cancel = p.Ops().Cancelled
			p.MutateBoard(func(b *core.Board) {
				// Net classes drive per-net width (impedance target first).
				opts.Schematic = p.Schematic()
				rep = router.Route(b, opts)
			})
		})
		return rep.Summary(), nil
	case "auto-place", "auto_place":
		refs, kv := splitRefsKV(args)
		// `palette` / `part` / `lib-gen` bind a footprint without placing it,
		// so a first auto-place used to find nothing to move.
		seated, err := seatPending(p, "auto-place", refs)
		if err != nil {
			return "", err
		}
		opts := placer.DefaultOptions()
		opts = placer.ParseOptions(opts, kv)
		var rep placer.Report
		runOp(p, "auto-place", func() {
			emit := progressEmitter(p, "auto-place")
			opts.Progress = func(done, total int) { emit("annealing", done, total) }
			opts.Cancel = p.Ops().Cancelled
			p.MutateBoard(func(b *core.Board) {
				rep, err = placer.Place(b, refs, opts)
			})
		})
		if err != nil {
			return "", err
		}
		rep.Seated = seated
		return rep.Summary(), nil
	case "outline":
		return setOutline(p, args)
	case "outline-poly", "outline_poly":
		return cmdOutlinePoly(p, args)
	case "place":
		return placeOne(p, args)
	case "place-legal", "place_legal":
		return cmdPlaceLegal(p, args)
	case "pack", "export":
		return packBoard(p, args)
	case "net":
		return addNet(p, args)
	case "delete-trace", "delete_trace":
		return deleteTrace(p, args)
	case "save":
		path := strings.TrimSpace(args)
		if path == "" {
			path = p.SavePath()
		}
		if path == "" {
			return "", fmt.Errorf("save: path required")
		}
		if err := p.SaveToPath(path); err != nil {
			return "", err
		}
		return "saved " + path, nil
	case "help":
		return "see GET /help", nil
	case "sym":
		return addSymbol(p, args, nil)
	case "lib":
		return addLibrary(p, args, nil)
	case "palette":
		return paletteCmd(p, args)
	case "trace":
		return addTrace(p, args)
	case "via":
		return addVia(p, args)
	case "rule-area", "rule_area":
		return addRuleArea(p, args)
	case "fab-rules", "fab_rules":
		return setFabRules(p, args)
	case "layer":
		return layerCmd(p, args)
	case "escape":
		return escapeCmd(p, args)
	case "stackup":
		if strings.TrimSpace(args) == "4" || strings.TrimSpace(args) == "4l" || strings.TrimSpace(args) == "4layer" {
			p.MutateBoard(func(b *core.Board) { b.Apply4Layer() })
			return "stackup: 4-layer (F.Cu / In1.Cu / In2.Cu / B.Cu)", nil
		}
		return "", fmt.Errorf("stackup 4")
	case "compact":
		return cmdCompact(p, args)
	case "screenshot":
		return screenshot(p, args)
	case "list-lib", "lib-list", "list_lib", "lib_list":
		return listLib(p), nil
	case "pour":
		return cmdPour(p, args)
	case "auto-pour", "auto_pour":
		return cmdAutoPour(p, args)
	case "clear-pour", "clear_pour":
		return cmdClearPour(p, args)
	case "stitch", "stitch-isolated-pads", "stitch_isolated_pads":
		return cmdStitch(p, args)
	case "nc":
		return cmdNC(p, args)
	case "fiducial":
		return cmdFiducial(p, args)
	case "diff", "diff-pair", "diff_pair":
		return cmdDiffPair(p, args)
	case "cutout":
		return cmdCutout(p, args)
	case "clear-cutouts", "clear_cutouts":
		return cmdClearCutouts(p, args)
	case "hole":
		return cmdHole(p, args)
	case "clear-holes", "clear_holes":
		return cmdClearHoles(p, args)
	case "keepout":
		return cmdKeepout(p, args)
	case "silk-line", "silk_line":
		return cmdSilkLine(p, args)
	case "silk-text", "silk_text":
		return cmdSilkText(p, args)
	case "move":
		return cmdMove(p, args)
	case "rotate":
		return cmdRotate(p, args)
	case "delete":
		return cmdDelete(p, args)
	case "unplace":
		return cmdUnplace(p, args)
	case "clear-board", "clear_board":
		return cmdClearBoard(p, args)
	case "clear-net", "clear_net":
		return cmdClearNet(p, args)
	case "delete-via", "delete_via":
		return cmdDeleteVia(p, args)
	case "edge-place", "edge_place":
		return cmdEdgePlace(p, args)
	case "edge-plan", "edge_plan":
		return cmdEdgePlan(p, args)
	case "reset":
		return cmdReset(p, args)
	case "class", "net-class", "net_class":
		return cmdNetClass(p, args)
	case "teardrop", "teardrops":
		return cmdTeardrop(p, args)
	case "impedance":
		return cmdImpedance(p, args)
	case "si-check", "si_check", "sicheck":
		return cmdSICheck(p, args)
	default:
		return "", fmt.Errorf("unknown verb %q — see GET /help", tool)
	}
}

// listLib prints one library entry per line (agent-parseable, like Rust list-lib).
func listLib(p *core.Project) string {
	lib := p.Library()
	keys := lib.List()
	if len(keys) == 0 {
		return "0 entries in library"
	}
	var b strings.Builder
	fmt.Fprintf(&b, "%d entries in library\n", len(keys))
	for _, key := range keys {
		e, ok := lib.Get(key)
		if !ok {
			continue
		}
		fmt.Fprintf(&b, "%s pads=%d", e.Key, len(e.Pads))
		if e.Source != "" {
			fmt.Fprintf(&b, " src=%s pins=%d", e.Source, len(e.Pins))
		}
		if e.EdgeMounted {
			b.WriteString(" edge")
			if e.EdgeSide != nil {
				fmt.Fprintf(&b, "=%s", *e.EdgeSide)
			}
		}
		if e.Elevated {
			b.WriteString(" elevated")
		}
		if e.BodyRect != nil {
			b.WriteString(" body")
		}
		if e.Description != "" {
			fmt.Fprintf(&b, " %q", e.Description)
		}
		b.WriteByte('\n')
	}
	return strings.TrimRight(b.String(), "\n")
}

func status(p *core.Project) string {
	// Name() takes its own RLock — fetch before holding project lock.
	name := p.Name()
	p.RLock()
	defer p.RUnlock()
	b := p.Board()
	sch := p.Schematic()
	ow, oh := 0.0, 0.0
	if b.Outline != nil {
		ow = b.Outline.Width().ToMM()
		oh = b.Outline.Height().ToMM()
	}
	return fmt.Sprintf(
		"name=%q footprints=%d traces=%d vias=%d nets=%d symbols=%d palette=%d outline=%.2fx%.2fmm",
		name, len(b.Footprints), len(b.Traces), len(b.Vias), len(sch.Nets),
		len(sch.Symbols), len(p.Palette()), ow, oh,
	)
}

func setOutline(p *core.Project, args string) (string, error) {
	fields := strings.Fields(args)
	if len(fields) < 2 {
		return "", fmt.Errorf("outline W H [radius=R] expected")
	}
	var w, h, rad float64
	if _, err := fmt.Sscanf(fields[0], "%f", &w); err != nil {
		return "", fmt.Errorf("outline width: %w", err)
	}
	if _, err := fmt.Sscanf(fields[1], "%f", &h); err != nil {
		return "", fmt.Errorf("outline height: %w", err)
	}
	for _, f := range fields[2:] {
		if strings.HasPrefix(f, "radius=") {
			fmt.Sscanf(f, "radius=%f", &rad)
		}
	}
	p.MutateBoard(func(b *core.Board) {
		r := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(w), core.FromMM(h)))
		b.Outline = &r
		if rad > 0 {
			b.OutlineCornerRadius = core.FromMM(rad)
		}
	})
	return fmt.Sprintf("outline %.2f x %.2f mm", w, h), nil
}

func placeOne(p *core.Project, args string) (string, error) {
	// place REF x y [rot=DEG] or [ROT_DEG]
	fields := strings.Fields(args)
	if len(fields) < 3 {
		return "", fmt.Errorf("place REF x y [rot=DEG]")
	}
	ref := fields[0]
	x, err1 := strconv.ParseFloat(fields[1], 64)
	y, err2 := strconv.ParseFloat(fields[2], 64)
	if err1 != nil || err2 != nil {
		return "", fmt.Errorf("place: bad coordinates")
	}
	rotSet := false
	rot := 0.0
	for _, f := range fields[3:] {
		if strings.HasPrefix(f, "rot=") {
			fmt.Sscanf(f, "rot=%f", &rot)
			rotSet = true
		} else if !strings.Contains(f, "=") {
			if v, err := strconv.ParseFloat(f, 64); err == nil {
				rot = v
				rotSet = true
			}
		}
	}
	// Prefer re-placing an existing board footprint.
	var found bool
	p.MutateBoard(func(b *core.Board) {
		fp := b.FootprintByRef(ref)
		if fp == nil {
			return
		}
		fp.Position = core.NewPoint(core.FromMM(x), core.FromMM(y))
		if rotSet {
			fp.Rotation = rot
		}
		fp.Pinned = true
		found = true
	})
	if found {
		return fmt.Sprintf("placed %s at %.2f,%.2f rot=%.0f", ref, x, y, rot), nil
	}
	// Else take from palette.
	fp, ok := p.PaletteTake(ref)
	if !ok {
		return "", fmt.Errorf("place: unknown ref %q (not on board or palette)", ref)
	}
	fp.Position = core.NewPoint(core.FromMM(x), core.FromMM(y))
	if rotSet {
		fp.Rotation = rot
	}
	fp.Pinned = true
	p.MutateBoard(func(b *core.Board) {
		b.AddFootprint(fp)
	})
	return fmt.Sprintf("placed %s at %.2f,%.2f rot=%.0f", ref, x, y, fp.Rotation), nil
}

// ─── seating bound-but-unplaced parts ────────────────────────────────

// pendingRefs lists the references that are bound to a footprint but absent
// from the board, in a stable order: palette items first (insertion order),
// then symbols whose key= names a library entry and that were never bound.
// refs, when non-empty, narrows the list to what the caller asked for.
func pendingRefs(p *core.Project, refs []string) []string {
	want := map[string]bool{}
	for _, r := range refs {
		want[strings.ToUpper(r)] = true
	}
	keep := func(ref string) bool { return len(want) == 0 || want[strings.ToUpper(ref)] }

	p.RLock()
	b := p.Board()
	var out []string
	keys := map[string]string{}
	seen := map[string]bool{}
	for _, fp := range p.Palette() {
		if seen[fp.Reference] || !keep(fp.Reference) || b.FootprintByRef(fp.Reference) != nil {
			continue
		}
		seen[fp.Reference] = true
		out = append(out, fp.Reference)
	}
	sch := p.Schematic()
	var maybe []string
	for _, id := range sch.SymbolOrder {
		s := sch.Symbols[id]
		if s == nil || s.Key == "" || seen[s.Reference] || !keep(s.Reference) {
			continue
		}
		if b.FootprintByRef(s.Reference) != nil {
			continue
		}
		seen[s.Reference] = true
		keys[s.Reference] = s.Key
		maybe = append(maybe, s.Reference)
	}
	p.RUnlock()

	// The library has its own lock; keep it out of the project one.
	for _, ref := range maybe {
		if _, ok := p.FindLibrary(keys[ref]); ok {
			out = append(out, ref)
		}
	}
	return out
}

// bindPalette makes sure ref has a palette footprint, binding the symbol's
// key= on the fly. False means nothing names a footprint for ref.
func bindPalette(p *core.Project, ref string) bool {
	p.RLock()
	key := ""
	for _, fp := range p.Palette() {
		if fp.Reference == ref {
			p.RUnlock()
			return true
		}
	}
	for _, s := range p.Schematic().Symbols {
		if s != nil && s.Reference == ref {
			key = s.Key
			break
		}
	}
	p.RUnlock()
	if key == "" {
		return false
	}
	_, err := paletteCmd(p, ref+" "+quoteArg(key))
	return err == nil
}

// seatPending drops every bound-but-unplaced part onto a deterministic grid
// inside the outline, so the annealer has something to move. It only has to
// start each part somewhere reproducible; auto-place does the real work.
// Rotation and side stay as the binding left them (0 / top by default).
func seatPending(p *core.Project, verb string, refs []string) (int, error) {
	pending := pendingRefs(p, refs)
	if len(pending) == 0 {
		return 0, nil
	}
	var o core.Rect
	p.RLock()
	has := p.Board().Outline != nil
	if has {
		o = *p.Board().Outline
	}
	p.RUnlock()
	if !has {
		return 0, fmt.Errorf("%s: board has no outline — run `outline W H` first", verb)
	}

	cols := int(math.Ceil(math.Sqrt(float64(len(pending)))))
	rows := (len(pending) + cols - 1) / cols
	w, h := o.Width().ToMM(), o.Height().ToMM()
	mx, my := math.Min(2.0, w/10), math.Min(2.0, h/10)
	cw := math.Max(0, w-2*mx) / float64(cols)
	ch := math.Max(0, h-2*my) / float64(rows)

	n := 0
	for i, ref := range pending {
		if !bindPalette(p, ref) {
			continue
		}
		fp, ok := p.PaletteTake(ref)
		if !ok {
			continue
		}
		p.RLock()
		stampNets(p.Schematic(), fp)
		p.RUnlock()
		fp.Position = core.NewPoint(
			core.FromMM(o.Min.X.ToMM()+mx+(float64(i%cols)+0.5)*cw),
			core.FromMM(o.Min.Y.ToMM()+my+(float64(i/cols)+0.5)*ch),
		)
		p.MutateBoard(func(b *core.Board) { b.AddFootprint(fp) })
		n++
	}
	return n, nil
}

func packBoard(p *core.Project, args string) (string, error) {
	provider := "jlcpcb"
	out := ""
	for _, f := range strings.Fields(args) {
		if strings.HasPrefix(f, "fab=") && IsKiCadProvider(strings.TrimPrefix(f, "fab=")) {
			return packKiCad(p, args)
		}
	}
	for _, f := range strings.Fields(args) {
		if strings.HasPrefix(f, "fab=") {
			provider = strings.TrimPrefix(f, "fab=")
		} else if strings.HasPrefix(f, "out=") {
			out = strings.TrimPrefix(f, "out=")
		} else if strings.HasPrefix(f, "teardrop=") || strings.HasPrefix(f, "teardrops=") {
			v := strings.TrimPrefix(strings.TrimPrefix(f, "teardrops="), "teardrop=")
			on := v == "true" || v == "1" || v == "on"
			p.MutateBoard(func(b *core.Board) { b.Teardrops = on })
		} else if !strings.Contains(f, "=") && out == "" {
			out = f
		}
	}
	if out == "" {
		if sp := p.SavePath(); sp != "" {
			out = filepath.Join(filepath.Dir(sp), "fab")
		} else {
			out = filepath.Join(os.TempDir(), "fragua-fab")
		}
	}
	// out= names a directory. Given a .zip path — the natural thing to type —
	// pack used to create a *directory* called foo.zip and hide the real
	// archive inside it; take the parent instead.
	if strings.EqualFold(filepath.Ext(out), ".zip") {
		out = filepath.Dir(out)
	}
	res, err := fab.Pack(p, provider, out)
	if err != nil {
		return "", err
	}
	return fmt.Sprintf("packed %s (erc_err=%d drc_err=%d)", res.ZipPath, res.ERCErrors, res.DRCErrors), nil
}

func addNet(p *core.Project, args string) (string, error) {
	// net NAME REF.PIN REF.PIN ...
	fields := strings.Fields(args)
	if len(fields) < 2 {
		return "", fmt.Errorf("net NAME REF.PIN ...")
	}
	name := fields[0]
	var conns []core.NetConnection
	// `net NAME REF.PIN ... class=X` used to SKIP the class token and drop it on
	// the floor: the class was never stored, so `class signal width=0.20` had no
	// effect on any net that asked for it and every trace on the board came out
	// at the router default. It is silent, and it is the difference between a
	// 0.20 mm trace leaving a 0.24 mm DFN pad and a 0.25 mm one that cannot.
	className := ""
	p.Lock()
	defer p.Unlock()
	sch := p.Schematic()
	board := p.Board()
	for _, tok := range fields[1:] {
		if strings.HasPrefix(tok, "class=") {
			className = strings.TrimPrefix(tok, "class=")
			continue
		}
		parts := strings.SplitN(tok, ".", 2)
		if len(parts) != 2 {
			return "", fmt.Errorf("net: bad pin %q", tok)
		}
		ref, pin := parts[0], parts[1]
		pin = resolvePinAlias(sch, board, ref, pin) // U1.GND → U1.57
		var sid core.ID
		for _, s := range sch.Symbols {
			if s != nil && s.Reference == ref {
				sid = s.ID
				break
			}
		}
		if sid.IsZero() {
			if fp := board.FootprintByRef(ref); fp != nil {
				for i := range fp.Pads {
					if fp.Pads[i].Number == pin {
						n := name
						fp.Pads[i].Net = &n
					}
				}
				continue
			}
			return "", fmt.Errorf("net: unknown symbol %q", ref)
		}
		conns = append(conns, core.NetConnection{SymbolID: sid, PinNumber: pin})
		if fp := board.FootprintByRef(ref); fp != nil {
			for i := range fp.Pads {
				if fp.Pads[i].Number == pin {
					n := name
					fp.Pads[i].Net = &n
				}
			}
		}
	}
	if sch.Nets == nil {
		sch.Nets = make(map[string]*core.Net)
	}
	// `net` ACCUMULATES. A ground net on a module-heavy board does not fit on
	// one line, and every script in the wild splits it - but this used to
	// overwrite sch.Nets[name], so only the last line survived in the
	// schematic. The board side never had the bug (the loop above tags
	// fp.Pads[i].Net on every line, cumulatively), so the router and the pour
	// saw the whole net while ERC saw a fraction of it and reported dozens of
	// phantom `floating_pin` warnings on the pins that had been dropped.
	// Use `clear-net NAME` to genuinely start a net over.
	if prev := sch.Nets[name]; prev != nil {
		seen := make(map[core.NetConnection]bool, len(prev.Connections)+len(conns))
		merged := make([]core.NetConnection, 0, len(prev.Connections)+len(conns))
		for _, c := range append(append([]core.NetConnection{}, prev.Connections...), conns...) {
			if seen[c] {
				continue
			}
			seen[c] = true
			merged = append(merged, c)
		}
		conns = merged
	}
	if className == "" && sch.NetToClass != nil {
		className = sch.NetToClass[name]
	}
	if className == "" && sch.Nets[name] != nil {
		className = sch.Nets[name].Class
	}
	sch.Nets[name] = &core.Net{Name: name, Connections: conns, Class: className}
	if className != "" {
		if sch.NetToClass == nil {
			sch.NetToClass = map[string]string{}
		}
		sch.NetToClass[name] = className
	}
	p.Events().Publish(core.Event{Kind: core.EventSchematicChanged})
	if className != "" {
		return fmt.Sprintf("net %s (%d pins, class=%s)", name, len(conns), className), nil
	}
	return fmt.Sprintf("net %s (%d pins)", name, len(conns)), nil
}

func deleteTrace(p *core.Project, args string) (string, error) {
	id := strings.TrimSpace(args)
	if id == "" {
		return "", fmt.Errorf("delete-trace ID")
	}
	var n int
	p.MutateBoard(func(b *core.Board) {
		out := b.Traces[:0]
		for _, tr := range b.Traces {
			if tr.ID.String() != id {
				out = append(out, tr)
			} else {
				n++
			}
		}
		b.Traces = out
	})
	return fmt.Sprintf("deleted %d traces", n), nil
}

// ─── sym / lib ───────────────────────────────────────────────────────

func addSymbol(p *core.Project, args string, pins []core.SchPin) (string, error) {
	// sym REF KIND [key=K] [value=V] [rot=DEG] [x=N] [y=N] [desc=...]
	fields := tokenize(args)
	if len(fields) < 2 {
		return "", fmt.Errorf("sym needs: sym REF KIND [...key=value...]")
	}
	ref := fields[0]
	kind, err := expandKind(fields[1])
	if err != nil {
		return "", err
	}
	var key, value, desc, lcsc, mpn, mfr string
	var rot, xMM, yMM float64
	var hasPos bool
	for _, t := range fields[2:] {
		switch {
		case strings.HasPrefix(t, "key="):
			key = strings.TrimPrefix(t, "key=")
		case strings.HasPrefix(t, "value="):
			value = strings.TrimPrefix(t, "value=")
		case strings.HasPrefix(t, "rot="):
			fmt.Sscanf(t, "rot=%f", &rot)
		case strings.HasPrefix(t, "rotation="):
			fmt.Sscanf(t, "rotation=%f", &rot)
		case strings.HasPrefix(t, "x="):
			fmt.Sscanf(t, "x=%f", &xMM)
			hasPos = true
		case strings.HasPrefix(t, "y="):
			fmt.Sscanf(t, "y=%f", &yMM)
			hasPos = true
		case strings.HasPrefix(t, "desc="):
			desc = strings.TrimPrefix(t, "desc=")
		case strings.HasPrefix(t, "description="):
			desc = strings.TrimPrefix(t, "description=")
		case strings.HasPrefix(t, "lcsc="):
			lcsc = strings.TrimPrefix(t, "lcsc=")
		case strings.HasPrefix(t, "mpn="):
			mpn = strings.TrimPrefix(t, "mpn=")
		case strings.HasPrefix(t, "manufacturer="), strings.HasPrefix(t, "mfr="):
			if strings.HasPrefix(t, "mfr=") {
				mfr = strings.TrimPrefix(t, "mfr=")
			} else {
				mfr = strings.TrimPrefix(t, "manufacturer=")
			}
		}
	}
	if kind == "generic_ic" && len(pins) == 0 {
		return "", fmt.Errorf("sym: kind=generic_ic requires pin lines")
	}
	sk := core.SymbolKind{Kind: kind}
	if kind == "generic_ic" {
		sk.ICPins = pins
	}
	id := core.NewID()
	p.MutateSchematic(func(s *core.Schematic) {
		if s.Symbols == nil {
			s.Symbols = make(map[string]*core.Symbol)
		}
		// auto grid if no position
		n := float64(len(s.Symbols))
		if !hasPos {
			row := float64(int(n) / 6)
			col := n - row*6
			xMM = 15 + col*25
			yMM = 15 + row*20
		}
		sym := &core.Symbol{
			ID: id, Reference: ref, Value: value, Kind: sk,
			Position: core.NewPoint(core.FromMM(xMM), core.FromMM(yMM)),
			Rotation: rot, Key: key, Description: desc,
			LcscID: lcsc, MPN: mpn, Manufacturer: mfr,
		}
		// replace existing by reference
		for k, old := range s.Symbols {
			if old != nil && old.Reference == ref {
				delete(s.Symbols, k)
				break
			}
		}
		s.Symbols[id.String()] = sym
		// keep order unique by ref
		out := s.SymbolOrder[:0]
		for _, oid := range s.SymbolOrder {
			if old := s.Symbols[oid]; old != nil && old.Reference != ref {
				out = append(out, oid)
			}
		}
		s.SymbolOrder = append(out, id.String())
	})
	return fmt.Sprintf("Added %s (%s)", ref, id.String()), nil
}

func addLibrary(p *core.Project, args string, pads []core.LibraryPad) (string, error) {
	// lib KEY [value=V] [rot=DEG] [edge=true|false] [elevated=true] [desc=...]
	fields := tokenize(args)
	if len(fields) < 1 {
		return "", fmt.Errorf("lib needs: lib KEY [...]")
	}
	key := fields[0]
	if len(pads) == 0 {
		return "", fmt.Errorf("lib %s needs at least one indented `pad ...` line", key)
	}
	entry := core.LibraryEntry{
		Key:         key,
		Pads:        pads,
		Attachments: []core.Attachment{},
		Silk:        []core.LibrarySilk{},
	}
	for _, t := range fields[1:] {
		switch {
		case strings.HasPrefix(t, "value="):
			entry.DefaultValue = strings.TrimPrefix(t, "value=")
		case strings.HasPrefix(t, "default_value="):
			entry.DefaultValue = strings.TrimPrefix(t, "default_value=")
		case strings.HasPrefix(t, "rot="):
			var r float64
			fmt.Sscanf(t, "rot=%f", &r)
			entry.DefaultRotationDeg = float32(r)
		case strings.HasPrefix(t, "edge="):
			entry.EdgeMounted = strings.EqualFold(strings.TrimPrefix(t, "edge="), "true")
		case strings.HasPrefix(t, "elevated="):
			entry.Elevated = strings.EqualFold(strings.TrimPrefix(t, "elevated="), "true")
		case strings.HasPrefix(t, "desc="):
			entry.Description = strings.TrimPrefix(t, "desc=")
		case strings.HasPrefix(t, "description="):
			entry.Description = strings.TrimPrefix(t, "description=")
		case strings.HasPrefix(t, "lcsc="):
			s := strings.TrimPrefix(t, "lcsc=")
			entry.LcscID = &s
		case strings.HasPrefix(t, "mpn="):
			s := strings.TrimPrefix(t, "mpn=")
			entry.MPN = &s
		case strings.HasPrefix(t, "manufacturer="), strings.HasPrefix(t, "mfr="):
			s := strings.TrimPrefix(t, "manufacturer=")
			s = strings.TrimPrefix(s, "mfr=")
			entry.Manufacturer = &s
		case strings.HasPrefix(t, "model="):
			entry.Model = strings.TrimPrefix(t, "model=")
		}
	}
	if _, err := p.PutLibrary(entry); err != nil {
		return "", err
	}
	return fmt.Sprintf("lib %s (%d pads)", key, len(pads)), nil
}

// ─── palette ─────────────────────────────────────────────────────────

func paletteCmd(p *core.Project, args string) (string, error) {
	fields := tokenize(args)
	if len(fields) == 0 {
		return "", fmt.Errorf("palette REF KEY | palette list")
	}
	if strings.EqualFold(fields[0], "list") {
		p.RLock()
		defer p.RUnlock()
		items := p.Palette()
		if len(items) == 0 {
			return "0 palette item(s)", nil
		}
		var b strings.Builder
		fmt.Fprintf(&b, "%d palette item(s):", len(items))
		for _, fp := range items {
			fmt.Fprintf(&b, "\n  %s key=%s pads=%d", fp.Reference, fp.Key, len(fp.Pads))
		}
		return b.String(), nil
	}
	// palette REF KEY [rot=] [value=] [layer=top|bottom]
	if len(fields) < 2 {
		return "", fmt.Errorf("palette REF KEY [rot=...] [value=...]")
	}
	ref, key := fields[0], fields[1]
	var value, model string
	rot := 0.0
	layerTok := "Top"
	for _, t := range fields[2:] {
		switch {
		case strings.HasPrefix(t, "rot="):
			fmt.Sscanf(t, "rot=%f", &rot)
		case strings.HasPrefix(t, "rotation="):
			fmt.Sscanf(t, "rotation=%f", &rot)
		case strings.HasPrefix(t, "value="):
			value = strings.TrimPrefix(t, "value=")
		case strings.HasPrefix(t, "layer="):
			layerTok = strings.TrimPrefix(t, "layer=")
		case strings.HasPrefix(t, "model="):
			model = strings.TrimPrefix(t, "model=")
		}
	}

	p.Lock()
	layer := parseLayerTokenOn(layerTok, p.Board().StackupOrDefault())
	entry, found := p.FindLibrary(key)
	sch := p.Schematic()
	var sym *core.Symbol
	for _, s := range sch.Symbols {
		if s != nil && s.Reference == ref {
			sym = s
			break
		}
	}
	// freeform: no library entry → build pads from symbol kind / pin count
	if !found {
		if strings.EqualFold(key, "freeform") || key == "" {
			var pads []core.LibraryPad
			if sym != nil {
				switch strings.ToLower(sym.Kind.Kind) {
				case "resistor", "capacitor", "inductor", "led", "diode":
					pads = core.ResistorCapPads()
				default:
					n := len(sym.Kind.Pins())
					if n == 0 {
						n = 2
					}
					pads = core.FreeformPads(n)
					// match pin numbers from schematic when possible
					pins := sym.Kind.Pins()
					for i := range pads {
						if i < len(pins) {
							pads[i].Number = pins[i].Number
							pads[i].Name = pins[i].Name
						}
					}
				}
			} else {
				pads = core.ResistorCapPads()
			}
			fk := key
			if fk == "" || strings.EqualFold(fk, "freeform") {
				fk = "freeform"
			}
			entry = core.LibraryEntry{Key: fk, Pads: pads}
		} else {
			p.Unlock()
			return "", fmt.Errorf("palette: no library entry with key %s", key)
		}
	}
	// stamp nets from schematic
	if value == "" && sym != nil {
		value = sym.Value
	}
	if rot == 0 && entry.DefaultRotationDeg != 0 {
		rot = float64(entry.DefaultRotationDeg)
	}
	desc := entry.Description
	if sym != nil && sym.Description != "" {
		desc = sym.Description
	}
	p.Unlock()

	fp := entry.ToFootprint(ref, value, layer, rot)
	fp.Description = desc
	if model != "" {
		fp.Model = model
	}
	if sym != nil && sym.LcscID == "" && isValuePart(sym.Kind.Kind) && (entry.DefaultValue == "" || !sameValue(fp.Value, entry.DefaultValue)) {
		// A library entry's LCSC id names ONE part. For a passive that is one
		// value, so unless the entry pins that value (value=) and the symbol
		// agrees, the symbol must not inherit it: a shared r_0603 with
		// lcsc=C25804 (10k) used to stamp C25804 on the 2.2k pull-ups too, and
		// the BOM would have sent 10k to assembly. Empty cell over invention.
		fp.LcscID, fp.MPN, fp.Manufacturer = "", "", ""
	}
	if sym != nil {
		if sym.LcscID != "" {
			fp.LcscID = sym.LcscID
		}
		if sym.MPN != "" {
			fp.MPN = sym.MPN
		}
		if sym.Manufacturer != "" {
			fp.Manufacturer = sym.Manufacturer
		}
		if fp.Value == "" && sym.Value != "" {
			fp.Value = sym.Value
		}
	}
	stampNets(sch, fp)
	p.PaletteAdd(*fp)
	return fmt.Sprintf("Spawned %s from %s", ref, key), nil
}

// stampNets tags fp's pads with the nets its schematic symbol sits on, by pad
// number and then by pad name. palette does this when it binds, but a `net`
// line only reaches footprints already on the *board* — so a part bound first
// and seated later has to pick its nets up here or it routes to nothing.
func stampNets(sch *core.Schematic, fp *core.Footprint) {
	var sym *core.Symbol
	for _, s := range sch.Symbols {
		if s != nil && s.Reference == fp.Reference {
			sym = s
			break
		}
	}
	if sym == nil {
		return
	}
	netForPin := func(pin string) *string {
		for _, net := range sch.Nets {
			if net == nil {
				continue
			}
			for _, c := range net.Connections {
				if c.SymbolID == sym.ID && c.PinNumber == pin {
					n := net.Name
					return &n
				}
			}
		}
		return nil
	}
	for i := range fp.Pads {
		if n := netForPin(fp.Pads[i].Number); n != nil {
			fp.Pads[i].Net = n
		} else if fp.Pads[i].Name != "" {
			if n := netForPin(fp.Pads[i].Name); n != nil {
				fp.Pads[i].Net = n
			}
		}
	}
}

// ─── trace / via ─────────────────────────────────────────────────────

func addTrace(p *core.Project, args string) (string, error) {
	// Supported:
	//   trace NET x1 y1 x2 y2 [layer=Top|Bottom] [width=0.15]
	//   trace LAYER NET x1 y1 x2 y2 [width=N]   (Rust form)
	fields := tokenize(args)
	if len(fields) < 5 {
		return "", fmt.Errorf("trace NET x1 y1 x2 y2 [layer=...] [width=...]")
	}
	layerTok := "Top"
	width := 0.15
	var net string
	var x1, y1, x2, y2 float64
	var err error

	// Detect Rust form: first token is layer name and second is net
	if isLayerToken(fields[0]) && len(fields) >= 6 {
		layerTok = fields[0]
		net = fields[1]
		x1, err = strconv.ParseFloat(fields[2], 64)
		if err != nil {
			return "", fmt.Errorf("trace x1: %w", err)
		}
		y1, err = strconv.ParseFloat(fields[3], 64)
		if err != nil {
			return "", fmt.Errorf("trace y1: %w", err)
		}
		x2, err = strconv.ParseFloat(fields[4], 64)
		if err != nil {
			return "", fmt.Errorf("trace x2: %w", err)
		}
		y2, err = strconv.ParseFloat(fields[5], 64)
		if err != nil {
			return "", fmt.Errorf("trace y2: %w", err)
		}
		width = 0.25
		for _, t := range fields[6:] {
			if strings.HasPrefix(t, "width=") {
				fmt.Sscanf(t, "width=%f", &width)
			}
		}
	} else {
		net = fields[0]
		x1, err = strconv.ParseFloat(fields[1], 64)
		if err != nil {
			return "", fmt.Errorf("trace x1: %w", err)
		}
		y1, err = strconv.ParseFloat(fields[2], 64)
		if err != nil {
			return "", fmt.Errorf("trace y1: %w", err)
		}
		x2, err = strconv.ParseFloat(fields[3], 64)
		if err != nil {
			return "", fmt.Errorf("trace x2: %w", err)
		}
		y2, err = strconv.ParseFloat(fields[4], 64)
		if err != nil {
			return "", fmt.Errorf("trace y2: %w", err)
		}
		for _, t := range fields[5:] {
			if strings.HasPrefix(t, "layer=") {
				layerTok = strings.TrimPrefix(t, "layer=")
			} else if strings.HasPrefix(t, "width=") {
				fmt.Sscanf(t, "width=%f", &width)
			}
		}
	}
	id := core.NewID()
	layerName := ""
	p.MutateBoard(func(b *core.Board) {
		stack := b.StackupOrDefault()
		layer := parseLayerTokenOn(layerTok, stack)
		layerName = stack.LayerName(int(layer.Index))
		b.Traces = append(b.Traces, core.Trace{
			ID: id, Layer: layer, Net: net, Width: core.FromMM(width),
			Start: core.NewPoint(core.FromMM(x1), core.FromMM(y1)),
			End:   core.NewPoint(core.FromMM(x2), core.FromMM(y2)),
		})
	})
	return fmt.Sprintf("trace %s on %s (%s)", id.String(), layerName, net), nil
}

func addVia(p *core.Project, args string) (string, error) {
	// via NET x y [drill=0.3] [dia=0.6] [diameter=0.6]
	fields := tokenize(args)
	if len(fields) < 3 {
		return "", fmt.Errorf("via NET x y [drill=0.3] [dia=0.6]")
	}
	net := fields[0]
	x, err1 := strconv.ParseFloat(fields[1], 64)
	y, err2 := strconv.ParseFloat(fields[2], 64)
	if err1 != nil || err2 != nil {
		return "", fmt.Errorf("via: bad coordinates")
	}
	drill, dia := 0.3, 0.6
	p.RLock()
	if fab := core.ActiveFabRules(p.Board()); fab.MinViaDrillMM > 0 {
		drill = fab.MinViaDrillMM
		if fab.MinViaDiameterMM > 0 {
			dia = fab.MinViaDiameterMM
		}
	}
	p.RUnlock()
	for _, t := range fields[3:] {
		switch {
		case strings.HasPrefix(t, "drill="):
			fmt.Sscanf(t, "drill=%f", &drill)
		case strings.HasPrefix(t, "dia="):
			fmt.Sscanf(t, "dia=%f", &dia)
		case strings.HasPrefix(t, "diameter="):
			fmt.Sscanf(t, "diameter=%f", &dia)
		}
	}
	id := core.NewID()
	p.MutateBoard(func(b *core.Board) {
		b.Vias = append(b.Vias, core.Via{
			ID: id, Net: net,
			Position: core.NewPoint(core.FromMM(x), core.FromMM(y)),
			Drill:    core.FromMM(drill),
			Diameter: core.FromMM(dia),
		})
	})
	return fmt.Sprintf("via %s (%s)", id.String(), net), nil
}

// ─── rule-area / fab-rules ───────────────────────────────────────────

func addRuleArea(p *core.Project, args string) (string, error) {
	// rule-area NAME X1 Y1 X2 Y2 [clearance=N] [width=N] [via_drill=N] [via_dia=N] [priority=N]
	fields := tokenize(args)
	if len(fields) < 5 {
		return "", fmt.Errorf("rule-area NAME X1 Y1 X2 Y2 [clearance=N] ...")
	}
	name := fields[0]
	x1, e1 := strconv.ParseFloat(fields[1], 64)
	y1, e2 := strconv.ParseFloat(fields[2], 64)
	x2, e3 := strconv.ParseFloat(fields[3], 64)
	y2, e4 := strconv.ParseFloat(fields[4], 64)
	if e1 != nil || e2 != nil || e3 != nil || e4 != nil {
		return "", fmt.Errorf("rule-area: bad coordinates")
	}
	area := core.RuleArea{
		ID:   core.NewID(),
		Name: name,
		Rect: core.RectFromCorners(
			core.NewPoint(core.FromMM(x1), core.FromMM(y1)),
			core.NewPoint(core.FromMM(x2), core.FromMM(y2)),
		),
	}
	for _, t := range fields[5:] {
		switch {
		case strings.HasPrefix(t, "clearance="):
			var v float64
			fmt.Sscanf(t, "clearance=%f", &v)
			area.ClearanceMM = &v
		case strings.HasPrefix(t, "width="):
			var v float64
			fmt.Sscanf(t, "width=%f", &v)
			area.TraceWidthMM = &v
		case strings.HasPrefix(t, "via_drill="):
			var v float64
			fmt.Sscanf(t, "via_drill=%f", &v)
			area.ViaDrillMM = &v
		case strings.HasPrefix(t, "via_dia="):
			var v float64
			fmt.Sscanf(t, "via_dia=%f", &v)
			area.ViaDiameterMM = &v
		case strings.HasPrefix(t, "priority="):
			fmt.Sscanf(t, "priority=%d", &area.Priority)
		}
	}
	if area.ClearanceMM == nil && area.TraceWidthMM == nil &&
		area.ViaDrillMM == nil && area.ViaDiameterMM == nil {
		return "", fmt.Errorf("rule-area: set at least one of clearance/width/via_drill/via_dia")
	}
	// replace same name
	p.MutateBoard(func(b *core.Board) {
		out := b.RuleAreas[:0]
		for _, a := range b.RuleAreas {
			if a.Name != name {
				out = append(out, a)
			}
		}
		b.RuleAreas = append(out, area)
	})
	cl := "—"
	if area.ClearanceMM != nil {
		cl = fmt.Sprintf("%.3f", *area.ClearanceMM)
	}
	return fmt.Sprintf("rule-area %s clearance=%s mm", name, cl), nil
}

func escapeCmd(p *core.Project, args string) (string, error) {
	fields := tokenize(args)
	if len(fields) < 1 {
		return "", fmt.Errorf("escape via-in-pad REF.PAD | via-in-pad-stranded [on|off] | list")
	}
	switch strings.ToLower(fields[0]) {
	case "list":
		p.RLock()
		defer p.RUnlock()
		b := p.Board()
		var s strings.Builder
		if b.AutoViaInPadStranded {
			s.WriteString("auto via-in-pad-stranded: on\n")
		}
		for _, e := range b.EscapeExceptions {
			fmt.Fprintf(&s, "  %s.%s %s\n", e.Ref, e.Pad, e.Mode)
		}
		if s.Len() == 0 {
			return "escape: (none)", nil
		}
		return strings.TrimSpace(s.String()), nil
	case "via-in-pad-stranded", "via_in_pad_stranded":
		on := true
		if len(fields) > 1 && (fields[1] == "off" || fields[1] == "false" || fields[1] == "0") {
			on = false
		}
		p.MutateBoard(func(b *core.Board) { b.AutoViaInPadStranded = on })
		if on {
			return "escape: via-in-pad-stranded on", nil
		}
		return "escape: via-in-pad-stranded off", nil
	case "via-in-pad", "via_in_pad":
		if len(fields) < 2 {
			return "", fmt.Errorf("escape via-in-pad REF.PAD")
		}
		ref, pad := fields[1], ""
		if i := strings.IndexByte(fields[1], '.'); i >= 0 {
			ref, pad = fields[1][:i], fields[1][i+1:]
		}
		p.MutateBoard(func(b *core.Board) {
			for _, e := range b.EscapeExceptions {
				if strings.EqualFold(e.Ref, ref) && e.Pad == pad && e.Mode == core.EscapeViaInPad {
					return
				}
			}
			b.EscapeExceptions = append(b.EscapeExceptions, core.EscapeException{
				Ref: ref, Pad: pad, Mode: core.EscapeViaInPad,
			})
		})
		if pad == "" {
			return fmt.Sprintf("escape: via-in-pad %s (all pads)", ref), nil
		}
		return fmt.Sprintf("escape: via-in-pad %s.%s", ref, pad), nil
	default:
		return "", fmt.Errorf("escape: unknown %q (via-in-pad|via-in-pad-stranded|list)", fields[0])
	}
}

func setFabRules(p *core.Project, args string) (string, error) {
	// fab-rules PRESET | clear | list
	fields := strings.Fields(args)
	if len(fields) < 1 {
		return "", fmt.Errorf("fab-rules jlcpcb|jlcpcb-2l-via02|jlcpcb-4l|clear|list")
	}
	key := strings.ToLower(fields[0])
	switch key {
	case "clear", "none":
		p.MutateBoard(func(b *core.Board) { b.FabRules = nil })
		return "fab rules cleared", nil
	case "list":
		return "fab-rules presets: jlcpcb-2l (via 0.30/0.60 standard), jlcpcb-2l-via02, jlcpcb-4l, jlcpcb-4l-via02", nil
	}
	rules := core.FabRulesPreset(key)
	if rules == nil {
		return "", fmt.Errorf("fab-rules: unknown preset %q (have jlcpcb-2l, jlcpcb-2l-via02, jlcpcb-4l, jlcpcb-4l-via02)", fields[0])
	}
	p.MutateBoard(func(b *core.Board) { b.FabRules = rules })
	// also set session profile for pack/drc
	maxSz := [2]float64{100, 100}
	if rules.MaxBoardSizeMM != nil {
		maxSz = *rules.MaxBoardSizeMM
	}
	p.SetFabProfile(&core.FabProfileHandle{
		Name: rules.Preset, MinTraceWidthMM: rules.MinTraceWidthMM,
		MinClearanceMM: rules.MinClearanceMM, MinDrillMM: rules.MinViaDrillMM,
		MinAnnularRingMM: rules.MinAnnularRingMM, MinViaDiameterMM: rules.MinViaDiameterMM,
		MinEdgeClearanceMM: rules.MinEdgeClearanceMM,
		MinHoleToHoleMM:    rules.MinHoleToHoleMM, MinSliverMM: rules.MinSliverMM,
		MaxBoardSizeMM: maxSz,
	})
	return fmt.Sprintf(
		"fab rules `%s`: trace %.3f mm, space %.3f mm, via drill %.3f mm, via dia %.3f mm",
		rules.Preset, rules.MinTraceWidthMM, rules.MinClearanceMM, rules.MinViaDrillMM, rules.MinViaDiameterMM,
	), nil
}

// ─── layer ───────────────────────────────────────────────────────────

func layerCmd(p *core.Project, args string) (string, error) {
	fields := tokenize(args)
	if len(fields) < 1 {
		return "", fmt.Errorf("layer list|add|remove|rename ...")
	}
	switch strings.ToLower(fields[0]) {
	case "list":
		p.RLock()
		defer p.RUnlock()
		s := p.Board().StackupOrDefault()
		var b strings.Builder
		fmt.Fprintf(&b, "%d copper layer(s), %.2f mm stack:", len(s.Layers), s.TotalThicknessMM())
		for i, ls := range s.Layers {
			kind := string(ls.Kind)
			if kind == "" {
				kind = "signal"
			}
			name := s.LayerName(i)
			oz := s.CopperOz(i)
			fmt.Fprintf(&b, "\n  [%d] %s (%s, %.0f oz)", i, name, kind, oz)
			if ls.AssignedNet != "" {
				fmt.Fprintf(&b, " net=%s", ls.AssignedNet)
			}
		}
		if len(s.Dielectrics) > 0 {
			b.WriteString("\n  dielectric:")
			for i, d := range s.Dielectrics {
				if i > 0 {
					b.WriteString(" /")
				}
				fmt.Fprintf(&b, " %.3f mm", d.ThicknessMM)
				if d.Er > 0 {
					fmt.Fprintf(&b, " (er=%.1f)", d.Er)
				}
			}
		}
		return b.String(), nil
	case "add":
		// layer add NAME (signal|plane|mixed) [thickness=N]
		if len(fields) < 3 {
			return "", fmt.Errorf("layer add NAME (signal|plane|mixed) [thickness=N]")
		}
		name, kindStr := fields[1], strings.ToLower(fields[2])
		kind := core.LayerKindSignal
		switch kindStr {
		case "signal":
			kind = core.LayerKindSignal
		case "plane", "power":
			kind = core.LayerKindPower
		case "mixed":
			kind = core.LayerKindMixed
		default:
			return "", fmt.Errorf("layer add: unknown kind %q", fields[2])
		}
		thickness := 0.035
		dielH, dielEr := 0.0, 0.0
		for _, t := range fields[3:] {
			if strings.HasPrefix(t, "thickness=") {
				fmt.Sscanf(t, "thickness=%f", &thickness)
			}
			if strings.HasPrefix(t, "dielectric=") {
				fmt.Sscanf(t, "dielectric=%f", &dielH)
			}
			if strings.HasPrefix(t, "er=") {
				fmt.Sscanf(t, "er=%f", &dielEr)
			}
		}
		p.MutateBoard(func(b *core.Board) {
			s := b.StackupOrDefault()
			slab := core.Dielectric{ThicknessMM: 1.5, Er: 4.6}
			if len(s.Dielectrics) > 0 {
				var sumT, sumE float64
				for _, d := range s.Dielectrics {
					sumT += d.ThicknessMM
					sumE += d.Er
				}
				n := float64(len(s.Dielectrics))
				slab = core.Dielectric{ThicknessMM: sumT / n, Er: sumE / n}
				if slab.Er <= 0 {
					slab.Er = 4.6
				}
			}
			if dielH > 0 {
				slab.ThicknessMM = dielH
			}
			if dielEr > 0 {
				slab.Er = dielEr
			}
			oz := thickness / 0.035
			if oz <= 0 {
				oz = 1
			}
			s.PushLayer(core.LayerSpec{
				Name: name, Kind: kind, CopperWeightOz: oz, ThicknessUM: thickness * 1000,
			}, slab)
			b.Stackup = &s
		})
		return fmt.Sprintf("added layer `%s`", name), nil
	case "remove":
		if len(fields) < 2 {
			return "", fmt.Errorf("layer remove NAME")
		}
		name := fields[1]
		var err error
		p.MutateBoard(func(b *core.Board) {
			s := b.StackupOrDefault()
			target, ok := s.FindLayerByName(name)
			if !ok {
				err = fmt.Errorf("layer.remove: no layer named `%s`", name)
				return
			}
			uses := 0
			for _, fp := range b.Footprints {
				if fp == nil {
					continue
				}
				if fp.Layer == target {
					uses++
				}
				for _, pad := range fp.Pads {
					if pad.Layer == target {
						uses++
					}
				}
			}
			for _, tr := range b.Traces {
				if tr.Layer == target {
					uses++
				}
			}
			for _, pour := range b.Pours {
				if pour.Layer == target {
					uses++
				}
			}
			if uses > 0 {
				err = fmt.Errorf("layer.remove: %d item(s) still on layer `%s`", uses, name)
				return
			}
			if !s.RemoveNamed(name) {
				err = fmt.Errorf("layer.remove: refused to remove `%s` (would leave fewer than 2 layers)", name)
				return
			}
			b.Stackup = &s
		})
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("removed layer `%s`", name), nil
	case "rename":
		if len(fields) < 3 {
			return "", fmt.Errorf("layer rename OLD NEW")
		}
		oldN, newN := fields[1], fields[2]
		var err error
		p.MutateBoard(func(b *core.Board) {
			s := b.StackupOrDefault()
			found := false
			for i := range s.Layers {
				if s.Layers[i].Name == oldN {
					s.Layers[i].Name = newN
					found = true
					break
				}
			}
			if !found {
				err = fmt.Errorf("layer.rename: no layer named `%s`", oldN)
				return
			}
			b.Stackup = &s
		})
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("renamed layer `%s` → `%s`", oldN, newN), nil
	case "4", "4l", "4layer":
		p.MutateBoard(func(b *core.Board) { b.Apply4Layer() })
		return "stackup: 4-layer (F.Cu / In1.Cu / In2.Cu / B.Cu)", nil
	case "dielectric":
		// layer dielectric [i] [thickness=N] [er=N]
		idx := 0
		th, er := 0.0, 0.0
		for _, t := range fields[1:] {
			if strings.HasPrefix(t, "thickness=") {
				fmt.Sscanf(t, "thickness=%f", &th)
				continue
			}
			if strings.HasPrefix(t, "er=") {
				fmt.Sscanf(t, "er=%f", &er)
				continue
			}
			fmt.Sscanf(t, "%d", &idx)
		}
		var err error
		p.MutateBoard(func(b *core.Board) {
			s := b.StackupOrDefault()
			if idx < 0 || idx >= len(s.Dielectrics) {
				err = fmt.Errorf("layer dielectric: index %d out of range (%d slabs)", idx, len(s.Dielectrics))
				return
			}
			if th > 0 {
				s.Dielectrics[idx].ThicknessMM = th
			}
			if er > 0 {
				s.Dielectrics[idx].Er = er
			}
			if s.Dielectrics[idx].ThicknessMM <= 0 || s.Dielectrics[idx].Er <= 0 {
				err = fmt.Errorf("layer dielectric: thickness and Er must both be set (got H=%.3f Er=%.2f)",
					s.Dielectrics[idx].ThicknessMM, s.Dielectrics[idx].Er)
				return
			}
			b.Stackup = &s
		})
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("dielectric[%d] updated", idx), nil
	default:
		return "", fmt.Errorf("layer: unknown subcommand `%s` (list|add|remove|rename|dielectric|4layer)", fields[0])
	}
}

// ─── screenshot ──────────────────────────────────────────────────────

func screenshot(p *core.Project, args string) (string, error) {
	// screenshot PATH [view=board|schematic] [width=PX]
	fields := tokenize(args)
	if len(fields) < 1 {
		return "", fmt.Errorf("screenshot PATH [view=board|schematic]")
	}
	path := fields[0]
	view := "board"
	for _, t := range fields[1:] {
		if strings.HasPrefix(t, "view=") {
			view = strings.ToLower(strings.TrimPrefix(t, "view="))
		} else if strings.HasPrefix(t, "path=") {
			path = strings.TrimPrefix(t, "path=")
		}
	}
	if path == "" {
		return "", fmt.Errorf("screenshot: path is empty")
	}
	p.RLock()
	var content string
	switch view {
	case "board", "":
		content = render.BoardSVG(p.Board())
	case "schematic":
		content = render.SchematicSVG(p.Schematic())
	default:
		p.RUnlock()
		return "", fmt.Errorf("screenshot: unknown view %q (use board or schematic)", view)
	}
	p.RUnlock()

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil && filepath.Dir(path) != "." {
		return "", fmt.Errorf("screenshot: mkdir: %w", err)
	}
	// Write SVG; if path ends with .png still write SVG bytes with a note
	// (PNG pipeline not wired yet). Agents can use SVG.
	outPath := path
	if strings.HasSuffix(strings.ToLower(path), ".png") {
		outPath = strings.TrimSuffix(path, filepath.Ext(path)) + ".svg"
	}
	if err := os.WriteFile(outPath, []byte(content), 0o644); err != nil {
		return "", fmt.Errorf("screenshot: write %s: %w", outPath, err)
	}
	return fmt.Sprintf("screenshot: wrote %s (%d bytes, view=%s)", outPath, len(content), view), nil
}

// ─── helpers ─────────────────────────────────────────────────────────

func tokenize(s string) []string {
	// split on whitespace; support key="quoted value"
	var out []string
	s = strings.TrimSpace(s)
	for len(s) > 0 {
		if s[0] == '"' {
			end := 1
			for end < len(s) && s[end] != '"' {
				if s[end] == '\\' && end+1 < len(s) {
					end += 2
					continue
				}
				end++
			}
			if end >= len(s) {
				out = append(out, s[1:])
				break
			}
			out = append(out, s[1:end])
			s = strings.TrimSpace(s[end+1:])
			continue
		}
		// token until space; if key="..." keep value
		i := 0
		for i < len(s) && s[i] != ' ' && s[i] != '\t' {
			if s[i] == '=' && i+1 < len(s) && s[i+1] == '"' {
				// key="..."
				j := i + 2
				for j < len(s) && s[j] != '"' {
					if s[j] == '\\' && j+1 < len(s) {
						j += 2
						continue
					}
					j++
				}
				if j < len(s) {
					// unquote into token
					key := s[:i]
					val := s[i+2 : j]
					out = append(out, key+"="+val)
					s = strings.TrimSpace(s[j+1:])
					i = -1
					break
				}
			}
			i++
		}
		if i < 0 {
			continue
		}
		out = append(out, s[:i])
		s = strings.TrimSpace(s[i:])
	}
	return out
}

func expandKind(s string) (string, error) {
	switch strings.ToLower(s) {
	case "ic", "generic_ic", "genericic":
		return "generic_ic", nil
	case "r", "resistor":
		return "resistor", nil
	case "c", "capacitor":
		return "capacitor", nil
	case "l", "inductor":
		return "inductor", nil
	case "led":
		return "led", nil
	case "d", "diode":
		return "diode", nil
	default:
		return "", fmt.Errorf("kind: expected ic/r/c/l/led/d (or full names), got %q", s)
	}
}

func expandSide(s string) core.PinSide {
	switch strings.ToUpper(s) {
	case "L", "LEFT":
		return core.PinLeft
	case "R", "RIGHT":
		return core.PinRight
	case "T", "TOP":
		return core.PinTop
	case "B", "BOTTOM":
		return core.PinBottom
	default:
		return core.PinLeft
	}
}

func expandRole(s string) core.PinRole {
	switch strings.ToLower(s) {
	case "passive":
		return core.PinPassive
	case "input", "in":
		return core.PinInput
	case "output", "out":
		return core.PinOutput
	case "bidir", "io":
		return core.PinBidir
	case "power_out", "power-out":
		return core.PinPowerOut
	case "power_in", "power-in", "power":
		return core.PinPowerIn
	case "nc", "no_connect", "no-connect", "unused":
		return core.PinNC
	default:
		return core.PinPassive
	}
}

func isLayerToken(s string) bool {
	switch strings.ToLower(s) {
	case "top", "bottom", "f.cu", "b.cu":
		return true
	default:
		return strings.HasPrefix(strings.ToLower(s), "in")
	}
}

// parseLayerTokenOn resolves a layer token against the board stackup, so
// `layer=In1.Cu` / `layer=B.Cu` land on the right copper whatever the stack
// depth. Unknown tokens fall back to the top layer, as they always have.
func parseLayerTokenOn(s string, stack core.LayerStackup) core.Layer {
	if l, ok := stack.ResolveLayerName(s); ok {
		return l
	}
	return core.LayerTop
}

// isValuePart reports whether a symbol kind's identity is its value (a 10k
// resistor and a 2.2k resistor are different parts on the same footprint).
func isValuePart(kind string) bool {
	switch strings.ToLower(kind) {
	case "resistor", "capacitor", "inductor":
		return true
	}
	return false
}

// sameValue compares part values loosely: case, spaces and the ohm sign do
// not make a different part; an empty symbol value takes the entry default.
func sameValue(symValue, entryDefault string) bool {
	norm := func(s string) string {
		s = strings.ToLower(strings.TrimSpace(s))
		return strings.NewReplacer("Ω", "", "ohm", "", " ", "").Replace(s)
	}
	na, nb := norm(symValue), norm(entryDefault)
	return na == "" || na == nb
}
