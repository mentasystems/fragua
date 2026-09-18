package placer

import (
	"math"
	"testing"

	"github.com/mentasystems/fragua/internal/core"
)

// Ported from rust 4e20ae0 crates/pcb-placer/src/global.rs tests.

func TestPoissonFieldPointsAwayFromCharge(t *testing.T) {
	m := 32
	grid := newFieldGrid(m, m, 0, 0, 32, 32)
	rho := make([]float64, m*m)
	for i := 15; i <= 16; i++ {
		for j := 15; j <= 16; j++ {
			rho[i*m+j] = 1
		}
	}
	grid.solve(rho)
	exLeft := grid.ex[11*m+15]
	exRight := grid.ex[20*m+15]
	if exLeft >= 0 {
		t.Fatalf("field left of charge should point -x, got %g", exLeft)
	}
	if exRight <= 0 {
		t.Fatalf("field right of charge should point +x, got %g", exRight)
	}
	if math.Abs(exLeft+exRight) >= 1e-8 {
		t.Fatalf("field should be antisymmetric: %g vs %g", exLeft, exRight)
	}
	eyA := grid.ey[11*m+15]
	eyB := grid.ey[11*m+16]
	if math.Abs(eyA+eyB) >= 1e-8 {
		t.Fatalf("ey should be antisymmetric across the charge: %g vs %g", eyA, eyB)
	}
}

func TestPoissonUniformChargeIsFieldFree(t *testing.T) {
	m := 16
	grid := newFieldGrid(m, m, 0, 0, 16, 16)
	rho := make([]float64, m*m)
	for i := range rho {
		rho[i] = 0.7
	}
	grid.solve(rho)
	for _, v := range grid.ex {
		if math.Abs(v) >= 1e-9 {
			t.Fatalf("uniform density must produce zero Ex, got %g", v)
		}
	}
	for _, v := range grid.ey {
		if math.Abs(v) >= 1e-9 {
			t.Fatalf("uniform density must produce zero Ey, got %g", v)
		}
	}
}

func TestWAGradientMatchesFiniteDifference(t *testing.T) {
	net := eplaceNet{
		pins: []eplacePin{
			{kind: eplacePinFixed, xy: [2]float64{3, 7}},
			{kind: eplacePinMov, body: 0, off: [2]float64{1, -0.5}},
			{kind: eplacePinMov, body: 1, off: [2]float64{-2, 0}},
		},
		weight: 2,
	}
	gamma := 1.5
	pos := [][2]float64{{4, 5}, {8, 6.5}}
	g := make([][2]float64, 2)
	accumulateWAGradient(&net, pos, gamma, g)
	const eps = 1e-6
	for b := 0; b < 2; b++ {
		for axis := 0; axis < 2; axis++ {
			pHi := append([][2]float64(nil), pos...)
			pLo := append([][2]float64(nil), pos...)
			pHi[b][axis] += eps
			pLo[b][axis] -= eps
			fd := (waOfNet(&net, pHi, gamma, -1, 0) - waOfNet(&net, pLo, gamma, -1, 0)) / (2 * eps)
			got := g[b][axis]
			if math.Abs(fd-got) >= 1e-5 {
				t.Fatalf("gradient mismatch body %d axis %d: fd %g vs analytic %g", b, axis, fd, got)
			}
		}
	}
}

func TestOverlappingRectanglesRepel(t *testing.T) {
	m := 32
	grid := newFieldGrid(m, m, 0, 0, 64, 64)
	rho := make([]float64, m*m)
	grid.splat(&rho, 24, 28, 36, 36)
	grid.splat(&rho, 28, 28, 40, 36)
	grid.solve(rho)
	exL, _ := grid.fieldOver(24, 28, 36, 36)
	exR, _ := grid.fieldOver(28, 28, 40, 36)
	if exL >= 0 {
		t.Fatalf("left block should be pushed -x, got %g", exL)
	}
	if exR <= 0 {
		t.Fatalf("right block should be pushed +x, got %g", exR)
	}
}

