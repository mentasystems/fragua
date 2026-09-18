// Package router implements multi-layer grid autorouting (A* multi-source tree).
//
// Process approximates Rust pcb_router Grid engine defaults:
//  1. Collect multi-pad nets, stable sort (fewest pads first, then name)
//  2. Stamp foreign pads/traces/vias inflated by clearance (ceil quant guard)
//  3. Multi-source tree growth (Prim-style from same-net copper)
//  4. Light RR&R, then negotiated congestion on leftovers
//  5. Organic string-pull post-pass when Options.Organic (default true)
//  6. Per-net Ok/Failed with length_mm and lower_bound_mm
package router

import (
	"container/heap"
	"fmt"
	"math"
	"sort"
	"strings"
	"time"

	"github.com/mentasystems/fragua/internal/core"
)

// Options configures a route run.
// Defaults match Rust pcb_router::RouteOptions::default.
type Options struct {
	CellMM          float64
	ClearanceMM     float64
	TraceWidthMM    float64
	ViaDrillMM      float64
	ViaDiameterMM   float64
	ViaCost         float64
	MaxSeconds      float64 // wall budget in seconds; normalised by ClampBudget
	Organic         bool
	OrganicFilletMM float64 // reserved; string-pull only in this pass
	FineEscape      bool    // opt-in; ignored (not implemented)
	Negotiate       bool    // extra negotiation rounds on leftovers
	SearchClearMM   float64 // pad-edge search gap; 0 → fab ceiling
	Teardrops       bool    // add copper teardrops at pad/via junctions
	TeardropsSet    bool    // true if teardrop= was in the script args
	// Engine selects the search: "grid" (default Theta*/RR&R) or "topo"
	// (Delaunay homotopy, rust 4e20ae0 port). Empty means grid. Topo is
	// opt-in so the default agent path cannot regress.
	Engine string
	// ClearanceSet marks an explicit clearance= from the script args. The
	// fab floor still wins when the request is tighter than the process
	// allows, but a caller asking for more air than the fab minimum gets it
	// — without this flag the fab ceiling silently overwrote the request.
	ClearanceSet bool
	// Schematic supplies the net classes that drive per-net trace width
	// (impedance target, then class width). nil → TraceWidthMM everywhere.
	Schematic *core.Schematic
	// Progress, when set, is called once per net attempted so a host can
	// stream "nets done / total" to a watching human. Must not block.
	Progress func(Progress)
	// Cancel, when set, stops the run at the next net boundary. The engine
	// is anytime, so a cancelled run keeps the copper already committed —
	// same contract as running out of clock.
	Cancel func() bool

	// widths is derived from Schematic + stackup inside Route.
	widths *netWidths
	// neck is the escape plan of the net being routed: near its own pads the
	// trace may narrow so a controlled-impedance width can leave a fine-pitch
	// package. nil ⇒ nominal width everywhere (the first attempt).
	neck *neckPlan
	// overrideMM replaces the class width for the net being routed — the
	// reduced tier a net falls back to when nothing else fits.
	overrideMM float64
	// pending is the set of nets that still have to be routed. Copper laid
	// across their pads' escape corridors is what walls a header in, so the
	// search pays a toll to cross one instead of taking the shortcut.
	pending map[string]bool
	// exploreTrees lets a net look for a tree shape other than the one the
	// router builds by default: alternative seeds, trunk-first growth, and
	// a second look at a branch that found no path. Only the two
	// last-chance passes set it — a search that finds nothing is the most
	// expensive search there is, so it is worth its price only where one
	// net is tried once and nobody is queued behind it.
	exploreTrees bool
	// sameLayerOnly forbids A* layer changes. Short hops and switching
	// nets (LX / SW) try this before a via-jumper so a 2 mm top path is
	// not beaten by an 8-cell via to the bottom.
	sameLayerOnly bool
}

// Route budget, in seconds. A route call is never unbounded: an autorouter
// that runs for hours is a bug, not a thorough search. The engine is anytime
// (per-net budgets, deadline checks inside A*), so a clamped run returns the
// best tree it has when the clock stops.
const (
	// DefaultBudgetSeconds is used when max_seconds is absent, zero,
	// negative or non-finite. Ten minutes is what a route call spends
	// unless it is asked for more, and every existing caller gets exactly
	// this.
	DefaultBudgetSeconds = 600.0
	// MaxBudgetSeconds is the hard ceiling a caller may ask for with
	// max_seconds. It is above the default on purpose: a dense board can
	// still be converging when ten minutes are up — drone-x sat on 53 of 54
	// nets across six 600 s runs — and an operator who knows that should be
	// able to buy the router an hour without editing the source.
	MaxBudgetSeconds = 3600.0
)

// Per-net first-pass budget. A net used to get a flat 3 s (6 s for a fat
// bus) no matter how much clock the call actually had, so a 600 s run
// starved a corner-to-corner power hop that needs ~7 s of A* and then
// reported it as "unreachable". The slice is now a share of what is left.
const (
	minNetBudget    = 3 * time.Second
	minFatNetBudget = 6 * time.Second
	maxNetBudget    = 45 * time.Second
	// repairReserve is the fraction of the remaining clock the first pass
	// leaves untouched, so RR&R, jumpers and negotiation still have time.
	repairReserve = 0.5
)

// netBudget shares the clock still left across the nets still to route:
// an equal slice of half the remainder, doubled for a fat bus, clamped
// between the old floor and a sane ceiling. Nets that finish in
// milliseconds (most of them) hand their slice back to whoever is next.
func netBudget(left time.Duration, netsLeft, padCount int) time.Duration {
	floor := minNetBudget
	if padCount >= 8 {
		floor = minFatNetBudget
	}
	if netsLeft < 1 {
		netsLeft = 1
	}
	share := time.Duration(float64(left) * (1 - repairReserve) / float64(netsLeft))
	if padCount >= 8 {
		share *= 2
	}
	if share < floor {
		share = floor
	}
	if share > maxNetBudget {
		share = maxNetBudget
	}
	if share > left {
		share = left
	}
	return share
}

// repairBudget slices what is left of the clock across the attempts a repair
// pass still owes. The first pass has always sliced (netBudget); the repair
// passes never did — they handed each leftover the whole remaining deadline,
// so the first hard net spent the lot and the rip sets that would have freed
// the next one never ran. drone-x reached the 600 s wall with the negotiator
// still working its first victim set.
//
// attemptsLeft counts the attempts this pass has yet to make, so the share
// grows as the queue drains and a pass that finishes early hands the rest
// back. Never past the wall, and never a deadline in the past: a slice of
// zero would turn "no time left" into a search that cannot even start.
func repairBudget(deadline time.Time, hasDeadline bool, attemptsLeft, padCount int) time.Time {
	if !hasDeadline {
		return deadline
	}
	left := time.Until(deadline)
	if left <= 0 {
		return deadline
	}
	slice := netBudget(left, attemptsLeft, padCount)
	if d := time.Now().Add(slice); d.Before(deadline) {
		return d
	}
	return deadline
}

// ClampBudget normalises a wall-clock budget in seconds. Zero, negative,
// NaN and ±Inf all mean "use the default" — never "run forever" — and no
// run may ask for more than MaxBudgetSeconds.
func ClampBudget(s float64) float64 {
	if math.IsNaN(s) || math.IsInf(s, 0) || s <= 0 {
		return DefaultBudgetSeconds
	}
	if s > MaxBudgetSeconds {
		return MaxBudgetSeconds
	}
	return s
}

// DefaultOptions returns Rust-aligned 2-layer Grid defaults.
func DefaultOptions() Options {
	return Options{
		CellMM:          0.25,
		ClearanceMM:     0.40,
		TraceWidthMM:    0.25,
		ViaDrillMM:      0.30,
		ViaDiameterMM:   0.60,
		ViaCost:         8.0,
		MaxSeconds:      DefaultBudgetSeconds,
		Organic:         true,
		OrganicFilletMM: 3.0,
		FineEscape:      false,
		Negotiate:       false,
	}
}

// ParseOptions overlays key=value tokens from a script args string.
func ParseOptions(o Options, args string) Options {
	for _, f := range strings.Fields(args) {
		kv := strings.SplitN(f, "=", 2)
		if len(kv) != 2 {
			continue
		}
		k, v := kv[0], kv[1]
		switch k {
		case "organic":
			o.Organic = v == "true" || v == "1"
			continue
		case "fine_escape":
			o.FineEscape = v == "true" || v == "1"
			continue
		case "negotiate":
			o.Negotiate = v == "true" || v == "1"
			continue
		case "teardrop", "teardrops":
			o.Teardrops = v == "true" || v == "1" || v == "on"
			o.TeardropsSet = true
			continue
		case "engine":
			switch strings.ToLower(v) {
			case "topo", "topological":
				o.Engine = EngineTopo
			default:
				o.Engine = EngineGrid
			}
			continue
		}
		var x float64
		fmt.Sscanf(v, "%f", &x) // safe-ignore: unparsable numeric options intentionally leave x=0 and fall through to defaults
		switch k {
		case "max_seconds":
			o.MaxSeconds = ClampBudget(x)
		case "clearance":
			o.ClearanceMM = x
			o.ClearanceSet = x > 0
		case "width", "trace_width":
			o.TraceWidthMM = x
		case "cell":
			o.CellMM = x
		case "via_cost":
			o.ViaCost = x
		case "via_drill":
			o.ViaDrillMM = x
		case "via_diameter":
			o.ViaDiameterMM = x
		case "organic_fillet_mm":
			o.OrganicFilletMM = x
		}
	}
	return o
}

// Progress is one step of a route run, for a UI that is watching.
type Progress struct {
	// Phase is "route" for the first pass, "repair" for RR&R / negotiation.
	Phase string
	Net   string
	// Done / Total count nets, not seconds: a repair pass reports against
	// the leftovers it owns, not the whole board.
	Done, Total int
	ElapsedMS   int64
}

// Outcome for one net.
type Outcome struct {
	Status        string  `json:"status"` // ok | failed | skipped
	TraceSegments int     `json:"trace_segments,omitempty"`
	LengthMM      float64 `json:"length_mm,omitempty"`
	LowerBoundMM  float64 `json:"lower_bound_mm,omitempty"`
	Reason        string  `json:"reason,omitempty"`
	// Necked: the net kept its nominal width but narrowed at a pad escape.
	Necked bool `json:"necked,omitempty"`
	// FallbackWidthMM: the nominal width did not fit even with escape necks,
	// so the whole net routed one width tier down.
	FallbackWidthMM float64 `json:"fallback_width_mm,omitempty"`
}

// Report summarises a route call.
type Report struct {
	PerNet          []NetResult `json:"per_net"`
	TraceCount      int         `json:"trace_count"`
	ViaCount        int         `json:"via_count"`
	Iterations      int         `json:"iterations"`
	TotalLengthMM   float64     `json:"total_length_mm"`
	TotalLowerBound float64     `json:"total_lower_bound_mm"`
	Failed          int         `json:"failed"`
	ElapsedMS       int64       `json:"elapsed_ms"`
}

// NetResult pairs a net name with its outcome.
type NetResult struct {
	Net     string  `json:"net"`
	Outcome Outcome `json:"outcome"`
}

// Summary is agent-friendly text. Width degradation is never silent: a net
// that necked at its escapes, or fell back to a narrower tier, is counted in
// the headline. The two counts are exclusive — a net that had to fall back is
// reported as a fallback even if it also necked.
func (r Report) Summary() string {
	ok, necked, reduced := 0, 0, 0
	for _, n := range r.PerNet {
		if n.Outcome.Status != "ok" {
			continue
		}
		ok++
		switch {
		case n.Outcome.FallbackWidthMM > 0:
			reduced++
		case n.Outcome.Necked:
			necked++
		}
	}
	var notes []string
	if necked > 0 {
		notes = append(notes, fmt.Sprintf("%d necked at escapes", necked))
	}
	if reduced > 0 {
		notes = append(notes, fmt.Sprintf("%d at reduced width", reduced))
	}
	widths := ""
	if len(notes) > 0 {
		widths = " (" + strings.Join(notes, ", ") + ")"
	}
	return fmt.Sprintf("route: %d/%d nets ok%s, %d traces, %d vias, %.1f mm copper, %d ms",
		ok, len(r.PerNet), widths, r.TraceCount, r.ViaCount, r.TotalLengthMM, r.ElapsedMS)
}

type padLoc struct {
	ref   string
	p     core.Point
	layer uint8
	// through: the pad is drilled, so it is copper on every layer. The
	// router may land on either side without a via — and must not drill
	// one next to the pad's own hole.
	through bool
}

