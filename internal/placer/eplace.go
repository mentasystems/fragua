// ePlace-style electrostatic global placement.
//
// Ported from crates/pcb-placer/src/global.rs at rust tip 4e20ae0
// (Lu et al. 2015 / DREAMPlace formulation). Every footprint is a
// positive charge whose magnitude is its inflated body area. Bin-wise
// charge density drives Poisson ∇²ψ = -ρ, solved spectrally (DCT);
// the field E = -∇ψ pushes overlapping parts apart. Wirelength is the
// weighted-average (WA) smooth model of HPWL. The objective
// f = WL + λ·Φ is minimised with Nesterov's accelerated gradient and
// a Barzilai–Borwein step. λ starts where the two gradients balance
// and grows until density overflow drops under target.
//
// The result intentionally still has small residual overlaps: this
// stage finds the *structure*; SA legalises against the hard solder
// gap and polishes rotations. Deterministic: no RNG.

package placer

import (
	"math"
	"sort"

	"github.com/mentasystems/fragua/internal/core"
)

// GlobalReport is the outcome of the electrostatic global phase.
type GlobalReport struct {
	// Iterations actually run (Nesterov loop, not the legalisation spread).
	Iterations int `json:"iterations"`
	// Overflow is the fraction of movable charge sitting above the
	// target density after the legalisation spread (0 = perfectly spread).
	Overflow float64 `json:"overflow"`
	// HPWLMM is raw (unweighted) HPWL after the global phase.
	HPWLMM float64 `json:"hpwl_mm"`
}

type eplaceBody struct {
	fp *core.Footprint
	// Inflated bbox offsets from position.
	infMin [2]float64
	infMax [2]float64
	charge float64
	// axis whose coordinate is frozen (edge-mounted): (axis, fixed value).
	edgeLock *eplaceLock
	rotDelta float64
}

type eplaceLock struct {
	axis  int
	value float64
}

type eplacePinKind uint8

const (
	eplacePinFixed eplacePinKind = iota
	eplacePinMov
)

type eplacePin struct {
	kind eplacePinKind
	xy   [2]float64 // Fixed: world; Mov: unused
	body int
	off  [2]float64
}

type eplaceNet struct {
	pins   []eplacePin
	weight float64
}

