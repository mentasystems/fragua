package router

import (
	"math"
	"sort"
)

// Constrained-enough Delaunay for the topological router: Bowyer–Watson
// on pad sites, then the triangle dual the homotopy A* walks. PCB site
// counts are tens to low hundreds; O(n²) is fine and keeps us free of
// a geometry crate (Rust used spade).

type dVertex struct {
	x, y float64
	site int // index into topo sites; -1 for the super-triangle
}

type dTriangle struct {
	v [3]int
}

type delaunay struct {
	verts []dVertex
	tris  []dTriangle
	// Dual: one node per inner triangle.
	centroid []p2
	// adj[face] = (neighbour face, undirected site pair of the crossed edge)
	adj [][]dAdj
}

type dAdj struct {
	nb     int
	sa, sb int
}

func buildDelaunay(sites []topoSite) *delaunay {
	if len(sites) < 3 {
		return &delaunay{}
	}
	minX, minY := math.Inf(1), math.Inf(1)
	maxX, maxY := math.Inf(-1), math.Inf(-1)
	verts := make([]dVertex, 0, len(sites)+3)
	for i, s := range sites {
		if s.pos[0] < minX {
			minX = s.pos[0]
		}
		if s.pos[1] < minY {
			minY = s.pos[1]
		}
		if s.pos[0] > maxX {
			maxX = s.pos[0]
		}
		if s.pos[1] > maxY {
			maxY = s.pos[1]
		}
		// Skip near-duplicates — they break circumcircle tests.
		dup := false
		for _, v := range verts {
			if dist2(v.xy(), s.pos) < 1e-9 {
				dup = true
				break
			}
		}
		if dup {
			continue
		}
		verts = append(verts, dVertex{x: s.pos[0], y: s.pos[1], site: i})
	}
	if len(verts) < 3 {
		return &delaunay{}
	}
	dx, dy := maxX-minX, maxY-minY
	diag := math.Hypot(dx, dy)
	if diag < 1 {
		diag = 1
	}
	midX := (minX + maxX) / 2
	super := []dVertex{
		{x: midX, y: minY - 3*diag, site: -1},
		{x: minX - 3*diag, y: maxY + diag, site: -1},
		{x: maxX + 3*diag, y: maxY + diag, site: -1},
	}
	base := len(super)
	all := append(super, verts...)
	tris := []dTriangle{{v: [3]int{0, 1, 2}}}

	orient := func(a, b, c dVertex) float64 {
		return (b.x-a.x)*(c.y-a.y) - (b.y-a.y)*(c.x-a.x)
	}
	inCirc := func(t dTriangle, p dVertex) bool {
		a, b, c := all[t.v[0]], all[t.v[1]], all[t.v[2]]
		if orient(a, b, c) < 0 {
			b, c = c, b
		}
		adx, ady := a.x-p.x, a.y-p.y
		bdx, bdy := b.x-p.x, b.y-p.y
		cdx, cdy := c.x-p.x, c.y-p.y
		al := adx*adx + ady*ady
		bl := bdx*bdx + bdy*bdy
		cl := cdx*cdx + cdy*cdy
		det := adx*(bdy*cl-bl*cdy) - ady*(bdx*cl-bl*cdx) + al*(bdx*cdy-bdy*cdx)
		return det > 1e-18
	}

	for i := range verts {
		pi := base + i
		p := all[pi]
		type edge struct{ a, b int }
		var bad []int
		for ti, t := range tris {
			if inCirc(t, p) {
				bad = append(bad, ti)
			}
		}
		seen := map[edge]int{}
		key := func(a, b int) edge {
			if a > b {
				a, b = b, a
			}
			return edge{a, b}
		}
		for _, ti := range bad {
			t := tris[ti]
			for k := 0; k < 3; k++ {
				e := key(t.v[k], t.v[(k+1)%3])
				seen[e]++
			}
		}
		dead := map[int]bool{}
		for _, ti := range bad {
			dead[ti] = true
		}
		var kept []dTriangle
		for ti, t := range tris {
			if !dead[ti] {
				kept = append(kept, t)
			}
		}
		// Map iteration order of `seen` is not stable across hosts.
		// Sort the cavity boundary so the new triangles (and later
		// dense face ids) are the same run to run.
		var boundary []edge
		for e, n := range seen {
			if n == 1 {
				boundary = append(boundary, e)
			}
		}
		sort.Slice(boundary, func(i, j int) bool {
			if boundary[i].a != boundary[j].a {
				return boundary[i].a < boundary[j].a
			}
			return boundary[i].b < boundary[j].b
		})
		for _, e := range boundary {
			a, b := all[e.a], all[e.b]
			v := [3]int{e.a, e.b, pi}
			if orient(a, b, p) < 0 {
				v[0], v[1] = v[1], v[0]
			}
			kept = append(kept, dTriangle{v: v})
		}
		tris = kept
	}

	var inner []dTriangle
	for _, t := range tris {
		if all[t.v[0]].site < 0 || all[t.v[1]].site < 0 || all[t.v[2]].site < 0 {
			continue
		}
		inner = append(inner, t)
	}
	sort.Slice(inner, func(i, j int) bool {
		return triLess(all, inner[i], inner[j])
	})

	d := &delaunay{verts: all, tris: inner}
	d.deriveDual()
	return d
}

