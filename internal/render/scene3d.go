package render

import (
	"errors"
	"math"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
)

// Product-shot palette. Soldermask green, ENIG pads, bare FR-4 on the
// edge — the stack a fabrication render is expected to show, without a
// STEP model for every part.
var (
	colMask    = srgb8(18, 122, 58)
	colMaskBot = srgb8(10, 74, 38)
	colFR4     = srgb8(198, 164, 106)
	colCopper  = srgb8(196, 122, 52)
	colENIG    = srgb8(230, 196, 112)
	colSilk    = srgb8(244, 241, 232)
	colHole    = srgb8(12, 12, 14)
	colPlastic = srgb8(62, 66, 74)
	colTin     = srgb8(210, 214, 218)
	colMLCC    = srgb8(122, 96, 64)
	// Mid-dark, not near-black: a #161616 body encodes to the same PNG
	// value on the top and the side, so the box stops reading as extruded.
	colIC    = srgb8(74, 76, 84)
	colCan   = srgb8(196, 202, 210)
	colShell = srgb8(88, 94, 104)
	colPin1  = srgb8(236, 236, 232)
	colChip  = srgb8(68, 62, 56)
)

const (
	copperRiseMM = 0.10 // visual copper height; 35 µm would vanish at board scale
	padRiseMM    = 0.16
	maskGapMM    = 0.02 // bodies sit a hair above the mask so they win the z-buffer
)

// Shot3D tunes a product-shot render. The zero value is a 1600px-wide
// image with 2× supersampling and a frame matched to the board.
type Shot3D struct {
	Width   int // pixels; 0 → 1600
	Height  int // pixels; 0 → derived from the board aspect
	Samples int // supersample factor, 1..3; 0 → 2
}

// BoardPNG3D renders board as a PNG product shot: thickness, soldermask,
// copper, silkscreen, drills, and box bodies where no CAD model exists.
func BoardPNG3D(board *core.Board, opt Shot3D) ([]byte, error) {
	if board == nil || (board.Outline == nil && len(board.OutlinePoly) < 3) {
		return nil, errNoOutline
	}
	scn, err := buildScene(board)
	if err != nil {
		return nil, err
	}
	w, h := framePixels(scn, opt)
	samples := opt.Samples
	if samples == 0 {
		samples = 2
	}
	cam := fitCamera(scn, w, h)
	img := renderMesh(scn.mesh, shot{cam: cam, shadow: scn.shadow}, samples)
	return encodePNG(img)
}

var errNoOutline = errors.New("render: board has no outline")

type scene struct {
	mesh   mesh
	minX   float64
	minY   float64
	maxX   float64
	maxY   float64
	topZ   float64
	peakZ  float64
	shadow shadow
}

type bodyRec struct {
	corners [4]vec2
	zTop    float64
}

func framePixels(s scene, opt Shot3D) (int, int) {
	w := opt.Width
	if w <= 0 {
		w = 1600
	}
	if w < 32 {
		w = 32
	}
	if w > 8192 {
		w = 8192
	}
	h := opt.Height
	if h <= 0 {
		bw := s.maxX - s.minX
		bh := s.maxY - s.minY
		if bw < 1 {
			bw = 1
		}
		if bh < 1 {
			bh = 1
		}
		// Depth foreshortens one axis; keep a landscape product frame.
		frame := (bw / bh) * 0.90
		if frame < 1.08 {
			frame = 1.08
		}
		if frame > 2.55 {
			frame = 2.55
		}
		h = int(math.Round(float64(w) / frame))
	}
	if h < 32 {
		h = 32
	}
	if h > 8192 {
		h = 8192
	}
	return w, h
}

func fitCamera(s scene, w, h int) camera {
	// Slightly off-axis, mostly above: the front edge (min Y) is nearest
	// so silkscreen reads upright, and the right edge shows thickness.
	const (
		azimuth = 34 * math.Pi / 180
		elev    = 40 * math.Pi / 180
		fovY    = 28 * math.Pi / 180
	)
	target := vec3{
		(s.minX + s.maxX) / 2,
		(s.minY + s.maxY) / 2,
		s.topZ * 0.45,
	}
	dir := vec3{
		math.Sin(azimuth) * math.Cos(elev),
		-math.Cos(azimuth) * math.Cos(elev),
		math.Sin(elev),
	}.norm()
	upHint := vec3{0, 0, 1}
	// Provisional basis; distance is solved against it.
	forward := dir.scale(-1) // from eye toward target; eye = target + dir*dist, forward = -dir
	right := forward.cross(upHint).norm()
	up := right.cross(forward).norm()

	pad := 1.6
	peak := s.peakZ
	pts := []vec3{
		{s.minX - pad, s.minY - pad, 0},
		{s.maxX + pad, s.minY - pad, 0},
		{s.maxX + pad, s.maxY + pad, 0},
		{s.minX - pad, s.maxY + pad, 0},
		{s.minX - pad, s.minY - pad, peak},
		{s.maxX + pad, s.minY - pad, peak},
		{s.maxX + pad, s.maxY + pad, peak},
		{s.minX - pad, s.maxY + pad, peak},
	}
	var maxR float64
	for _, p := range pts {
		if d := p.sub(target).len(); d > maxR {
			maxR = d
		}
	}
	if maxR < 1 {
		maxR = 1
	}
	dist := maxR
	aspect := float64(w) / float64(h)
	for i := 0; i < 32; i++ {
		eye := target.add(dir.scale(dist))
		if fits(eye, forward, right, up, fovY, aspect, pts) {
			break
		}
		dist *= 1.07
	}
	dist *= 1.08
	eye := target.add(dir.scale(dist))
	return camera{eye: eye, forward: forward, right: right, up: up, fovY: fovY, w: w, h: h}
}

