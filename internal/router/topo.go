// Topological routing engine — rubber-band homotopy over a Delaunay dual.
//
// Ported from crates/pcb-router/src/topo.rs at rust tip 4e20ae0.
// A route is first a HOMOTOPY CLASS (the sequence of triangulation
// edges the connection crosses, chosen by A* over the dual with
// capacity costs) and only then realised geometrically: waypoints in
// each crossed-edge free window, string-pull, exact clearance check.
// Failed realisations penalise the crossed edges and try the next
// class. The geometric validator is the final arbiter.
//
// Layer/via model (v1): A* over (face, layer); a via is just another
// move. Opt-in via Options.Engine == "topo". The default Theta*/RR&R
// path is unchanged.

package router

import (
	"container/heap"
	"fmt"
	"math"
	"sort"
	"time"

	"github.com/mentasystems/fragua/internal/core"
)

// Engine names. Empty / "grid" is the default Theta* path.
const (
	EngineGrid = "grid"
	EngineTopo = "topo"
)

type topoSite struct {
	pos  p2
	half p2
}

func (s topoSite) reach(d p2) float64 {
	return math.Abs(d[0])*s.half[0] + math.Abs(d[1])*s.half[1]
}

type layerCdt struct {
	mesh    *delaunay
	used    map[[2]int]float64
	penalty map[[2]int]float64
}

type topoEngine struct {
	board    *core.Board
	opts     Options
	sites    []topoSite
	layers   [2]layerCdt
	blockers []string
}

type topoEndPt struct {
	pos         p2
	top, bottom bool
}

type topoPad struct {
	pos     p2
	through bool
	layer   core.Layer
}

type topoNetResult struct {
	net          string
	ok           bool
	reason       string
	lengthMM     float64
	lowerBoundMM float64
	segments     int
	vias         int
}

type topoLeg struct {
	layer   int
	crossed [][2]int
	faces   []int
	via     *p2
}

type topoCommitted struct {
	lengthMM  float64
	segments  int
	vias      int
	legs      []topoDrawn
	viaPoints []p2
}

type topoDrawn struct {
	layer core.Layer
	pts   []p2
}

func siteKey(a, b int) [2]int {
	if a > b {
		a, b = b, a
	}
	return [2]int{a, b}
}

// routeTopo drives the topological engine and shapes outcomes into the
// same Report the grid driver produces. It re-routes the whole board
// (clears existing copper) — matching rust route_all.
func routeTopo(board *core.Board, opts Options) Report {
	start := time.Now()
	opts.MaxSeconds = ClampBudget(opts.MaxSeconds)
	if board != nil && opts.TeardropsSet {
		board.Teardrops = opts.Teardrops
	}
	rep := Report{Iterations: 1}
	if board == nil || board.Outline == nil {
		rep.ElapsedMS = time.Since(start).Milliseconds()
		return rep
	}
	opts = applyFabCeiling(board, opts)
	opts.widths = newNetWidths(board, opts)

	results := topoRouteAll(board, opts)
	for _, r := range results {
		out := Outcome{Status: "failed", Reason: r.reason}
		if r.ok {
			out = Outcome{
				Status:        "ok",
				TraceSegments: r.segments,
				LengthMM:      r.lengthMM,
				LowerBoundMM:  r.lowerBoundMM,
			}
			rep.TotalLengthMM += r.lengthMM
			rep.TotalLowerBound += r.lowerBoundMM
			rep.TraceCount += r.segments
			rep.ViaCount += r.vias
		} else {
			rep.Failed++
		}
		rep.PerNet = append(rep.PerNet, NetResult{Net: r.net, Outcome: out})
	}
	sort.Slice(rep.PerNet, func(i, j int) bool { return rep.PerNet[i].Net < rep.PerNet[j].Net })

	if opts.Organic {
		before := append([]core.Trace(nil), board.Traces...)
		organicPass(board, opts)
		if !copperClearanceLegal(board, commitClearance(board)) {
			board.Traces = before
		}
		refreshReportLengths(board, &rep)
	}
	stitchIsolatedPads(board, opts)
	dropDuplicateVias(board)

	okNets := map[string]bool{}
	for _, n := range rep.PerNet {
		if n.Outcome.Status == "ok" {
			okNets[n.Net] = true
		}
	}
	vias, traces := 0, 0
	for _, v := range board.Vias {
		if okNets[v.Net] {
			vias++
		}
	}
	for _, tr := range board.Traces {
		if okNets[tr.Net] {
			traces++
		}
	}
	rep.ViaCount, rep.TraceCount = vias, traces
	rep.ElapsedMS = time.Since(start).Milliseconds()
	return rep
}