// globalPlace runs the literature ePlace solve on fps and writes the
// solution back. Rotation is only touched by the 90° probe (SA owns
// later flips). No RNG — same input yields the same placement.
func globalPlace(board *core.Board, fps []*core.Footprint, opts Options) GlobalReport {
	if board == nil || board.Outline == nil || len(fps) == 0 {
		return GlobalReport{HPWLMM: rawHPWL(board)}
	}
	o := board.Outline
	ox, oy := o.Min.X.ToMM(), o.Min.Y.ToMM()
	wMM, hMM := o.Width().ToMM(), o.Height().ToMM()
	if wMM <= 1.0 || hMM <= 1.0 {
		return GlobalReport{HPWLMM: rawHPWL(board)}
	}

	halfGap := math.Max(opts.MinGapMM, math.Max(opts.MinClearanceMM, opts.SolderGapMM)) / 2.0

	var bodies []eplaceBody
	bodyOf := map[string]int{}
	var pos [][2]float64
	for _, fp := range fps {
		if fp == nil {
			continue
		}
		raw, ok := footprintBounds(fp)
		if !ok {
			continue
		}
		inf := raw
		if !fp.Elevated {
			if m, ok := fpBoundsWithMargin(fp); ok {
				inf = m
			}
		}
		px, py := fp.Position.X.ToMM(), fp.Position.Y.ToMM()
		infMin := [2]float64{inf.Min.X.ToMM() - px - halfGap, inf.Min.Y.ToMM() - py - halfGap}
		infMax := [2]float64{inf.Max.X.ToMM() - px + halfGap, inf.Max.Y.ToMM() - py + halfGap}
		rawMin := [2]float64{raw.Min.X.ToMM() - px, raw.Min.Y.ToMM() - py}
		rawMax := [2]float64{raw.Max.X.ToMM() - px, raw.Max.Y.ToMM() - py}
		charge := math.Max(infMax[0]-infMin[0], 0.01) * math.Max(infMax[1]-infMin[1], 0.01)
		var lock *eplaceLock
		preQ := uint8(0)
		if fp.EdgeMounted {
			var declared *core.EdgeSide
			if fp.EdgeSide != nil {
				w := worldSide(*fp.EdgeSide, fp.Rotation)
				declared = &w
			}
			q, axis, value := edgePlan(px, py, rawMin, rawMax, infMin, infMax, ox, oy, wMM, hMM, declared)
			preQ = q
			if q != 0 {
				infMin, infMax = rotBBox(infMin, infMax, q)
			}
			lock = &eplaceLock{axis: axis, value: value}
		}
		bodyOf[fp.ID.String()] = len(bodies)
		bodies = append(bodies, eplaceBody{
			fp: fp, infMin: infMin, infMax: infMax, charge: charge,
			edgeLock: lock, rotDelta: 90.0 * float64(preQ),
		})
		pos = append(pos, [2]float64{px, py})
	}
	if len(bodies) == 0 {
		return GlobalReport{HPWLMM: rawHPWL(board)}
	}

	// Nets: footprints in board order so float-summation is stable.
	type namedPins struct {
		name string
		pins []eplacePin
	}
	netPins := map[string][]eplacePin{}
	for _, fp := range footprintsAll(board) {
		if fp == nil {
			continue
		}
		mov, isMov := bodyOf[fp.ID.String()]
		px, py := fp.Position.X.ToMM(), fp.Position.Y.ToMM()
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Net == nil || *pad.Net == "" {
				continue
			}
			c := core.PadWorldCenter(fp, pad)
			var pin eplacePin
			if isMov {
				q := uint8(int(math.Round(bodies[mov].rotDelta/90.0))) % 4
				pin = eplacePin{
					kind: eplacePinMov, body: mov,
					off: rotOff([2]float64{c.X.ToMM() - px, c.Y.ToMM() - py}, q),
				}
			} else {
				pin = eplacePin{kind: eplacePinFixed, xy: [2]float64{c.X.ToMM(), c.Y.ToMM()}}
			}
			netPins[*pad.Net] = append(netPins[*pad.Net], pin)
		}
	}
	names := make([]string, 0, len(netPins))
	for n := range netPins {
		names = append(names, n)
	}
	sort.Strings(names)
	var nets []eplaceNet
	for _, name := range names {
		pins := netPins[name]
		if len(pins) < 2 {
			continue
		}
		hasMov := false
		for _, p := range pins {
			if p.kind == eplacePinMov {
				hasMov = true
				break
			}
		}
		if !hasMov {
			continue
		}
		nets = append(nets, eplaceNet{pins: pins, weight: 4.0 / float64(len(pins)-1)})
	}
	netsOfBody := make([][]int, len(bodies))
	for ni, net := range nets {
		seen := map[int]bool{}
		for _, pin := range net.pins {
			if pin.kind == eplacePinMov && !seen[pin.body] {
				seen[pin.body] = true
				netsOfBody[pin.body] = append(netsOfBody[pin.body], ni)
			}
		}
	}

	m := opts.DensityBins
	if m < 16 {
		m = 16
	}
	if m > 256 {
		m = 256
	}
	if m == 0 {
		m = 64
	}
	grid := newFieldGrid(m, m, ox, oy, wMM, hMM)
	fixedRho := make([]float64, m*m)
	for _, fp := range footprintsAll(board) {
		if fp == nil {
			continue
		}
		if _, mov := bodyOf[fp.ID.String()]; mov {
			continue
		}
		b, ok := fpBoundsWithMargin(fp)
		if !ok {
			continue
		}
		grid.splat(&fixedRho,
			b.Min.X.ToMM()-halfGap, b.Min.Y.ToMM()-halfGap,
			b.Max.X.ToMM()+halfGap, b.Max.Y.ToMM()+halfGap)
	}
	totalMovable := 0.0
	for _, b := range bodies {
		totalMovable += b.charge
	}
	targetDensity := opts.TargetDensity
	if targetDensity <= 0 {
		targetDensity = 1.0
	}
	gTmp := newFieldGrid(m, m, ox, oy, wMM, hMM)
	fixedOverflow := gTmp.overflow(fixedRho, targetDensity, totalMovable)

	n := len(bodies)
	iters := opts.GlobalIterations
	if iters < 1 {
		iters = 600
	}
	gamma := math.Max(0.04*math.Max(wMM, hMM), 1.0)
	const gammaFloor = 0.5
	overflowTarget := opts.TargetOverflow
	if overflowTarget < 0.005 {
		overflowTarget = 0.08
		if opts.TargetOverflow > 0 {
			overflowTarget = math.Max(opts.TargetOverflow, 0.005)
		}
	}
	edgeClear := math.Max(opts.EdgeClearanceMM, 0)

	project := func(p *[2]float64, b *eplaceBody) {
		projectPosition(p, b, ox, oy, wMM, hMM, edgeClear)
	}
	for i := range pos {
		project(&pos[i], &bodies[i])
	}

	lambda := 0.0
	lambdaFrozen := false
	x := append([][2]float64(nil), pos...)
	y := append([][2]float64(nil), pos...)
	yPrev := append([][2]float64(nil), y...)
	gPrev := make([][2]float64, n)
	aK := 1.0
	maxMove := 2.0 * math.Max(grid.hx, grid.hy)
	ran := 0
	overflow := math.Inf(1)
	plateauBest := math.Inf(1)
	plateauAge := 0

	for k := 0; k < iters; k++ {
		ran = k + 1
		rho := append([]float64(nil), fixedRho...)
		for i := range y {
			b := &bodies[i]
			grid.splat(&rho,
				y[i][0]+b.infMin[0], y[i][1]+b.infMin[1],
				y[i][0]+b.infMax[0], y[i][1]+b.infMax[1])
		}
		grid.solve(rho)
		overflow = math.Max(0, grid.overflow(rho, targetDensity, totalMovable)-fixedOverflow)

		gWL := make([][2]float64, n)
		for i := range nets {
			accumulateWAGradient(&nets[i], y, gamma, gWL)
		}
		gD := make([][2]float64, n)
		for i := range bodies {
			b := &bodies[i]
			ex, ey := grid.fieldOver(
				y[i][0]+b.infMin[0], y[i][1]+b.infMin[1],
				y[i][0]+b.infMax[0], y[i][1]+b.infMax[1])
			gD[i] = [2]float64{-b.charge * ex, -b.charge * ey}
		}
		sWL, sD := 0.0, 0.0
		for i := 0; i < n; i++ {
			sWL += math.Abs(gWL[i][0]) + math.Abs(gWL[i][1])
			sD += math.Abs(gD[i][0]) + math.Abs(gD[i][1])
		}
		balance := 0.0
		if sD > 1e-12 {
			balance = math.Max(sWL/sD, 1e-9)
		}
		if !lambdaFrozen {
			if overflow > overflowTarget && balance > 0 {
				lambda = balance
				lambdaFrozen = true
			} else {
				lambda = 0.05 * balance
			}
		} else if overflow > overflowTarget {
			lambda *= 1.06
		} else {
			lambda *= 0.96
		}
		if lambdaFrozen && balance > 0 {
			lo, hi := 0.01*balance, 100.0*balance
			if lambda < lo {
				lambda = lo
			}
			if lambda > hi {
				lambda = hi
			}
		}
		g := make([][2]float64, n)
		for i := 0; i < n; i++ {
			g[i] = [2]float64{
				gWL[i][0] + lambda*gD[i][0],
				gWL[i][1] + lambda*gD[i][1],
			}
			if bodies[i].edgeLock != nil {
				g[i][bodies[i].edgeLock.axis] = 0
			}
		}

		sy, ss := 0.0, 0.0
		for i := 0; i < n; i++ {
			for d := 0; d < 2; d++ {
				dy := y[i][d] - yPrev[i][d]
				dg := g[i][d] - gPrev[i][d]
				sy += dy * dg
				ss += dg * dg
			}
		}
		gInf := 0.0
		for i := 0; i < n; i++ {
			gInf = math.Max(gInf, math.Max(math.Abs(g[i][0]), math.Abs(g[i][1])))
		}
		alpha := 0.0
		if k > 0 && ss > 1e-12 && sy > 0 {
			alpha = sy / ss
		} else if gInf > 1e-12 {
			alpha = math.Min(grid.hx, grid.hy) / gInf
		}

		copy(yPrev, y)
		copy(gPrev, g)

		xNew := make([][2]float64, n)
		for i := 0; i < n; i++ {
			dx := clampF(alpha*g[i][0], -maxMove, maxMove)
			dy := clampF(alpha*g[i][1], -maxMove, maxMove)
			xNew[i] = [2]float64{y[i][0] - dx, y[i][1] - dy}
			project(&xNew[i], &bodies[i])
		}
		aNext := 0.5 * (1.0 + math.Sqrt(4.0*aK*aK+1.0))
		coef := (aK - 1.0) / aNext
		for i := 0; i < n; i++ {
			y[i] = [2]float64{
				xNew[i][0] + coef*(xNew[i][0]-x[i][0]),
				xNew[i][1] + coef*(xNew[i][1]-x[i][1]),
			}
			project(&y[i], &bodies[i])
		}
		x = xNew
		aK = aNext

		if k%25 == 24 {
			for i := 0; i < n; i++ {
				if bodies[i].edgeLock != nil {
					continue
				}
				base := 0.0
				for _, ni := range netsOfBody[i] {
					base += waOfNet(&nets[ni], x, gamma, -1, 0)
				}
				bestQ := uint8(0)
				bestVal := base
				for q := uint8(1); q < 4; q++ {
					val := 0.0
					for _, ni := range netsOfBody[i] {
						val += waOfNet(&nets[ni], x, gamma, i, q)
					}
					if val < bestVal-1e-9 {
						bestVal = val
						bestQ = q
					}
				}
				if bestQ != 0 {
					for _, ni := range netsOfBody[i] {
						for j := range nets[ni].pins {
							p := &nets[ni].pins[j]
							if p.kind == eplacePinMov && p.body == i {
								p.off = rotOff(p.off, bestQ)
							}
						}
					}
					bodies[i].infMin, bodies[i].infMax = rotBBox(bodies[i].infMin, bodies[i].infMax, bestQ)
					bodies[i].rotDelta = math.Mod(bodies[i].rotDelta+90.0*float64(bestQ), 360.0)
					project(&x[i], &bodies[i])
					y[i] = x[i]
				}
			}
		}

		gamma = math.Max(gamma*0.985, gammaFloor)

		if overflow <= overflowTarget {
			for i := range x {
				bodies[i].fp.Position = core.NewPoint(core.FromMM(x[i][0]), core.FromMM(x[i][1]))
			}
			hpwl := rawHPWL(board)
			if hpwl < plateauBest*0.998 {
				plateauBest = hpwl
				plateauAge = 0
			} else {
				plateauAge++
			}
			if plateauAge >= 50 && k >= 100 {
				break
			}
		} else {
			plateauAge = 0
		}
	}

	// Pure density descent so SA inherits a feasible (near-legal) layout.
	for step := 0; step < 200; step++ {
		rho := append([]float64(nil), fixedRho...)
		for i := range x {
			b := &bodies[i]
			grid.splat(&rho,
				x[i][0]+b.infMin[0], x[i][1]+b.infMin[1],
				x[i][0]+b.infMax[0], x[i][1]+b.infMax[1])
		}
		grid.solve(rho)
		overflow = math.Max(0, grid.overflow(rho, targetDensity, totalMovable)-fixedOverflow)
		if overflow <= 0.25*overflowTarget {
			break
		}
		gInf := 0.0
		g := make([][2]float64, n)
		for i := range bodies {
			b := &bodies[i]
			ex, ey := grid.fieldOver(
				x[i][0]+b.infMin[0], x[i][1]+b.infMin[1],
				x[i][0]+b.infMax[0], x[i][1]+b.infMax[1])
			g[i] = [2]float64{-b.charge * ex, -b.charge * ey}
			if b.edgeLock != nil {
				g[i][b.edgeLock.axis] = 0
			}
			gInf = math.Max(gInf, math.Max(math.Abs(g[i][0]), math.Abs(g[i][1])))
		}
		if gInf < 1e-12 {
			// Exact coincidence makes the Neumann field zero at every
			// body centre (the uniform-charge identity). A deterministic
			// sub-bin nudge restores a gradient so the spread can run.
			if overflow <= overflowTarget {
				break
			}
			for i := 0; i < n; i++ {
				x[i][0] += 0.05 * float64(i%3-1)
				x[i][1] += 0.05 * float64((i/2)%3-1)
				project(&x[i], &bodies[i])
			}
			continue
		}
		alpha := math.Min(grid.hx, grid.hy) / gInf
		for i := 0; i < n; i++ {
			x[i] = [2]float64{x[i][0] - alpha*g[i][0], x[i][1] - alpha*g[i][1]}
			project(&x[i], &bodies[i])
		}
	}

	for i := range x {
		b := &bodies[i]
		b.fp.Position = core.NewPoint(core.FromMM(x[i][0]), core.FromMM(x[i][1]))
		if b.rotDelta != 0 {
			b.fp.Rotation = remEuclid(b.fp.Rotation+b.rotDelta, 360)
		}
	}
	return GlobalReport{Iterations: ran, Overflow: overflow, HPWLMM: rawHPWL(board)}
}

