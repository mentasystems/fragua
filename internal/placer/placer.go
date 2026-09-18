// Package placer implements global electrostatic placement + SA legalisation.
//
// SA-only (GlobalStage=false) matches Rust pcb_placer::place with the
// same seed, movable order, xorshift64* draws, schedule, hard floor,
// and weighted-HPWL + soft-gap score. GlobalStage=true runs the
// literature ePlace solve (Poisson/DCT + WA wirelength + Nesterov,
// ported from rust 4e20ae0 global.rs) then retunes SA to the
// post-global refinement schedule. globalForce is kept only as the
// old cheap-force baseline for tests.
package placer

import (
	"fmt"
	"math"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
)

// Options configures a place run. Defaults follow Rust PlaceOptions.
type Options struct {
	Seed            uint64
	SolderGapMM     float64
	Iterations      int
	MoveStdMM       float64
	GlobalStage     bool
	EdgeClearanceMM float64
	MinGapMM        float64
	MinClearanceMM  float64
	GapPenalty      float64
	InitialTemp     float64
	FinalTemp       float64
	MaxStepMM       float64
	MinStepMM       float64
	Decouple        bool
	// GlobalIterations is the Nesterov ceiling for ePlace (Rust default 600).
	// The loop exits early once overflow is legal and HPWL plateaus.
	GlobalIterations int
	// DensityBins is the Poisson grid resolution per axis (clamped 16–256).
	DensityBins int
	// TargetDensity is the bin utilisation ePlace treats as legal packing.
	TargetDensity float64
	// TargetOverflow is the movable-charge overflow at which ePlace stops
	// growing λ (Rust / ePlace-conventional 0.08).
	TargetOverflow float64
	// Progress, when set, is called every ProgressEvery iterations of the
	// SA loop so a host can stream a bar. Must not block.
	Progress func(done, total int)
	// Cancel, when set, ends the anneal early. The board keeps the best
	// arrangement found so far — never a half-applied move.
	Cancel func() bool
}

// progressEvery is how often the SA loop reports: often enough to animate a
// bar, rare enough that the callback is noise next to the annealing itself.
const progressEvery = 200

// DefaultOptions matches Rust PlaceOptions::default (SA-only temps;
// after a global stage the loop retunes T 5→0.05 / step 8→0.25).
func DefaultOptions() Options {
	return Options{
		Seed:             42,
		SolderGapMM:      core.MinFootprintGapMM,
		Iterations:       8000,
		MoveStdMM:        20.0,
		GlobalStage:      true,
		EdgeClearanceMM:  0.8,
		MinGapMM:         2.0,
		MinClearanceMM:   0.5,
		GapPenalty:       16.0,
		InitialTemp:      50.0,
		FinalTemp:        0.05,
		MaxStepMM:        20.0,
		MinStepMM:        0.5,
		Decouple:         true,
		GlobalIterations: 600,
		DensityBins:      64,
		TargetDensity:    1.0,
		TargetOverflow:   0.08,
	}
}

// ParseOptions overlays script args.
func ParseOptions(o Options, args string) Options {
	for _, f := range strings.Fields(args) {
		kv := strings.SplitN(f, "=", 2)
		if len(kv) != 2 {
			continue
		}
		var x float64
		fmt.Sscanf(kv[1], "%f", &x)
		switch kv[0] {
		case "seed":
			o.Seed = uint64(x)
		case "iterations":
			o.Iterations = int(x)
		case "solder_gap":
			o.SolderGapMM = x
		case "global_stage", "global":
			o.GlobalStage = x != 0
		case "decouple":
			o.Decouple = x != 0
		case "iters":
			o.Iterations = int(x)
		case "global_iterations":
			o.GlobalIterations = int(x)
		case "density_bins":
			o.DensityBins = int(x)
		case "target_density":
			o.TargetDensity = x
		case "target_overflow":
			o.TargetOverflow = x
		}
	}
	return o
}