// Route autoroutes all multi-pad nets on the board. Mutates board traces/vias
// (appends to existing copper unless caller cleared first).
func Route(board *core.Board, opts Options) Report {
	if opts.TeardropsSet && board != nil {
		board.Teardrops = opts.Teardrops
	}
	start := time.Now()
	// Every run is bounded: max_seconds=0 is "the default budget", not
	// "forever". The search is anytime, so the deadline caps the wall
	// clock without throwing away the copper already committed.
	opts.MaxSeconds = ClampBudget(opts.MaxSeconds)
	if strings.EqualFold(opts.Engine, EngineTopo) {
		return routeTopo(board, opts)
	}
	deadline := start.Add(time.Duration(opts.MaxSeconds * float64(time.Second)))
	hasDeadline := true
	pastDeadline := func() bool {
		// A cancel is the same event as running out of clock: stop looking,
		// keep what is committed, report the leftovers honestly.
		if opts.Cancel != nil && opts.Cancel() {
			return true
		}
		return hasDeadline && time.Now().After(deadline)
	}
	report := func(phase, net string, done, total int) {
		if opts.Progress == nil {
			return
		}
		opts.Progress(Progress{
			Phase: phase, Net: net, Done: done, Total: total,
			ElapsedMS: time.Since(start).Milliseconds(),
		})
	}

	rep := Report{Iterations: 1}

	// 1. Collect multi-pad nets from footprints in stable order.
	nets := map[string][]padLoc{}
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net == "" {
				continue
			}
			layer := pad.Layer.Index
			nets[*pad.Net] = append(nets[*pad.Net], padLoc{
				ref:     fp.Reference,
				p:       core.PadWorldCenter(fp, pad),
				layer:   layer,
				through: pad.Drill != nil && *pad.Drill > 0,
			})
		}
	}

	names := make([]string, 0, len(nets))
	for n := range nets {
		names = append(names, n)
	}
	// Power first so +3V3 can span; then fat signal buses (>=8 pads) while
	// the field is still open — a bus that spans the board cannot be routed
	// over leftovers; small signals fill in around it afterwards.
	sort.Slice(names, func(i, j int) bool {
		pi, pj := netOrderKey(names[i]), netOrderKey(names[j])
		if pi != pj {
			return pi < pj
		}
		if pi > 2 {
			if ni, nj := len(nets[names[i]]), len(nets[names[j]]); ni >= 8 || nj >= 8 {
				if (ni >= 8) != (nj >= 8) {
					return ni > nj
				}
			}
		}
		if len(nets[names[i]]) != len(nets[names[j]]) {
			return len(nets[names[i]]) < len(nets[names[j]])
		}
		ti, tj := signalTieBreak(names[i]), signalTieBreak(names[j])
		if ti != tj {
			return ti < tj
		}
		return names[i] < names[j]
	})

	// 2. Fine-pitch: tighten cell/trace/via so a 0.4 mm QFN can escape,
	// then staggered dogbone fanout, then grid with all copper stamped.
	opts = applyFabCeiling(board, opts)
	// Per-net width comes from the class after the fab ceiling: the fab
	// floor is a floor for class widths too, never a ceiling.
	opts.widths = newNetWidths(board, opts)
	_ = planFanout(board, opts)
	g := newGrid(board, opts)

	// Pour-only nets: the pour is the connection (Rust route_one_net).
	pourNets := map[string]bool{}
	for _, p := range board.Pours {
		pourNets[p.Net] = true
	}

	// Nets that actually cost search time, for the per-net budget share,
	// and the same set as the initial "still to route" toll list.
	routableLeft := 0
	pending := map[string]bool{}
	for _, name := range names {
		if !pourNets[name] && len(nets[name]) >= 2 {
			routableLeft++
			pending[name] = true
		}
	}
	// The grid carries the toll list in its own options so a rebuild
	// (a rejected commit, a rip) keeps it.
	setPending := func(cur string) {
		delete(pending, cur)
		opts.pending = pending
		g.opts.pending = pending
	}

	var failedNets []string
	for ni, name := range names {
		report("route", name, ni, len(names))
		pads := nets[name]
		// Rust: 1-pad nets and pour nets are Outcome::Ok with 0 copper.
		if pourNets[name] || len(pads) < 2 {
			rep.PerNet = append(rep.PerNet, NetResult{Net: name, Outcome: Outcome{
				Status: "ok", Reason: "pour_or_single",
			}})
			continue
		}
		if pastDeadline() {
			rep.PerNet = append(rep.PerNet, NetResult{Net: name, Outcome: Outcome{Status: "failed", Reason: "budget"}})
			rep.Failed++
			failedNets = append(failedNets, name)
			continue
		}

		netDead := deadline
		if hasDeadline {
			if slice := netBudget(time.Until(deadline), routableLeft, len(pads)); slice > 0 {
				netDead = time.Now().Add(slice)
			}
		}
		routableLeft--
		setPending(name)
		out := routeNet(board, g, name, pads, opts, netDead, hasDeadline)
		if out.Status != "ok" && len(pads) <= 3 && !pastDeadline() &&
			!preferSameLayer(name, pads, opts.Schematic) {
			cheap := opts
			cheap.ViaCost = 2
			out = routeNet(board, g, name, pads, cheap, deadline, hasDeadline)
		}
		rep.PerNet = append(rep.PerNet, NetResult{Net: name, Outcome: out})
		if out.Status == "ok" {
			rep.TraceCount += out.TraceSegments
			rep.TotalLengthMM += out.LengthMM
			rep.TotalLowerBound += out.LowerBoundMM
		} else {
			rep.Failed++
			failedNets = append(failedNets, name)
		}
		// After the last power rail, string-pull so the fat +3V3
		// maze does not wall off the QFN for signal nets.
		if netOrderKey(name) <= 1 && opts.Organic {
			laterPower := false
			seen := false
			for _, n := range names {
				if n == name {
					seen = true
					continue
				}
				if seen && netOrderKey(n) <= 1 && len(nets[n]) >= 2 && !pourNets[n] {
					laterPower = true
					break
				}
			}
			if !laterPower {
				organicPass(board, opts)
				g = newGrid(board, opts)
			}
		}
	}

	// From here only the leftovers are still owed copper, so they are the
	// only pads whose escape corridors still carry a toll.
	pending = map[string]bool{}
	for _, n := range failedNets {
		pending[n] = true
	}
	opts.pending, g.opts.pending = pending, pending
	// Options for the passes that may look for a different tree shape. It is
	// one extra call per leftover, never a multiplier inside RR&R or the
	// negotiator: those try a leftover once per rip set, and twelve shapes
	// per rip set costs far more clock than it wins. Measured on drone-x,
	// exploring inside the negotiator took 53/54 down to 50/54 — the three
	// nets it lost were ones the negotiator used to have time to rescue.
	exOpts := opts
	exOpts.exploreTrees = true

	// retryLeftovers rebuilds the grid and gives every net still open one
	// more routeNet on the board as it now stands.
	retryLeftovers := func(o Options) {
		g = newGrid(board, o)
		left := []string{}
		for i, name := range failedNets {
			if pastDeadline() {
				left = append(left, name)
				continue
			}
			report("repair", name, i, len(failedNets))
			setPending(name)
			netDead := repairBudget(deadline, hasDeadline, len(failedNets)-i, len(nets[name]))
			out := routeNet(board, g, name, nets[name], o, netDead, hasDeadline)
			if out.Status == "ok" {
				rep.Failed--
				rep.TraceCount += out.TraceSegments
				rep.TotalLengthMM += out.LengthMM
				for i := range rep.PerNet {
					if rep.PerNet[i].Net == name {
						rep.PerNet[i].Outcome = out
					}
				}
			} else {
				left = append(left, name)
			}
		}
		failedNets = left
	}

	// 4. RR&R: snapshot, rip one blocking signal net, retry the failed
	// net then the victim. Restore the snapshot if either side fails.
	if len(failedNets) > 0 && !pastDeadline() {
		rep.Iterations = 2
		still := []string{}
		for i, name := range failedNets {
			if pastDeadline() {
				still = append(still, name)
				continue
			}
			report("repair", name, i, len(failedNets))
			setPending(name)
			victim := pickRipVictim(board, nets[name], name)
			snapT := append([]core.Trace(nil), board.Traces...)
			snapV := append([]core.Via(nil), board.Vias...)
			if victim != "" {
				ripNetCopper(board, victim)
				g = newGrid(board, opts)
			}
			// The leftover gets a share; the victim's restore below keeps
			// the full wall, because a victim that cannot come back costs
			// the rescue outright (both-or-neither).
			tryDead := repairBudget(deadline, hasDeadline, len(failedNets)-i, len(nets[name]))
			out := routeNet(board, g, name, nets[name], opts, tryDead, hasDeadline)
			kept := false
			if out.Status != "ok" {
				board.Traces = snapT
				board.Vias = snapV
				g = newGrid(board, opts)
				still = append(still, name)
			} else if victim != "" {
				vout := routeNet(board, g, victim, nets[victim], opts, deadline, hasDeadline)
				if vout.Status != "ok" {
					// Both-or-neither: killing the victim just moves
					// the failure (GPIO6 was dying so QSPI could live).
					board.Traces = snapT
					board.Vias = snapV
					g = newGrid(board, opts)
					still = append(still, name)
				} else {
					kept = true
				}
			} else {
				kept = true
			}
			if kept {
				rep.Failed--
				rep.TraceCount += out.TraceSegments
				rep.TotalLengthMM += out.LengthMM
				for i := range rep.PerNet {
					if rep.PerNet[i].Net == name {
						rep.PerNet[i].Outcome = out
					}
				}
			}
		}
		failedNets = still
	}

	// 4b. Last chance: rebuild grid and try direct/jumper on leftovers, and
	// the first of the two places a leftover may also look for a tree shape
	// other than the one it was built with — one call per net, on a board
	// nobody else is queued behind.
	if len(failedNets) > 0 && !pastDeadline() {
		retryLeftovers(exOpts)
	}

	// 4c. Negotiated congestion: sharing-aware probe + multi-victim
	// rip of long-haul copper (fanout vias stay). Always runs on
	// leftovers — that is what closes QFN contention the single-victim
	// RR&R cannot. Plain options: twelve tree shapes per rip set would
	// spend the clock this pass needs to work through its rip sets.
	if len(failedNets) > 0 && !pastDeadline() {
		failedNets = negotiateLeftovers(board, &g, nets, failedNets, &rep, opts, deadline, hasDeadline)
	}

	// 4d. The negotiator moved other people's copper; whatever is still open
	// gets one more look at a different tree shape against the board it left
	// behind. Last thing before the post-passes, so it can starve nothing.
	if len(failedNets) > 0 && !pastDeadline() {
		retryLeftovers(exOpts)
	}

	// Drop dogbone islands of nets that never finished — they are
	// NetSplit errors and not connections.
	stripStrandedFanout(board)

	// 5. Organic string-pull (default on). Roll back if it would make
	// copper illegal under DRC min-clearance (0.20 mm), not just the
	// router's 0.40 mm search clearance.
	if opts.Organic && !pastDeadline() {
		before := append([]core.Trace(nil), board.Traces...)
		organicPass(board, opts)
		if !copperClearanceLegal(board, commitClearance(board)) {
			board.Traces = before
		}
		refreshReportLengths(board, &rep)
	}

	// Pour stitching: tie floating plane pads to their pour.
	if !pastDeadline() {
		stitchIsolatedPads(board, opts)
	}
	// Last, so it catches the tree, the jumper and the stitch alike: no net
	// ships the same hole ordered twice.
	dropDuplicateVias(board)

	// Via count for nets that finished ok.
	okNets := map[string]bool{}
	for _, n := range rep.PerNet {
		if n.Outcome.Status == "ok" {
			okNets[n.Net] = true
		}
	}
	viaCount := 0
	for _, v := range board.Vias {
		if okNets[v.Net] {
			viaCount++
		}
	}
	rep.ViaCount = viaCount
	// TraceCount from board for accuracy after organic re-segmentation.
	traceCount := 0
	for _, tr := range board.Traces {
		if okNets[tr.Net] {
			traceCount++
		}
	}
	rep.TraceCount = traceCount

	rep.ElapsedMS = time.Since(start).Milliseconds()
	return rep
}

func refreshReportLengths(board *core.Board, rep *Report) {
	byNet := map[string]float64{}
	for _, tr := range board.Traces {
		byNet[tr.Net] += hypotMM(tr.Start, tr.End)
	}
	rep.TotalLengthMM = 0
	for i := range rep.PerNet {
		n := rep.PerNet[i].Net
		if rep.PerNet[i].Outcome.Status != "ok" {
			continue
		}
		if L, ok := byNet[n]; ok {
			rep.PerNet[i].Outcome.LengthMM = L
			rep.PerNet[i].Outcome.TraceSegments = 0
			for _, tr := range board.Traces {
				if tr.Net == n {
					rep.PerNet[i].Outcome.TraceSegments++
				}
			}
			rep.TotalLengthMM += L
		}
	}
}

// footprintsStable returns footprints in deterministic order (FootprintOrder, then ref).
func ripNetCopper(board *core.Board, net string) (tr []core.Trace, vias []core.Via) {
	keptT := board.Traces[:0]
	for _, t := range board.Traces {
		if t.Net == net {
			tr = append(tr, t)
			continue
		}
		keptT = append(keptT, t)
	}
	board.Traces = keptT
	keptV := board.Vias[:0]
	for _, v := range board.Vias {
		if v.Net == net {
			vias = append(vias, v)
			continue
		}
		keptV = append(keptV, v)
	}
	board.Vias = keptV
	return tr, vias
}

func pickRipVictim(board *core.Board, pads []padLoc, failed string) string {
	if len(pads) == 0 {
		return ""
	}
	bestNet, bestD := "", 1e9
	for _, t := range board.Traces {
		if t.Net == failed || netOrderKey(t.Net) < 3 {
			continue
		}
		if hypotMM(t.Start, t.End) < 1.6 {
			continue // leave QFN dogbones alone
		}
		for _, p := range pads {
			d := distPointSeg(p.p.X.ToMM(), p.p.Y.ToMM(), t.Start.X.ToMM(), t.Start.Y.ToMM(), t.End.X.ToMM(), t.End.Y.ToMM())
			if d < bestD {
				bestD, bestNet = d, t.Net
			}
		}
	}
	if bestD < 3.0 {
		return bestNet
	}
	return ""
}

func distPointSeg(px, py, x0, y0, x1, y1 float64) float64 {
	dx, dy := x1-x0, y1-y0
	l2 := dx*dx + dy*dy
	if l2 < 1e-12 {
		return math.Hypot(px-x0, py-y0)
	}
	t := ((px-x0)*dx + (py-y0)*dy) / l2
	if t < 0 {
		t = 0
	} else if t > 1 {
		t = 1
	}
	return math.Hypot(px-(x0+t*dx), py-(y0+t*dy))
}

func signalTieBreak(name string) int {
	switch strings.ToUpper(name) {
	case "SCK", "SCL", "SCLK", "CLK":
		return 0
	case "MOSI", "SDA", "SDO":
		return 1
	case "MISO", "SDI":
		return 2
	case "NSS", "CS", "NRESET", "BUSY", "DIO1":
		return 3
	default:
		return 4
	}
}

// applyFabCeiling sets working clearance / width / via to the board's
// JLCPCB (or other) fab profile — that profile is the ceiling: we never
// route tighter, and we do use those mins so a 0.4 mm QFN can escape.
func applyFabCeiling(board *core.Board, opts Options) Options {
	fab := core.ActiveFabRules(board)
	if fab.MinClearanceMM > 0 {
		// The fab minimum is a floor, not a fixed value: an explicit
		// clearance= wider than the process minimum is a legal request and
		// is honoured. Only the default (and anything tighter than the fab
		// allows) collapses onto the floor.
		cl := fab.MinClearanceMM
		if opts.ClearanceSet && opts.ClearanceMM > cl {
			cl = opts.ClearanceMM
		}
		opts.ClearanceMM = cl
		// Search a hair above the fab floor so grid quantisation cannot
		// land a trace 50 nm inside the DRC limit (VBUS vs USB shield).
		opts.SearchClearMM = cl + 0.02
	}
	if fab.MinViaDrillMM > 0 {
		opts.ViaDrillMM = fab.MinViaDrillMM
	}
	if fab.MinViaDiameterMM > 0 {
		opts.ViaDiameterMM = fab.MinViaDiameterMM
	}
	fine := false
	for _, fp := range footprintsStable(board) {
		// WLP-6 is 6 pads at 0.4 mm; the old ≥8 gate left default 0.25 mm
		// traces on 0.25 mm lands and SEL could not leave the pin field.
		if len(fp.Pads) >= 6 && minPadPitchMM(fp) < 0.50 {
			fine = true
			break
		}
	}
	if fine {
		if fab.MinTraceWidthMM > 0 {
			opts.TraceWidthMM = fab.MinTraceWidthMM
		} else if opts.TraceWidthMM > 0.15 {
			opts.TraceWidthMM = 0.15
		}
		cell := fab.MinClearanceMM
		if cell < 0.10 {
			cell = 0.10
		}
		if opts.CellMM > cell {
			opts.CellMM = cell
		}
	} else if fab.MinTraceWidthMM > 0 && opts.TraceWidthMM < fab.MinTraceWidthMM {
		opts.TraceWidthMM = fab.MinTraceWidthMM
	}
	// Persist the ceiling so pack/DRC use the same floor we routed at.
	if board != nil && (board.FabRules == nil || board.FabRules.MinClearanceMM <= 0) {
		cp := fab
		board.FabRules = &cp
	}
	// Adopt fab mins everywhere: never tighter than the fab, and do not
	// keep leftover 0.12/0.20 areas that block a 0.4 mm QFN escape.
	if board != nil && fab.MinClearanceMM > 0 {
		cl := fab.MinClearanceMM
		for i := range board.RuleAreas {
			if board.RuleAreas[i].ClearanceMM != nil {
				board.RuleAreas[i].ClearanceMM = &cl
			}
		}
		if board.Outline != nil {
			name := fab.Preset
			if name == "" {
				name = "fab-ceiling"
			}
			has := false
			for i := range board.RuleAreas {
				if board.RuleAreas[i].Name == name {
					has = true
					break
				}
			}
			if !has {
				board.RuleAreas = append(board.RuleAreas, core.RuleArea{
					ID: core.NewID(), Name: name, Rect: *board.Outline,
					ClearanceMM: &cl, Priority: 50,
				})
			}
		}
	}
	return opts
}

// signalOtherLayer is the opposite outer signal copper (F.Cu ↔ B.Cu).
// Index 1 is In1 (GND plane) on 4-layer — never treat that as a hop layer.
func signalOtherLayer(g *grid, from uint8) uint8 {
	last := uint8(1)
	if g != nil && g.layers >= 2 {
		last = uint8(g.layers - 1)
	}
	if from == last {
		return 0
	}
	return last
}

func (o Options) padClear() float64 {
	if o.SearchClearMM > 0 {
		return o.SearchClearMM
	}
	return 0.127
}

// shortNetSpanMM is the pad-pad distance (or HPWL) below which a net
// should stay on one layer when a path exists. ~4–5 mm covers an
// inductor–LX hop and a Cout–OUT hop on a boost island.
const shortNetSpanMM = 5.0

func preferSameLayer(name string, pads []padLoc, sch *core.Schematic) bool {
	if isSwitchNet(name) {
		return true
	}
	if sch != nil && sch.NetToClass != nil {
		if cn := sch.NetToClass[name]; cn != "" && isSwitchNet(cn) {
			return true
		}
	}
	return padSpanMM(pads) <= shortNetSpanMM
}

func isSwitchNet(name string) bool {
	u := strings.ToUpper(strings.TrimPrefix(name, "+"))
	switch u {
	case "LX", "SW", "SWITCH", "VSW", "SWX", "SWITCHING":
		return true
	}
	return strings.HasPrefix(u, "LX") || strings.HasPrefix(u, "SW")
}

func padSpanMM(pads []padLoc) float64 {
	if len(pads) < 2 {
		return 0
	}
	minX, minY := pads[0].p.X.ToMM(), pads[0].p.Y.ToMM()
	maxX, maxY := minX, minY
	maxPair := 0.0
	for i := range pads {
		x, y := pads[i].p.X.ToMM(), pads[i].p.Y.ToMM()
		if x < minX {
			minX = x
		}
		if y < minY {
			minY = y
		}
		if x > maxX {
			maxX = x
		}
		if y > maxY {
			maxY = y
		}
		for j := i + 1; j < len(pads); j++ {
			d := hypotMM(pads[i].p, pads[j].p)
			if d > maxPair {
				maxPair = d
			}
		}
	}
	hpwl := (maxX - minX) + (maxY - minY)
	if maxPair > hpwl {
		return maxPair
	}
	return hpwl
}

func netOrderKey(name string) int {
	u := strings.ToUpper(name)
	switch {
	case u == "V5" || u == "+5V" || u == "5V" || u == "VIN" || u == "VBUS" || u == "VSTOR" || u == "BATT" || u == "VBAT":
		return 0
	case u == "+3V3" || u == "3V3" || u == "VCC" || u == "VDD" || u == "VSW" || u == "+1V1" || u == "1V1" ||
		u == "+3V0" || u == "3V0" || u == "OUT" || u == "VOUT" || u == "LX" || u == "SW":
		return 1
	case u == "GND" || strings.HasPrefix(u, "GND") || u == "VSS" || u == "AGND":
		return 2
	default:
		return 3
	}
}

func footprintsStable(board *core.Board) []*core.Footprint {
	seen := map[string]bool{}
	var out []*core.Footprint
	for _, id := range board.FootprintOrder {
		fp := board.Footprints[id]
		if fp == nil || seen[id] {
			continue
		}
		seen[id] = true
		out = append(out, fp)
	}
	// Leftovers not in order (legacy loads).
	var extra []*core.Footprint
	for id, fp := range board.Footprints {
		if fp == nil || seen[id] {
			continue
		}
		extra = append(extra, fp)
	}
	sort.Slice(extra, func(i, j int) bool {
		if extra[i].Reference != extra[j].Reference {
			return extra[i].Reference < extra[j].Reference
		}
		return extra[i].ID.String() < extra[j].ID.String()
	})
	return append(out, extra...)
}