func fits(eye, forward, right, up vec3, fovY, aspect float64, pts []vec3) bool {
	tanY := math.Tan(fovY / 2)
	tanX := tanY * aspect
	for _, p := range pts {
		rel := p.sub(eye)
		depth := rel.dot(forward)
		if depth < 1 {
			return false
		}
		if math.Abs(rel.dot(right)) > depth*tanX*0.88 {
			return false
		}
		if math.Abs(rel.dot(up)) > depth*tanY*0.88 {
			return false
		}
	}
	return true
}

func buildScene(board *core.Board) (scene, error) {
	outer, ok := boardOutline(board)
	if !ok || len(outer) < 3 {
		return scene{}, errNoOutline
	}
	th := board.StackupOrDefault().TotalThicknessMM()
	if th < 0.4 || th > 6 {
		th = 1.6
	}
	bevel := math.Min(0.32, th*0.2)
	inner := insetConvex(outer, bevel)
	if len(inner) != len(outer) {
		inner = outer
		bevel = 0
	}

	var s scene
	s.topZ = th
	minX, minY := outer[0].x, outer[0].y
	maxX, maxY := outer[0].x, outer[0].y
	for _, p := range outer[1:] {
		minX, minY = math.Min(minX, p.x), math.Min(minY, p.y)
		maxX, maxY = math.Max(maxX, p.x), math.Max(maxY, p.y)
	}
	s.minX, s.minY, s.maxX, s.maxY = minX, minY, maxX, maxY

	// Substrate: bottom, vertical FR-4, chamfer in soldermask, top face.
	s.mesh.fan(outer, 0, colMaskBot, 0.02, 0)
	s.mesh.wall(outer, 0, th-bevel, colFR4, 0.04, 0)
	if bevel > 0.05 {
		s.mesh.wallPair(outer, inner, th-bevel, th, colMask, 0.05, 0.02)
		s.mesh.fan(inner, th, colMask, 0.08, 0.03)
	} else {
		s.mesh.fan(outer, th, colMask, 0.08, 0.03)
	}

	// Milled cutouts read as openings in the mask. A filled polygon sitting
	// just above the soldermask is enough at a product-shot angle; the wall
	// below it shows the FR-4 edge of the cut when the camera can see in.
	for _, c := range cutoutPolys(board) {
		s.mesh.wall(c, 0, th, colFR4, 0.04, 0.08)
		s.mesh.fan(c, th, colHole, 0, 0.34)
	}

	for _, h := range boardHoles(board) {
		r := h.Diameter.ToMM() / 2
		if r <= 0 {
			continue
		}
		cx, cy := h.Center.X.ToMM(), h.Center.Y.ToMM()
		s.mesh.disk(cx, cy, th, r+0.18, 28, colFR4, 0.05, 0.10)
		s.mesh.disk(cx, cy, th, r, 28, colHole, 0, 0.36)
		s.mesh.cylinder(cx, cy, 0, th, r, 20, colHole, 0, 0.20, false)
	}

	s.addCopper(board, th)
	bodies := s.addBodies(board, th)
	s.addSilk(board, th, bodies)

	s.peakZ = th + 1.2
	for _, b := range bodies {
		if b.zTop > s.peakZ {
			s.peakZ = b.zTop
		}
	}
	// Header pins rise above the plastic; peakZ tracks the tallest pin.
	if s.peakZ < th+1 {
		s.peakZ = th + 1
	}

	hx := (maxX - minX) / 2 * 1.18
	hy := (maxY - minY) / 2 * 1.18
	s.shadow = shadow{
		center: vec3{(minX + maxX) / 2, (minY + maxY) / 2, 0},
		ax:     vec3{hx, 0, 0},
		ay:     vec3{0, hy, 0},
		ok:     hx > 0.5 && hy > 0.5,
	}
	return s, nil
}

func (m *mesh) fan(poly []vec2, z float64, col fcol, metal, bias float64) {
	if len(poly) < 3 {
		return
	}
	var c vec2
	for _, p := range poly {
		c.x += p.x
		c.y += p.y
	}
	c.x /= float64(len(poly))
	c.y /= float64(len(poly))
	ctr := vec3{c.x, c.y, z}
	for i := 0; i < len(poly); i++ {
		j := (i + 1) % len(poly)
		m.tri(ctr, vec3{poly[i].x, poly[i].y, z}, vec3{poly[j].x, poly[j].y, z}, col, metal, bias)
	}
}

func (m *mesh) wall(poly []vec2, z0, z1 float64, col fcol, metal, bias float64) {
	for i := 0; i < len(poly); i++ {
		j := (i + 1) % len(poly)
		a := vec3{poly[i].x, poly[i].y, z0}
		b := vec3{poly[i].x, poly[i].y, z1}
		c := vec3{poly[j].x, poly[j].y, z1}
		d := vec3{poly[j].x, poly[j].y, z0}
		m.quad(a, b, c, d, col, metal, bias)
	}
}