func projectPosition(p *[2]float64, b *eplaceBody, ox, oy, w, h, edge float64) {
	e := edge
	if b.edgeLock != nil {
		e = 0
	}
	loX := ox + e - b.infMin[0]
	hiX := ox + w - e - b.infMax[0]
	loY := oy + e - b.infMin[1]
	hiY := oy + h - e - b.infMax[1]
	if loX <= hiX {
		p[0] = clampF(p[0], loX, hiX)
	} else {
		p[0] = (loX + hiX) / 2
	}
	if loY <= hiY {
		p[1] = clampF(p[1], loY, hiY)
	} else {
		p[1] = (loY + hiY) / 2
	}
	if b.edgeLock != nil {
		p[b.edgeLock.axis] = b.edgeLock.value
	}
}

func edgePlan(px, py float64, rawMin, rawMax, infMin, infMax [2]float64, ox, oy, w, h float64, declared *core.EdgeSide) (q uint8, axis int, lock float64) {
	const touchTol = 0.5
	const overhangTol = 0.5
	dLeft := math.Abs(px + rawMin[0] - ox)
	dRight := math.Abs(ox + w - (px + rawMax[0]))
	dBottom := math.Abs(py + rawMin[1] - oy)
	dTop := math.Abs(oy + h - (py + rawMax[1]))
	dists := [4]float64{dLeft, dRight, dBottom, dTop}
	edge := 0
	for i := 1; i < 4; i++ {
		if dists[i] < dists[edge] {
			edge = i
		}
	}
	backoff := 0.0
	if declared != nil {
		var target core.EdgeSide
		switch edge {
		case 0:
			target = core.EdgeLeft
		case 1:
			target = core.EdgeRight
		case 2:
			target = core.EdgeBottom
		default:
			target = core.EdgeTop
		}
		q = 0
		for cand := uint8(0); cand < 4; cand++ {
			if worldSide(*declared, 90.0*float64(cand)) == target {
				q = cand
				break
			}
		}
	} else {
		bestQ, bestBack, bestRes := uint8(0), 0.0, math.Inf(1)
		for cand := uint8(0); cand < 4; cand++ {
			rmin, rmax := rotBBox(rawMin, rawMax, cand)
			imin, imax := rotBBox(infMin, infMax, cand)
			var mOut float64
			switch edge {
			case 0:
				mOut = rmin[0] - imin[0]
			case 1:
				mOut = imax[0] - rmax[0]
			case 2:
				mOut = rmin[1] - imin[1]
			default:
				mOut = imax[1] - rmax[1]
			}
			bo := clampF(mOut-overhangTol, 0, touchTol)
			res := math.Max(mOut-overhangTol-bo, 0)
			if res < bestRes-1e-9 {
				bestQ, bestBack, bestRes = cand, bo, res
			}
		}
		q, backoff = bestQ, bestBack
	}
	rmin, rmax := rotBBox(rawMin, rawMax, q)
	switch edge {
	case 0:
		return q, 0, ox - rmin[0] + backoff
	case 1:
		return q, 0, ox + w - rmax[0] - backoff
	case 2:
		return q, 1, oy - rmin[1] + backoff
	default:
		return q, 1, oy + h - rmax[1] - backoff
	}
}