// growthOrder picks which unconnected terminal joins the Prim tree next. The
// terminals of a multi-terminal net are the same whichever order they are
// tied in, but the copper is not: every branch laid narrows the field the next
// one has to cross. Two seven-terminal I2C stars one pad pitch apart on
// drone-x proved it — `SCL` closed in every run and `SDA` in none, because the
// one order the router ever built happened to work for one of them.
type growthOrder uint8

const (
	// growNearest attaches the terminal closest to the tree. Shortest
	// copper, but it leaves the board-spanning branch for last, when the
	// field is at its most congested — and a net is all-or-nothing, so
	// that last branch failing throws the whole tree away.
	growNearest growthOrder = iota
	// growFarthest attaches the most remote terminal first, so the trunk
	// crosses the board while the field is still open and the terminals
	// left tap into it over a short distance.
	growFarthest
)

// treeAttempt is one tree shape for a net: where the growth starts and which
// terminal it reaches for next.
type treeAttempt struct {
	seed  int
	order growthOrder
}

// maxTreeAttempts bounds the tree shapes one net may try, so a rail with
// thirty terminals cannot spend the whole clock on itself.
const maxTreeAttempts = 12

// maxBranchTries is how often one branch of a tree may be searched for in a
// single attempt: once, plus one reprieve after the rest of the tree is down.
const maxBranchTries = 2

// treeAttempts enumerates the shapes to try, deterministically. The first
// entry is always (seed0, growNearest) — the tree the router has always built.
//
// Without explore the plan is exactly what the first pass has always done:
// every seed for a net of six terminals or fewer, and one shape for anything
// bigger. That cut-off is the defect — it is why a seven-terminal bus got one
// shape and no second opinion — but the first pass is not where to pay for
// the cure, so explore widens the plan only for the repair passes.
//
// With explore, both growth orders are on the table and alternative seeds come
// extremity-first: a tree rooted at an outlying terminal grows as a chain
// across the board, one rooted in the middle grows as a star whose last spoke
// has to cross every earlier spoke.
func treeAttempts(pads []padLoc, seed0 int, explore bool) []treeAttempt {
	if seed0 < 0 || seed0 >= len(pads) {
		seed0 = 0
	}
	if len(pads) < 2 {
		return []treeAttempt{{seed: seed0, order: growNearest}}
	}
	if !explore {
		out := []treeAttempt{{seed: seed0, order: growNearest}}
		if len(pads) <= 6 {
			for i := range pads {
				if i != seed0 {
					out = append(out, treeAttempt{seed: i, order: growNearest})
				}
			}
		}
		return out
	}
	// With a single hop there is only one order, so a two-terminal net
	// costs exactly what it costs today.
	orders := []growthOrder{growNearest}
	if len(pads) >= 3 {
		orders = []growthOrder{growNearest, growFarthest}
	}

	var cx, cy float64
	for _, p := range pads {
		cx += p.p.X.ToMM()
		cy += p.p.Y.ToMM()
	}
	cx /= float64(len(pads))
	cy /= float64(len(pads))
	alts := make([]int, 0, len(pads)-1)
	for i := range pads {
		if i != seed0 {
			alts = append(alts, i)
		}
	}
	sort.Slice(alts, func(a, b int) bool {
		da := math.Hypot(pads[alts[a]].p.X.ToMM()-cx, pads[alts[a]].p.Y.ToMM()-cy)
		db := math.Hypot(pads[alts[b]].p.X.ToMM()-cx, pads[alts[b]].p.Y.ToMM()-cy)
		if math.Abs(da-db) > 1e-9 {
			return da > db
		}
		return alts[a] < alts[b]
	})

	out := make([]treeAttempt, 0, maxTreeAttempts)
	for _, o := range orders {
		out = append(out, treeAttempt{seed: seed0, order: o})
	}
	for _, s := range alts {
		for _, o := range orders {
			if len(out) >= maxTreeAttempts {
				return out
			}
			out = append(out, treeAttempt{seed: s, order: o})
		}
	}
	return out
}

// nextTerminal picks the unconnected terminal that joins the tree next, by
// its Manhattan distance to the nearest terminal already in it: the closest
// one, or — when this attempt lays the trunk first — the most remote one.
// Terminals in skip are the ones that already refused this sweep. Ties go to
// the lowest index, so the growth is the same on every run.
func nextTerminal(pads []padLoc, connected, skip map[int]bool, order growthOrder) int {
	best, bestD := -1, math.MaxFloat64
	if order == growFarthest {
		bestD = -1
	}
	for j := range pads {
		if connected[j] || skip[j] {
			continue
		}
		toTree := math.MaxFloat64
		for i := range pads {
			if !connected[i] {
				continue
			}
			if d := manhattanMM(pads[i].p, pads[j].p); d < toTree {
				toTree = d
			}
		}
		if toTree == math.MaxFloat64 {
			continue
		}
		if order == growFarthest {
			if toTree > bestD {
				bestD, best = toTree, j
			}
			continue
		}
		if toTree < bestD {
			bestD, best = toTree, j
		}
	}
	return best
}

func nearestConnectedPad(pads []padLoc, connected map[int]bool, g gpos) int {
	best, bestD := -1, math.MaxFloat64
	for i := range pads {
		if !connected[i] {
			continue
		}
		dx := pads[i].p.X.ToMM() - g.x.ToMM()
		dy := pads[i].p.Y.ToMM() - g.y.ToMM()
		d := dx*dx + dy*dy
		if d < bestD {
			bestD, best = d, i
		}
	}
	if best >= 0 && bestD < 1.0 { // within 1 mm of a connected pad
		return best
	}
	return -1
}

// pickSeed prefers a pad that already has a fanout via so A* can drop
// to the other layer instead of fighting the QFN top-side corridor.
func closestNetVia(board *core.Board, net string, pad core.Point, pads []padLoc, padIdx int) (core.Point, bool) {
	px, py := pad.X.ToMM(), pad.Y.ToMM()
	// A dogbone stub owns its via even when the barrel sat farther
	// than the old 1.2 mm window (outer ring is 1.35–2.40 mm).
	if v, ok := stubOwnedVia(board, net, pad); ok {
		return v, true
	}
	var best core.Point
	bestD := 2.6
	found := false
	for _, v := range board.Vias {
		if v.Net != net {
			continue
		}
		d := math.Hypot(v.Position.X.ToMM()-px, v.Position.Y.ToMM()-py)
		if d >= bestD {
			continue
		}
		// Reject if some other pad of this net is closer to the via.
		otherCloser := false
		vx, vy := v.Position.X.ToMM(), v.Position.Y.ToMM()
		for i, p := range pads {
			if i == padIdx {
				continue
			}
			if math.Hypot(vx-p.p.X.ToMM(), vy-p.p.Y.ToMM()) < d-1e-6 {
				otherCloser = true
				break
			}
		}
		if otherCloser {
			continue
		}
		best, bestD, found = v.Position, d, true
	}
	return best, found
}

func stubOwnedVia(board *core.Board, net string, pad core.Point) (core.Point, bool) {
	if board == nil {
		return core.Point{}, false
	}
	px, py := pad.X.ToMM(), pad.Y.ToMM()
	for _, t := range board.Traces {
		if t.Net != net {
			continue
		}
		sx, sy := t.Start.X.ToMM(), t.Start.Y.ToMM()
		ex, ey := t.End.X.ToMM(), t.End.Y.ToMM()
		padAtStart := math.Hypot(sx-px, sy-py) < 0.35
		padAtEnd := math.Hypot(ex-px, ey-py) < 0.35
		if !padAtStart && !padAtEnd {
			continue
		}
		ox, oy := ex, ey
		if padAtEnd {
			ox, oy = sx, sy
		}
		for _, v := range board.Vias {
			if v.Net != net {
				continue
			}
			if math.Hypot(v.Position.X.ToMM()-ox, v.Position.Y.ToMM()-oy) < 0.35 {
				return v.Position, true
			}
		}
	}
	return core.Point{}, false
}

func pickSeed(board *core.Board, net string, pads []padLoc) int {
	nearVia := func(i int) bool {
		_, ok := closestNetVia(board, net, pads[i].p, pads, i)
		return ok
	}
	// Power: start from an open discrete, not a QFN pin boxed by vias.
	if netOrderKey(net) <= 1 {
		for i := range pads {
			if !nearVia(i) {
				return i
			}
		}
	}
	for i := range pads {
		if nearVia(i) {
			return i
		}
	}
	return 0
}

func existingNetSources(board *core.Board, g *grid, net string, pads []padLoc, connected map[int]bool) []cellKey {
	seen := map[cellKey]bool{}
	var out []cellKey
	add := func(x, y core.Length, layer uint8) {
		cx, cy, ok := g.worldToCell(x, y)
		if !ok {
			cx, cy = clampCell(core.NewPoint(x, y), g)
		}
		k := cellKey{cx, cy, layer}
		if seen[k] {
			return
		}
		seen[k] = true
		out = append(out, k)
	}
	nearConnected := func(x, y core.Length) bool {
		if connected == nil || len(pads) == 0 {
			return true
		}
		px, py := x.ToMM(), y.ToMM()
		// Belong to the closest pad of this net — a 1.6 mm radius
		// would swallow the neighbouring QFN pin's dogbone.
		bestI, bestD := -1, 1e9
		for i, p := range pads {
			d := math.Hypot(px-p.p.X.ToMM(), py-p.p.Y.ToMM())
			if d < bestD {
				bestD, bestI = d, i
			}
		}
		return bestI >= 0 && connected[bestI] && bestD < 1.5
	}
	for _, v := range board.Vias {
		if v.Net != net || !nearConnected(v.Position.X, v.Position.Y) {
			continue
		}
		for L := uint8(0); L < uint8(g.layers); L++ {
			add(v.Position.X, v.Position.Y, L)
		}
	}
	for _, t := range board.Traces {
		if t.Net != net {
			continue
		}
		// Only the end that sits on the connected tree. Adding both
		// ends of a long dogbone lets A* "reach" the far pad without
		// copper between the islands (SEL split on WLP-6).
		if nearConnected(t.Start.X, t.Start.Y) {
			add(t.Start.X, t.Start.Y, t.Layer.Index)
		}
		if nearConnected(t.End.X, t.End.Y) {
			add(t.End.X, t.End.Y, t.Layer.Index)
		}
	}
	return out
}

// routeNet lays one net, widening the search when its nominal width does not
// fit through the escape: nominal everywhere → nominal with a short neck at
// each pad escape → the next width tier down. Only a class net whose nominal
// is wider than the router default ever gets past the first attempt, so every
// other net routes exactly as it did before.
func routeNet(board *core.Board, g *grid, name string, pads []padLoc, opts Options, deadline time.Time, hasDeadline bool) Outcome {
	base := opts.TraceWidthMM // router default, already at the fab floor
	nominal := opts.netWidthMax(name)
	spent := func() bool { return hasDeadline && time.Now().After(deadline) }
	attempt := func(o Options) Outcome {
		snap := len(board.Traces)
		out := routeNetAt(board, g, name, pads, o, deadline, hasDeadline)
		if out.Status != "ok" || o.neck == nil {
			return out
		}
		// Only claim a neck when narrow copper actually landed.
		for _, tr := range board.Traces[snap:] {
			if tr.Net == name && tr.Width.ToMM() < o.widthFor(name, tr.Layer.Index)-1e-6 {
				out.Necked = true
				break
			}
		}
		return out
	}

	out := attempt(opts)
	if out.Status == "ok" || nominal <= base+1e-9 {
		return out
	}
	// Tier 2: keep the controlled width in the open field, neck the escapes.
	if plan := newNeckPlan(pads, base, nominal); plan != nil && !spent() {
		plan.net, plan.g = name, g
		o := opts
		o.neck = plan
		if necked := attempt(o); necked.Status == "ok" {
			return necked
		}
	}
	// Tier 3: the width itself does not fit. Drop one tier and say so.
	if fb := opts.fallbackWidth(name); fb < nominal-1e-9 && !spent() {
		o := opts
		o.overrideMM = fb
		if plan := newNeckPlan(pads, base, fb); plan != nil {
			plan.net, plan.g = name, g
			o.neck = plan
		}
		if low := attempt(o); low.Status == "ok" {
			low.FallbackWidthMM = fb
			return low
		}
	}
	return out
}

// routeNetAt lays one net at one width, and leaves the board exactly as it
// found it when the net does not finish. The partial passes (routeDirect,
// routeClearHops) commit copper as they go; without this rollback a net that
// died later kept those stubs, which DRC reports as dangling pads and which
// the next attempt at the same net simply laid a second time on top of.
func routeNetAt(board *core.Board, g *grid, name string, pads []padLoc, opts Options, deadline time.Time, hasDeadline bool) Outcome {
	snapT, snapV := len(board.Traces), len(board.Vias)
	out := routeNetAtOnce(board, g, name, pads, opts, deadline, hasDeadline)
	if out.Status != "ok" && (len(board.Traces) > snapT || len(board.Vias) > snapV) {
		board.Traces = board.Traces[:snapT]
		board.Vias = board.Vias[:snapV]
		g.rebuild(board)
	}
	return out
}

// rebuild re-stamps the grid from the board while keeping the state that is
// not derived from copper: banned via sites, and the negotiation mode a
// caller switched on around this call.
func (g *grid) rebuild(board *core.Board) {
	bans, share, present := g.viaBan, g.share, g.present
	*g = *newGrid(board, g.opts)
	g.viaBan, g.share, g.present = bans, share, present
}

func routeNetAtOnce(board *core.Board, g *grid, name string, pads []padLoc, opts Options, deadline time.Time, hasDeadline bool) Outcome {
	// This net's nominal width drives planning clearance and the grid's
	// own-net paint; each segment then takes its own layer's width.
	opts.TraceWidthMM = opts.netWidthMax(name)
	defer func(p *neckPlan) { g.opts.neck = p }(g.opts.neck)
	g.opts.neck = opts.neck
	defer func(w float64) { g.opts.TraceWidthMM = w }(g.opts.TraceWidthMM)
	g.opts.TraceWidthMM = opts.TraceWidthMM

	if d := routeDirect(board, g, name, pads, opts); d.Status == "ok" {
		return d
	}
	// Partial: commit every short straight/L that is already legal so a
	// 6-pad rail like +1V1 ties its decaps before A* walks the QFN maze.
	already, preSegs, preLen := routeClearHops(board, g, name, pads, opts)
	if len(already) == len(pads) && len(pads) >= 2 {
		return Outcome{Status: "ok", Reason: "clear-hops", TraceSegments: preSegs, LengthMM: preLen, LowerBoundMM: lowerBoundMM(pads)}
	}
	seed0 := pickSeed(board, name, pads)
	if len(already) > 0 {
		// Grow from the cluster we already tied, not a boxed QFN pin.
		for i := range pads {
			if already[i] {
				seed0 = i
				break
			}
		}
	}
	// Short / switching nets try a single-layer tree before any via
	// jumper. ViaCost=8 is only 2 mm of extra length, so A* otherwise
	// hops to the bottom around the first slightly-blocked L-shape.
	attempts := []Options{opts}
	if preferSameLayer(name, pads, opts.Schematic) {
		same := opts
		same.sameLayerOnly = true
		same.ViaCost = 1e6
		attempts = []Options{same, opts}
	}
	// One tree shape per attempt, cheapest-first: the shape the router has
	// always built, then the alternatives. Only a net that failed ever gets
	// past the first entry, and the deadline gates every one after it.
	var out Outcome
	for _, o := range attempts {
		for i, at := range treeAttempts(pads, seed0, o.exploreTrees) {
			if i > 0 && hasDeadline && time.Now().After(deadline) {
				break
			}
			out = routeNetFrom(board, g, name, pads, at.seed, at.order, already, o, deadline, hasDeadline)
			// A commit rejected for a via site bans that cell; the same search
			// then finds a different barrel. Bounded so a net cannot spin — and
			// only on the first shape, because a ban outlives the net that
			// earned it and hands every later net a smaller board.
			for retry := 0; i == 0 && retry < 3 && out.Reason == "drc-via"; retry++ {
				if hasDeadline && time.Now().After(deadline) {
					break
				}
				out = routeNetFrom(board, g, name, pads, at.seed, at.order, already, o, deadline, hasDeadline)
			}
			if out.Status == "ok" {
				out.TraceSegments += preSegs
				out.LengthMM += preLen
				return out
			}
		}
	}
	if (!hasDeadline || time.Now().Before(deadline)) &&
		(len(pads) <= 4 || netOrderKey(name) <= 1) {
		if j := routeViaJumper(board, g, name, pads, opts, deadline, hasDeadline); j.Status == "ok" {
			j.TraceSegments += preSegs
			j.LengthMM += preLen
			return j
		}
	}
	return out
}