func (m *mesh) wallPair(outer, inner []vec2, z0, z1 float64, col fcol, metal, bias float64) {
	n := len(outer)
	if len(inner) != n {
		return
	}
	for i := 0; i < n; i++ {
		j := (i + 1) % n
		a := vec3{outer[i].x, outer[i].y, z0}
		b := vec3{inner[i].x, inner[i].y, z1}
		c := vec3{inner[j].x, inner[j].y, z1}
		d := vec3{outer[j].x, outer[j].y, z0}
		m.quad(a, b, c, d, col, metal, bias)
	}
}

func (m *mesh) disk(cx, cy, z, r float64, n int, col fcol, metal, bias float64) {
	if r <= 0 || n < 3 {
		return
	}
	ctr := vec3{cx, cy, z}
	for i := 0; i < n; i++ {
		a0 := float64(i) / float64(n) * 2 * math.Pi
		a1 := float64(i+1) / float64(n) * 2 * math.Pi
		p0 := vec3{cx + r*math.Cos(a0), cy + r*math.Sin(a0), z}
		p1 := vec3{cx + r*math.Cos(a1), cy + r*math.Sin(a1), z}
		m.tri(ctr, p0, p1, col, metal, bias)
	}
}

func (m *mesh) cylinder(cx, cy, z0, z1, r float64, n int, col fcol, metal, bias float64, cap bool) {
	if r <= 0 || n < 3 || z1 <= z0 {
		return
	}
	for i := 0; i < n; i++ {
		a0 := float64(i) / float64(n) * 2 * math.Pi
		a1 := float64(i+1) / float64(n) * 2 * math.Pi
		p0 := vec2{cx + r*math.Cos(a0), cy + r*math.Sin(a0)}
		p1 := vec2{cx + r*math.Cos(a1), cy + r*math.Sin(a1)}
		m.quad(
			vec3{p0.x, p0.y, z0}, vec3{p0.x, p0.y, z1},
			vec3{p1.x, p1.y, z1}, vec3{p1.x, p1.y, z0},
			col, metal, bias)
	}
	if cap {
		m.disk(cx, cy, z1, r, n, col, metal, bias+0.02)
	}
}

func (m *mesh) prism(c [4]vec2, z0, z1 float64, col fcol, metal, bias float64) {
	if z1 < z0 {
		z0, z1 = z1, z0
	}
	var bot, top [4]vec3
	for i := 0; i < 4; i++ {
		bot[i] = vec3{c[i].x, c[i].y, z0}
		top[i] = vec3{c[i].x, c[i].y, z1}
	}
	// Sides a step darker than the lid so a box reads as a box even when
	// the key light is nearly overhead.
	side := scaleCol(col, 0.78)
	lid := scaleCol(col, 1.22)
	for i := 0; i < 4; i++ {
		j := (i + 1) % 4
		m.quad(bot[i], top[i], top[j], bot[j], side, metal, bias)
	}
	m.quad(top[0], top[1], top[2], top[3], lid, metal, bias+0.01)
}

func scaleCol(c fcol, s float64) fcol {
	return fcol{math.Min(1, c.r*s), math.Min(1, c.g*s), math.Min(1, c.b*s)}
}

func (m *mesh) ribbon(p0, p1 vec2, width, z float64, col fcol, metal, bias float64) {
	dx, dy := p1.x-p0.x, p1.y-p0.y
	l := math.Hypot(dx, dy)
	if l < 1e-4 || width <= 0 {
		return
	}
	// Overlap the caps so segment joints do not gap. Bias keeps the overlap stable.
	ex, ey := dx/l*width*0.5, dy/l*width*0.5
	nx, ny := -dy/l*width*0.5, dx/l*width*0.5
	a := vec2{p0.x - ex + nx, p0.y - ey + ny}
	b := vec2{p1.x + ex + nx, p1.y + ey + ny}
	c := vec2{p1.x + ex - nx, p1.y + ey - ny}
	d := vec2{p0.x - ex - nx, p0.y - ey - ny}
	m.quad(vec3{a.x, a.y, z}, vec3{b.x, b.y, z}, vec3{c.x, c.y, z}, vec3{d.x, d.y, z}, col, metal, bias)
	// A thin side so the trace catches light along its length.
	z1 := z + copperRiseMM*0.45
	m.quad(vec3{a.x, a.y, z}, vec3{a.x, a.y, z1}, vec3{b.x, b.y, z1}, vec3{b.x, b.y, z}, col, metal, bias)
	m.quad(vec3{c.x, c.y, z}, vec3{d.x, d.y, z}, vec3{d.x, d.y, z1}, vec3{c.x, c.y, z1}, col, metal, bias)
}

func (s *scene) addCopper(board *core.Board, th float64) {
	z0 := th
	for i, tr := range board.Traces {
		if tr.Layer.Index != 0 {
			continue
		}
		w := tr.Width.ToMM()
		if w < 0.08 {
			w = 0.15
		}
		p0 := vec2{tr.Start.X.ToMM(), tr.Start.Y.ToMM()}
		p1 := vec2{tr.End.X.ToMM(), tr.End.Y.ToMM()}
		s.mesh.ribbon(p0, p1, w, z0+0.01, colCopper, 0.55, 0.14+float64(i)*1e-5)
	}
	bi := 0
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			pth := pad.Drill != nil && *pad.Drill > 0
			if !pth && (fp.Layer.Index != 0 || pad.Layer.Index != 0) {
				continue
			}
			corners := padCorners(fp, pad)
			s.mesh.prism(corners, z0, z0+padRiseMM, colENIG, 0.72, 0.22+float64(bi)*1e-5)
			bi++
			if pth {
				c := core.PadWorldCenter(fp, pad)
				r := pad.Drill.ToMM() / 2
				if r < 0.05 {
					r = 0.15
				}
				s.mesh.disk(c.X.ToMM(), c.Y.ToMM(), z0+padRiseMM, r, 14, colHole, 0, 0.40)
			}
		}
	}
	for i, v := range board.Vias {
		r := v.Diameter.ToMM() / 2
		if r < 0.08 {
			r = 0.3
		}
		dr := v.Drill.ToMM() / 2
		if dr < 0.05 {
			dr = r * 0.45
		}
		if dr >= r {
			dr = r * 0.55
		}
		cx, cy := v.Position.X.ToMM(), v.Position.Y.ToMM()
		bias := 0.26 + float64(i)*1e-5
		s.mesh.disk(cx, cy, z0+padRiseMM, r, 12, colENIG, 0.6, bias)
		s.mesh.disk(cx, cy, z0+padRiseMM, dr, 12, colHole, 0, bias+0.08)
	}
}