// Report summarises placement.
type Report struct {
	InitialHPWLMM float64  `json:"initial_hpwl_mm"`
	FinalHPWLMM   float64  `json:"final_hpwl_mm"`
	Moved         []string `json:"moved"`
	Iterations    int      `json:"iterations"`
	// Seated counts parts the caller materialised on the board for this run.
	// Place only moves footprints that already exist; the palette lives on the
	// project, so the script layer seats them and reports the count here.
	Seated int `json:"seated"`
	// Global is the ePlace stage outcome; nil when GlobalStage was off.
	Global *GlobalReport `json:"global,omitempty"`
}

// Summary is agent-friendly.
func (r Report) Summary() string {
	seated := ""
	if r.Seated > 0 {
		seated = fmt.Sprintf("seated %d new, ", r.Seated)
	}
	return fmt.Sprintf("place: %sHPWL %.2f → %.2f mm, moved %d parts, %d iters",
		seated, r.InitialHPWLMM, r.FinalHPWLMM, len(r.Moved), r.Iterations)
}

type pos struct {
	x, y core.Length
	rot  float64
}

// Place runs optional ePlace global placement then SA legalisation
// on refs (nil refs = all non-edge-mounted footprints, footprint-order).
func Place(board *core.Board, refs []string, opts Options) (Report, error) {
	if board.Outline == nil {
		return Report{}, fmt.Errorf("place: board has no outline")
	}

	var fps []*core.Footprint
	if len(refs) == 0 {
		for _, id := range board.FootprintOrder {
			fp := board.Footprints[id]
			if fp != nil && !fp.EdgeMounted && !fp.Pinned {
				fps = append(fps, fp)
			}
		}
		if len(fps) == 0 {
			for _, fp := range board.Footprints {
				if fp != nil && !fp.EdgeMounted && !fp.Pinned {
					fps = append(fps, fp)
				}
			}
		}
	} else {
		for _, r := range refs {
			if fp := board.FootprintByRef(r); fp != nil {
				fps = append(fps, fp)
			}
		}
	}
	initHPWL := rawHPWL(board)
	if board.Outline != nil {
		for _, fp := range footprintsAll(board) {
			// A pinned part keeps the coordinates the agent gave it, even
			// when the library calls the footprint edge-mounted: a THT
			// connector often has to sit inboard of the outline.
			if fp.EdgeMounted && !fp.Pinned {
				SnapToNearestEdge(fp, *board.Outline, EdgeClearancesFor(board))
			}
		}
	}
	if len(fps) == 0 {
		// A bare auto-place over a board whose every part is already
		// anchored (pinned or edge-mounted) is a no-op, not an error:
		// the agent has said where everything goes.
		if len(refs) == 0 && len(board.Footprints) > 0 {
			return Report{InitialHPWLMM: initHPWL, FinalHPWLMM: rawHPWL(board)}, nil
		}
		return Report{}, fmt.Errorf("place: no movable footprints")
	}
	var global *GlobalReport
	if opts.GlobalStage {
		g := globalPlace(board, fps, opts)
		global = &g
	}

	// After a global stage Rust retunes SA into a refinement role.
	initT, finalT := opts.InitialTemp, opts.FinalTemp
	maxStep, minStep := opts.MaxStepMM, opts.MinStepMM
	if opts.GlobalStage {
		initT, finalT = 5.0, 0.05
		maxStep, minStep = 8.0, 0.25
		if opts.MoveStdMM > 0 && opts.MoveStdMM != 20.0 && opts.MoveStdMM != 8.0 {
			maxStep = opts.MoveStdMM
		}
	} else if opts.MoveStdMM > 0 && opts.MoveStdMM != 20.0 {
		maxStep = opts.MoveStdMM
	}

	maxIter := opts.Iterations
	if maxIter <= 0 {
		maxIter = 8000
	}
	prng := newRNG(opts.Seed)
	cooling := math.Pow(finalT/initT, 1.0/float64(maxIter))
	curScore := compositeScore(board, fps, opts)
	bestScore := curScore
	best := map[string]pos{}
	start := map[string]pos{}
	for _, fp := range fps {
		p := pos{fp.Position.X, fp.Position.Y, fp.Rotation}
		best[fp.Reference] = p
		start[fp.Reference] = p
	}

	o := board.Outline
	hardClear := math.Max(opts.MinClearanceMM, opts.SolderGapMM)
	ranIter := maxIter
	for iter := 0; iter < maxIter; iter++ {
		if iter%progressEvery == 0 {
			if opts.Progress != nil {
				opts.Progress(iter, maxIter)
			}
			if opts.Cancel != nil && opts.Cancel() {
				ranIter = iter
				break
			}
		}
		temp := initT * math.Pow(cooling, float64(iter))
		progress := float64(iter) / float64(maxIter)
		step := maxStep*(1.0-progress) + minStep*progress
		fp := fps[int(prng.nextU64()%uint64(len(fps)))]
		old := pos{fp.Position.X, fp.Position.Y, fp.Rotation}
		beforeGap := minGapAgainstOthers(board, fp)

		roll := prng.nextU32() % 16
		switch roll {
		case 0:
			fp.Rotation = math.Mod(fp.Rotation+90, 360)
		case 1:
			ow, oh := o.Width().ToMM(), o.Height().ToMM()
			const margin = 2.0
			spanX := math.Max(0, ow-2*margin)
			spanY := math.Max(0, oh-2*margin)
			nx := o.Min.X.ToMM() + margin + prng.nextF64()*spanX
			ny := o.Min.Y.ToMM() + margin + prng.nextF64()*spanY
			fp.Position = core.NewPoint(core.FromMM(nx), core.FromMM(ny))
		default:
			dx := (prng.nextF64() - 0.5) * 2.0 * step
			dy := (prng.nextF64() - 0.5) * 2.0 * step
			// Rust: new_pos = old + Length::from_mm(d) — not FromMM(old_mm+d).
			fp.Position = core.NewPoint(old.x+core.FromMM(dx), old.y+core.FromMM(dy))
		}

		if !padsInside(fp, o, opts.EdgeClearanceMM) ||
			firstOverlapperGap(board, fp, opts.SolderGapMM) || hitsNoPlace(board, fp) {
			fp.Position = core.NewPoint(old.x, old.y)
			fp.Rotation = old.rot
			continue
		}
		afterGap := minGapAgainstOthers(board, fp)
		if afterGap < hardClear && afterGap <= beforeGap {
			fp.Position = core.NewPoint(old.x, old.y)
			fp.Rotation = old.rot
			continue
		}

		newScore := compositeScore(board, fps, opts)
		dE := newScore - curScore
		accept := dE <= 0
		if !accept && temp > 0 {
			accept = prng.nextF64() < math.Exp(-dE/temp)
		}
		if accept {
			curScore = newScore
			if curScore < bestScore {
				bestScore = curScore
				for _, f := range fps {
					best[f.Reference] = pos{f.Position.X, f.Position.Y, f.Rotation}
				}
			}
		} else {
			fp.Position = core.NewPoint(old.x, old.y)
			fp.Rotation = old.rot
		}
	}

	for _, fp := range fps {
		b := best[fp.Reference]
		fp.Position = core.NewPoint(b.x, b.y)
		fp.Rotation = b.rot
	}

	if opts.Decouple {
		PullPassivesToAnchors(board, fps, opts)
	}

	// Count what moved against the final positions, not against the
	// annealer's best: the decouple pass moves parts too, and reporting
	// "moved 0 parts" next to a changed HPWL is simply untrue.
	var moved []string
	for _, fp := range fps {
		s := start[fp.Reference]
		dx := math.Abs((fp.Position.X - s.x).ToMM())
		dy := math.Abs((fp.Position.Y - s.y).ToMM())
		if dx >= 0.05 || dy >= 0.05 || fp.Rotation != s.rot {
			moved = append(moved, fp.Reference)
		}
	}

	return Report{
		InitialHPWLMM: initHPWL,
		FinalHPWLMM:   rawHPWL(board),
		Moved:         moved,
		Iterations:    ranIter,
		Global:        global,
	}, nil
}