func worldSide(local core.EdgeSide, rotationDeg float64) core.EdgeSide {
	ccw := func(s core.EdgeSide) uint32 {
		switch s {
		case core.EdgeTop:
			return 0
		case core.EdgeLeft:
			return 1
		case core.EdgeBottom:
			return 2
		default:
			return 3
		}
	}
	from := func(i uint32) core.EdgeSide {
		switch i % 4 {
		case 0:
			return core.EdgeTop
		case 1:
			return core.EdgeLeft
		case 2:
			return core.EdgeBottom
		default:
			return core.EdgeRight
		}
	}
	q := uint32(math.Round(remEuclid(rotationDeg, 360)/90.0)) % 4
	return from(ccw(local) + q)
}

func rotOff(off [2]float64, quarter uint8) [2]float64 {
	switch quarter % 4 {
	case 1:
		return [2]float64{-off[1], off[0]}
	case 2:
		return [2]float64{-off[0], -off[1]}
	case 3:
		return [2]float64{off[1], -off[0]}
	default:
		return off
	}
}

func rotBBox(min, max [2]float64, quarter uint8) ([2]float64, [2]float64) {
	corners := [4][2]float64{
		{min[0], min[1]}, {min[0], max[1]}, {max[0], min[1]}, {max[0], max[1]},
	}
	nmin := [2]float64{math.Inf(1), math.Inf(1)}
	nmax := [2]float64{math.Inf(-1), math.Inf(-1)}
	for _, c := range corners {
		r := rotOff(c, quarter)
		for d := 0; d < 2; d++ {
			if r[d] < nmin[d] {
				nmin[d] = r[d]
			}
			if r[d] > nmax[d] {
				nmax[d] = r[d]
			}
		}
	}
	return nmin, nmax
}