// routeDirect lays straight / L-shape segments when every hop is
// short and geometrically clear of foreign pads. Closes tight nets
// (fecha QG on the SOT-23 cluster) that the grid search over-clears.
func routeDirect(board *core.Board, g *grid, name string, pads []padLoc, opts Options) Outcome {
	if len(pads) < 2 || len(pads) > 6 {
		return Outcome{Status: "failed", Reason: "direct"}
	}
	span := 0.0
	for i := range pads {
		for j := i + 1; j < len(pads); j++ {
			d := manhattanMM(pads[i].p, pads[j].p)
			if d > span {
				span = d
			}
		}
	}
	if span > 25 {
		return Outcome{Status: "failed", Reason: "direct-span"}
	}
	// Every hop this function lays is a single trace on ONE layer, so a net
	// whose pads are not all on the same layer cannot be closed here — it
	// needs a via and this path never emits one. Without this test the fast
	// path drew a bottom-layer trace from the centre of a TOP pad and returned
	// "ok": the router reported the net routed, DRC found the pad with no
	// copper (a *warning*), and `pack` would have shipped an open net. Its
	// sibling routeClearHops has always skipped cross-layer pairs; this is the
	// same rule, hoisted because a mixed-layer net has no legal hop at all.
	for i := range pads {
		if pads[i].layer != pads[0].layer {
			return Outcome{Status: "failed", Reason: "direct-layer"}
		}
	}
	connected := map[int]bool{0: true}
	var segs []core.Trace
	length := 0.0
	for len(connected) < len(pads) {
		bestI, bestJ, bestD := -1, -1, math.MaxFloat64
		for j := range pads {
			if connected[j] {
				continue
			}
			// Walk the pad slice, not the connected set: ranging a Go map
			// picks a different winner between equally-close pads on every
			// run, which is one of the reasons two identical route calls
			// used to disagree.
			for i := range pads {
				if !connected[i] {
					continue
				}
				d := manhattanMM(pads[i].p, pads[j].p)
				if d < bestD {
					bestD, bestI, bestJ = d, i, j
				}
			}
		}
		if bestJ < 0 {
			return Outcome{Status: "failed", Reason: "direct"}
		}
		a, b := pads[bestI].p, pads[bestJ].p
		layer := pads[bestJ].layer
		ax, ay := a.X.ToMM(), a.Y.ToMM()
		bx, by := b.X.ToMM(), b.Y.ToMM()
		var path [][2]float64
		switch {
		case g.segmentPadClear(ax, ay, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {bx, by}}
		case g.segmentPadClear(ax, ay, bx, ay, layer, name, opts) &&
			g.segmentPadClear(bx, ay, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {bx, ay}, {bx, by}}
		case g.segmentPadClear(ax, ay, ax, by, layer, name, opts) &&
			g.segmentPadClear(ax, by, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {ax, by}, {bx, by}}
		default:
			return Outcome{Status: "failed", Reason: "direct-block"}
		}
		for k := 0; k+1 < len(path); k++ {
			s := core.NewPoint(core.FromMM(path[k][0]), core.FromMM(path[k][1]))
			e := core.NewPoint(core.FromMM(path[k+1][0]), core.FromMM(path[k+1][1]))
			segs = append(segs, opts.escapeTraces(name, layer, s, e)...)
			length += hypotMM(s, e)
		}
		connected[bestJ] = true
	}
	snapT := len(board.Traces)
	board.Traces = append(board.Traces, segs...)
	if !copperClearanceFrom(board, snapT, commitClearance(board)) {
		board.Traces = board.Traces[:snapT]
		return Outcome{Status: "failed", Reason: "direct-drc", LowerBoundMM: lowerBoundMM(pads)}
	}
	for _, t := range segs {
		g.blockSegObstacle(t.Start.X, t.Start.Y, t.End.X, t.End.Y, t.Layer.Index, name, t.Width.ToMM()/2)
	}
	return Outcome{Status: "ok", Reason: "direct", TraceSegments: len(segs), LengthMM: length, LowerBoundMM: lowerBoundMM(pads)}
}

// segmentPadClear walks a→b and asks for the gap the trace needs at every
// sample — narrower inside an escape zone, nominal outside.
func (g *grid) segmentPadClear(x0, y0, x1, y1 float64, layer uint8, net string, o Options) bool {
	dx, dy := x1-x0, y1-y0
	n := int(math.Hypot(dx, dy) / 0.08)
	if n < 2 {
		n = 2
	}
	for i := 0; i <= n; i++ {
		t := float64(i) / float64(n)
		x, y := x0+t*dx, y0+t*dy
		if g.nearForeignPad(x, y, layer, net, o.needAt(x, y)) {
			return false
		}
	}
	return true
}

// routeViaJumper drops a via beside each pad and routes the other
// layer — the usual header-to-header escape when the top is packed.
func routeViaJumper(board *core.Board, g *grid, name string, pads []padLoc, opts Options, deadline time.Time, hasDeadline bool) Outcome {
	if len(pads) < 2 {
		return Outcome{Status: "failed", Reason: "jumper"}
	}
	type site struct {
		pad padLoc
		via core.Point
		// noVia: the pad is drilled, so it already reaches both layers.
		// Jumping off it needs no second hole beside its own.
		noVia bool
	}
	var sites []site
	holeMark := g.holes.mark()
	defer func() { g.holes.truncate(holeMark) }()
	dirs := [][2]float64{
		{0, -0.55}, {0, 0.55}, {-0.55, 0}, {0.55, 0},
		{0.55, -0.55}, {-0.55, -0.55}, {0.55, 0.55}, {-0.55, 0.55},
		{0, -0.85}, {0, 0.85}, {-0.85, 0}, {0.85, 0},
		{0, -1.15}, {1.15, 0}, {-1.15, 0}, {0, 1.15},
		{0, -1.55}, {1.55, 0}, {-1.55, 0}, {0, 1.55},
		{0, -2.00}, {2.00, 0}, {-2.00, 0}, {0, 2.00},
	}
	for _, p := range pads {
		if p.through {
			sites = append(sites, site{pad: p, via: p.p, noVia: true})
			continue
		}
		var chosen *core.Point
		px, py := p.p.X.ToMM(), p.p.Y.ToMM()
		for _, d := range dirs {
			x, y := px+d[0], py+d[1]
			if board.Outline != nil && !outlineContains(board.Outline, x, y, 0.35) {
				continue
			}
			if !g.holes.ok(x, y, opts.ViaDrillMM) {
				continue
			}
			if g.nearForeignPad(x, y, p.layer, name, opts.needAt(x, y)) {
				continue
			}
			other := signalOtherLayer(g, p.layer)
			if g.nearForeignPad(x, y, other, name, opts.needAt(x, y)) {
				continue
			}
			pt := core.NewPoint(core.FromMM(x), core.FromMM(y))
			chosen = &pt
			g.holes.add(x, y, opts.ViaDrillMM)
			break
		}
		if chosen == nil {
			return Outcome{Status: "failed", Reason: "jumper-site"}
		}
		sites = append(sites, site{pad: p, via: *chosen})
	}
	// Stubs + vias first so A* can use them as sources.
	var stubs []core.Trace
	var vias []core.Via
	var sources []cellKey
	for _, s := range sites {
		if !s.noVia {
			vias = append(vias, core.Via{
				ID: core.NewID(), Net: name, Position: s.via,
				Drill: core.FromMM(opts.ViaDrillMM), Diameter: core.FromMM(opts.ViaDiameterMM),
			})
			stubs = append(stubs, opts.escapeTraces(name, s.pad.layer, s.pad.p, s.via)...)
		}
		if cx, cy, ok := g.worldToCell(s.via.X, s.via.Y); ok {
			for L := uint8(0); L < uint8(g.layers); L++ {
				sources = append(sources, cellKey{cx, cy, L})
			}
		}
	}
	// Connect via sites on the opposite layer (Prim).
	connected := map[int]bool{0: true}
	var hops []core.Trace
	for len(connected) < len(sites) {
		bestJ, bestD := -1, math.MaxFloat64
		for j := range sites {
			if connected[j] {
				continue
			}
			for i := range sites {
				if !connected[i] {
					continue
				}
				d := manhattanMM(sites[i].via, sites[j].via)
				if d < bestD {
					bestD, bestJ = d, j
				}
			}
		}
		if bestJ < 0 {
			return Outcome{Status: "failed", Reason: "jumper"}
		}
		other := signalOtherLayer(g, sites[bestJ].pad.layer)
		path, ok := g.aStarMulti(sources, sites[bestJ].via, other, name, deadline, hasDeadline)
		if !ok {
			// try the pad's own layer
			path, ok = g.aStarMulti(sources, sites[bestJ].via, sites[bestJ].pad.layer, name, deadline, hasDeadline)
		}
		if !ok || len(path) < 2 {
			return Outcome{Status: "failed", Reason: "jumper-path"}
		}
		for k := 0; k < len(path)-1; k++ {
			a, b := path[k], path[k+1]
			if a.layer != b.layer {
				continue
			}
			hops = append(hops, opts.escapeTraces(name, a.layer,
				core.NewPoint(a.x, a.y), core.NewPoint(b.x, b.y))...)
		}
		if cx, cy, ok := g.worldToCell(sites[bestJ].via.X, sites[bestJ].via.Y); ok {
			for L := uint8(0); L < uint8(g.layers); L++ {
				sources = append(sources, cellKey{cx, cy, L})
			}
		}
		connected[bestJ] = true
	}
	snapT, snapV := len(board.Traces), len(board.Vias)
	board.Traces = append(board.Traces, stubs...)
	board.Traces = append(board.Traces, hops...)
	board.Vias = append(board.Vias, vias...)
	if !copperClearanceFrom(board, snapT, commitClearance(board)) ||
		!viaClearanceFrom(board, snapV, commitClearance(board)) {
		board.Traces = board.Traces[:snapT]
		board.Vias = board.Vias[:snapV]
		return Outcome{Status: "failed", Reason: "jumper-drc"}
	}
	for _, t := range stubs {
		g.blockSegObstacle(t.Start.X, t.Start.Y, t.End.X, t.End.Y, t.Layer.Index, name, t.Width.ToMM()/2)
	}
	for _, t := range hops {
		g.blockSegObstacle(t.Start.X, t.Start.Y, t.End.X, t.End.Y, t.Layer.Index, name, t.Width.ToMM()/2)
	}
	for _, v := range vias {
		g.blockViaObstacle(v.Position.X, v.Position.Y, name, v.Diameter.ToMM()/2, v.Drill.ToMM())
	}
	length := 0.0
	for _, t := range stubs {
		length += hypotMM(t.Start, t.End)
	}
	for _, t := range hops {
		length += hypotMM(t.Start, t.End)
	}
	return Outcome{Status: "ok", Reason: "jumper", TraceSegments: len(stubs) + len(hops), LengthMM: length}
}

func routeClearHops(board *core.Board, g *grid, name string, pads []padLoc, opts Options) (map[int]bool, int, float64) {
	connected := map[int]bool{}
	if len(pads) < 2 {
		return connected, 0, 0
	}
	parent := make([]int, len(pads))
	for i := range parent {
		parent[i] = i
	}
	var find func(int) int
	find = func(x int) int {
		if parent[x] != x {
			parent[x] = find(parent[x])
		}
		return parent[x]
	}
	unite := func(a, b int) {
		ra, rb := find(a), find(b)
		if ra != rb {
			parent[rb] = ra
		}
	}
	type pair struct {
		i, j int
		d    float64
	}
	var pairs []pair
	for i := 0; i < len(pads); i++ {
		for j := i + 1; j < len(pads); j++ {
			if pads[i].layer != pads[j].layer {
				continue
			}
			d := manhattanMM(pads[i].p, pads[j].p)
			if d > 8 {
				continue
			}
			pairs = append(pairs, pair{i, j, d})
		}
	}
	sort.Slice(pairs, func(a, b int) bool { return pairs[a].d < pairs[b].d })
	segs := 0
	length := 0.0
	for _, pr := range pairs {
		if find(pr.i) == find(pr.j) {
			continue
		}
		a, b := pads[pr.i].p, pads[pr.j].p
		layer := pads[pr.i].layer
		ax, ay := a.X.ToMM(), a.Y.ToMM()
		bx, by := b.X.ToMM(), b.Y.ToMM()
		var path [][2]float64
		switch {
		case g.segmentPadClear(ax, ay, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {bx, by}}
		case g.segmentPadClear(ax, ay, bx, ay, layer, name, opts) &&
			g.segmentPadClear(bx, ay, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {bx, ay}, {bx, by}}
		case g.segmentPadClear(ax, ay, ax, by, layer, name, opts) &&
			g.segmentPadClear(ax, by, bx, by, layer, name, opts):
			path = [][2]float64{{ax, ay}, {ax, by}, {bx, by}}
		default:
			continue
		}
		snapT := len(board.Traces)
		for k := 0; k+1 < len(path); k++ {
			s := core.NewPoint(core.FromMM(path[k][0]), core.FromMM(path[k][1]))
			e := core.NewPoint(core.FromMM(path[k+1][0]), core.FromMM(path[k+1][1]))
			board.Traces = append(board.Traces, opts.escapeTraces(name, layer, s, e)...)
		}
		if !copperClearanceFrom(board, snapT, commitClearance(board)) {
			board.Traces = board.Traces[:snapT]
			continue
		}
		for _, t := range board.Traces[snapT:] {
			g.blockSegObstacle(t.Start.X, t.Start.Y, t.End.X, t.End.Y, t.Layer.Index, t.Net, t.Width.ToMM()/2)
			length += hypotMM(t.Start, t.End)
			segs++
		}
		unite(pr.i, pr.j)
	}
	groups := map[int][]int{}
	for i := range pads {
		groups[find(i)] = append(groups[find(i)], i)
	}
	// Largest cluster wins, lowest pad index breaks a tie. Ranging the map
	// picked a different cluster of the same size on every run, and which
	// cluster seeds the tree changes the copper the net ends up with.
	best, bestN := -1, 1
	for i := range pads {
		r := find(i)
		if r != i {
			continue
		}
		if len(groups[r]) > bestN {
			best, bestN = r, len(groups[r])
		}
	}
	if best >= 0 {
		for _, i := range groups[best] {
			connected[i] = true
		}
	}
	return connected, segs, length
}