// globalForce is the pre-ePlace cheap attraction/repulsion stand-in.
// Kept so tests can show that real ePlace improves HPWL/overlap versus it.
func globalForce(board *core.Board, fps []*core.Footprint, opts Options) {
	type posSnap struct {
		x, y core.Length
		rot  float64
	}
	bestHPWL := rawHPWL(board)
	best := map[string]posSnap{}
	for _, fp := range fps {
		best[fp.Reference] = posSnap{fp.Position.X, fp.Position.Y, fp.Rotation}
	}
	for iter := 0; iter < 80; iter++ {
		netPads := map[string][][2]float64{}
		for _, fp := range board.Footprints {
			if fp == nil {
				continue
			}
			for i := range fp.Pads {
				pad := &fp.Pads[i]
				if pad.Net == nil || *pad.Net == "" {
					continue
				}
				c := core.PadWorldCenter(fp, pad)
				netPads[*pad.Net] = append(netPads[*pad.Net], [2]float64{c.X.ToMM(), c.Y.ToMM()})
			}
		}
		forces := map[string][2]float64{}
		for _, fp := range fps {
			fx, fy := 0.0, 0.0
			n := 0
			for i := range fp.Pads {
				pad := &fp.Pads[i]
				if pad.Net == nil {
					continue
				}
				mates := netPads[*pad.Net]
				if len(mates) < 2 {
					continue
				}
				cx, cy := 0.0, 0.0
				for _, m := range mates {
					cx += m[0]
					cy += m[1]
				}
				cx /= float64(len(mates))
				cy /= float64(len(mates))
				pc := core.PadWorldCenter(fp, pad)
				fx += cx - pc.X.ToMM()
				fy += cy - pc.Y.ToMM()
				n++
			}
			if n > 0 {
				forces[fp.Reference] = [2]float64{fx / float64(n), fy / float64(n)}
			}
		}
		step := 0.15
		for _, fp := range fps {
			f := forces[fp.Reference]
			nx := fp.Position.X.ToMM() + f[0]*step
			ny := fp.Position.Y.ToMM() + f[1]*step
			nx, ny = clampToOutline(nx, ny, board.Outline, 1.0)
			fp.Position = core.NewPoint(core.FromMM(nx), core.FromMM(ny))
		}
		for i := 0; i < len(fps); i++ {
			for j := i + 1; j < len(fps); j++ {
				a, b := fps[i], fps[j]
				dx := b.Position.X.ToMM() - a.Position.X.ToMM()
				dy := b.Position.Y.ToMM() - a.Position.Y.ToMM()
				d := math.Hypot(dx, dy)
				if d < 0.01 {
					d = 0.01
				}
				if d < opts.SolderGapMM*2 {
					push := (opts.SolderGapMM*2 - d) * 0.1
					ux, uy := dx/d, dy/d
					a.Position = core.NewPoint(
						core.FromMM(a.Position.X.ToMM()-ux*push),
						core.FromMM(a.Position.Y.ToMM()-uy*push),
					)
					b.Position = core.NewPoint(
						core.FromMM(b.Position.X.ToMM()+ux*push),
						core.FromMM(b.Position.Y.ToMM()+uy*push),
					)
				}
			}
		}
		h := rawHPWL(board)
		if h < bestHPWL {
			bestHPWL = h
			for _, fp := range fps {
				best[fp.Reference] = posSnap{fp.Position.X, fp.Position.Y, fp.Rotation}
			}
		}
	}
	for _, fp := range fps {
		b := best[fp.Reference]
		fp.Position = core.NewPoint(b.x, b.y)
		fp.Rotation = b.rot
	}
}