func (s *scene) addBodies(board *core.Board, th float64) []bodyRec {
	var out []bodyRec
	zBase := th + maskGapMM
	for _, fp := range footprintsStable(board) {
		if fp == nil || fp.Fiducial || len(fp.Pads) == 0 {
			continue
		}
		if fp.Layer.Index != 0 {
			continue
		}
		kind := classifyPart(fp)
		corners, height, col, metal, ok := bodyOf(fp, kind)
		if !ok || height <= 0 {
			continue
		}
		z0 := zBase
		if fp.Elevated {
			z0 += 8
		}
		z1 := z0 + height
		s.mesh.prism(corners, z0, z1, col, metal, 0.48)
		out = append(out, bodyRec{corners: corners, zTop: z1})
		switch kind {
		case kindHeader:
			s.addHeaderPins(fp, zBase)
			if z1+6 > s.peakZ {
				s.peakZ = zBase + 8.6
			}
		case kindSwitch:
			btn := insetQuad(corners, 0.28)
			s.mesh.prism(btn, z1, z1+0.42, srgb8(48, 48, 52), 0.12, 0.62)
		case kindLED:
			c := quadCenter(corners)
			rad := math.Min(quadSpan(corners)*0.22, 0.45)
			s.mesh.disk(c.x, c.y, z1, rad, 12, brighter(col), 0.25, 0.64)
		case kindQFN, kindSOIC, kindSOT, kindIC:
			s.addPin1(fp, corners, z1)
		}
		if z1 > s.peakZ {
			s.peakZ = z1
		}
	}
	return out
}

func (s *scene) addHeaderPins(fp *core.Footprint, zBase float64) {
	for i := range fp.Pads {
		c := core.PadWorldCenter(fp, &fp.Pads[i])
		r := 0.28
		if fp.Pads[i].Drill != nil && *fp.Pads[i].Drill > 0 {
			r = math.Min(0.42, fp.Pads[i].Drill.ToMM()*0.42)
		}
		s.mesh.cylinder(c.X.ToMM(), c.Y.ToMM(), zBase, zBase+8.5, r, 8, colTin, 0.65, 0.70, true)
	}
	if s.peakZ < zBase+8.5 {
		s.peakZ = zBase + 8.5
	}
}

func (s *scene) addPin1(fp *core.Footprint, corners [4]vec2, zTop float64) {
	var pad *core.Pad
	for i := range fp.Pads {
		if fp.Pads[i].Number == "1" {
			pad = &fp.Pads[i]
			break
		}
	}
	if pad == nil {
		pad = &fp.Pads[0]
	}
	c := core.PadWorldCenter(fp, pad)
	target := vec2{c.X.ToMM(), c.Y.ToMM()}
	best := corners[0]
	bestD := 1e18
	for _, q := range corners {
		d := (q.x-target.x)*(q.x-target.x) + (q.y-target.y)*(q.y-target.y)
		if d < bestD {
			bestD = d
			best = q
		}
	}
	mid := quadCenter(corners)
	dot := vec2{best.x*0.72 + mid.x*0.28, best.y*0.72 + mid.y*0.28}
	rad := math.Min(0.32, quadSpan(corners)*0.06)
	if rad < 0.12 {
		rad = 0.12
	}
	s.mesh.disk(dot.x, dot.y, zTop, rad, 10, colPin1, 0.1, 0.66)
}

func (s *scene) addSilk(board *core.Board, th float64, bodies []bodyRec) {
	z := th + 0.035
	for i, ln := range board.SilkLines {
		if ln.Layer != "" && ln.Layer != core.SilkTop {
			continue
		}
		w := ln.Width.ToMM()
		if w < 0.05 {
			w = 0.12
		}
		s.mesh.ribbon(
			vec2{ln.Start.X.ToMM(), ln.Start.Y.ToMM()},
			vec2{ln.End.X.ToMM(), ln.End.Y.ToMM()},
			w, z, colSilk, 0.05, 0.18+float64(i)*1e-5)
	}
	for _, t := range board.SilkTexts {
		if t.Layer != "" && t.Layer != core.SilkTop {
			continue
		}
		s.strokeText(t.Text, t.Position, t.Size, t.Rotation, t.Anchor, t.Width, z, 0.18, nil)
	}
	for _, fp := range footprintsStable(board) {
		if fp == nil || fp.Layer.Index != 0 {
			continue
		}
		drewText := false
		for _, item := range fp.Silk {
			if item.Layer != "" && item.Layer != core.SilkTop {
				continue
			}
			switch item.Kind {
			case "line":
				w := item.Width.ToMM()
				if w < 0.05 {
					w = 0.12
				}
				a := core.LocalToWorld(fp, item.Start)
				b := core.LocalToWorld(fp, item.End)
				s.mesh.ribbon(vec2{a.X.ToMM(), a.Y.ToMM()}, vec2{b.X.ToMM(), b.Y.ToMM()}, w, z, colSilk, 0.05, 0.19)
			case "text":
				txt := core.ResolveSilkText(fp, item.Text)
				origin := core.LocalToWorld(fp, item.Position)
				rot := fp.Rotation + item.Rotation
				s.strokeText(txt, origin, item.Size, rot, item.Anchor, item.Width, z, 0.20, bodies)
				drewText = true
			}
		}
		if !drewText && fp.Reference != "" {
			// Park the designator just off the body, on the mask.
			off := core.LocalToWorld(fp, core.NewPoint(0, core.FromMM(courtyardNorth(fp)+0.7)))
			s.strokeText(fp.Reference, off, core.FromMM(0.7), fp.Rotation, core.SilkAnchorMiddle, 0, z, 0.20, nil)
		}
	}
}