func topoRouteAll(board *core.Board, opts Options) []topoNetResult {
	board.ClearRoute()
	if board.Outline == nil {
		return nil
	}
	nets := map[string][]topoPad{}
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net == "" {
				continue
			}
			c := core.PadWorldCenter(fp, pad)
			nets[*pad.Net] = append(nets[*pad.Net], topoPad{
				pos:     ptMM(c),
				through: pad.Drill != nil && *pad.Drill > 0,
				layer:   pad.Layer,
			})
		}
	}
	order := make([]string, 0, len(nets))
	for n, pads := range nets {
		if len(pads) >= 2 {
			order = append(order, n)
		}
	}
	spreadOf := func(pads []topoPad) float64 {
		minx, miny := math.Inf(1), math.Inf(1)
		maxx, maxy := math.Inf(-1), math.Inf(-1)
		for _, p := range pads {
			minx = math.Min(minx, p.pos[0])
			miny = math.Min(miny, p.pos[1])
			maxx = math.Max(maxx, p.pos[0])
			maxy = math.Max(maxy, p.pos[1])
		}
		return (maxx - minx) + (maxy - miny)
	}
	sort.Slice(order, func(i, j int) bool {
		pa, pb := nets[order[i]], nets[order[j]]
		if len(pa) != len(pb) {
			return len(pa) < len(pb)
		}
		sa, sb := spreadOf(pa), spreadOf(pb)
		if sa != sb {
			return sa < sb
		}
		return order[i] < order[j]
	})

	type snap struct {
		res    []topoNetResult
		traces []core.Trace
		vias   []core.Via
	}
	var best *snap
	for round := 0; round < 5; round++ {
		board.ClearRoute()
		eng := buildTopoEngine(board, opts)
		queue := append([]string(nil), order...)
		ripped := map[string]int{}
		byNet := map[string]topoNetResult{}
		for len(queue) > 0 {
			net := queue[0]
			queue = queue[1:]
			eng.blockers = nil
			res := eng.routeNet(net, nets[net])
			if !res.ok {
				victim := ""
				for _, b := range eng.blockers {
					if r, ok := byNet[b]; ok && b != net && r.ok && ripped[b] < 3 {
						victim = b
						break
					}
				}
				if victim != "" {
					eng.ripNet(net)
					eng.ripNet(victim)
					ripped[victim]++
					delete(byNet, victim)
					queue = append([]string{net}, append(queue, victim)...)
					continue
				}
			}
			byNet[net] = res
		}
		var results []topoNetResult
		var fails []string
		for _, n := range order {
			if r, ok := byNet[n]; ok {
				results = append(results, r)
				if !r.ok {
					fails = append(fails, n)
				}
			}
		}
		better := best == nil
		if best != nil {
			bf := 0
			for _, r := range best.res {
				if !r.ok {
					bf++
				}
			}
			bl, nl := 0.0, 0.0
			for _, r := range best.res {
				bl += r.lengthMM
			}
			for _, r := range results {
				nl += r.lengthMM
			}
			better = len(fails) < bf || (len(fails) == bf && nl < bl)
		}
		if better {
			tr := append([]core.Trace(nil), board.Traces...)
			vi := append([]core.Via(nil), board.Vias...)
			best = &snap{res: results, traces: tr, vias: vi}
		}
		if len(fails) == 0 {
			break
		}
		failSet := map[string]bool{}
		for _, n := range fails {
			failSet[n] = true
		}
		sort.SliceStable(order, func(i, j int) bool {
			return failSet[order[i]] && !failSet[order[j]]
		})
	}
	if best == nil {
		return nil
	}
	board.Traces = best.traces
	board.Vias = best.vias
	return best.res
}