func clampToOutline(x, y float64, o *core.Rect, marginMM float64) (float64, float64) {
	if o == nil {
		return x, y
	}
	minX := o.Min.X.ToMM() + marginMM
	minY := o.Min.Y.ToMM() + marginMM
	maxX := o.Max.X.ToMM() - marginMM
	maxY := o.Max.Y.ToMM() - marginMM
	if x < minX {
		x = minX
	}
	if y < minY {
		y = minY
	}
	if x > maxX {
		x = maxX
	}
	if y > maxY {
		y = maxY
	}
	return x, y
}

func footprintBounds(fp *core.Footprint) (core.Rect, bool) {
	if fp == nil || len(fp.Pads) == 0 {
		return core.Rect{}, false
	}
	r := core.PadWorldAABB(fp, &fp.Pads[0])
	for i := 1; i < len(fp.Pads); i++ {
		r = r.Union(core.PadWorldAABB(fp, &fp.Pads[i]))
	}
	return r, true
}

func padsInside(fp *core.Footprint, o *core.Rect, edgeMM float64) bool {
	b, ok := footprintBounds(fp)
	if !ok || o == nil {
		return false
	}
	e := core.FromMM(edgeMM)
	if fp.EdgeMounted {
		e = 0
	}
	return b.Min.X >= o.Min.X+e && b.Min.Y >= o.Min.Y+e &&
		b.Max.X <= o.Max.X-e && b.Max.Y <= o.Max.Y-e
}

