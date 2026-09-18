package router

import (
	"math"
	"testing"

	"github.com/mentasystems/fragua/internal/core"
)

// Ported from rust 4e20ae0 crates/pcb-router/tests/topo_engine.rs.

func topoPadAt(num string, ox, oy float64, net string) core.Pad {
	n := net
	return core.Pad{
		Number: num,
		Offset: core.NewPoint(core.FromMM(ox), core.FromMM(oy)),
		Size:   [2]core.Length{core.FromMM(1.0), core.FromMM(1.2)},
		Layer:  core.LayerTop,
		Net:    &n,
	}
}

func topoFP(ref string, x, y float64, pads []core.Pad) *core.Footprint {
	return &core.Footprint{
		ID:        core.NewID(),
		Reference: ref,
		Library:   "demo",
		Position:  core.NewPoint(core.FromMM(x), core.FromMM(y)),
		Layer:     core.LayerTop,
		Pads:      pads,
	}
}

func crossingBoard() *core.Board {
	b := core.NewBoard()
	o := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(30), core.FromMM(30)))
	b.Outline = &o
	b.AddFootprint(topoFP("A1", 5, 5, []core.Pad{topoPadAt("1", 0, 0, "A")}))
	b.AddFootprint(topoFP("A2", 25, 25, []core.Pad{topoPadAt("1", 0, 0, "A")}))
	b.AddFootprint(topoFP("B1", 25, 5, []core.Pad{topoPadAt("1", 0, 0, "B")}))
	b.AddFootprint(topoFP("B2", 5, 25, []core.Pad{topoPadAt("1", 0, 0, "B")}))
	b.AddFootprint(topoFP("W1", 15, 15, []core.Pad{
		topoPadAt("1", 0, 0, "W"),
		topoPadAt("2", 0, 3, "W"),
	}))
	return b
}

func TestTopoEngineRoutesCrossingNets(t *testing.T) {
	b := crossingBoard()
	opts := DefaultOptions()
	opts.Engine = EngineTopo
	opts.Organic = false
	opts.MaxSeconds = 10
	rep := Route(b, opts)
	var failed []string
	for _, n := range rep.PerNet {
		if n.Outcome.Status != "ok" {
			failed = append(failed, n.Net+":"+n.Outcome.Reason)
		}
	}
	if len(failed) > 0 {
		t.Fatalf("topo engine failed nets: %v (report %s)", failed, rep.Summary())
	}
	if len(b.Traces) == 0 {
		t.Fatal("no copper emitted")
	}
	o := b.Outline
	for _, tr := range b.Traces {
		for _, p := range []core.Point{tr.Start, tr.End} {
			if p.X < o.Min.X || p.X > o.Max.X || p.Y < o.Min.Y || p.Y > o.Max.Y {
				t.Fatalf("trace point left the outline: %+v", p)
			}
		}
	}
}

func TestTopoEngineIsDeterministic(t *testing.T) {
	opts := DefaultOptions()
	opts.Engine = EngineTopo
	opts.MaxSeconds = 10
	run := func() (float64, [][4]int64) {
		b := crossingBoard()
		r := Route(b, opts)
		sig := make([][4]int64, len(b.Traces))
		for i, tr := range b.Traces {
			sig[i] = [4]int64{int64(tr.Start.X), int64(tr.Start.Y), int64(tr.End.X), int64(tr.End.Y)}
		}
		return r.TotalLengthMM, sig
	}
	// Several pairs: map-iteration bugs often hide in a single pair
	// on one host and show on another (ubuntu CI).
	l0, s0 := run()
	sortSig(s0)
	for trial := 0; trial < 8; trial++ {
		l2, s2 := run()
		if len(s0) != len(s2) {
			t.Fatalf("trial %d: copper count differs: %d vs %d", trial, len(s0), len(s2))
		}
		sortSig(s2)
		for i := range s0 {
			if s0[i] != s2[i] {
				t.Fatalf("trial %d: copper differs between identical runs at %d: %v vs %v", trial, i, s0[i], s2[i])
			}
		}
		if math.Abs(l0-l2) >= 1e-9 {
			t.Fatalf("trial %d: length differs: %g vs %g", trial, l0, l2)
		}
	}
}

func sortSig(s [][4]int64) {
	less := func(a, b [4]int64) bool {
		for k := 0; k < 4; k++ {
			if a[k] != b[k] {
				return a[k] < b[k]
			}
		}
		return false
	}
	for i := 0; i < len(s); i++ {
		for j := i + 1; j < len(s); j++ {
			if less(s[j], s[i]) {
				s[i], s[j] = s[j], s[i]
			}
		}
	}
}

func TestParseOptionsEngineTopo(t *testing.T) {
	o := DefaultOptions()
	if o.Engine != "" && o.Engine != EngineGrid {
		t.Fatalf("default engine should be grid, got %q", o.Engine)
	}
	o = ParseOptions(o, "engine=topo max_seconds=30")
	if o.Engine != EngineTopo {
		t.Fatalf("engine=topo: got %q", o.Engine)
	}
	o = ParseOptions(DefaultOptions(), "engine=grid")
	if o.Engine != EngineGrid {
		t.Fatalf("engine=grid: got %q", o.Engine)
	}
}

func TestDefaultRouteIsNotTopo(t *testing.T) {
	b := crossingBoard()
	opts := DefaultOptions()
	opts.MaxSeconds = 2
	opts.Organic = false
	rep := Route(b, opts)
	if opts.Engine == EngineTopo {
		t.Fatal("default options selected topo")
	}
	// Grid engine must still be able to touch this board (regression lock).
	if len(rep.PerNet) == 0 {
		t.Fatal("grid engine produced no net results")
	}
}

func TestDelaunayHasInnerFaces(t *testing.T) {
	sites := []topoSite{
		{pos: p2{0, 0}, half: p2{0.5, 0.5}},
		{pos: p2{10, 0}, half: p2{0.5, 0.5}},
		{pos: p2{5, 8}, half: p2{0.5, 0.5}},
		{pos: p2{5, 3}, half: p2{0.5, 0.5}},
	}
	d := buildDelaunay(sites)
	if len(d.tris) == 0 {
		t.Fatal("expected inner triangles")
	}
	if _, ok := d.faceContaining(p2{5, 3}); !ok {
		t.Fatal("interior site should land in a face")
	}
}

func TestDelaunayMeshIsDeterministic(t *testing.T) {
	sites := []topoSite{
		{pos: p2{5, 5}, half: p2{0.5, 0.6}},
		{pos: p2{25, 25}, half: p2{0.5, 0.6}},
		{pos: p2{25, 5}, half: p2{0.5, 0.6}},
		{pos: p2{5, 25}, half: p2{0.5, 0.6}},
		{pos: p2{15, 15}, half: p2{0.5, 0.6}},
		{pos: p2{15, 18}, half: p2{0.5, 0.6}},
	}
	sig := func() string {
		d := buildDelaunay(sites)
		var b []byte
		for i, t := range d.tris {
			b = append(b, byte(t.v[0]), byte(t.v[1]), byte(t.v[2]), byte(i))
			for _, ad := range d.adj[i] {
				b = append(b, byte(ad.nb), byte(ad.sa), byte(ad.sb))
			}
		}
		f, ok := d.faceContaining(p2{5, 5})
		if !ok {
			t.Fatal("A1 not in a face")
		}
		b = append(b, byte(f))
		return string(b)
	}
	want := sig()
	for i := 0; i < 16; i++ {
		if got := sig(); got != want {
			t.Fatalf("delaunay mesh differed on trial %d", i)
		}
	}
}