func pinCoord(p eplacePin, pos [][2]float64, rotBody int, rotQ uint8) [2]float64 {
	if p.kind == eplacePinFixed {
		return p.xy
	}
	off := p.off
	if rotBody == p.body && rotQ != 0 {
		off = rotOff(off, rotQ)
	}
	return [2]float64{pos[p.body][0] + off[0], pos[p.body][1] + off[1]}
}

func waOfNet(net *eplaceNet, pos [][2]float64, gamma float64, rotBody int, rotQ uint8) float64 {
	coords := make([][2]float64, len(net.pins))
	for i, p := range net.pins {
		coords[i] = pinCoord(p, pos, rotBody, rotQ)
	}
	total := 0.0
	for axis := 0; axis < 2; axis++ {
		vals := make([]float64, len(coords))
		vmax, vmin := math.Inf(-1), math.Inf(1)
		for i, c := range coords {
			vals[i] = c[axis]
			if vals[i] > vmax {
				vmax = vals[i]
			}
			if vals[i] < vmin {
				vmin = vals[i]
			}
		}
		sHi, xHi, sLo, xLo := 0.0, 0.0, 0.0, 0.0
		for _, v := range vals {
			eHi := math.Exp((v - vmax) / gamma)
			sHi += eHi
			xHi += v * eHi
			eLo := math.Exp(-(v - vmin) / gamma)
			sLo += eLo
			xLo += v * eLo
		}
		total += xHi/sHi - xLo/sLo
	}
	return net.weight * total
}