// noPlaceRects collects the placement exclusions declared on the board: every
// keepout with no_place set, as a rectangle. A polygon keepout contributes its
// bounding box, which is the conservative reading.
func noPlaceRects(board *core.Board) []core.Rect {
	if board == nil {
		return nil
	}
	var out []core.Rect
	for i := range board.Keepouts {
		k := &board.Keepouts[i]
		if !k.NoPlace {
			continue
		}
		switch {
		case k.Rect != nil:
			out = append(out, *k.Rect)
		case len(k.Polygon) > 0:
			r := core.Rect{Min: k.Polygon[0], Max: k.Polygon[0]}
			for _, p := range k.Polygon[1:] {
				r = r.Union(core.Rect{Min: p, Max: p})
			}
			out = append(out, r)
		}
	}
	return out
}

// hitsNoPlace reports whether a footprint's extent lands in a no_place keepout.
// Without this the annealer seats parts inside milled slots, under a module's
// PCB antenna and on top of mounting screws: it optimises wirelength against
// the outline only, and a keepout is invisible to wirelength.
func hitsNoPlace(board *core.Board, fp *core.Footprint) bool {
	b, ok := footprintBounds(fp)
	if !ok {
		return false
	}
	for _, r := range noPlaceRects(board) {
		if b.Intersects(r) {
			return true
		}
	}
	return false
}

// noPlaceOverlapMM2 is the total area the given footprints put inside no_place
// keepouts. hitsNoPlace refuses to move a part *into* an exclusion; this makes
// a part that was already sitting in one strictly worse off staying there, so
// the anneal walks it out instead of leaving it where the script dropped it.
func noPlaceOverlapMM2(board *core.Board, fps []*core.Footprint) float64 {
	rects := noPlaceRects(board)
	if len(rects) == 0 {
		return 0
	}
	total := 0.0
	for _, fp := range fps {
		b, ok := footprintBounds(fp)
		if !ok {
			continue
		}
		for _, r := range rects {
			w := math.Min(b.Max.X.ToMM(), r.Max.X.ToMM()) - math.Max(b.Min.X.ToMM(), r.Min.X.ToMM())
			h := math.Min(b.Max.Y.ToMM(), r.Max.Y.ToMM()) - math.Max(b.Min.Y.ToMM(), r.Min.Y.ToMM())
			if w > 0 && h > 0 {
				total += w * h
			}
		}
	}
	return total
}

// canCollide reports whether two footprints can physically clash.
//
// Two surface-mount parts on opposite faces of the board cannot: they are a
// board thickness apart. The placer used to compare every pair of pad boxes
// regardless of layer, so putting a wire header on the bottom copper bought no
// room at all - which on a small carrier is most of the board. A through-hole
// pad is copper on every layer, so a part that has one collides with both
// faces; DRC already reasons this way.
func canCollide(a, b *core.Footprint) bool {
	if a.Layer == b.Layer {
		return true
	}
	return hasThroughHole(a) || hasThroughHole(b)
}

func hasThroughHole(fp *core.Footprint) bool {
	for i := range fp.Pads {
		if fp.Pads[i].Drill != nil && *fp.Pads[i].Drill > 0 {
			return true
		}
	}
	return false
}