func buildTopoEngine(board *core.Board, opts Options) *topoEngine {
	var sites []topoSite
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			c := core.PadWorldCenter(fp, pad)
			w, h := core.PadWorldSize(fp, pad)
			sites = append(sites, topoSite{
				pos:  ptMM(c),
				half: p2{w.ToMM() / 2, h.ToMM() / 2},
			})
		}
	}
	mesh := buildDelaunay(sites)
	eng := &topoEngine{board: board, opts: opts, sites: sites}
	for i := 0; i < 2; i++ {
		eng.layers[i] = layerCdt{
			mesh:    mesh,
			used:    map[[2]int]float64{},
			penalty: map[[2]int]float64{},
		}
	}
	return eng
}

func (e *topoEngine) routeNet(net string, pads []topoPad) topoNetResult {
	wMM := e.opts.TraceWidthMM
	if e.opts.widths != nil {
		if w := e.opts.widths.maxMM(net); w > 0 {
			wMM = w
		}
	}
	clr := e.opts.ClearanceMM
	ep := func(p topoPad) topoEndPt {
		return topoEndPt{
			pos:    p.pos,
			top:    p.through || p.layer.IsTop(),
			bottom: p.through || !p.layer.IsTop(),
		}
	}
	attach := []topoEndPt{ep(pads[0])}
	remaining := make([]topoEndPt, 0, len(pads)-1)
	for _, p := range pads[1:] {
		remaining = append(remaining, ep(p))
	}
	total, lower := 0.0, 0.0
	segs, vias := 0, 0
	for len(remaining) > 0 {
		bi, bj, bd := 0, 0, math.Inf(1)
		for i, c := range attach {
			for j, r := range remaining {
				d := dist2(c.pos, r.pos)
				if d < bd {
					bi, bj, bd = i, j, d
				}
			}
		}
		from := attach[bi]
		to := remaining[bj]
		remaining = append(remaining[:bj], remaining[bj+1:]...)
		lower += bd
		done, reason := e.findAndRealise(net, from, to, wMM, clr)
		if reason != "" {
			return topoNetResult{
				net: net, ok: false,
				reason: fmt.Sprintf("no path from (%.2f, %.2f) to (%.2f, %.2f) mm: %s",
					from.pos[0], from.pos[1], to.pos[0], to.pos[1], reason),
				lengthMM: total, lowerBoundMM: lower, segments: segs, vias: vias,
			}
		}
		total += done.lengthMM
		segs += done.segments
		vias += done.vias
		attach = append(attach, to)
		for _, leg := range done.legs {
			for _, p := range leg.pts {
				attach = append(attach, topoEndPt{pos: p, top: leg.layer.IsTop(), bottom: !leg.layer.IsTop()})
			}
		}
		for _, v := range done.viaPoints {
			attach = append(attach, topoEndPt{pos: v, top: true, bottom: true})
		}
	}
	return topoNetResult{net: net, ok: true, lengthMM: total, lowerBoundMM: lower, segments: segs, vias: vias}
}

func (e *topoEngine) findAndRealise(net string, from, to topoEndPt, wMM, clr float64) (topoCommitted, string) {
	viaR := e.opts.ViaDiameterMM / 2
	obs := [2]*obstacleSet{
		e.validationObstacles(net, 0, clr),
		e.validationObstacles(net, 1, clr),
	}
	for i := range e.layers {
		e.layers[i].penalty = map[[2]int]float64{}
	}
	for attempt := 0; attempt < 6; attempt++ {
		plan, ok := e.mlAStar(from, to, wMM, clr, viaR, obs)
		if !ok {
			return topoCommitted{}, "no clear homotopy on either layer (vias included)"
		}
		strategy := attempt % 3
		var legs []topoDrawn
		var vias []p2
		allClear := true
		cursor := from.pos
		for _, lg := range plan {
			layer := core.LayerTop
			if lg.layer == 1 {
				layer = core.LayerBottom
			}
			target := to.pos
			if lg.via != nil {
				target = *lg.via
			}
			waypoints := e.layers[lg.layer].waypoints(e.sites, lg.crossed, lg.faces, cursor, target, wMM, clr, strategy)
			repaired := repairChain(waypoints, obs[lg.layer], wMM/2, clr)
			if repaired == nil {
				repaired = waypoints
			}
			pulled := stringPull(repaired, obs[lg.layer], wMM/2, clr)
			if !polylineClear(pulled, obs[lg.layer], wMM/2, clr) {
				if b := obs[lg.layer].firstBlockingNet(pulled, wMM/2, clr); b != "" && b != net {
					found := false
					for _, x := range e.blockers {
						if x == b {
							found = true
							break
						}
					}
					if !found {
						e.blockers = append(e.blockers, b)
					}
				}
				for _, ed := range lg.crossed {
					e.layers[lg.layer].penalty[ed] += 25
				}
				allClear = false
				break
			}
			if lg.via != nil {
				vias = append(vias, *lg.via)
				cursor = *lg.via
			}
			legs = append(legs, topoDrawn{layer: layer, pts: pulled})
		}
		if allClear {
			for _, lg := range plan {
				for _, ed := range lg.crossed {
					e.layers[lg.layer].used[ed] += wMM + clr
				}
			}
			return e.commit(net, legs, vias, wMM), ""
		}
	}
	return topoCommitted{}, "no clear homotopy on either layer (vias included)"
}