func (s *scene) strokeText(text string, origin core.Point, size core.Length, rot float64, anchor core.SilkAnchor, width core.Length, zBoard, bias float64, bodies []bodyRec) {
	if strings.TrimSpace(text) == "" {
		return
	}
	if size <= 0 {
		size = core.FromMM(0.8)
	}
	z := zBoard
	col := colSilk
	// A designator whose origin lands on a body is printed on the package,
	// above the epoxy, instead of disappearing under it.
	for _, b := range bodies {
		if pointInQuad(vec2{origin.X.ToMM(), origin.Y.ToMM()}, b.corners) {
			z = b.zTop + 0.02
			bias = 0.72
			col = srgb8(210, 210, 206)
			break
		}
	}
	stroke := width
	if stroke <= 0 {
		stroke = core.DefaultSilkStroke(size)
	}
	sw := stroke.ToMM()
	if sw < 0.05 {
		sw = 0.08
	}
	for _, poly := range core.TextPolylines(text, origin, size, rot, anchor) {
		for i := 1; i < len(poly); i++ {
			p0 := vec2{poly[i-1].X.ToMM(), poly[i-1].Y.ToMM()}
			p1 := vec2{poly[i].X.ToMM(), poly[i].Y.ToMM()}
			s.mesh.ribbon(p0, p1, sw, z, col, 0.04, bias)
		}
	}
}

func courtyardNorth(fp *core.Footprint) float64 {
	if fp.BodyRect != nil {
		return fp.BodyRect.MaxYMM
	}
	_, _, _, y1 := padLocalAABB(fp)
	return y1
}

type partKind int

const (
	kindR partKind = iota
	kindC
	kindL
	kindLED
	kindDiode
	kindHeader
	kindUSB
	kindCrystal
	kindSwitch
	kindQFN
	kindSOIC
	kindSOT
	kindIC
	kindModule
)

func classifyPart(fp *core.Footprint) partKind {
	key := strings.ToLower(fp.Key + " " + fp.Description)
	prefix := refPrefix(fp.Reference)
	switch {
	case strings.Contains(key, "header") || strings.Contains(key, "pinhead"):
		return kindHeader
	case strings.Contains(key, "usbc") || strings.Contains(key, "usb_c") || strings.Contains(key, "usb-c"):
		return kindUSB
	case strings.Contains(key, "xtal") || strings.Contains(key, "crystal") || prefix == "Y":
		return kindCrystal
	case strings.Contains(key, "led") || prefix == "LED":
		return kindLED
	case strings.Contains(key, "sw_") || strings.Contains(key, "switch") || prefix == "SW":
		return kindSwitch
	case strings.Contains(key, "qfn") || strings.Contains(key, "qfp") || strings.Contains(key, "bga") || strings.Contains(key, "wlp"):
		return kindQFN
	case strings.Contains(key, "soic") || strings.Contains(key, "sop") || strings.Contains(key, "ssop") || strings.Contains(key, "tssop"):
		return kindSOIC
	case strings.Contains(key, "sot"):
		return kindSOT
	case strings.Contains(key, "r_") || strings.Contains(key, "resistor") || prefix == "R":
		return kindR
	case strings.Contains(key, "c_") || strings.Contains(key, "cap") || prefix == "C":
		return kindC
	case prefix == "L":
		return kindL
	case prefix == "D":
		return kindDiode
	}
	x0, y0, x1, y1 := padLocalAABB(fp)
	if math.Max(x1-x0, y1-y0) >= 10 {
		return kindModule
	}
	if prefix == "J" || prefix == "P" {
		if hasDrill(fp) {
			return kindHeader
		}
		return kindModule
	}
	return kindIC
}