// firstOverlapper: pad-bbox union expanded by MIN_FOOTPRINT_GAP/2 intersects another.
func firstOverlapper(board *core.Board, probe *core.Footprint) bool {
	return firstOverlapperGap(board, probe, core.MinFootprintGapMM)
}

// firstOverlapperGap is firstOverlapper with the assembly gap made explicit.
// The 2.0 mm package constant is a sensible default for a roomy board and a
// veto on a small one: on a 30 x 30 mm carrier it puts a 1 mm halo round every
// 0603 and there is simply nowhere left to anneal to. `auto-place solder_gap=N`
// parses into Options.SolderGapMM, but until now only the soft penalty and the
// afterGap test read it - the hard rejection here used the constant, so the
// option could not actually buy any room.
func firstOverlapperGap(board *core.Board, probe *core.Footprint, gapMM float64) bool {
	if gapMM < 0 {
		gapMM = 0
	}
	half := core.FromMM(gapMM / 2)
	pb, ok := collisionBounds(probe, half)
	if !ok {
		return false
	}
	for _, id := range board.FootprintOrder {
		fp := board.Footprints[id]
		if fp == nil || fp == probe || fp.ID == probe.ID {
			continue
		}
		if !canCollide(probe, fp) {
			continue
		}
		ob, ok := collisionBounds(fp, half)
		if !ok {
			continue
		}
		if pb.Intersects(ob) {
			return true
		}
	}
	// FootprintOrder may miss map-only entries.
	if len(board.FootprintOrder) == 0 {
		for _, fp := range board.Footprints {
			if fp == nil || fp == probe || fp.ID == probe.ID {
				continue
			}
			if !canCollide(probe, fp) {
				continue
			}
			ob, ok := collisionBounds(fp, half)
			if !ok {
				continue
			}
			if pb.Intersects(ob) {
				return true
			}
		}
	}
	return false
}

// collisionBounds is the rectangle the placer must keep clear of its
// neighbours: the pad AABB grown by half the assembly gap, unioned with the
// courtyard when the library declares one. Testing pads alone let auto-place
// seat a part inside a neighbour's declared body — a module wider than its
// pad rows, or a screw terminal's wire mouth — which DRC then reported as a
// courtyard_overlap the agent had to repair by hand.
//
// Only a DECLARED courtyard counts (body_rect or a placement_margin). The
// pads+CourtyardMarginMM fallback is left out on purpose: it is a DRC
// convention, not a package outline, and folding it in here would override
// the assembly gap the caller asked for on plain chip parts.
func collisionBounds(fp *core.Footprint, halfGap core.Length) (core.Rect, bool) {
	b, ok := footprintBounds(fp)
	if !ok {
		return core.Rect{}, false
	}
	b = b.Expand(halfGap)
	if fp.BodyRect != nil || !fp.PlacementMargin.IsZero() {
		if c, ok := core.CourtyardWorld(fp); ok {
			b = b.Union(c)
		}
	}
	return b, true
}

func aabbGapMM(a, b core.Rect) float64 {
	var dx, dy float64
	if a.Max.X < b.Min.X {
		dx = (b.Min.X - a.Max.X).ToMM()
	} else if b.Max.X < a.Min.X {
		dx = (a.Min.X - b.Max.X).ToMM()
	} else {
		overlap := minLen(a.Max.X, b.Max.X) - maxLen(a.Min.X, b.Min.X)
		dx = -overlap.ToMM()
	}
	if a.Max.Y < b.Min.Y {
		dy = (b.Min.Y - a.Max.Y).ToMM()
	} else if b.Max.Y < a.Min.Y {
		dy = (a.Min.Y - b.Max.Y).ToMM()
	} else {
		overlap := minLen(a.Max.Y, b.Max.Y) - maxLen(a.Min.Y, b.Min.Y)
		dy = -overlap.ToMM()
	}
	if dx >= 0 && dy >= 0 {
		return math.Min(dx, dy)
	}
	if dx >= 0 {
		return dx
	}
	if dy >= 0 {
		return dy
	}
	if dx < dy {
		return dx
	}
	return dy
}

func minLen(a, b core.Length) core.Length {
	if a < b {
		return a
	}
	return b
}