func accumulateWAGradient(net *eplaceNet, pos [][2]float64, gamma float64, g [][2]float64) {
	coords := make([][2]float64, len(net.pins))
	for i, p := range net.pins {
		coords[i] = pinCoord(p, pos, -1, 0)
	}
	for axis := 0; axis < 2; axis++ {
		vals := make([]float64, len(coords))
		vmax, vmin := math.Inf(-1), math.Inf(1)
		for i, c := range coords {
			vals[i] = c[axis]
			if vals[i] > vmax {
				vmax = vals[i]
			}
			if vals[i] < vmin {
				vmin = vals[i]
			}
		}
		eHi := make([]float64, len(vals))
		eLo := make([]float64, len(vals))
		sHi, sLo := 0.0, 0.0
		xwHi, xwLo := 0.0, 0.0
		for j, v := range vals {
			eHi[j] = math.Exp((v - vmax) / gamma)
			sHi += eHi[j]
			xwHi += v * eHi[j]
			eLo[j] = math.Exp(-(v - vmin) / gamma)
			sLo += eLo[j]
			xwLo += v * eLo[j]
		}
		xwHi /= sHi
		xwLo /= sLo
		for j, pin := range net.pins {
			if pin.kind != eplacePinMov {
				continue
			}
			dHi := eHi[j] / sHi * (1.0 + (vals[j]-xwHi)/gamma)
			dLo := eLo[j] / sLo * (1.0 - (vals[j]-xwLo)/gamma)
			g[pin.body][axis] += net.weight * (dHi - dLo)
		}
	}
}