func routeNetFrom(board *core.Board, g *grid, name string, pads []padLoc, seed int, order growthOrder, already map[int]bool, opts Options, deadline time.Time, hasDeadline bool) Outcome {
	defer func(v float64, s bool) {
		g.opts.ViaCost = v
		g.opts.sameLayerOnly = s
	}(g.opts.ViaCost, g.opts.sameLayerOnly)
	g.opts.ViaCost = opts.ViaCost
	g.opts.sameLayerOnly = opts.sameLayerOnly
	lb := lowerBoundMM(pads)

	// Prim growth: connected set starts with pad 0; multi-source A* from
	// all same-net tree cells to the nearest unconnected pad.
	if seed < 0 || seed >= len(pads) {
		seed = 0
	}
	connected := map[int]bool{seed: true}
	for i, ok := range already {
		if ok && i >= 0 && i < len(pads) {
			connected[i] = true
		}
	}
	sources := existingNetSources(board, g, name, pads, connected)
	sources = append(sources, g.padSources(pads[seed])...)

	type pendingVia struct {
		v core.Via
	}
	// One path run may split into a necked stretch and a nominal one.
	var pendT []core.Trace
	var pendV []pendingVia
	type paintRec struct {
		x, y int
		l    uint8
		prev string
	}
	var paints []paintRec
	recordPaint := func(cx, cy int, layer uint8, net string, radius int) {
		for dy := -radius; dy <= radius; dy++ {
			for dx := -radius; dx <= radius; dx++ {
				x, y := cx+dx, cy+dy
				if x < 0 || y < 0 || x >= g.w || y >= g.h {
					continue
				}
				idx := y*g.w + x
				cur := g.blocked[layer][idx]
				// Own-net and free cells may be claimed; never overwrite foreign or hard obstacles.
				if cur == "" || cur == net {
					paints = append(paints, paintRec{x, y, layer, cur})
					g.blocked[layer][idx] = net
				}
			}
		}
	}
	rollback := func() {
		for i := len(paints) - 1; i >= 0; i-- {
			p := paints[i]
			g.blocked[p.l][p.y*g.w+p.x] = p.prev
		}
		paints = nil
	}
	// holeMark/rollbackHoles: pending vias are provisional drills. They are
	// registered as the path is built so two vias of the same net keep their
	// gap, then dropped again if the net is abandoned (the commit loop
	// re-registers the ones that survive).

	copperR := g.copperRadius()
	holeMark := g.holes.mark()
	addVia := func(v core.Via) {
		pendV = append(pendV, pendingVia{v: v})
		g.holes.add(v.Position.X.ToMM(), v.Position.Y.ToMM(), v.Drill.ToMM())
	}
	segs := 0
	length := 0.0
	failed := false
	reason := "unreachable"

	// Terminals whose hop found no path in the current sweep. A branch that
	// cannot be laid now is not the same thing as a net that cannot be laid:
	// the branches still owed copper will widen the tree, and A* searches
	// from every cell of it. One reprieve each — a branch that still finds
	// nothing with the rest of the tree down is not going to come good, and
	// a fruitless A* walks the whole grid before it says so.
	deferred := map[int]bool{}
	tries := make([]int, len(pads))
	grew := false

	for len(connected) < len(pads) {
		if hasDeadline && time.Now().After(deadline) {
			failed = true
			reason = "budget"
			break
		}
		bestJ := nextTerminal(pads, connected, deferred, order)
		if bestJ < 0 {
			unparked := false
			if grew {
				for j := range pads {
					if deferred[j] && tries[j] < maxBranchTries {
						delete(deferred, j)
						unparked = true
					}
				}
			}
			if !unparked {
				failed = true
				break
			}
			grew = false
			continue
		}
		tries[bestJ]++

		goalLayer := pads[bestJ].layer
		goalP := pads[bestJ].p
		// A drilled pad is copper on every layer: arriving on either
		// side lands on the pin, and no via is owed.
		anyLayer := pads[bestJ].through
		// Prefer a fanout via sitting off this pad — it sits outside
		// the QFN body and is reachable on either layer.
		if v, ok := closestNetVia(board, name, goalP, pads, bestJ); ok {
			goalP, anyLayer = v, true
		}
		path, ok := g.aStarMultiAt(sources, goalP, goalLayer, anyLayer, name, deadline, hasDeadline)
		if !ok && !anyLayer && !opts.sameLayerOnly {
			path, ok = g.aStarMulti(sources, goalP, signalOtherLayer(g, goalLayer), name, deadline, hasDeadline)
		}
		if !ok {
			// A* also returns "no path" when it runs out of clock. Saying
			// "unreachable" for that is a lie that sends the next reader
			// hunting for a geometric blockage that is not there.
			timedOut := g.timedOut || (hasDeadline && time.Now().After(deadline))
			if timedOut {
				reason = fmt.Sprintf("budget:%s.p%d(%.1f,%.1f) from %d", name, bestJ,
					pads[bestJ].p.X.ToMM(), pads[bestJ].p.Y.ToMM(), len(connected))
			} else {
				reason = fmt.Sprintf("unreachable:%s.p%d(%.1f,%.1f) from %d", name, bestJ,
					pads[bestJ].p.X.ToMM(), pads[bestJ].p.Y.ToMM(), len(connected))
			}
			if timedOut || !opts.exploreTrees {
				failed = true
				break
			}
			// Park this branch and lay the others first — the tree they add
			// is more copper to search from, and A* is multi-source.
			deferred[bestJ] = true
			continue
		}
		if len(path) < 2 {
			sources = append(sources, g.padSources(pads[bestJ])...)
			connected[bestJ] = true
			grew = true
			continue
		}

		// Do NOT rewrite path ends to pad centres: a Theta* parent
		// 10 mm away would then chord through a neighbour pad
		// (fecha NRESET/NSS vs U2 header, 0.16 mm DRC).
		for k := 0; k < len(path)-1; k++ {
			a, b := path[k], path[k+1]
			if a.layer != b.layer {
				addVia(core.Via{
					ID:       core.NewID(),
					Position: core.NewPoint(a.x, a.y),
					Drill:    core.FromMM(opts.ViaDrillMM),
					Diameter: core.FromMM(opts.ViaDiameterMM),
					Net:      name,
				})
				cx, cy, okc := g.worldToCell(a.x, a.y)
				if okc {
					for L := uint8(0); L < uint8(g.layers); L++ {
						recordPaint(cx, cy, L, name, copperR)
						sources = append(sources, cellKey{cx, cy, L})
					}
				}
				continue
			}
			pendT = append(pendT, opts.escapeTraces(name, a.layer,
				core.NewPoint(a.x, a.y), core.NewPoint(b.x, b.y))...)
			c0x, c0y, ok0 := g.worldToCell(a.x, a.y)
			c1x, c1y, ok1 := g.worldToCell(b.x, b.y)
			if ok0 || ok1 {
				if !ok0 {
					c0x, c0y = c1x, c1y
				}
				if !ok1 {
					c1x, c1y = c0x, c0y
				}
				for _, c := range bresenham(c0x, c0y, c1x, c1y) {
					recordPaint(c[0], c[1], a.layer, name, copperR)
					sources = append(sources, cellKey{c[0], c[1], a.layer})
				}
			}
			segs++
			length += hypotMM(core.NewPoint(a.x, a.y), core.NewPoint(b.x, b.y))
		}
		// Always land a same-layer stub on the pad centre. A last
		// step that is only a via (layer flip at the pad cell) used
		// to leave U1's pad as its own island.
		padP := pads[bestJ].p
		from := path[len(path)-1]
		if from.layer != goalLayer && !pads[bestJ].through {
			addVia(core.Via{
				ID: core.NewID(), Position: core.NewPoint(from.x, from.y),
				Drill: core.FromMM(opts.ViaDrillMM), Diameter: core.FromMM(opts.ViaDiameterMM), Net: name,
			})
			from.layer = goalLayer
		}
		goalLayer = from.layer
		if from.x != padP.X || from.y != padP.Y {
			fx, fy := from.x.ToMM(), from.y.ToMM()
			px, py := padP.X.ToMM(), padP.Y.ToMM()
			if g.segmentPadClear(fx, fy, px, py, goalLayer, name, opts) {
				pendT = append(pendT, opts.escapeTraces(name, goalLayer,
					core.NewPoint(from.x, from.y), padP)...)
				segs++
				length += hypotMM(core.NewPoint(from.x, from.y), padP)
			}
		}
		sources = append(sources, g.padSources(pads[bestJ])...)
		connected[bestJ] = true
		grew = true
		// Pick up this pad's fanout via now that it is in the tree.
		sources = append(sources, existingNetSources(board, g, name, pads, map[int]bool{bestJ: true})...)
	}

	if failed {
		rollback()
		g.holes.truncate(holeMark)
		return Outcome{Status: "failed", Reason: reason, LowerBoundMM: lb}
	}
	g.holes.truncate(holeMark)

	// Commit copper and expand grid halos to full clearance so later nets
	// treat this net as a foreign obstacle (search paint was bare half-width).
	snapT, snapV := len(board.Traces), len(board.Vias)
	for _, tr := range pendT {
		board.Traces = append(board.Traces, tr)
		g.blockSegObstacle(tr.Start.X, tr.Start.Y, tr.End.X, tr.End.Y, tr.Layer.Index, tr.Net, tr.Width.ToMM()/2)
	}
	for _, p := range pendV {
		board.Vias = append(board.Vias, p.v)
		g.blockViaObstacle(p.v.Position.X, p.v.Position.Y, p.v.Net, p.v.Diameter.ToMM()/2, p.v.Drill.ToMM())
	}
	if bad := viaClearanceOffenders(board, snapV, commitClearance(board)); len(bad) > 0 {
		// The grid clearance disk is an approximation of the exact rule, so
		// A* can still propose a barrel the commit refuses. Ban those sites
		// and let the caller ask again rather than losing the whole net.
		var banned [][2]int
		for _, i := range bad {
			if cx, cy, ok := g.worldToCell(board.Vias[i].Position.X, board.Vias[i].Position.Y); ok {
				banned = append(banned, [2]int{cx, cy})
			}
		}
		board.Traces = board.Traces[:snapT]
		board.Vias = board.Vias[:snapV]
		bans := g.viaBan
		*g = *newGrid(board, g.opts)
		g.viaBan = bans
		for _, c := range banned {
			g.banVia(c[0], c[1])
		}
		reason := "drc"
		if len(banned) > 0 {
			reason = "drc-via" // retryable: routeNetAt asks again
		}
		return Outcome{Status: "failed", Reason: reason, LowerBoundMM: lb}
	}
	if !copperClearanceFrom(board, snapT, commitClearance(board)) {
		board.Traces = board.Traces[:snapT]
		board.Vias = board.Vias[:snapV]
		*g = *newGrid(board, g.opts)
		return Outcome{Status: "failed", Reason: "drc", LowerBoundMM: lb}
	}
	if padIslands(board, name, pads) > 1 {
		board.Traces = board.Traces[:snapT]
		board.Vias = board.Vias[:snapV]
		*g = *newGrid(board, g.opts)
		return Outcome{Status: "failed", Reason: "split", LowerBoundMM: lb}
	}
	return Outcome{
		Status: "ok", TraceSegments: segs, LengthMM: length, LowerBoundMM: lb,
	}
}

// padIslands counts electrically separate groups of pads on net.
// padIslands counts electrically separate groups of pads on net. Copper
// joins where it *touches*, not only where endpoints coincide: a branch that
// leaves the middle of a trunk (which is what multi-source growth on a power
// rail produces) is a T-junction, and an endpoint-only union-find used to
// report those perfectly good rails as split nets.
func padIslands(board *core.Board, net string, pads []padLoc) int {
	if len(pads) < 2 {
		return 1
	}
	const touch = 0.40
	type pt struct{ x, y float64 }
	var nodes []pt
	add := func(x, y float64) int {
		for i, n := range nodes {
			if math.Hypot(n.x-x, n.y-y) < touch {
				return i
			}
		}
		nodes = append(nodes, pt{x, y})
		return len(nodes) - 1
	}
	for _, p := range pads {
		add(p.p.X.ToMM(), p.p.Y.ToMM())
	}
	nPad := len(pads)
	type seg struct {
		a, b [2]float64
		ia   int
		ib   int
	}
	var segs []seg
	for _, t := range board.Traces {
		if t.Net != net {
			continue
		}
		ax, ay := t.Start.X.ToMM(), t.Start.Y.ToMM()
		bx, by := t.End.X.ToMM(), t.End.Y.ToMM()
		segs = append(segs, seg{a: [2]float64{ax, ay}, b: [2]float64{bx, by}, ia: add(ax, ay), ib: add(bx, by)})
	}
	for _, v := range board.Vias {
		if v.Net == net {
			add(v.Position.X.ToMM(), v.Position.Y.ToMM())
		}
	}
	parent := make([]int, len(nodes))
	for i := range parent {
		parent[i] = i
	}
	var find func(int) int
	find = func(x int) int {
		if parent[x] != x {
			parent[x] = find(parent[x])
		}
		return parent[x]
	}
	unite := func(a, b int) {
		ra, rb := find(a), find(b)
		if ra != rb {
			parent[rb] = ra
		}
	}
	for i := 0; i < len(nodes); i++ {
		for j := i + 1; j < len(nodes); j++ {
			if math.Hypot(nodes[i].x-nodes[j].x, nodes[i].y-nodes[j].y) < touch {
				unite(i, j)
			}
		}
	}
	for _, sg := range segs {
		unite(sg.ia, sg.ib)
	}
	// T-junctions: a node sitting anywhere along a segment is copper on it.
	for _, sg := range segs {
		for i := range nodes {
			if find(i) == find(sg.ia) {
				continue
			}
			if pointSegDist(p2{nodes[i].x, nodes[i].y}, p2(sg.a), p2(sg.b)) < touch {
				unite(sg.ia, i)
			}
		}
	}
	// Crossings: two segments of the same net that meet away from any node.
	for i := 0; i < len(segs); i++ {
		for j := i + 1; j < len(segs); j++ {
			if find(segs[i].ia) == find(segs[j].ia) {
				continue
			}
			if segSegDist(p2(segs[i].a), p2(segs[i].b), p2(segs[j].a), p2(segs[j].b)) < touch {
				unite(segs[i].ia, segs[j].ia)
			}
		}
	}
	seen := map[int]bool{}
	for i := 0; i < nPad; i++ {
		seen[find(i)] = true
	}
	return len(seen)
}

func lowerBoundMM(pads []padLoc) float64 {
	if len(pads) < 2 {
		return 0
	}
	in := make([]bool, len(pads))
	in[0] = true
	total := 0.0
	for n := 1; n < len(pads); n++ {
		best := math.MaxFloat64
		bestJ := -1
		for j := range pads {
			if in[j] {
				continue
			}
			for i := range pads {
				if !in[i] {
					continue
				}
				d := manhattanMM(pads[i].p, pads[j].p)
				if d < best {
					best, bestJ = d, j
				}
			}
		}
		if bestJ < 0 {
			break
		}
		in[bestJ] = true
		total += best
	}
	return total
}

func manhattanMM(a, b core.Point) float64 {
	dx := math.Abs(float64(a.X-b.X)) / 1e6
	dy := math.Abs(float64(a.Y-b.Y)) / 1e6
	return dx + dy
}

func hypotMM(a, b core.Point) float64 {
	dx := float64(a.X-b.X) / 1e6
	dy := float64(a.Y-b.Y) / 1e6
	return math.Hypot(dx, dy)
}

// copperClearanceLegal reports whether every pair of different-net traces
// on the same layer (and every trace vs foreign pad) keeps minClearanceMM.
// commitClearance is the DRC floor used when accepting a net. Honours
// a fine-pitch rule area (0.12 mm on the RP2040 QFN) so escapes that
// DRC will accept are not rejected by a flat 0.20 mm gate.
func commitClearance(board *core.Board) float64 {
	fab := core.ActiveFabRules(board)
	floor := fab.MinClearanceMM
	if floor <= 0 {
		floor = 0.127
	}
	return floor
}

func copperClearanceLegal(board *core.Board, minClearanceMM float64) bool {
	return copperClearanceFrom(board, 0, minClearanceMM)
}

// copperClearanceFrom checks only traces[fromT:] against earlier copper
// and pads. Used at commit so a slightly-tight fanout stub cannot veto
// every later net.
func copperClearanceFrom(board *core.Board, fromT int, minClearanceMM float64) bool {
	trs := board.Traces
	if fromT < 0 {
		fromT = 0
	}
	res := &core.RuleResolver{Defaults: core.DefaultRules(), Areas: nil}
	if board != nil {
		res.Areas = board.RuleAreas
		if fab := core.ActiveFabRules(board); fab.MinClearanceMM > 0 {
			res.Defaults.Clearance = core.FromMM(fab.MinClearanceMM)
		}
	}
	needAt := func(x, y float64, netA, netB string) float64 {
		n := res.ClearanceBetween(core.NewPoint(core.FromMM(x), core.FromMM(y)), netA, netB).ToMM()
		if n < minClearanceMM {
			n = minClearanceMM
		}
		if fab := core.ActiveFabRules(board); fab.MinClearanceMM > 0 && n < fab.MinClearanceMM {
			n = fab.MinClearanceMM
		}
		return n
	}
	for i := fromT; i < len(trs); i++ {
		a := trs[i]
		ax0, ay0 := a.Start.X.ToMM(), a.Start.Y.ToMM()
		ax1, ay1 := a.End.X.ToMM(), a.End.Y.ToMM()
		mx, my := (ax0+ax1)/2, (ay0+ay1)/2
		for j := 0; j < i; j++ {
			b := trs[j]
			if a.Layer.Index != b.Layer.Index || a.Net == b.Net {
				continue
			}
			gap := segSegDistMM(
				[2]float64{ax0, ay0}, [2]float64{ax1, ay1},
				[2]float64{b.Start.X.ToMM(), b.Start.Y.ToMM()},
				[2]float64{b.End.X.ToMM(), b.End.Y.ToMM()},
			) - a.Width.ToMM()/2 - b.Width.ToMM()/2
			if gap+1e-6 < needAt(mx, my, a.Net, b.Net) {
				return false
			}
		}
		half := a.Width.ToMM() / 2
		aa := [2]float64{ax0, ay0}
		bb := [2]float64{ax1, ay1}
		// Vias are copper on every layer, so a new trace has to clear them
		// too — the mirror of viaClearanceFrom, and just as missing before.
		for _, v := range board.Vias {
			if v.Net == a.Net {
				continue
			}
			gap := distPointSeg(v.Position.X.ToMM(), v.Position.Y.ToMM(), ax0, ay0, ax1, ay1) -
				half - v.Diameter.ToMM()/2
			if gap+1e-6 < needAt(mx, my, a.Net, v.Net) {
				return false
			}
		}
		for _, fp := range footprintsStable(board) {
			for k := range fp.Pads {
				pad := &fp.Pads[k]
				if pad.Net != nil && *pad.Net == a.Net {
					continue
				}
				if pad.Layer.Index != a.Layer.Index && (pad.Drill == nil || *pad.Drill == 0) {
					continue
				}
				aabb := core.PadWorldAABB(fp, pad)
				d := segAABBDistMM(aa, bb, aabb)
				on := ""
				if pad.Net != nil {
					on = *pad.Net
				}
				if d-half+1e-6 < needAt(mx, my, a.Net, on) {
					return false
				}
			}
		}
	}
	return true
}

// viaClearanceFrom checks vias[fromV:] — the barrels a commit is about to
// keep — against every piece of foreign copper already on the board. A via
// spans the whole stack, so layer never exempts it. copperClearanceFrom only
// ever looked at traces, so until now nothing validated a via at commit and
// the pour stitcher happily dropped one on top of a signal.
func viaClearanceFrom(board *core.Board, fromV int, minClearanceMM float64) bool {
	return len(viaClearanceOffenders(board, fromV, minClearanceMM)) == 0
}