func maxLen(a, b core.Length) core.Length {
	if a > b {
		return a
	}
	return b
}

func minGapAgainstOthers(board *core.Board, probe *core.Footprint) float64 {
	pb, ok := footprintBounds(probe)
	if !ok {
		return math.Inf(1)
	}
	m := math.Inf(1)
	visit := func(fp *core.Footprint) {
		if fp == nil || fp == probe || fp.ID == probe.ID || !canCollide(probe, fp) {
			return
		}
		ob, ok := footprintBounds(fp)
		if !ok {
			return
		}
		g := aabbGapMM(pb, ob)
		if g < m {
			m = g
		}
	}
	if len(board.FootprintOrder) > 0 {
		for _, id := range board.FootprintOrder {
			visit(board.Footprints[id])
		}
	} else {
		for _, fp := range board.Footprints {
			visit(fp)
		}
	}
	return m
}

func pairGapPenalty(a, b *core.Footprint, minGap float64) float64 {
	ab, okA := footprintBounds(a)
	bb, okB := footprintBounds(b)
	if !okA || !okB {
		return 0
	}
	gap := aabbGapMM(ab, bb)
	if gap >= minGap {
		return 0
	}
	s := minGap - math.Max(gap, 0)
	return s * s
}

func totalGapPenalty(board *core.Board, minGap float64) float64 {
	var fps []*core.Footprint
	if len(board.FootprintOrder) > 0 {
		for _, id := range board.FootprintOrder {
			if fp := board.Footprints[id]; fp != nil {
				fps = append(fps, fp)
			}
		}
	} else {
		for _, fp := range board.Footprints {
			if fp != nil {
				fps = append(fps, fp)
			}
		}
	}
	sum := 0.0
	for i := 0; i < len(fps); i++ {
		for j := i + 1; j < len(fps); j++ {
			sum += pairGapPenalty(fps[i], fps[j], minGap)
		}
	}
	return sum
}

func netWeight(nPads int) float64 {
	d := nPads - 1
	if d < 1 {
		d = 1
	}
	return 4.0 / float64(d)
}

// rawHPWL is unweighted half-perimeter wirelength (report metric).
func rawHPWL(board *core.Board) float64 {
	type bb struct {
		minX, minY, maxX, maxY float64
		n                      int
	}
	// Ordered alongside the index: summing over a Go map iterates in a
	// random order, and the float rounding that follows was enough to flip
	// SA accept/reject decisions, so the same seed gave different boards.
	nets := map[string]*bb{}
	var order []*bb
	walk := func(fp *core.Footprint) {
		if fp == nil {
			return
		}
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net == "" {
				continue
			}
			c := core.PadWorldCenter(fp, pad)
			x, y := c.X.ToMM(), c.Y.ToMM()
			e := nets[*pad.Net]
			if e == nil {
				e = &bb{minX: x, minY: y, maxX: x, maxY: y, n: 1}
				nets[*pad.Net] = e
				order = append(order, e)
				continue
			}
			if x < e.minX {
				e.minX = x
			}
			if y < e.minY {
				e.minY = y
			}
			if x > e.maxX {
				e.maxX = x
			}
			if y > e.maxY {
				e.maxY = y
			}
			e.n++
		}
	}
	if len(board.FootprintOrder) > 0 {
		for _, id := range board.FootprintOrder {
			walk(board.Footprints[id])
		}
	} else {
		for _, fp := range board.Footprints {
			walk(fp)
		}
	}
	total := 0.0
	for _, e := range order {
		if e.n >= 2 && e.maxX >= e.minX && e.maxY >= e.minY {
			total += (e.maxX - e.minX) + (e.maxY - e.minY)
		}
	}
	return total
}