type topoOpen struct {
	f    float64
	node int
}

type topoHeap []topoOpen

func (h topoHeap) Len() int { return len(h) }
func (h topoHeap) Less(i, j int) bool {
	if h[i].f != h[j].f {
		return h[i].f < h[j].f
	}
	return h[i].node < h[j].node
}
func (h topoHeap) Swap(i, j int) { h[i], h[j] = h[j], h[i] }
func (h *topoHeap) Push(x any)   { *h = append(*h, x.(topoOpen)) }
func (h *topoHeap) Pop() any     { n := len(*h); x := (*h)[n-1]; *h = (*h)[:n-1]; return x }

type topoStepKind uint8

const (
	topoStepCross topoStepKind = iota
	topoStepFlip
)

type topoCame struct {
	prev int
	kind topoStepKind
	edge [2]int
	spot p2
}

// mlAStar walks the (face, layer) dual. Tie-breaks, all host-stable:
//   - open set: lower f, then lower node (face*2+layer)
//   - relaxation: strict <, so the first neighbour in the sorted adj
//     list keeps an equal-cost parent (adj is sorted by nb, sa, sb)
//   - start/goal face: lowest face id among triangles containing the
//     pad (a pad centre is a CDT vertex and sits in several faces)
func (e *topoEngine) mlAStar(from, to topoEndPt, wMM, clr, viaR float64, obs [2]*obstacleSet) ([]topoLeg, bool) {
	mesh := e.layers[0].mesh
	if mesh == nil {
		return nil, false
	}
	nf := len(mesh.centroid)
	if nf == 0 || e.layers[1].mesh == nil || len(e.layers[1].mesh.centroid) != nf {
		return nil, false
	}
	fFrom, ok := mesh.faceContaining(from.pos)
	if !ok {
		return nil, false
	}
	fTo, ok := mesh.faceContaining(to.pos)
	if !ok {
		return nil, false
	}
	var startLayers []int
	if from.top {
		startLayers = append(startLayers, 0)
	}
	if from.bottom {
		startLayers = append(startLayers, 1)
	}
	goalOK := [2]bool{to.top, to.bottom}
	viaCost := 0.25 * e.opts.ViaCost

	n := nf * 2
	g := make([]float64, n)
	for i := range g {
		g[i] = math.Inf(1)
	}
	came := make([]topoCame, n)
	hasCame := make([]bool, n)
	h := &topoHeap{}
	heap.Init(h)
	for _, l := range startLayers {
		node := fFrom*2 + l
		g[node] = 0
		heap.Push(h, topoOpen{f: 0, node: node})
	}
	closed := map[int]bool{}
	goal := -1
	for h.Len() > 0 {
		cur := heap.Pop(h).(topoOpen)
		face, layer := cur.node/2, cur.node%2
		if face == fTo && goalOK[layer] {
			goal = cur.node
			break
		}
		if closed[cur.node] {
			continue
		}
		closed[cur.node] = true
		lc := &e.layers[layer]
		for _, ad := range lc.mesh.adj[face] {
			pa, pb := e.sites[ad.sa].pos, e.sites[ad.sb].pos
			length := dist2(pa, pb)
			if length < 1e-9 {
				continue
			}
			d := p2{(pb[0] - pa[0]) / length, (pb[1] - pa[1]) / length}
			key := siteKey(ad.sa, ad.sb)
			used := lc.used[key]
			free := length - e.sites[ad.sa].reach(d) - e.sites[ad.sb].reach(d) - used
			need := wMM + 2*clr
			if free < 0.5*need {
				continue
			}
			squeeze := 0.0
			if free < need {
				squeeze = 8 * (need - free)
			}
			step := dist2(lc.mesh.centroid[face], lc.mesh.centroid[ad.nb]) + 0.3*used + squeeze + lc.penalty[key]
			nnode := ad.nb*2 + layer
			cand := g[cur.node] + step
			if cand < g[nnode] {
				g[nnode] = cand
				came[nnode] = topoCame{prev: cur.node, kind: topoStepCross, edge: key}
				hasCame[nnode] = true
				heap.Push(h, topoOpen{f: cand + dist2(lc.mesh.centroid[ad.nb], to.pos), node: nnode})
			}
		}
		other := 1 - layer
		nnode := face*2 + other
		if g[cur.node]+viaCost < g[nnode] {
			if spot, ok := findViaSpot(lc.mesh.centroid[face], viaR, clr, obs[0], obs[1]); ok {
				cand := g[cur.node] + viaCost
				if cand < g[nnode] {
					g[nnode] = cand
					came[nnode] = topoCame{prev: cur.node, kind: topoStepFlip, spot: spot}
					hasCame[nnode] = true
					heap.Push(h, topoOpen{f: cand + dist2(spot, to.pos), node: nnode})
				}
			}
		}
	}
	if goal < 0 {
		return nil, false
	}
	var rev []struct {
		node int
		step topoCame
	}
	cur := goal
	for hasCame[cur] {
		st := came[cur]
		rev = append(rev, struct {
			node int
			step topoCame
		}{cur, st})
		cur = st.prev
	}
	for i, j := 0, len(rev)-1; i < j; i, j = i+1, j-1 {
		rev[i], rev[j] = rev[j], rev[i]
	}
	var plan []topoLeg
	legLayer := cur % 2
	var edges [][2]int
	faces := []int{cur / 2}
	for _, item := range rev {
		switch item.step.kind {
		case topoStepCross:
			edges = append(edges, item.step.edge)
			faces = append(faces, item.node/2)
		case topoStepFlip:
			spot := item.step.spot
			plan = append(plan, topoLeg{layer: legLayer, crossed: edges, faces: faces, via: &spot})
			edges = nil
			legLayer = item.node % 2
			faces = []int{item.node / 2}
		}
	}
	plan = append(plan, topoLeg{layer: legLayer, crossed: edges, faces: faces})
	return plan, true
}