// viaClearanceOffenders returns the indexes of vias[fromV:] that break the
// rule, so a caller can ban those sites and try the net again instead of
// simply losing it.
func viaClearanceOffenders(board *core.Board, fromV int, minClearanceMM float64) []int {
	if board == nil || fromV >= len(board.Vias) {
		return nil
	}
	if fromV < 0 {
		fromV = 0
	}
	res := &core.RuleResolver{Defaults: core.DefaultRules(), Areas: board.RuleAreas}
	if fab := core.ActiveFabRules(board); fab.MinClearanceMM > 0 {
		res.Defaults.Clearance = core.FromMM(fab.MinClearanceMM)
	}
	need := func(x, y float64, netA, netB string) float64 {
		n := res.ClearanceBetween(core.NewPoint(core.FromMM(x), core.FromMM(y)), netA, netB).ToMM()
		if n < minClearanceMM {
			n = minClearanceMM
		}
		return n
	}
	var bad []int
	for i := fromV; i < len(board.Vias); i++ {
		v := board.Vias[i]
		vx, vy := v.Position.X.ToMM(), v.Position.Y.ToMM()
		vr := v.Diameter.ToMM() / 2
		hit := false
		for _, tr := range board.Traces {
			if tr.Net == v.Net {
				continue
			}
			gap := distPointSeg(vx, vy, tr.Start.X.ToMM(), tr.Start.Y.ToMM(), tr.End.X.ToMM(), tr.End.Y.ToMM()) -
				vr - tr.Width.ToMM()/2
			if gap+1e-6 < need(vx, vy, v.Net, tr.Net) {
				hit = true
				break
			}
		}
		for j := 0; !hit && j < len(board.Vias); j++ {
			if j == i {
				continue
			}
			w := board.Vias[j]
			if w.Net == v.Net {
				continue
			}
			gap := math.Hypot(w.Position.X.ToMM()-vx, w.Position.Y.ToMM()-vy) - vr - w.Diameter.ToMM()/2
			if gap+1e-6 < need(vx, vy, v.Net, w.Net) {
				hit = true
			}
		}
		for _, fp := range footprintsStable(board) {
			if hit {
				break
			}
			for k := range fp.Pads {
				pad := &fp.Pads[k]
				on := ""
				if pad.Net != nil {
					on = *pad.Net
				}
				if on == v.Net {
					continue
				}
				aabb := core.PadWorldAABB(fp, pad)
				d := pointRectDist(vx, vy,
					aabb.Min.X.ToMM(), aabb.Min.Y.ToMM(), aabb.Max.X.ToMM(), aabb.Max.Y.ToMM()) - vr
				if d+1e-6 < need(vx, vy, v.Net, on) {
					hit = true
					break
				}
			}
		}
		if hit {
			bad = append(bad, i)
		}
	}
	return bad
}

func segSegDistMM(a0, a1, b0, b1 [2]float64) float64 {
	if segsIntersect(p2(a0), p2(a1), p2(b0), p2(b1)) {
		return 0
	}
	return math.Min(
		math.Min(pointSegDist(p2(a0), p2(b0), p2(b1)), pointSegDist(p2(a1), p2(b0), p2(b1))),
		math.Min(pointSegDist(p2(b0), p2(a0), p2(a1)), pointSegDist(p2(b1), p2(a0), p2(a1))),
	)
}

func segAABBDistMM(a, b [2]float64, r core.Rect) float64 {
	rx0, ry0 := r.Min.X.ToMM(), r.Min.Y.ToMM()
	rx1, ry1 := r.Max.X.ToMM(), r.Max.Y.ToMM()
	inside := func(p [2]float64) bool {
		return p[0] >= rx0 && p[0] <= rx1 && p[1] >= ry0 && p[1] <= ry1
	}
	if inside(a) || inside(b) {
		return 0
	}
	corners := [4][2]float64{{rx0, ry0}, {rx1, ry0}, {rx1, ry1}, {rx0, ry1}}
	best := math.Inf(1)
	for i := 0; i < 4; i++ {
		d := segSegDistMM(a, b, corners[i], corners[(i+1)%4])
		if d < best {
			best = d
		}
	}
	return best
}

// --- organic string-pull ---

type p2 [2]float64

func ptMM(p core.Point) p2 {
	return p2{p.X.ToMM(), p.Y.ToMM()}
}

func dist2(a, b p2) float64 {
	return math.Hypot(a[0]-b[0], a[1]-b[1])
}

// organicPass short-cuts polylines when clearance still holds (Rust organic string-pull core).
func organicPass(board *core.Board, opts Options) {
	if board.Outline == nil {
		return
	}
	// Stable net list from copper insertion order.
	var nets []string
	seen := map[string]bool{}
	for _, tr := range board.Traces {
		if !seen[tr.Net] {
			seen[tr.Net] = true
			nets = append(nets, tr.Net)
		}
	}
	sort.Strings(nets)

	before := append([]core.Trace(nil), board.Traces...)
	clr := opts.ClearanceMM

	for _, net := range nets {
		if netOrderKey(net) <= 1 {
			continue // keep power trees; string-pull was splitting +1V1
		}
		// Layers that actually have copper for this net.
		layerSet := map[uint8]bool{}
		for _, tr := range board.Traces {
			if tr.Net == net {
				layerSet[tr.Layer.Index] = true
			}
		}
		var layers []uint8
		for L := range layerSet {
			layers = append(layers, L)
		}
		sort.Slice(layers, func(i, j int) bool { return layers[i] < layers[j] })

		for _, layer := range layers {
			obs := collectObstacles(board, net, layer, opts)
			chains := extractChains(board, net, layer)
			for _, ch := range chains {
				if len(ch.pts) < 3 {
					continue
				}
				hw := ch.width.ToMM() / 2
				pts := make([]p2, len(ch.pts))
				for i, p := range ch.pts {
					pts[i] = ptMM(p)
				}
				// Pin vertices that land on same-net pads so string-pull
				// cannot drop a tree spoke (e.g. L-junction at the middle pad).
				pinned := padPinnedVertices(board, net, pts)
				pulled := stringPullPinned(pts, pinned, obs, hw, clr)
				if len(pulled) >= 2 && polylineClear(pulled, obs, hw, clr) {
					replaceChain(board, net, layer, ch, pulled)
				}
			}
		}
	}

	// Final legality sweep: roll back whole pass if any chain collides.
	for _, net := range nets {
		layerSet := map[uint8]bool{}
		for _, tr := range board.Traces {
			if tr.Net == net {
				layerSet[tr.Layer.Index] = true
			}
		}
		for layer := range layerSet {
			obs := collectObstacles(board, net, layer, opts)
			for _, ch := range extractChains(board, net, layer) {
				pts := make([]p2, len(ch.pts))
				for i, p := range ch.pts {
					pts[i] = ptMM(p)
				}
				hw := ch.width.ToMM() / 2
				if !polylineClear(pts, obs, hw, clr) {
					board.Traces = before
					return
				}
			}
		}
	}
}

type obstacle struct {
	// kind: rect (pad), capsule (trace), circle (via)
	kind  int // 0 rect, 1 capsule, 2 circle
	min   p2
	max   p2
	a, b  p2
	halfW float64
	c     p2
	r     float64
	clrMM float64
	net   string
}

type obstacleSet struct {
	items   []obstacle
	outline core.Rect
	edgeMM  float64
}

func collectObstacles(board *core.Board, net string, layer uint8, opts Options) *obstacleSet {
	os := &obstacleSet{
		outline: *board.Outline,
		edgeMM:  0.3,
	}
	clr := opts.ClearanceMM
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net != nil && *pad.Net == net {
				continue
			}
			// SMD only on its layer; through-hole on all.
			if pad.Drill == nil || *pad.Drill <= 0 {
				if pad.Layer.Index != layer {
					continue
				}
			}
			c := core.PadWorldCenter(fp, pad)
			cm := ptMM(c)
			hw := pad.Size[0].ToMM() / 2
			hh := pad.Size[1].ToMM() / 2
			padClr := clr
			n := ""
			if pad.Net != nil {
				n = *pad.Net
			}
			os.items = append(os.items, obstacle{
				kind:  0,
				min:   p2{cm[0] - hw, cm[1] - hh},
				max:   p2{cm[0] + hw, cm[1] + hh},
				clrMM: padClr,
				net:   n,
			})
		}
	}
	for _, tr := range board.Traces {
		if tr.Net == net || tr.Layer.Index != layer {
			continue
		}
		os.items = append(os.items, obstacle{
			kind:  1,
			a:     ptMM(tr.Start),
			b:     ptMM(tr.End),
			halfW: tr.Width.ToMM() / 2,
			clrMM: clr,
			net:   tr.Net,
		})
	}
	for _, v := range board.Vias {
		if v.Net == net {
			continue
		}
		os.items = append(os.items, obstacle{
			kind:  2,
			c:     ptMM(v.Position),
			r:     v.Diameter.ToMM() / 2,
			clrMM: clr,
			net:   v.Net,
		})
	}
	for _, k := range board.Keepouts {
		if k.Rect == nil {
			continue
		}
		// Place-only keepouts do not block copper (match grid stamp).
		if k.NoPlace && !k.NoCopper {
			continue
		}
		os.items = append(os.items, obstacle{
			kind:  0,
			min:   ptMM(k.Rect.Min),
			max:   ptMM(k.Rect.Max),
			clrMM: 0,
		})
	}
	return os
}

func pointSegDist(p, a, b p2) float64 {
	ab0, ab1 := b[0]-a[0], b[1]-a[1]
	len2 := ab0*ab0 + ab1*ab1
	if len2 <= 1e-18 {
		return dist2(p, a)
	}
	t := ((p[0]-a[0])*ab0 + (p[1]-a[1])*ab1) / len2
	if t < 0 {
		t = 0
	} else if t > 1 {
		t = 1
	}
	return dist2(p, p2{a[0] + t*ab0, a[1] + t*ab1})
}

func segsIntersect(a, b, c, d p2) bool {
	orient := func(p, q, r p2) float64 {
		return (q[0]-p[0])*(r[1]-p[1]) - (q[1]-p[1])*(r[0]-p[0])
	}
	o1, o2 := orient(a, b, c), orient(a, b, d)
	o3, o4 := orient(c, d, a), orient(c, d, b)
	return ((o1 > 0) != (o2 > 0)) && ((o3 > 0) != (o4 > 0))
}

func segSegDist(a, b, c, d p2) float64 {
	if segsIntersect(a, b, c, d) {
		return 0
	}
	return math.Min(
		math.Min(pointSegDist(a, c, d), pointSegDist(b, c, d)),
		math.Min(pointSegDist(c, a, b), pointSegDist(d, a, b)),
	)
}

func segRectDist(a, b, min, max p2) float64 {
	inside := func(p p2) bool {
		return p[0] >= min[0] && p[0] <= max[0] && p[1] >= min[1] && p[1] <= max[1]
	}
	if inside(a) || inside(b) {
		return 0
	}
	corners := [4]p2{
		{min[0], min[1]}, {max[0], min[1]}, {max[0], max[1]}, {min[0], max[1]},
	}
	best := math.Inf(1)
	for i := 0; i < 4; i++ {
		d := segSegDist(a, b, corners[i], corners[(i+1)%4])
		if d < best {
			best = d
		}
		if best == 0 {
			return 0
		}
	}
	return best
}

func (os *obstacleSet) approachClear(a, b p2, hw, clr float64) bool {
	// Outline band: centreline must stay inside outline shrunk by hw+edge.
	edge := hw + os.edgeMM
	omin := p2{os.outline.Min.X.ToMM() + edge, os.outline.Min.Y.ToMM() + edge}
	omax := p2{os.outline.Max.X.ToMM() - edge, os.outline.Max.Y.ToMM() - edge}
	if omax[0] > omin[0] && omax[1] > omin[1] {
		// Both endpoints and a few samples must stay inside.
		for _, t := range []float64{0, 0.5, 1} {
			p := p2{a[0] + t*(b[0]-a[0]), a[1] + t*(b[1]-a[1])}
			if p[0] < omin[0] || p[0] > omax[0] || p[1] < omin[1] || p[1] > omax[1] {
				return false
			}
		}
	}
	const eps = 1e-4
	for _, ob := range os.items {
		need := hw + math.Max(clr, ob.clrMM)
		var d float64
		switch ob.kind {
		case 0: // rect pad / keepout
			d = segRectDist(a, b, ob.min, ob.max)
		case 1: // foreign trace capsule
			d = segSegDist(a, b, ob.a, ob.b) - ob.halfW
			if d < 0 {
				d = 0
			}
		case 2: // via
			d = pointSegDist(ob.c, a, b) - ob.r
			if d < 0 {
				d = 0
			}
		}
		if d+eps < need {
			return false
		}
	}
	return true
}

func polylineClear(pts []p2, obs *obstacleSet, hw, clr float64) bool {
	for i := 0; i+1 < len(pts); i++ {
		if !obs.approachClear(pts[i], pts[i+1], hw, clr) {
			return false
		}
	}
	return true
}

// firstBlockingNet is the first foreign net a polyline hits — used by
// the topological engine to pick a rip-up victim.
func (os *obstacleSet) firstBlockingNet(pts []p2, hw, clr float64) string {
	if os == nil {
		return ""
	}
	for i := 0; i+1 < len(pts); i++ {
		if n := os.blockingNet(pts[i], pts[i+1], hw, clr); n != "" {
			return n
		}
	}
	return ""
}

func (os *obstacleSet) blockingNet(a, b p2, hw, clr float64) string {
	const eps = 1e-4
	for _, ob := range os.items {
		need := hw + math.Max(clr, ob.clrMM)
		var d float64
		switch ob.kind {
		case 0:
			d = segRectDist(a, b, ob.min, ob.max)
		case 1:
			d = segSegDist(a, b, ob.a, ob.b) - ob.halfW
			if d < 0 {
				d = 0
			}
		case 2:
			d = pointSegDist(ob.c, a, b) - ob.r
			if d < 0 {
				d = 0
			}
		}
		if d+eps < need && ob.net != "" {
			return ob.net
		}
	}
	return ""
}

func stringPull(pts []p2, obs *obstacleSet, hw, clr float64) []p2 {
	return stringPullPinned(pts, nil, obs, hw, clr)
}

func padPinnedVertices(board *core.Board, net string, pts []p2) []bool {
	pin := make([]bool, len(pts))
	if len(pts) == 0 {
		return pin
	}
	const tol = 0.40 // mm — ~one coarse cell; pad centre vs grid vertex
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net != net {
				continue
			}
			c := ptMM(core.PadWorldCenter(fp, pad))
			best, bestD := 0, math.Inf(1)
			for j, p := range pts {
				d := dist2(c, p)
				if d < bestD {
					best, bestD = j, d
				}
			}
			if bestD <= tol {
				pin[best] = true
			}
		}
	}
	if len(pin) > 0 {
		pin[0] = true
		pin[len(pin)-1] = true
	}
	return pin
}

func stringPullPinned(pts []p2, pinned []bool, obs *obstacleSet, hw, clr float64) []p2 {
	if len(pts) <= 2 {
		return append([]p2(nil), pts...)
	}
	out := make([]p2, 0, len(pts))
	i := 0
	out = append(out, pts[0])
	for i+1 < len(pts) {
		j := i + 1
		// Never jump over a pinned pad vertex.
		limit := len(pts) - 1
		if len(pinned) == len(pts) {
			for k := i + 1; k < len(pts)-1; k++ {
				if pinned[k] {
					limit = k
					break
				}
			}
		}
		for cand := limit; cand >= i+2; cand-- {
			if obs.approachClear(pts[i], pts[cand], hw, clr) {
				j = cand
				break
			}
		}
		out = append(out, pts[j])
		i = j
	}
	return out
}

type chain struct {
	pts   []core.Point
	width core.Length
	// original segment indices into board.Traces (for replace)
	segIDs []core.ID
}

func ptKey(p core.Point) [2]int64 { return [2]int64{int64(p.X), int64(p.Y)} }

func extractChains(board *core.Board, net string, layer uint8) []chain {
	type segRef struct {
		idx int
		tr  core.Trace
	}
	var segs []segRef
	for i, tr := range board.Traces {
		if tr.Net == net && tr.Layer.Index == layer {
			segs = append(segs, segRef{i, tr})
		}
	}
	if len(segs) == 0 {
		return nil
	}
	adj := map[[2]int64][]int{}
	for i, s := range segs {
		adj[ptKey(s.tr.Start)] = append(adj[ptKey(s.tr.Start)], i)
		adj[ptKey(s.tr.End)] = append(adj[ptKey(s.tr.End)], i)
	}
	hard := map[[2]int64]bool{}
	for _, v := range board.Vias {
		if v.Net == net {
			hard[ptKey(v.Position)] = true
		}
	}
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net != nil && *pad.Net == net {
				hard[ptKey(core.PadWorldCenter(fp, pad))] = true
			}
		}
	}
	isAnchor := func(k [2]int64) bool {
		if hard[k] {
			return true
		}
		ss := adj[k]
		if len(ss) != 2 {
			return true
		}
		return segs[ss[0]].tr.Width != segs[ss[1]].tr.Width
	}

	used := make([]bool, len(segs))
	var chains []chain
	var anchors [][2]int64
	for k := range adj {
		if isAnchor(k) {
			anchors = append(anchors, k)
		}
	}
	sort.Slice(anchors, func(i, j int) bool {
		if anchors[i][0] != anchors[j][0] {
			return anchors[i][0] < anchors[j][0]
		}
		return anchors[i][1] < anchors[j][1]
	})

	for _, startKey := range anchors {
		for _, first := range adj[startKey] {
			if used[first] {
				continue
			}
			pts := []core.Point{}
			ids := []core.ID{}
			curKey := startKey
			curSeg := first
			// start point
			t0 := segs[curSeg].tr
			if ptKey(t0.Start) == startKey {
				pts = append(pts, t0.Start)
			} else {
				pts = append(pts, t0.End)
			}
			for {
				used[curSeg] = true
				t := segs[curSeg].tr
				ids = append(ids, t.ID)
				var nxt [2]int64
				var nxtPt core.Point
				if ptKey(t.Start) == curKey {
					nxt = ptKey(t.End)
					nxtPt = t.End
				} else {
					nxt = ptKey(t.Start)
					nxtPt = t.Start
				}
				pts = append(pts, nxtPt)
				if isAnchor(nxt) {
					break
				}
				cands := adj[nxt]
				nextSeg := -1
				for _, s := range cands {
					if s != curSeg && !used[s] {
						nextSeg = s
						break
					}
				}
				if nextSeg < 0 {
					break
				}
				curKey = nxt
				curSeg = nextSeg
			}
			if len(pts) >= 2 {
				chains = append(chains, chain{pts: pts, width: segs[first].tr.Width, segIDs: ids})
			}
		}
	}
	return chains
}