func bodyOf(fp *core.Footprint, kind partKind) (corners [4]vec2, height float64, col fcol, metal float64, ok bool) {
	switch kind {
	case kindR, kindC, kindL, kindLED, kindDiode:
		corners, ok = passiveCorners(fp)
		if !ok {
			return
		}
		switch kind {
		case kindC:
			return corners, 0.8, colMLCC, 0.08, true
		case kindL:
			return corners, 0.85, srgb8(64, 64, 70), 0.25, true
		case kindLED:
			return corners, 0.75, ledColor(fp), 0.18, true
		case kindDiode:
			return corners, 0.7, colIC, 0.15, true
		default:
			return corners, 0.8, colChip, 0.12, true
		}
	case kindHeader:
		corners, ok = aabbCorners(fp, -0.15)
		return corners, 2.54, colPlastic, 0.08, ok
	case kindUSB:
		corners, ok = aabbCorners(fp, 0.05)
		return corners, 3.2, colShell, 0.55, ok
	case kindCrystal:
		corners, ok = aabbCorners(fp, 0.05)
		return corners, 0.8, colCan, 0.7, ok
	case kindSwitch:
		corners, ok = aabbCorners(fp, -0.35)
		return corners, 1.55, colPlastic, 0.1, ok
	case kindQFN:
		corners, ok = aabbCorners(fp, 0.12)
		return corners, 1.15, colIC, 0.28, ok
	case kindSOIC:
		if c, good := gullCorners(fp); good {
			return c, 1.55, colIC, 0.22, true
		}
		corners, ok = aabbCorners(fp, 0.45)
		return corners, 1.55, colIC, 0.22, ok
	case kindSOT:
		if c, good := gullCorners(fp); good {
			return c, 1.15, colIC, 0.2, true
		}
		corners, ok = aabbCorners(fp, 0.25)
		return corners, 1.15, colIC, 0.2, ok
	case kindModule:
		corners, ok = aabbCorners(fp, 0.06)
		return corners, 2.5, srgb8(58, 62, 70), 0.18, ok
	default:
		if c, good := gullCorners(fp); good {
			return c, 1.25, colIC, 0.22, true
		}
		corners, ok = aabbCorners(fp, 0.3)
		return corners, 1.2, colIC, 0.2, ok
	}
}

func ledColor(fp *core.Footprint) fcol {
	v := strings.ToLower(fp.Value + " " + fp.Key + " " + fp.Description)
	switch {
	case strings.Contains(v, "blue"):
		return srgb8(36, 78, 196)
	case strings.Contains(v, "yellow") || strings.Contains(v, "amber"):
		return srgb8(196, 148, 32)
	case strings.Contains(v, "white"):
		return srgb8(230, 230, 226)
	case strings.Contains(v, "green"):
		return srgb8(32, 150, 64)
	default:
		return srgb8(176, 40, 36)
	}
}

func passiveCorners(fp *core.Footprint) ([4]vec2, bool) {
	var corners [4]vec2
	if len(fp.Pads) < 2 {
		return corners, false
	}
	a := fp.Pads[0]
	b := fp.Pads[1]
	ax, ay := a.Offset.X.ToMM(), a.Offset.Y.ToMM()
	bx, by := b.Offset.X.ToMM(), b.Offset.Y.ToMM()
	dx, dy := bx-ax, by-ay
	dist := math.Hypot(dx, dy)
	if dist < 1e-4 {
		return aabbCorners(fp, 0.1)
	}
	ux, uy := dx/dist, dy/dist
	px, py := -uy, ux
	e0 := padExtentAlong(&a, ux, uy)
	e1 := padExtentAlong(&b, ux, uy)
	bodyLen := dist - e0 - e1 + 0.22
	if bodyLen < 0.28 {
		bodyLen = dist * 0.46
	}
	w0 := padExtentAlong(&a, px, py) * 2
	w1 := padExtentAlong(&b, px, py) * 2
	bodyW := math.Min(w0, w1) * 0.92
	if bodyW < 0.25 {
		bodyW = 0.4
	}
	mx, my := (ax+bx)/2, (ay+by)/2
	hl, hw := bodyLen/2, bodyW/2
	local := [4]vec2{
		{mx - ux*hl - px*hw, my - uy*hl - py*hw},
		{mx + ux*hl - px*hw, my + uy*hl - py*hw},
		{mx + ux*hl + px*hw, my + uy*hl + py*hw},
		{mx - ux*hl + px*hw, my - uy*hl + py*hw},
	}
	for i, p := range local {
		corners[i] = localToWorld(fp, p.x, p.y)
	}
	return corners, true
}

func gullCorners(fp *core.Footprint) ([4]vec2, bool) {
	var corners [4]vec2
	if len(fp.Pads) < 3 {
		return corners, false
	}
	minX, minY, maxX, maxY := padCenterSpan(fp)
	midX := (minX + maxX) / 2
	midY := (minY + maxY) / 2
	sepX := maxX - minX
	sepY := maxY - minY
	var x0, y0, x1, y1 float64
	if sepX >= sepY {
		leftInner, rightInner := -1e9, 1e9
		leftN, rightN := 0, 0
		oy0, oy1 := 1e9, -1e9
		for i := range fp.Pads {
			p := &fp.Pads[i]
			cx := p.Offset.X.ToMM()
			cy := p.Offset.Y.ToMM()
			hw, hh := p.Size[0].ToMM()/2, p.Size[1].ToMM()/2
			if cy-hh < oy0 {
				oy0 = cy - hh
			}
			if cy+hh > oy1 {
				oy1 = cy + hh
			}
			if cx < midX {
				leftN++
				if inner := cx + hw; inner > leftInner {
					leftInner = inner
				}
			} else {
				rightN++
				if inner := cx - hw; inner < rightInner {
					rightInner = inner
				}
			}
		}
		if leftN == 0 || rightN == 0 || leftInner+0.2 >= rightInner {
			return corners, false
		}
		x0, x1 = leftInner-0.12, rightInner+0.12
		y0, y1 = oy0-0.35, oy1+0.35
	} else {
		botInner, topInner := -1e9, 1e9
		botN, topN := 0, 0
		ox0, ox1 := 1e9, -1e9
		for i := range fp.Pads {
			p := &fp.Pads[i]
			cx := p.Offset.X.ToMM()
			cy := p.Offset.Y.ToMM()
			hw, hh := p.Size[0].ToMM()/2, p.Size[1].ToMM()/2
			if cx-hw < ox0 {
				ox0 = cx - hw
			}
			if cx+hw > ox1 {
				ox1 = cx + hw
			}
			if cy < midY {
				botN++
				if inner := cy + hh; inner > botInner {
					botInner = inner
				}
			} else {
				topN++
				if inner := cy - hh; inner < topInner {
					topInner = inner
				}
			}
		}
		if botN == 0 || topN == 0 || botInner+0.2 >= topInner {
			return corners, false
		}
		y0, y1 = botInner-0.12, topInner+0.12
		x0, x1 = ox0-0.35, ox1+0.35
	}
	if x1-x0 < 0.3 || y1-y0 < 0.3 {
		return corners, false
	}
	local := [4][2]float64{{x0, y0}, {x1, y0}, {x1, y1}, {x0, y1}}
	for i, p := range local {
		corners[i] = localToWorld(fp, p[0], p[1])
	}
	return corners, true
}