// triLess is a host-stable order on triangles: sorted site indices,
// then vertex indices. Face ids in the dual follow this, so A* ties
// (equal f, then lower node = face*2+layer) pick the same homotopy.
func triLess(all []dVertex, a, b dTriangle) bool {
	ka, kb := canonSites(all, a), canonSites(all, b)
	for k := 0; k < 3; k++ {
		if ka[k] != kb[k] {
			return ka[k] < kb[k]
		}
	}
	va, vb := canonVerts(a), canonVerts(b)
	for k := 0; k < 3; k++ {
		if va[k] != vb[k] {
			return va[k] < vb[k]
		}
	}
	return false
}

func canonSites(all []dVertex, t dTriangle) [3]int {
	s := [3]int{all[t.v[0]].site, all[t.v[1]].site, all[t.v[2]].site}
	return sort3(s)
}

func canonVerts(t dTriangle) [3]int {
	return sort3(t.v)
}

func sort3(s [3]int) [3]int {
	if s[0] > s[1] {
		s[0], s[1] = s[1], s[0]
	}
	if s[1] > s[2] {
		s[1], s[2] = s[2], s[1]
	}
	if s[0] > s[1] {
		s[0], s[1] = s[1], s[0]
	}
	return s
}

func (v dVertex) xy() p2 { return p2{v.x, v.y} }

func (d *delaunay) deriveDual() {
	n := len(d.tris)
	d.centroid = make([]p2, n)
	d.adj = make([][]dAdj, n)
	type ekey struct{ a, b int }
	type hit struct{ face, va, vb int }
	edges := map[ekey][]hit{}
	for i, t := range d.tris {
		var c p2
		for k := 0; k < 3; k++ {
			v := d.verts[t.v[k]]
			c[0] += v.x
			c[1] += v.y
		}
		d.centroid[i] = p2{c[0] / 3, c[1] / 3}
		for k := 0; k < 3; k++ {
			a, b := t.v[k], t.v[(k+1)%3]
			if a > b {
				a, b = b, a
			}
			k2 := ekey{a, b}
			edges[k2] = append(edges[k2], hit{face: i, va: t.v[k], vb: t.v[(k+1)%3]})
		}
	}
	type pair struct {
		a, b   hit
		sa, sb int
	}
	var innerEdges []pair
	for _, hits := range edges {
		if len(hits) != 2 {
			continue // hull: crossing would leave the convex hull of sites
		}
		a, b := hits[0], hits[1]
		sa := d.verts[a.va].site
		sb := d.verts[a.vb].site
		if sa > sb {
			sa, sb = sb, sa
		}
		if sa < 0 || sb < 0 {
			continue
		}
		innerEdges = append(innerEdges, pair{a: a, b: b, sa: sa, sb: sb})
	}
	sort.Slice(innerEdges, func(i, j int) bool {
		x, y := innerEdges[i], innerEdges[j]
		if x.sa != y.sa {
			return x.sa < y.sa
		}
		if x.sb != y.sb {
			return x.sb < y.sb
		}
		if x.a.face != y.a.face {
			return x.a.face < y.a.face
		}
		return x.b.face < y.b.face
	})
	for _, e := range innerEdges {
		d.adj[e.a.face] = append(d.adj[e.a.face], dAdj{nb: e.b.face, sa: e.sa, sb: e.sb})
		d.adj[e.b.face] = append(d.adj[e.b.face], dAdj{nb: e.a.face, sa: e.sa, sb: e.sb})
	}
	// A* expands neighbours in this order; equal-cost updates keep the
	// first (strict <), so the list itself must be host-stable.
	for i := range d.adj {
		sort.Slice(d.adj[i], func(a, b int) bool {
			x, y := d.adj[i][a], d.adj[i][b]
			if x.nb != y.nb {
				return x.nb < y.nb
			}
			if x.sa != y.sa {
				return x.sa < y.sa
			}
			return x.sb < y.sb
		})
	}
}

func (d *delaunay) faceContaining(p p2) (int, bool) {
	if len(d.tris) == 0 {
		return 0, false
	}
	// A pad centre sits ON a vertex, so several triangles contain it
	// (barycentric weight 0 on the opposite edge). Take the lowest
	// face id — ids are assigned after a stable triangle sort.
	hit := -1
	best, bestD := 0, math.Inf(1)
	for i, t := range d.tris {
		a, b, c := d.verts[t.v[0]].xy(), d.verts[t.v[1]].xy(), d.verts[t.v[2]].xy()
		if baryInside(a, b, c, p) && (hit < 0 || i < hit) {
			hit = i
		}
		cen := d.centroid[i]
		dd := dist2(cen, p)
		if dd < bestD || (dd == bestD && i < best) {
			best, bestD = i, dd
		}
	}
	if hit >= 0 {
		return hit, true
	}
	return best, true
}

func baryInside(a, b, c, p p2) bool {
	v0x, v0y := c[0]-a[0], c[1]-a[1]
	v1x, v1y := b[0]-a[0], b[1]-a[1]
	v2x, v2y := p[0]-a[0], p[1]-a[1]
	den := v0x*v1y - v1x*v0y
	if math.Abs(den) < 1e-18 {
		return false
	}
	u := (v2x*v1y - v1x*v2y) / den
	v := (v0x*v2y - v2x*v0y) / den
	w := 1 - u - v
	const eps = 1e-9
	return u >= -eps && v >= -eps && w >= -eps
}