func replaceChain(board *core.Board, net string, layer uint8, ch chain, pts []p2) {
	drop := map[core.ID]bool{}
	for _, id := range ch.segIDs {
		drop[id] = true
	}
	var kept []core.Trace
	for _, tr := range board.Traces {
		if !drop[tr.ID] {
			kept = append(kept, tr)
		}
	}
	for i := 0; i+1 < len(pts); i++ {
		kept = append(kept, core.Trace{
			ID:    core.NewID(),
			Layer: core.Layer{Index: layer},
			Start: core.NewPoint(core.FromMM(pts[i][0]), core.FromMM(pts[i][1])),
			End:   core.NewPoint(core.FromMM(pts[i+1][0]), core.FromMM(pts[i+1][1])),
			Width: ch.width,
			Net:   net,
		})
	}
	board.Traces = kept
}

// --- grid A* ---

type gpos struct {
	x, y  core.Length
	layer uint8
}

type cellKey struct {
	x, y int
	l    uint8
}

type padObs struct {
	net                    string
	layer                  uint8
	through                bool
	minX, minY, maxX, maxY float64
}

type grid struct {
	originX, originY core.Length
	cell             core.Length
	cellMM           float64
	w, h             int
	layers           int
	// blocked[layer][y*w+x] = net name occupying, "" free, "*" hard obstacle.
	// Own-net pads/traces store the net name so the same net may re-enter.
	blocked [][]string
	// padCell marks cells stamped from a pad AABB. Sharing may walk
	// through foreign *trace* copper; pad cells stay hard.
	padCell [][]string
	plane   []bool // power/plane layers: vias only, no lateral traces
	pads    []padObs
	opts    Options
	board   *core.Board
	theta   bool
	share   bool    // PathFinder: foreign traces are costly, not walls
	present float64 // extra step cost per shared cell
	// holes tracks every drill on the board so a via is never placed
	// inside another hole's fab clearance.
	holes *holeMap
	// viaBan are cells a commit already proved unusable for a barrel.
	// It survives grid rebuilds so a retry does not walk into them again.
	viaBan map[[2]int]bool
	// padIdx buckets pads by position so a clearance query tests the few
	// pads that can be in reach instead of every pad on the board.
	padIdx *padIndex
	// timedOut records why the last A* returned nothing: a spent deadline
	// or a genuine dead end. They are not the same failure and must not be
	// reported as if they were.
	timedOut bool
}

// padIndex is a uniform bucket grid over pad AABBs. nearForeignPad is the
// router's hottest call — a Theta* line-of-sight test samples its chord every
// 0.08 mm and asks for each sample — and scanning the whole pad list there
// made one cross-board hop cost seconds of A*. Every pad is filed in each
// bucket its AABB overlaps, so a query only has to read the buckets its own
// reach touches; the answer is identical to the linear scan.
type padIndex struct {
	cellMM     float64
	minX, minY float64
	w, h       int
	buckets    [][]int32
}

// padIndexCellMM is the bucket pitch. Pads are millimetre-scale, so a 1 mm
// bucket keeps both the fill factor and the per-query bucket count small.
const padIndexCellMM = 1.0

func newPadIndex(pads []padObs, minX, minY, maxX, maxY float64) *padIndex {
	if len(pads) == 0 {
		return nil
	}
	w := int((maxX-minX)/padIndexCellMM) + 1
	h := int((maxY-minY)/padIndexCellMM) + 1
	if w < 1 {
		w = 1
	}
	if h < 1 {
		h = 1
	}
	pi := &padIndex{cellMM: padIndexCellMM, minX: minX, minY: minY, w: w, h: h,
		buckets: make([][]int32, w*h)}
	for i := range pads {
		x0, y0 := pi.cellOf(pads[i].minX, pads[i].minY)
		x1, y1 := pi.cellOf(pads[i].maxX, pads[i].maxY)
		for by := y0; by <= y1; by++ {
			for bx := x0; bx <= x1; bx++ {
				pi.buckets[by*pi.w+bx] = append(pi.buckets[by*pi.w+bx], int32(i))
			}
		}
	}
	return pi
}

// cellOf clamps into the bucket grid: a pad hanging off the outline and a
// query beside it land in the same border bucket, so nothing is missed.
func (pi *padIndex) cellOf(x, y float64) (int, int) {
	bx := int((x - pi.minX) / pi.cellMM)
	by := int((y - pi.minY) / pi.cellMM)
	if bx < 0 {
		bx = 0
	} else if bx >= pi.w {
		bx = pi.w - 1
	}
	if by < 0 {
		by = 0
	} else if by >= pi.h {
		by = pi.h - 1
	}
	return bx, by
}

// maxGridDim caps cells per axis; larger boards coarsen the cell pitch.
const maxGridDim = 320

func newGrid(board *core.Board, opts Options) *grid {
	cellMM := opts.CellMM
	if cellMM < 0.05 {
		cellMM = 0.05
	}
	var o core.Rect
	if board.Outline != nil {
		o = *board.Outline
	} else {
		o = core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(100), core.FromMM(80)))
	}
	// Full outline so edge-mounted pads (fecha ESP 5V at x=69.25 on
	// 70 mm) and the perimeter channel Rust uses stay on-grid.
	minX, minY := o.Min.X, o.Min.Y
	maxX, maxY := o.Max.X, o.Max.Y
	widthMM := float64(maxX-minX) / 1e6
	heightMM := float64(maxY-minY) / 1e6
	if widthMM/cellMM > maxGridDim {
		cellMM = widthMM / maxGridDim
	}
	if heightMM/cellMM > maxGridDim {
		c2 := heightMM / maxGridDim
		if c2 > cellMM {
			cellMM = c2
		}
	}
	cell := core.FromMM(cellMM)
	opts.CellMM = cellMM

	gw := int((maxX-minX)/cell) + 1
	gh := int((maxY-minY)/cell) + 1
	if gw < 2 {
		gw = 2
	}
	if gh < 2 {
		gh = 2
	}
	layers := board.StackupOrDefault().CopperCount()
	if layers < 2 {
		layers = 2
	}
	g := &grid{
		originX: minX, originY: minY, cell: cell, cellMM: cellMM,
		w: gw, h: gh, layers: layers,
		blocked: make([][]string, layers),
		padCell: make([][]string, layers),
		plane:   make([]bool, layers),
		opts:    opts, board: board,
		theta: cellMM > 0.16,
		holes: newHoleMap(board),
	}
	st := board.StackupOrDefault()
	for L := 0; L < layers; L++ {
		g.blocked[L] = make([]string, gw*gh)
		g.padCell[L] = make([]string, gw*gh)
		g.plane[L] = st.IsPlane(L)
	}

	// Hard keepouts (no-copper).
	for _, k := range board.Keepouts {
		if k.Rect == nil {
			continue
		}
		if k.NoPlace && !k.NoCopper {
			continue
		}
		g.stampRectObstacle(*k.Rect)
	}

	stampInductorBodies(g, board)

	// Pads: stamp BARE copper only. A 2.54 mm header with a 0.40 mm
	// clearance halo on each pad overwrites its neighbour's centre
	// (fecha U1 V5/GND) and A* "arrives" on a foreign cell. Foreign
	// separation is searchClearanceOK, not the stamp.
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			net := ""
			if pad.Net != nil {
				net = *pad.Net
			}
			layersToStamp := []uint8{pad.Layer.Index}
			if pad.Drill != nil && *pad.Drill > 0 {
				layersToStamp = make([]uint8, layers)
				for L := 0; L < layers; L++ {
					layersToStamp[L] = uint8(L)
				}
			}
			aa := core.PadWorldAABB(fp, pad)
			g.pads = append(g.pads, padObs{
				net: net, layer: pad.Layer.Index,
				through: pad.Drill != nil && *pad.Drill > 0,
				minX:    aa.Min.X.ToMM(), minY: aa.Min.Y.ToMM(),
				maxX: aa.Max.X.ToMM(), maxY: aa.Max.Y.ToMM(),
			})
			label := net
			if label == "" {
				label = "*"
			}
			c0x, c0y := clampCell(aa.Min, g)
			c1x, c1y := clampCell(aa.Max, g)
			if c0x > c1x {
				c0x, c1x = c1x, c0x
			}
			if c0y > c1y {
				c0y, c1y = c1y, c0y
			}
			for _, layer := range layersToStamp {
				if int(layer) >= layers {
					layer = 0
				}
				for y := c0y; y <= c1y; y++ {
					for x := c0x; x <= c1x; x++ {
						if x < 0 || y < 0 || x >= g.w || y >= g.h {
							continue
						}
						idx := y*g.w + x
						cur := g.blocked[layer][idx]
						if cur == "*" {
							continue
						}
						if cur == "" {
							g.blocked[layer][idx] = label
						} else if cur != label && label == "*" {
							g.blocked[layer][idx] = "*"
						}
						if g.padCell[layer][idx] == "" {
							g.padCell[layer][idx] = label
						}
					}
				}
			}
		}
	}
	g.padIdx = newPadIndex(g.pads, minX.ToMM(), minY.ToMM(), maxX.ToMM(), maxY.ToMM())
	// Existing board copper: foreign obstacles use full clearance inflation.
	for _, tr := range board.Traces {
		g.blockSegObstacle(tr.Start.X, tr.Start.Y, tr.End.X, tr.End.Y, tr.Layer.Index, tr.Net, tr.Width.ToMM()/2)
	}
	for _, v := range board.Vias {
		g.blockViaObstacle(v.Position.X, v.Position.Y, v.Net, v.Diameter.ToMM()/2, v.Drill.ToMM())
	}
	return g
}

func (g *grid) copperRadius() int {
	// Bare copper half-width in cells (own-net tree paint).
	r := int(math.Ceil((g.opts.TraceWidthMM / 2) / g.cellMM))
	if r < 0 {
		r = 0
	}
	return r
}

// obstacleRadius: half-width + clearance, rounded up (quantization guard).
func (g *grid) obstacleRadius(halfWidthMM float64) int {
	r := int(math.Ceil((halfWidthMM + g.opts.ClearanceMM) / g.cellMM))
	if r < 1 {
		r = 1
	}
	return r
}

// stampInductorBodies blocks empty cells under an inductor body_rect so a
// foreign net (SEL under L2) cannot sneak through the ferrite. Own-net pad
// cells are stamped afterwards and stay passable.
func stampInductorBodies(g *grid, board *core.Board) {
	if g == nil || board == nil {
		return
	}
	for _, fp := range footprintsStable(board) {
		if fp == nil || fp.BodyRect == nil || !inductorFootprint(fp) {
			continue
		}
		body, ok := core.CourtyardWorld(fp)
		if !ok {
			continue
		}
		c0x, c0y := clampCell(body.Min, g)
		c1x, c1y := clampCell(body.Max, g)
		if c0x > c1x {
			c0x, c1x = c1x, c0x
		}
		if c0y > c1y {
			c0y, c1y = c1y, c0y
		}
		for L := uint8(0); L < uint8(g.layers); L++ {
			for y := c0y; y <= c1y; y++ {
				for x := c0x; x <= c1x; x++ {
					if x < 0 || y < 0 || x >= g.w || y >= g.h {
						continue
					}
					idx := y*g.w + x
					if g.blocked[L][idx] == "" {
						g.blocked[L][idx] = "*"
					}
				}
			}
		}
	}
}

func inductorFootprint(fp *core.Footprint) bool {
	if fp == nil {
		return false
	}
	ref := strings.ToUpper(strings.TrimSpace(fp.Reference))
	if len(ref) > 0 && ref[0] == 'L' && (len(ref) == 1 || !isAlpha(ref[1])) {
		return true
	}
	blob := strings.ToLower(fp.Key + " " + fp.Library + " " + fp.Description + " " + fp.Value)
	return strings.Contains(blob, "inductor")
}