// aabbCorners returns the pad-union rectangle in world space.
// inset shrinks it; a negative inset grows it (courtyard-ish modules and switches).
func aabbCorners(fp *core.Footprint, inset float64) ([4]vec2, bool) {
	var corners [4]vec2
	x0, y0, x1, y1 := padLocalAABB(fp)
	if x1-x0 < 1e-3 || y1-y0 < 1e-3 {
		return corners, false
	}
	x0 += inset
	y0 += inset
	x1 -= inset
	y1 -= inset
	if x1-x0 < 0.2 || y1-y0 < 0.2 {
		// Inset ate the part; fall back to a small pad around the origin.
		x0, y0, x1, y1 = padLocalAABB(fp)
	}
	local := [4][2]float64{{x0, y0}, {x1, y0}, {x1, y1}, {x0, y1}}
	for i, p := range local {
		corners[i] = localToWorld(fp, p[0], p[1])
	}
	return corners, true
}

func padCorners(fp *core.Footprint, pad *core.Pad) [4]vec2 {
	c := core.PadWorldCenter(fp, pad)
	rot := fp.Rotation * math.Pi / 180
	co, si := math.Cos(rot), math.Sin(rot)
	hw, hh := pad.Size[0].ToMM()/2, pad.Size[1].ToMM()/2
	local := [4]vec2{{-hw, -hh}, {hw, -hh}, {hw, hh}, {-hw, hh}}
	var out [4]vec2
	cx, cy := c.X.ToMM(), c.Y.ToMM()
	for i, p := range local {
		out[i] = vec2{cx + p.x*co - p.y*si, cy + p.x*si + p.y*co}
	}
	return out
}

func padLocalAABB(fp *core.Footprint) (x0, y0, x1, y1 float64) {
	x0, y0, x1, y1 = 1e9, 1e9, -1e9, -1e9
	for i := range fp.Pads {
		p := &fp.Pads[i]
		cx, cy := p.Offset.X.ToMM(), p.Offset.Y.ToMM()
		hw, hh := p.Size[0].ToMM()/2, p.Size[1].ToMM()/2
		x0 = math.Min(x0, cx-hw)
		y0 = math.Min(y0, cy-hh)
		x1 = math.Max(x1, cx+hw)
		y1 = math.Max(y1, cy+hh)
	}
	return x0, y0, x1, y1
}

func padCenterSpan(fp *core.Footprint) (minX, minY, maxX, maxY float64) {
	minX, minY, maxX, maxY = 1e9, 1e9, -1e9, -1e9
	for i := range fp.Pads {
		x, y := fp.Pads[i].Offset.X.ToMM(), fp.Pads[i].Offset.Y.ToMM()
		minX, minY = math.Min(minX, x), math.Min(minY, y)
		maxX, maxY = math.Max(maxX, x), math.Max(maxY, y)
	}
	return minX, minY, maxX, maxY
}

func padExtentAlong(pad *core.Pad, ux, uy float64) float64 {
	return pad.Size[0].ToMM()/2*math.Abs(ux) + pad.Size[1].ToMM()/2*math.Abs(uy)
}

func localToWorld(fp *core.Footprint, x, y float64) vec2 {
	p := core.LocalToWorld(fp, core.NewPoint(core.FromMM(x), core.FromMM(y)))
	return vec2{p.X.ToMM(), p.Y.ToMM()}
}

func hasDrill(fp *core.Footprint) bool {
	for i := range fp.Pads {
		if fp.Pads[i].Drill != nil && *fp.Pads[i].Drill > 0 {
			return true
		}
	}
	return false
}

func refPrefix(ref string) string {
	i := 0
	for i < len(ref) {
		c := ref[i]
		if (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') {
			i++
			continue
		}
		break
	}
	return strings.ToUpper(ref[:i])
}

func boardOutline(board *core.Board) ([]vec2, bool) {
	if len(board.OutlinePoly) >= 3 {
		p := make([]vec2, len(board.OutlinePoly))
		for i, v := range board.OutlinePoly {
			p[i] = vec2{v.X.ToMM(), v.Y.ToMM()}
		}
		return ensureCCW(p), true
	}
	o := board.Outline
	if o == nil {
		return nil, false
	}
	x0, y0 := o.Min.X.ToMM(), o.Min.Y.ToMM()
	x1, y1 := o.Max.X.ToMM(), o.Max.Y.ToMM()
	if x1-x0 < 0.2 || y1-y0 < 0.2 {
		return nil, false
	}
	return roundedRect(x0, y0, x1, y1, board.OutlineCornerRadius.ToMM(), 8), true
}