// fieldGrid is the bin grid + spectral Poisson solver (Neumann / DCT-II).
type fieldGrid struct {
	mx, my     int
	ox, oy     float64
	hx, hy     float64
	cosX, sinX []float64
	cosY, sinY []float64
	wu, wv     []float64
	ex, ey     []float64
}

func newFieldGrid(mx, my int, ox, oy, w, h float64) *fieldGrid {
	table := func(m int, f func(float64) float64) []float64 {
		t := make([]float64, m*m)
		for u := 0; u < m; u++ {
			for i := 0; i < m; i++ {
				t[u*m+i] = f(math.Pi * float64(u) * float64(2*i+1) / float64(2*m))
			}
		}
		return t
	}
	g := &fieldGrid{
		mx: mx, my: my, ox: ox, oy: oy,
		hx: w / float64(mx), hy: h / float64(my),
		cosX: table(mx, math.Cos), sinX: table(mx, math.Sin),
		cosY: table(my, math.Cos), sinY: table(my, math.Sin),
		wu: make([]float64, mx), wv: make([]float64, my),
		ex: make([]float64, mx*my), ey: make([]float64, mx*my),
	}
	for u := 0; u < mx; u++ {
		g.wu[u] = math.Pi * float64(u) / w
	}
	for v := 0; v < my; v++ {
		g.wv[v] = math.Pi * float64(v) / h
	}
	return g
}

func (g *fieldGrid) splat(rho *[]float64, x0, y0, x1, y1 float64) {
	area := math.Max((x1-x0)*(y1-y0), 1e-6)
	if x1-x0 < g.hx {
		c := (x0 + x1) / 2
		x0, x1 = c-g.hx/2, c+g.hx/2
	}
	if y1-y0 < g.hy {
		c := (y0 + y1) / 2
		y0, y1 = c-g.hy/2, c+g.hy/2
	}
	scale := area / ((x1 - x0) * (y1 - y0))
	i0, i1 := g.binRangeX(x0, x1)
	j0, j1 := g.binRangeY(y0, y1)
	r := *rho
	for i := i0; i <= i1; i++ {
		bx0 := g.ox + float64(i)*g.hx
		ovX := math.Max(math.Min(x1, bx0+g.hx)-math.Max(x0, bx0), 0)
		for j := j0; j <= j1; j++ {
			by0 := g.oy + float64(j)*g.hy
			ovY := math.Max(math.Min(y1, by0+g.hy)-math.Max(y0, by0), 0)
			r[i*g.my+j] += scale * ovX * ovY / (g.hx * g.hy)
		}
	}
}

func (g *fieldGrid) binRangeX(x0, x1 float64) (int, int) {
	i0 := int(math.Floor((x0 - g.ox) / g.hx))
	if i0 < 0 {
		i0 = 0
	}
	if i0 > g.mx-1 {
		i0 = g.mx - 1
	}
	i1 := int(math.Ceil((x1-g.ox)/g.hx)) - 1
	if i1 < i0 {
		i1 = i0
	}
	if i1 > g.mx-1 {
		i1 = g.mx - 1
	}
	return i0, i1
}

func (g *fieldGrid) binRangeY(y0, y1 float64) (int, int) {
	j0 := int(math.Floor((y0 - g.oy) / g.hy))
	if j0 < 0 {
		j0 = 0
	}
	if j0 > g.my-1 {
		j0 = g.my - 1
	}
	j1 := int(math.Ceil((y1-g.oy)/g.hy)) - 1
	if j1 < j0 {
		j1 = j0
	}
	if j1 > g.my-1 {
		j1 = g.my - 1
	}
	return j0, j1
}