func (e *topoEngine) validationObstacles(net string, layer uint8, _ float64) *obstacleSet {
	return collectObstacles(e.board, net, layer, e.opts)
}

func (e *topoEngine) ripNet(net string) {
	keptT := e.board.Traces[:0]
	for _, tr := range e.board.Traces {
		if tr.Net != net {
			keptT = append(keptT, tr)
		}
	}
	e.board.Traces = keptT
	keptV := e.board.Vias[:0]
	for _, v := range e.board.Vias {
		if v.Net != net {
			keptV = append(keptV, v)
		}
	}
	e.board.Vias = keptV
	for i := range e.layers {
		e.layers[i].used = map[[2]int]float64{}
		e.layers[i].penalty = map[[2]int]float64{}
	}
}

func (e *topoEngine) commit(net string, legs []topoDrawn, vias []p2, wMM float64) topoCommitted {
	out := topoCommitted{legs: legs, viaPoints: vias}
	for _, leg := range legs {
		for i := 0; i+1 < len(leg.pts); i++ {
			a, b := leg.pts[i], leg.pts[i+1]
			if dist2(a, b) < 1e-6 {
				continue
			}
			e.board.Traces = append(e.board.Traces, core.Trace{
				ID:    core.NewID(),
				Layer: leg.layer,
				Start: core.NewPoint(core.FromMM(a[0]), core.FromMM(a[1])),
				End:   core.NewPoint(core.FromMM(b[0]), core.FromMM(b[1])),
				Width: core.FromMM(wMM),
				Net:   net,
			})
			out.segments++
			out.lengthMM += dist2(a, b)
		}
	}
	for _, v := range vias {
		e.board.Vias = append(e.board.Vias, core.Via{
			ID:       core.NewID(),
			Position: core.NewPoint(core.FromMM(v[0]), core.FromMM(v[1])),
			Diameter: core.FromMM(e.opts.ViaDiameterMM),
			Drill:    core.FromMM(e.opts.ViaDrillMM),
			Net:      net,
		})
		out.vias++
	}
	return out
}