// weightedHPWL is the SA objective wirelength term (4/(n-1) per net).
func weightedHPWL(board *core.Board) float64 {
	type bb struct {
		minX, minY, maxX, maxY float64
		n                      int
	}
	// Ordered alongside the index: summing over a Go map iterates in a
	// random order, and the float rounding that follows was enough to flip
	// SA accept/reject decisions, so the same seed gave different boards.
	nets := map[string]*bb{}
	var order []*bb
	walk := func(fp *core.Footprint) {
		if fp == nil {
			return
		}
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net == "" {
				continue
			}
			c := core.PadWorldCenter(fp, pad)
			x, y := c.X.ToMM(), c.Y.ToMM()
			e := nets[*pad.Net]
			if e == nil {
				e = &bb{minX: x, minY: y, maxX: x, maxY: y, n: 1}
				nets[*pad.Net] = e
				order = append(order, e)
				continue
			}
			if x < e.minX {
				e.minX = x
			}
			if y < e.minY {
				e.minY = y
			}
			if x > e.maxX {
				e.maxX = x
			}
			if y > e.maxY {
				e.maxY = y
			}
			e.n++
		}
	}
	if len(board.FootprintOrder) > 0 {
		for _, id := range board.FootprintOrder {
			walk(board.Footprints[id])
		}
	} else {
		for _, fp := range board.Footprints {
			walk(fp)
		}
	}
	total := 0.0
	for _, e := range order {
		if e.n >= 2 {
			total += ((e.maxX - e.minX) + (e.maxY - e.minY)) * netWeight(e.n)
		}
	}
	return total
}

// noPlacePenalty weights a square millimetre of illegal overlap - with a
// no_place keepout or with another part - against a millimetre of wirelength.
// It only has to dominate the wirelength a part could win by squatting there.
const noPlacePenalty = 100.0

// overlapAreaMM2 totals the area the movable parts push into other footprints
// at the run's assembly gap.
//
// The annealer vetoes a move INTO an overlap but, without this term, has no
// gradient out of one - so a part that was already overlapping when Place was
// called just sat there, since every legal escape lengthened a net and lost on
// score. That is not a corner case: the force-directed pre-pass drags a decap
// straight onto its IC's pads by construction, and a script that seeds parts
// before calling auto-place lands in the same state. The veto stops it getting
// worse; this makes it get better. The ePlace global stage (and the old
// cheap-force baseline) can still hand SA a stacked decap; this term
// walks it out.
func overlapAreaMM2(board *core.Board, fps []*core.Footprint, gapMM float64) float64 {
	if gapMM < 0 {
		gapMM = 0
	}
	half := core.FromMM(gapMM / 2)
	movable := map[string]bool{}
	for _, fp := range fps {
		movable[fp.ID.String()] = true
	}
	total := 0.0
	for _, fp := range fps {
		b, ok := collisionBounds(fp, half)
		if !ok {
			continue
		}
		for _, other := range footprintsAll(board) {
			if other == fp || other.ID == fp.ID {
				continue
			}
			// Count a movable/movable pair once, not twice.
			if movable[other.ID.String()] && other.Reference < fp.Reference {
				continue
			}
			if !canCollide(fp, other) {
				continue
			}
			ob, ok := collisionBounds(other, half)
			if !ok {
				continue
			}
			w := math.Min(b.Max.X.ToMM(), ob.Max.X.ToMM()) - math.Max(b.Min.X.ToMM(), ob.Min.X.ToMM())
			h := math.Min(b.Max.Y.ToMM(), ob.Max.Y.ToMM()) - math.Max(b.Min.Y.ToMM(), ob.Min.Y.ToMM())
			if w > 0 && h > 0 {
				total += w * h
			}
		}
	}
	return total
}

func compositeScore(board *core.Board, movable []*core.Footprint, opts Options) float64 {
	return weightedHPWL(board) + opts.GapPenalty*totalGapPenalty(board, opts.MinGapMM) +
		noPlacePenalty*(noPlaceOverlapMM2(board, movable)+
			overlapAreaMM2(board, movable, opts.SolderGapMM))
}

// LegalAt reports whether fp (already posed) clears the solder floor
// and stays inside the outline — used by place-legal.
func LegalAt(board *core.Board, fp *core.Footprint) bool {
	if board.Outline == nil {
		return false
	}
	if !padsInside(fp, board.Outline, 0.8) {
		return false
	}
	if hitsNoPlace(board, fp) {
		return false
	}
	return !firstOverlapper(board, fp)
}