func (g *fieldGrid) solve(rho []float64) {
	mx, my := g.mx, g.my
	t := make([]float64, mx*my)
	for u := 0; u < mx; u++ {
		for i := 0; i < mx; i++ {
			c := g.cosX[u*mx+i]
			if c == 0 {
				continue
			}
			row := rho[i*my : (i+1)*my]
			out := t[u*my : (u+1)*my]
			for j := 0; j < my; j++ {
				out[j] += c * row[j]
			}
		}
	}
	norm := 1.0 / float64(mx*my)
	a := make([]float64, mx*my)
	for u := 0; u < mx; u++ {
		alphaU := 2.0
		if u == 0 {
			alphaU = 1.0
		}
		for v := 0; v < my; v++ {
			if u == 0 && v == 0 {
				continue
			}
			alphaV := 2.0
			if v == 0 {
				alphaV = 1.0
			}
			s := 0.0
			for j := 0; j < my; j++ {
				s += t[u*my+j] * g.cosY[v*my+j]
			}
			w2 := g.wu[u]*g.wu[u] + g.wv[v]*g.wv[v]
			a[u*my+v] = alphaU * alphaV * norm * s / w2
		}
	}
	tx := make([]float64, mx*my)
	ty := make([]float64, mx*my)
	for i := 0; i < mx; i++ {
		for u := 0; u < mx; u++ {
			su := g.sinX[u*mx+i] * g.wu[u]
			cu := g.cosX[u*mx+i]
			if su == 0 && cu == 0 {
				continue
			}
			arow := a[u*my : (u+1)*my]
			xrow := tx[i*my : (i+1)*my]
			yrow := ty[i*my : (i+1)*my]
			for v := 0; v < my; v++ {
				xrow[v] += su * arow[v]
				yrow[v] += cu * arow[v] * g.wv[v]
			}
		}
	}
	for i := 0; i < mx; i++ {
		for j := 0; j < my; j++ {
			ex, ey := 0.0, 0.0
			for v := 0; v < my; v++ {
				ex += tx[i*my+v] * g.cosY[v*my+j]
				ey += ty[i*my+v] * g.sinY[v*my+j]
			}
			g.ex[i*g.my+j] = ex
			g.ey[i*g.my+j] = ey
		}
	}
}

func (g *fieldGrid) fieldOver(x0, y0, x1, y1 float64) (float64, float64) {
	i0, i1 := g.binRangeX(x0, x1)
	j0, j1 := g.binRangeY(y0, y1)
	fx, fy, wSum := 0.0, 0.0, 0.0
	for i := i0; i <= i1; i++ {
		bx0 := g.ox + float64(i)*g.hx
		ovX := math.Max(math.Min(x1, bx0+g.hx)-math.Max(x0, bx0), 0)
		for j := j0; j <= j1; j++ {
			by0 := g.oy + float64(j)*g.hy
			ovY := math.Max(math.Min(y1, by0+g.hy)-math.Max(y0, by0), 0)
			w := ovX * ovY
			fx += w * g.ex[i*g.my+j]
			fy += w * g.ey[i*g.my+j]
			wSum += w
		}
	}
	if wSum > 1e-12 {
		return fx / wSum, fy / wSum
	}
	return 0, 0
}

func (g *fieldGrid) overflow(rho []float64, target, totalCharge float64) float64 {
	if totalCharge <= 1e-9 {
		return 0
	}
	binArea := g.hx * g.hy
	over := 0.0
	for _, r := range rho {
		if d := r - target; d > 0 {
			over += d * binArea
		}
	}
	return over / totalCharge
}

func fpBoundsWithMargin(fp *core.Footprint) (core.Rect, bool) {
	b, ok := footprintBounds(fp)
	if !ok {
		return b, false
	}
	if fp.PlacementMargin.IsZero() {
		return b, true
	}
	// local [top, right, bottom, left] → world after 90° snaps.
	t, r, bot, l := fp.PlacementMargin.TopMM, fp.PlacementMargin.RightMM, fp.PlacementMargin.BottomMM, fp.PlacementMargin.LeftMM
	rot := remEuclid(fp.Rotation, 360)
	switch {
	case rot >= 45 && rot < 135:
		t, r, bot, l = r, bot, l, t
	case rot >= 135 && rot < 225:
		t, r, bot, l = bot, l, t, r
	case rot >= 225 && rot < 315:
		t, r, bot, l = l, t, r, bot
	}
	return core.Rect{
		Min: core.NewPoint(b.Min.X-core.FromMM(l), b.Min.Y-core.FromMM(bot)),
		Max: core.NewPoint(b.Max.X+core.FromMM(r), b.Max.Y+core.FromMM(t)),
	}, true
}

func clampF(v, lo, hi float64) float64 {
	if v < lo {
		return lo
	}
	if v > hi {
		return hi
	}
	return v
}

func remEuclid(x, m float64) float64 {
	r := math.Mod(x, m)
	if r < 0 {
		r += m
	}
	return r
}