func roundedRect(x0, y0, x1, y1, r float64, n int) []vec2 {
	w, h := x1-x0, y1-y0
	if r < 0 {
		r = 0
	}
	if r > w/2 {
		r = w / 2
	}
	if r > h/2 {
		r = h / 2
	}
	if n < 1 {
		n = 1
	}
	if r < 0.02 {
		return []vec2{{x0, y0}, {x1, y0}, {x1, y1}, {x0, y1}}
	}
	var p []vec2
	// CCW from the bottom edge. Each corner is a quarter circle, angle increasing.
	arc := func(cx, cy, a0, a1 float64) {
		for i := 0; i <= n; i++ {
			a := a0 + (a1-a0)*float64(i)/float64(n)
			p = append(p, vec2{cx + r*math.Cos(a), cy + r*math.Sin(a)})
		}
	}
	arc(x1-r, y0+r, -math.Pi/2, 0)        // BR
	arc(x1-r, y1-r, 0, math.Pi/2)         // TR
	arc(x0+r, y1-r, math.Pi/2, math.Pi)   // TL
	arc(x0+r, y0+r, math.Pi, 3*math.Pi/2) // BL
	return p
}

func cutoutPolys(board *core.Board) [][]vec2 {
	var out [][]vec2
	for _, c := range board.Cutouts {
		if len(c.Polygon) < 3 {
			continue
		}
		p := make([]vec2, len(c.Polygon))
		for i, v := range c.Polygon {
			p[i] = vec2{v.X.ToMM(), v.Y.ToMM()}
		}
		out = append(out, p)
	}
	return out
}

func ensureCCW(p []vec2) []vec2 {
	if signedArea(p) < 0 {
		for i, j := 0, len(p)-1; i < j; i, j = i+1, j-1 {
			p[i], p[j] = p[j], p[i]
		}
	}
	return p
}

func signedArea(p []vec2) float64 {
	var a float64
	for i := 0; i < len(p); i++ {
		j := (i + 1) % len(p)
		a += p[i].x*p[j].y - p[j].x*p[i].y
	}
	return a / 2
}

func insetConvex(poly []vec2, d float64) []vec2 {
	if d <= 0 || len(poly) < 3 {
		return poly
	}
	n := len(poly)
	out := make([]vec2, n)
	for i := 0; i < n; i++ {
		prev := poly[(i-1+n)%n]
		cur := poly[i]
		next := poly[(i+1)%n]
		e0x, e0y := cur.x-prev.x, cur.y-prev.y
		e1x, e1y := next.x-cur.x, next.y-cur.y
		l0 := math.Hypot(e0x, e0y)
		l1 := math.Hypot(e1x, e1y)
		if l0 < 1e-8 || l1 < 1e-8 {
			out[i] = cur
			continue
		}
		e0x, e0y = e0x/l0, e0y/l0
		e1x, e1y = e1x/l1, e1y/l1
		// Left normals: inward for a CCW ring.
		n0x, n0y := -e0y, e0x
		n1x, n1y := -e1y, e1x
		bx, by := n0x+n1x, n0y+n1y
		bl := math.Hypot(bx, by)
		if bl < 1e-6 {
			out[i] = vec2{cur.x + n0x*d, cur.y + n0y*d}
			continue
		}
		bx, by = bx/bl, by/bl
		denom := bx*n0x + by*n0y
		if denom < 0.25 {
			denom = 0.25
		}
		out[i] = vec2{cur.x + bx*d/denom, cur.y + by*d/denom}
	}
	return out
}

func pointInQuad(p vec2, q [4]vec2) bool {
	sign := 0.0
	for i := 0; i < 4; i++ {
		j := (i + 1) % 4
		cross := (q[j].x-q[i].x)*(p.y-q[i].y) - (q[j].y-q[i].y)*(p.x-q[i].x)
		if cross == 0 {
			continue
		}
		if sign == 0 {
			sign = cross
			continue
		}
		if sign*cross < 0 {
			return false
		}
	}
	return true
}

func quadCenter(q [4]vec2) vec2 {
	return vec2{(q[0].x + q[1].x + q[2].x + q[3].x) / 4, (q[0].y + q[1].y + q[2].y + q[3].y) / 4}
}

func quadSpan(q [4]vec2) float64 {
	minX, minY := q[0].x, q[0].y
	maxX, maxY := q[0].x, q[0].y
	for _, p := range q {
		minX, minY = math.Min(minX, p.x), math.Min(minY, p.y)
		maxX, maxY = math.Max(maxX, p.x), math.Max(maxY, p.y)
	}
	return math.Min(maxX-minX, maxY-minY)
}

func insetQuad(q [4]vec2, t float64) [4]vec2 {
	c := quadCenter(q)
	var o [4]vec2
	for i, p := range q {
		o[i] = vec2{p.x + (c.x-p.x)*t, p.y + (c.y-p.y)*t}
	}
	return o
}

func brighter(c fcol) fcol {
	return fcol{
		r: math.Min(1, c.r*1.35+0.05),
		g: math.Min(1, c.g*1.35+0.05),
		b: math.Min(1, c.b*1.35+0.05),
	}
}