func (l *layerCdt) waypoints(sites []topoSite, crossed [][2]int, faces []int, from, to p2, wMM, clr float64, strategy int) []p2 {
	pts := make([]p2, 0, len(crossed)*2+2)
	pts = append(pts, from)
	for k, e := range crossed {
		if k < len(faces) && l.mesh != nil && faces[k] < len(l.mesh.centroid) {
			pts = append(pts, l.mesh.centroid[faces[k]])
		}
		if e[0] < 0 || e[1] < 0 || e[0] >= len(sites) || e[1] >= len(sites) {
			continue
		}
		pa, pb := sites[e[0]].pos, sites[e[1]].pos
		length := dist2(pa, pb)
		if length < 1e-9 {
			continue
		}
		used := l.used[siteKey(e[0], e[1])]
		d := p2{(pb[0] - pa[0]) / length, (pb[1] - pa[1]) / length}
		lo := sites[e[0]].reach(d) + clr + used + wMM/2
		hi := length - sites[e[1]].reach(d) - clr - wMM/2
		off := lo
		if hi > lo {
			switch strategy {
			case 0:
				off = lo
			case 1:
				off = (lo + hi) / 2
			default:
				off = hi
			}
		} else if lo > length*0.9 {
			off = length * 0.9
		}
		t := off / length
		if t < 0.05 {
			t = 0.05
		}
		if t > 0.95 {
			t = 0.95
		}
		pts = append(pts, p2{pa[0] + (pb[0]-pa[0])*t, pa[1] + (pb[1]-pa[1])*t})
	}
	if n := len(faces); n > 0 && l.mesh != nil && faces[n-1] < len(l.mesh.centroid) {
		pts = append(pts, l.mesh.centroid[faces[n-1]])
	}
	pts = append(pts, to)
	return pts
}

func repairChain(chain []p2, obs *obstacleSet, hw, clr float64) []p2 {
	if len(chain) < 2 {
		return chain
	}
	out := []p2{chain[0]}
	for i := 0; i+1 < len(chain); i++ {
		sub := clearSubpath(chain[i], chain[i+1], obs, hw, clr, 4)
		if sub == nil {
			return nil
		}
		out = append(out, sub[1:]...)
	}
	return out
}

func clearSubpath(a, b p2, obs *obstacleSet, hw, clr float64, depth uint8) []p2 {
	if polylineClear([]p2{a, b}, obs, hw, clr) {
		return []p2{a, b}
	}
	if depth == 0 {
		return nil
	}
	length := dist2(a, b)
	if length < 0.05 {
		return nil
	}
	mid := p2{(a[0] + b[0]) / 2, (a[1] + b[1]) / 2}
	n := p2{-(b[1] - a[1]) / length, (b[0] - a[0]) / length}
	for _, d := range []float64{0.4, 0.8, 1.6, 3.2} {
		for _, sgn := range []float64{1, -1} {
			m := p2{mid[0] + n[0]*d*sgn, mid[1] + n[1]*d*sgn}
			left := clearSubpath(a, m, obs, hw, clr, depth-1)
			if left == nil {
				continue
			}
			right := clearSubpath(m, b, obs, hw, clr, depth-1)
			if right == nil {
				continue
			}
			return append(left, right[1:]...)
		}
	}
	return nil
}

func findViaSpot(c p2, viaR, clr float64, top, bottom *obstacleSet) (p2, bool) {
	for ring := 0; ring < 5; ring++ {
		rad := 0.7 * float64(ring)
		steps := 1
		if ring > 0 {
			steps = 12
		}
		for k := 0; k < steps; k++ {
			ang := 2 * math.Pi * float64(k) / float64(steps)
			v := p2{c[0] + rad*math.Cos(ang), c[1] + rad*math.Sin(ang)}
			if polylineClear([]p2{v, v}, top, viaR, clr) && polylineClear([]p2{v, v}, bottom, viaR, clr) {
				return v, true
			}
		}
	}
	return p2{}, false
}