func isAlpha(b byte) bool {
	return (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z')
}

func (g *grid) stampRectObstacle(r core.Rect) {
	c0x, c0y, ok0 := g.worldToCell(r.Min.X, r.Min.Y)
	c1x, c1y, ok1 := g.worldToCell(r.Max.X, r.Max.Y)
	if !ok0 && !ok1 {
		c0x, c0y = clampCell(core.NewPoint(r.Min.X, r.Min.Y), g)
		c1x, c1y = clampCell(core.NewPoint(r.Max.X, r.Max.Y), g)
	} else {
		if !ok0 {
			c0x, c0y = clampCell(core.NewPoint(r.Min.X, r.Min.Y), g)
		}
		if !ok1 {
			c1x, c1y = clampCell(core.NewPoint(r.Max.X, r.Max.Y), g)
		}
	}
	if c0x > c1x {
		c0x, c1x = c1x, c0x
	}
	if c0y > c1y {
		c0y, c1y = c1y, c0y
	}
	for L := uint8(0); L < uint8(g.layers); L++ {
		for y := c0y; y <= c1y; y++ {
			for x := c0x; x <= c1x; x++ {
				if x < 0 || y < 0 || x >= g.w || y >= g.h {
					continue
				}
				g.blocked[L][y*g.w+x] = "*"
			}
		}
	}
}

func (g *grid) worldToCell(x, y core.Length) (int, int, bool) {
	cx := int((x - g.originX) / g.cell)
	cy := int((y - g.originY) / g.cell)
	if cx < 0 || cy < 0 || cx >= g.w || cy >= g.h {
		return 0, 0, false
	}
	return cx, cy, true
}

func (g *grid) cellToWorld(cx, cy int) (core.Length, core.Length) {
	return g.originX + core.Length(cx)*g.cell + g.cell/2,
		g.originY + core.Length(cy)*g.cell + g.cell/2
}

func (g *grid) blockSegObstacle(x0, y0, x1, y1 core.Length, layer uint8, net string, halfWMM float64) {
	if int(layer) >= g.layers {
		layer = 0
	}
	c0x, c0y, ok0 := g.worldToCell(x0, y0)
	c1x, c1y, ok1 := g.worldToCell(x1, y1)
	if !ok0 && !ok1 {
		return
	}
	if !ok0 {
		c0x, c0y = c1x, c1y
	}
	if !ok1 {
		c1x, c1y = c0x, c0y
	}
	r := g.obstacleRadius(halfWMM)
	for _, c := range bresenham(c0x, c0y, c1x, c1y) {
		g.paint(c[0], c[1], layer, net, r)
	}
}

func (g *grid) blockViaObstacle(x, y core.Length, net string, halfWMM, drillMM float64) {
	if net == "" {
		net = "*"
	}
	g.holes.add(x.ToMM(), y.ToMM(), drillMM)
	cx, cy, ok := g.worldToCell(x, y)
	if !ok {
		return
	}
	r := g.obstacleRadius(halfWMM)
	for L := uint8(0); L < uint8(g.layers); L++ {
		g.paint(cx, cy, L, net, r)
	}
}

func (g *grid) paint(cx, cy int, layer uint8, net string, radius int) {
	for dy := -radius; dy <= radius; dy++ {
		for dx := -radius; dx <= radius; dx++ {
			x, y := cx+dx, cy+dy
			if x < 0 || y < 0 || x >= g.w || y >= g.h {
				continue
			}
			idx := y*g.w + x
			cur := g.blocked[layer][idx]
			if cur == "*" {
				continue
			}
			if cur == "" || cur == net {
				g.blocked[layer][idx] = net
			} else if net == "*" {
				g.blocked[layer][idx] = "*"
			}
		}
	}
}

// searchClearanceOK: no foreign copper within ceil((halfWidth+0.20)/cell)
// of (cx,cy), and no foreign pad AABB closer than 0.20+half-width
// (continuous — catches Theta* chords the discrete disk misses).
func (g *grid) searchClearanceOK(cx, cy int, layer uint8, net string) bool {
	return g.searchClearanceNeed(cx, cy, layer, net, g.opts.padClear())
}

func (g *grid) searchClearanceNeed(cx, cy int, layer uint8, net string, padClear float64) bool {
	half := g.opts.TraceWidthMM / 2
	if g.opts.neck != nil {
		// Inside a pad escape zone the trace may neck down, so the search disk
		// shrinks with it — that is what lets a controlled width leave a
		// 0.4 mm-pitch package at all. Where the nominal turns out to fit, the
		// emitted copper stays nominal (neckAt); the search is the permissive
		// side of that pair.
		wx, wy := g.cellToWorld(cx, cy)
		half = g.opts.halfWidthAt(wx.ToMM(), wy.ToMM())
	}
	return g.clearanceForHalf(cx, cy, layer, net, half+padClear)
}

// widthFits reports whether a trace of half-width hw may sit at (x, y) in mm:
// clear of foreign pads continuously, and of foreign copper on the grid.
func (g *grid) widthFits(x, y float64, layer uint8, net string, hw float64) bool {
	need := hw + g.opts.padClear()
	if g.nearForeignPad(x, y, layer, net, need) {
		return false
	}
	cx, cy, ok := g.worldToCell(core.FromMM(x), core.FromMM(y))
	if !ok {
		return false
	}
	return g.clearanceForHalf(cx, cy, layer, net, need)
}

func (g *grid) clearanceForHalf(cx, cy int, layer uint8, net string, need float64) bool {
	r := int(math.Ceil(need / g.cellMM))
	if r < 1 {
		r = 1
	}
	r2 := r * r
	for dy := -r; dy <= r; dy++ {
		for dx := -r; dx <= r; dx++ {
			if dx*dx+dy*dy > r2 {
				continue
			}
			x, y := cx+dx, cy+dy
			if x < 0 || y < 0 || x >= g.w || y >= g.h {
				continue
			}
			cur := g.blocked[layer][y*g.w+x]
			if cur != "" && cur != net {
				if g.share && cur != "*" && (len(g.padCell) <= int(layer) || g.padCell[layer][y*g.w+x] == "" || g.padCell[layer][y*g.w+x] == net) {
					continue
				}
				return false
			}
		}
	}
	return true
}

// pendingEscapeReachMM is how far in front of a pad its escape corridor is
// assumed to run: room for a trace and its clearance, plus the turn it needs
// to leave.
const pendingEscapeReachMM = 1.2

// pendingEscapeToll is the extra step cost per unit of crowding (see
// pendingPenalty), and pendingTollMaxUnits caps it so no cell can ever cost
// more than going right round the board. Saturated it is about three
// quarters of a via: a shortcut across an unrouted connector's pin face has
// to be clearly better than the way round, not merely shorter.
const (
	pendingEscapeToll   = 1.5
	pendingTollMaxUnits = 4
)

// pendingPenalty prices the escape corridors of the pads still waiting for
// copper. Nothing is forbidden — a board with no other way still routes —
// but a rail that used to take the diagonal straight across an unrouted
// header's pin face now prefers the way round, which leaves those pins an
// escape instead of stranding the whole connector. Closing a long power haul
// early cost three signal nets their only way out until this existed.
//
// Crowding is measured in pad-index tiles: one unit for each tile within
// reach that an unrouted pad occupies. It is deliberately coarse, and it
// rises with both the number of unrouted pins nearby and their size — which
// is exactly the pressure being priced, since a 2 mm through-hole pin needs
// far more room to escape than a 0.4 mm fine-pitch pad.
func (g *grid) pendingPenalty(cx, cy int, layer uint8, net string) float64 {
	if len(g.opts.pending) == 0 || g.padIdx == nil {
		return 0
	}
	wx, wy := g.cellToWorld(cx, cy)
	x, y := wx.ToMM(), wy.ToMM()
	const reach = pendingEscapeReachMM
	x0, y0 := g.padIdx.cellOf(x-reach, y-reach)
	x1, y1 := g.padIdx.cellOf(x+reach, y+reach)
	units := 0
	for by := y0; by <= y1; by++ {
		row := by * g.padIdx.w
		for bx := x0; bx <= x1; bx++ {
			for _, i := range g.padIdx.buckets[row+bx] {
				p := &g.pads[i]
				if p.net == "" || p.net == net || !g.opts.pending[p.net] {
					continue
				}
				if !p.through && p.layer != layer {
					continue
				}
				if pointRectDist2(x, y, p.minX, p.minY, p.maxX, p.maxY) >= reach*reach {
					continue
				}
				units++
				if units >= pendingTollMaxUnits {
					return pendingEscapeToll * pendingTollMaxUnits
				}
			}
		}
	}
	return pendingEscapeToll * float64(units)
}

func (g *grid) nearForeignPad(x, y float64, layer uint8, net string, need float64) bool {
	hit := func(p *padObs) bool {
		if p.net == net || p.net == "" {
			return false
		}
		if !p.through && p.layer != layer {
			return false
		}
		return pointRectDist2(x, y, p.minX, p.minY, p.maxX, p.maxY) < need*need
	}
	if g.padIdx == nil {
		for i := range g.pads {
			if hit(&g.pads[i]) {
				return true
			}
		}
		return false
	}
	x0, y0 := g.padIdx.cellOf(x-need, y-need)
	x1, y1 := g.padIdx.cellOf(x+need, y+need)
	for by := y0; by <= y1; by++ {
		row := by * g.padIdx.w
		for bx := x0; bx <= x1; bx++ {
			for _, i := range g.padIdx.buckets[row+bx] {
				if hit(&g.pads[i]) {
					return true
				}
			}
		}
	}
	return false
}

// pointRectDist2 is pointRectDist squared: the hot clearance test compares
// against a squared reach, so it never pays for the square root.
func pointRectDist2(x, y, minX, minY, maxX, maxY float64) float64 {
	dx := 0.0
	if x < minX {
		dx = minX - x
	} else if x > maxX {
		dx = x - maxX
	}
	dy := 0.0
	if y < minY {
		dy = minY - y
	} else if y > maxY {
		dy = y - maxY
	}
	return dx*dx + dy*dy
}

func pointRectDist(x, y, minX, minY, maxX, maxY float64) float64 {
	dx := 0.0
	if x < minX {
		dx = minX - x
	} else if x > maxX {
		dx = x - maxX
	}
	dy := 0.0
	if y < minY {
		dy = minY - y
	} else if y > maxY {
		dy = y - maxY
	}
	if dx == 0 && dy == 0 {
		return 0
	}
	return math.Hypot(dx, dy)
}

func (g *grid) passable(cx, cy int, layer uint8, net string) bool {
	if cx < 0 || cy < 0 || cx >= g.w || cy >= g.h {
		return false
	}
	if int(layer) >= g.layers {
		return false
	}
	cur := g.blocked[layer][cy*g.w+cx]
	if cur != "" && cur != net {
		if !(g.share && cur != "*" && g.padCell != nil && (g.padCell[layer][cy*g.w+cx] == "" || g.padCell[layer][cy*g.w+cx] == net)) {
			return false
		}
	}
	// Keep new copper off the outline. Own-net cells (edge pads) stay
	// passable so USB-C / header pins can still be landed on.
	if cur != net && g.board != nil && g.board.Outline != nil {
		wx, wy := g.cellToWorld(cx, cy)
		m := 0.30 + g.opts.TraceWidthMM/2
		if !outlineContains(g.board.Outline, wx.ToMM(), wy.ToMM(), m) {
			return false
		}
	}
	return true
}

// aStarMulti: multi-source A*; prefers layer 0; vias cost ViaCost.
func (g *grid) aStarMulti(sources []cellKey, to core.Point, goalLayer uint8, net string, deadline time.Time, hasDeadline bool) ([]gpos, bool) {
	return g.aStarMultiAt(sources, to, goalLayer, false, net, deadline, hasDeadline)
}

// aStarMultiAt is aStarMulti with an explicit any-layer goal: a through-hole
// pad is copper top to bottom, so arriving on either side finishes the hop.
func (g *grid) aStarMultiAt(sources []cellKey, to core.Point, goalLayer uint8, anyLayer bool, net string, deadline time.Time, hasDeadline bool) ([]gpos, bool) {
	g.timedOut = false
	tx, ty, ok2 := g.worldToCell(to.X, to.Y)
	if !ok2 {
		tx, ty = clampCell(to, g)
	}
	if int(goalLayer) >= g.layers {
		goalLayer = 0
	}
	goal := cellKey{tx, ty, goalLayer}

	open := &astHeap{}
	heap.Init(open)
	came := map[cellKey]cellKey{}
	gScore := map[cellKey]float64{}
	closed := map[cellKey]bool{}

	if len(sources) == 0 {
		sources = []cellKey{{tx, ty, 0}}
	}
	seenSrc := map[cellKey]bool{}
	for _, s := range sources {
		if int(s.l) >= g.layers {
			s.l = 0
		}
		if s.x < 0 || s.y < 0 || s.x >= g.w || s.y >= g.h {
			continue
		}
		if !g.passable(s.x, s.y, s.l, net) {
			cur := g.blocked[s.l][s.y*g.w+s.x]
			if cur != "" && cur != net {
				continue
			}
		}
		if seenSrc[s] {
			continue
		}
		seenSrc[s] = true
		gScore[s] = 0
		heap.Push(open, &astNode{k: s, g: 0, f: heuristic(s, goal)})
	}
	if open.Len() == 0 {
		return nil, false
	}
	for _, s := range sources {
		if s.x == goal.x && s.y == goal.y && (anyLayer || s.l == goal.l) {
			if _, ok := gScore[s]; ok {
				wx, wy := g.cellToWorld(goal.x, goal.y)
				return []gpos{{x: wx, y: wy, layer: s.l}}, true
			}
		}
	}

	dirs := [][2]int{
		{1, 0}, {-1, 0}, {0, 1}, {0, -1},
		{1, 1}, {1, -1}, {-1, 1}, {-1, -1},
	}

	maxExpand := g.w * g.h * g.layers * 8
	if maxExpand < 10000 {
		maxExpand = 10000
	}
	if g.share && maxExpand > 40000 {
		maxExpand = 40000
	}
	expanded := 0
	const deadlineEvery = 256

	for open.Len() > 0 && expanded < maxExpand {
		expanded++
		if hasDeadline && expanded%deadlineEvery == 0 && time.Now().After(deadline) {
			g.timedOut = true
			return nil, false
		}
		cur := heap.Pop(open).(*astNode) // safe-ignore: astHeap only ever holds *astNode; a mismatch is a programmer error worth the panic
		if cur.k.x == goal.x && cur.k.y == goal.y && (anyLayer || cur.k.l == goal.l) {
			return g.reconstruct(came, cur.k), true
		}
		if closed[cur.k] {
			continue
		}
		closed[cur.k] = true

		if int(cur.k.l) >= len(g.plane) || !g.plane[cur.k.l] {
			for _, d := range dirs {
				nx, ny := cur.k.x+d[0], cur.k.y+d[1]
				if !g.passable(nx, ny, cur.k.l, net) {
					continue
				}
				if d[0] != 0 && d[1] != 0 {
					if !g.passable(cur.k.x+d[0], cur.k.y, cur.k.l, net) ||
						!g.passable(cur.k.x, cur.k.y+d[1], cur.k.l, net) {
						continue
					}
				}
				// Search-time clearance disk. Own-net cells (the pad we
				// are landing on) skip it: header pads sit 2.54 mm apart
				// and the disk would otherwise refuse the pin itself.
				// Long chords are checked continuously in lineOfSight;
				// per-step pad AABB is too expensive on a QFN board.
				need := g.opts.padClear()
				if g.blocked[cur.k.l][cur.k.y*g.w+cur.k.x] == net {
					need = 0.10 // leaving own pad (0.4 mm QFN escape)
				}
				if g.blocked[cur.k.l][ny*g.w+nx] != net {
					if !g.searchClearanceNeed(nx, ny, cur.k.l, net, need) {
						continue
					}
				}
				nk := cellKey{nx, ny, cur.k.l}
				step := 1.0
				if d[0] != 0 && d[1] != 0 {
					step = 1.414
				}
				if cur.k.l != 0 {
					step += 0.15
				}
				if g.share {
					step += g.shareCost(nx, ny, cur.k.l, net)
				}
				step += g.pendingPenalty(nx, ny, cur.k.l, net)
				bestParent := cur.k
				ng := gScore[cur.k] + step
				// Lazy Theta*: if the parent has line-of-sight to nk on the
				// same layer, parent→nk is one any-angle segment.
				if g.theta {
					if parent, ok := came[cur.k]; ok && parent.l == nk.l {
						if g.lineOfSight(parent, nk, net) {
							eu := hypotCells(parent, nk)
							// The chord skips the cells between its ends, so
							// it must still pay their tolls — otherwise a
							// Theta* shortcut buys the straight line across
							// an unrouted header for free.
							cand := gScore[parent] + eu
							if cand < ng {
								ng = cand
								bestParent = parent
							}
						}
					}
				}
				if old, ok := gScore[nk]; ok && ng >= old {
					continue
				}
				gScore[nk] = ng
				came[nk] = bestParent
				heap.Push(open, &astNode{k: nk, g: ng, f: ng + heuristic(nk, goal)})
			}
		} // end non-plane lateral walk
		// A layer change means a drilled via here — only where the fab's
		// hole-to-hole gap to every other drill still holds.
		if g.opts.sameLayerOnly || !g.viaSiteOK(cur.k.x, cur.k.y, net) {
			continue
		}
		for L := uint8(0); L < uint8(g.layers); L++ {
			if L == cur.k.l {
				continue
			}
			if !g.passable(cur.k.x, cur.k.y, L, net) {
				continue
			}
			nk := cellKey{cur.k.x, cur.k.y, L}
			ng := gScore[cur.k] + g.opts.ViaCost
			if L != 0 {
				ng += 0.5
			}
			if old, ok := gScore[nk]; ok && ng >= old {
				continue
			}
			gScore[nk] = ng
			came[nk] = cur.k
			heap.Push(open, &astNode{k: nk, g: ng, f: ng + heuristic(nk, goal)})
		}
	}
	return nil, false
}

func (g *grid) reconstruct(came map[cellKey]cellKey, end cellKey) []gpos {
	path := []gpos{}
	k := end
	for {
		wx, wy := g.cellToWorld(k.x, k.y)
		path = append(path, gpos{x: wx, y: wy, layer: k.l})
		prev, ok := came[k]
		if !ok {
			break
		}
		k = prev
	}
	for i, j := 0, len(path)-1; i < j; i, j = i+1, j-1 {
		path[i], path[j] = path[j], path[i]
	}
	return path
}

func bresenham(x0, y0, x1, y1 int) [][2]int {
	var out [][2]int
	dx := abs(x1 - x0)
	dy := abs(y1 - y0)
	sx, sy := 1, 1
	if x0 > x1 {
		sx = -1
	}
	if y0 > y1 {
		sy = -1
	}
	err := dx - dy
	x, y := x0, y0
	for {
		out = append(out, [2]int{x, y})
		if x == x1 && y == y1 {
			break
		}
		e2 := 2 * err
		if e2 > -dy {
			err -= dy
			x += sx
		}
		if e2 < dx {
			err += dx
			y += sy
		}
	}
	return out
}

func clampCell(p core.Point, g *grid) (int, int) {
	cx := int((p.X - g.originX) / g.cell)
	cy := int((p.Y - g.originY) / g.cell)
	if cx < 0 {
		cx = 0
	}
	if cy < 0 {
		cy = 0
	}
	if cx >= g.w {
		cx = g.w - 1
	}
	if cy >= g.h {
		cy = g.h - 1
	}
	return cx, cy
}

func hypotCells(a, b cellKey) float64 {
	dx := float64(a.x - b.x)
	dy := float64(a.y - b.y)
	return math.Hypot(dx, dy)
}

func heuristic(a, b cellKey) float64 {
	h := hypotCells(a, b)
	if a.l != b.l {
		h += 0.5
	}
	return h
}

func (g *grid) lineOfSight(a, b cellKey, net string) bool {
	if a.l != b.l {
		return false
	}
	for _, c := range bresenham(a.x, a.y, b.x, b.y) {
		if !g.passable(c[0], c[1], a.l, net) {
			return false
		}
		if g.blocked[a.l][c[1]*g.w+c[0]] != net && !g.searchClearanceOK(c[0], c[1], a.l, net) {
			return false
		}
	}
	// Continuous pad AABB so a long Theta* chord cannot thread a
	// neighbour pad the discrete disk never entered.
	ax, ay := g.cellToWorld(a.x, a.y)
	bx, by := g.cellToWorld(b.x, b.y)
	return g.segmentPadClear(ax.ToMM(), ay.ToMM(), bx.ToMM(), by.ToMM(), a.l, net, g.opts)
}

func (g *grid) stepPadClear(x0, y0, x1, y1 int, layer uint8, net string, padClear float64) bool {
	ax, ay := g.cellToWorld(x0, y0)
	bx, by := g.cellToWorld(x1, y1)
	o := g.opts
	o.SearchClearMM = padClear
	return g.segmentPadClear(ax.ToMM(), ay.ToMM(), bx.ToMM(), by.ToMM(), layer, net, o)
}

func abs(x int) int {
	if x < 0 {
		return -x
	}
	return x
}

type astNode struct {
	k    cellKey
	g, f float64
	idx  int
}

type astHeap []*astNode

func (h astHeap) Len() int           { return len(h) }
func (h astHeap) Less(i, j int) bool { return h[i].f < h[j].f }
func (h astHeap) Swap(i, j int)      { h[i], h[j] = h[j], h[i]; h[i].idx = i; h[j].idx = j }
func (h *astHeap) Push(x any) {
	n := x.(*astNode) // safe-ignore: container/heap Push contract; astHeap only ever holds *astNode
	n.idx = len(*h)
	*h = append(*h, n)
}
func (h *astHeap) Pop() any {
	old := *h
	n := len(old)
	item := old[n-1]
	*h = old[:n-1]
	return item
}