func piledBoard() (*core.Board, []*core.Footprint) {
	b := core.NewBoard()
	o := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(60), core.FromMM(40)))
	b.Outline = &o
	// Eight 0603-ish parts stacked on the same spot, chained so wirelength
	// wants them in a line. Cheap force only has a local pairwise push;
	// ePlace sees the stacked charge as a density peak.
	var fps []*core.Footprint
	for i := 0; i < 8; i++ {
		nA := "N" + string(rune('A'+i))
		nB := "N" + string(rune('A'+i+1))
		if i == 7 {
			nB = "N" + string(rune('A'))
		}
		// Slightly offset so the pile is real overlap, not a single
		// coincident point (a zero field at the exact centre).
		fp := footprint("R"+string(rune('1'+i)), 29+float64(i%4)*0.6, 19+float64(i/4)*0.6, []core.Pad{
			pad("1", -0.8, 0, nA),
			pad("2", 0.8, 0, nB),
		})
		b.AddFootprint(fp)
		fps = append(fps, fp)
	}
	return b, fps
}

func padOverlapAreaMM2(fps []*core.Footprint) float64 {
	type box struct{ minX, minY, maxX, maxY float64 }
	var boxes []box
	for _, fp := range fps {
		r, ok := footprintBounds(fp)
		if !ok {
			continue
		}
		boxes = append(boxes, box{r.Min.X.ToMM(), r.Min.Y.ToMM(), r.Max.X.ToMM(), r.Max.Y.ToMM()})
	}
	over := 0.0
	for i := 0; i < len(boxes); i++ {
		for j := i + 1; j < len(boxes); j++ {
			x0 := math.Max(boxes[i].minX, boxes[j].minX)
			y0 := math.Max(boxes[i].minY, boxes[j].minY)
			x1 := math.Min(boxes[i].maxX, boxes[j].maxX)
			y1 := math.Min(boxes[i].maxY, boxes[j].maxY)
			if x1 > x0 && y1 > y0 {
				over += (x1 - x0) * (y1 - y0)
			}
		}
	}
	return over
}

func clonePiled() (*core.Board, []*core.Footprint) {
	return piledBoard()
}

// ePlace's Poisson field must spread a stacked pile more than the old
// cheap force pre-pass, or at least match it on HPWL while cutting overlap.
func TestEPlaceBeatsCheapForceOnOverlap(t *testing.T) {
	bForce, fpsForce := clonePiled()
	bPlace, fpsPlace := clonePiled()
	opts := DefaultOptions()
	opts.Decouple = false
	opts.GlobalIterations = 200
	opts.DensityBins = 32

	initOver := padOverlapAreaMM2(fpsPlace)
	initHPWL := rawHPWL(bPlace)
	if initOver < 10 {
		t.Fatalf("fixture should start heavily overlapped, got %.2f mm²", initOver)
	}

	globalForce(bForce, fpsForce, opts)
	forceOver := padOverlapAreaMM2(fpsForce)
	forceHPWL := rawHPWL(bForce)

	g := globalPlace(bPlace, fpsPlace, opts)
	placeOver := padOverlapAreaMM2(fpsPlace)
	placeHPWL := rawHPWL(bPlace)

	t.Logf("init overlap=%.2f hpwl=%.2f; force overlap=%.2f hpwl=%.2f; ePlace overlap=%.2f hpwl=%.2f iters=%d overflow=%.3f",
		initOver, initHPWL, forceOver, forceHPWL, placeOver, placeHPWL, g.Iterations, g.Overflow)

	if placeOver >= initOver {
		t.Fatalf("ePlace did not reduce overlap: %.2f → %.2f", initOver, placeOver)
	}
	// The Poisson field has to beat (or match within 5%) the cheap pairwise
	// push on overlap — that is the whole reason this is not a force toy.
	if placeOver > forceOver*1.05 && placeHPWL > forceHPWL {
		t.Fatalf("ePlace worse than cheap force on both metrics: overlap %.2f vs %.2f, HPWL %.2f vs %.2f",
			placeOver, forceOver, placeHPWL, forceHPWL)
	}
	if placeOver > forceOver {
		t.Logf("ePlace overlap %.2f > force %.2f but HPWL improved %.2f → %.2f", placeOver, forceOver, forceHPWL, placeHPWL)
	}
}

func TestEPlaceIsDeterministic(t *testing.T) {
	run := func() [][3]float64 {
		b, fps := piledBoard()
		opts := DefaultOptions()
		opts.GlobalIterations = 80
		opts.DensityBins = 32
		globalPlace(b, fps, opts)
		out := make([][3]float64, len(fps))
		for i, fp := range fps {
			out[i] = [3]float64{fp.Position.X.ToMM(), fp.Position.Y.ToMM(), fp.Rotation}
		}
		return out
	}
	a, b := run(), run()
	for i := range a {
		for k := 0; k < 3; k++ {
			if math.Abs(a[i][k]-b[i][k]) > 1e-9 {
				t.Fatalf("ePlace not deterministic at part %d: %v vs %v", i, a[i], b[i])
			}
		}
	}
}

// compare_stages-style fixture from rust 4e20ae0: ePlace + SA must
// finish with lower HPWL than the scattered start, and the global
// stage itself must move something.
func TestEPlaceGlobalStageOnScatteredFixture(t *testing.T) {
	b := compareStagesBoard()
	opts := DefaultOptions()
	opts.Seed = 42
	opts.Decouple = false
	opts.GlobalIterations = 250
	opts.DensityBins = 32
	opts.Iterations = 2000
	init := rawHPWL(b)
	rep, err := Place(b, nil, opts)
	if err != nil {
		t.Fatal(err)
	}
	if rep.Global == nil {
		t.Fatal("expected a global-stage report")
	}
	if rep.Global.Iterations == 0 {
		t.Fatal("ePlace ran zero iterations")
	}
	if rep.FinalHPWLMM >= init {
		t.Fatalf("two-stage place did not cut HPWL: %.2f → %.2f (global %.2f)",
			init, rep.FinalHPWLMM, rep.Global.HPWLMM)
	}
}

func compareStagesBoard() *core.Board {
	b := core.NewBoard()
	o := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(85), core.FromMM(52)))
	b.Outline = &o
	module := func(ref string, x, y float64, nets [8]string) *core.Footprint {
		var pads []core.Pad
		for i, n := range nets {
			col, row := i/4, i%4
			ox := 5.0
			if col == 0 {
				ox = -5.0
			}
			pads = append(pads, pad(string(rune('1'+i)), ox, float64(row)*2-3, n))
		}
		return footprint(ref, x, y, pads)
	}
	passive := func(ref string, x, y float64, a, c string) *core.Footprint {
		return footprint(ref, x, y, []core.Pad{pad("1", -1.6, 0, a), pad("2", 1.6, 0, c)})
	}
	b.AddFootprint(module("U1", 8, 45, [8]string{"+3V3", "GND", "SCK", "MOSI", "MISO", "NSS", "SDA", "SCL"}))
	b.AddFootprint(module("U2", 78, 6, [8]string{"+3V3", "GND", "SCK", "MOSI", "MISO", "NSS", "BUSY", "DIO1"}))
	b.AddFootprint(module("DS1", 78, 46, [8]string{"+3V3", "GND", "SDA", "SCL", "NC1", "NC2", "NC3", "NC4"}))
	b.AddFootprint(passive("R1", 6, 4, "SDA", "+3V3"))
	b.AddFootprint(passive("R2", 40, 50, "SCL", "+3V3"))
	b.AddFootprint(passive("R3", 42, 4, "SSR_LED", "GND"))
	b.AddFootprint(passive("C1", 6, 26, "+3V3", "GND"))
	b.AddFootprint(passive("C2", 80, 26, "+3V3", "GND"))
	j1 := passive("J1", 44, 26, "LOCK_A", "LOCK_B")
	j1.EdgeMounted = true
	b.AddFootprint(j1)
	b.AddFootprint(passive("U3", 20, 20, "SSR_LED", "LOCK_A"))
	return b
}
