package render

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// KiCad loads VRML in units of 0.1 inch. StepUp exports store coordinates in
// those units and have no Transform. Files that store millimetres wrap the
// geometry in `Transform { scale 0.3937007874 ... }` (1/2.54). Applying every
// VRML transform, then multiplying by 2.54, yields millimetres for both.
const vrmlToMM = 2.54

type tokKind int

const (
	tIdent tokKind = iota
	tNum
	tStr
	tLBrace
	tRBrace
	tLBrack
	tRBrack
)

type tok struct {
	k tokKind
	s string
	n float64
}

type wrlMat struct {
	col   fcol
	metal float64
	trans float64
}

// xform maps a VRML-unit point. Nil is the identity. The 2.54 scale to
// millimetres is applied once, when a vertex is emitted.
type xform func(vec3) vec3

type wrlParser struct {
	toks []tok
	i    int
	mats map[string]wrlMat
	mesh *cadMesh
}

// parseWRL reads a VRML 2.0 subset: Transform, Group, Shape, IndexedFaceSet,
// Coordinate, and Material DEF/USE. Faces come back in millimetres, colored
// by the material that drew them.
func parseWRL(data []byte) (*cadMesh, error) {
	toks, err := tokenizeWRL(data)
	if err != nil {
		return nil, err
	}
	p := &wrlParser{toks: toks, mats: map[string]wrlMat{}, mesh: &cadMesh{}}
	for p.i < len(p.toks) {
		if err := p.parseNode(nil); err != nil {
			return nil, err
		}
	}
	if len(p.mesh.tris) == 0 {
		return nil, fmt.Errorf("wrl: no faces")
	}
	return p.mesh, nil
}

func tokenizeWRL(b []byte) ([]tok, error) {
	if len(b) >= 3 && b[0] == 0xEF && b[1] == 0xBB && b[2] == 0xBF {
		b = b[3:]
	}
	var out []tok
	i := 0
	for i < len(b) {
		c := b[i]
		if c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == ',' {
			i++
			continue
		}
		if c == '#' {
			for i < len(b) && b[i] != '\n' {
				i++
			}
			continue
		}
		switch c {
		case '{':
			out = append(out, tok{k: tLBrace})
			i++
			continue
		case '}':
			out = append(out, tok{k: tRBrace})
			i++
			continue
		case '[':
			out = append(out, tok{k: tLBrack})
			i++
			continue
		case ']':
			out = append(out, tok{k: tRBrack})
			i++
			continue
		case '"':
			j := i + 1
			var sb strings.Builder
			for j < len(b) && b[j] != '"' {
				if b[j] == '\\' && j+1 < len(b) {
					sb.WriteByte(b[j+1])
					j += 2
					continue
				}
				sb.WriteByte(b[j])
				j++
			}
			if j >= len(b) {
				return nil, fmt.Errorf("wrl: unterminated string")
			}
			out = append(out, tok{k: tStr, s: sb.String()})
			i = j + 1
			continue
		}
		if isNumStart(b, i) {
			j := scanNumber(b, i)
			n, err := strconv.ParseFloat(string(b[i:j]), 64)
			if err != nil {
				return nil, fmt.Errorf("wrl: bad number %q", string(b[i:j]))
			}
			out = append(out, tok{k: tNum, n: n})
			i = j
			continue
		}
		if isIdentStart(c) {
			j := i + 1
			for j < len(b) && isIdentCont(b[j]) {
				j++
			}
			out = append(out, tok{k: tIdent, s: string(b[i:j])})
			i = j
			continue
		}
		return nil, fmt.Errorf("wrl: unexpected byte %#x at %d", c, i)
	}
	return out, nil
}

func isIdentStart(c byte) bool {
	return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_'
}

func isIdentCont(c byte) bool {
	return isIdentStart(c) || (c >= '0' && c <= '9') || c == '-' || c == ':'
}

func isNumStart(b []byte, i int) bool {
	c := b[i]
	if c == '+' || c == '-' {
		if i+1 >= len(b) {
			return false
		}
		n := b[i+1]
		return (n >= '0' && n <= '9') || n == '.'
	}
	return (c >= '0' && c <= '9') || c == '.'
}

func scanNumber(b []byte, i int) int {
	j := i
	if b[j] == '+' || b[j] == '-' {
		j++
	}
	for j < len(b) && b[j] >= '0' && b[j] <= '9' {
		j++
	}
	if j < len(b) && b[j] == '.' {
		j++
		for j < len(b) && b[j] >= '0' && b[j] <= '9' {
			j++
		}
	}
	if j < len(b) && (b[j] == 'e' || b[j] == 'E') {
		k := j + 1
		if k < len(b) && (b[k] == '+' || b[k] == '-') {
			k++
		}
		if k < len(b) && b[k] >= '0' && b[k] <= '9' {
			j = k
			for j < len(b) && b[j] >= '0' && b[j] <= '9' {
				j++
			}
		}
	}
	return j
}

func (p *wrlParser) parseNode(xf xform) error {
	if p.i >= len(p.toks) {
		return nil
	}
	if p.peekIdent("DEF") {
		p.i++
		if _, ok := p.takeIdent(); !ok {
			return fmt.Errorf("wrl: DEF without a name")
		}
	}
	if p.peekIdent("USE") {
		p.i += 2
		return nil
	}
	typ, ok := p.takeIdent()
	if !ok {
		p.i++
		return nil
	}
	if p.i >= len(p.toks) || p.toks[p.i].k != tLBrace {
		return nil
	}
	p.i++ // consume '{'
	switch typ {
	case "Transform":
		return p.parseTransform(xf)
	case "Group", "Switch", "Collision", "Anchor":
		return p.parseChildren(xf, identXform)
	case "Shape":
		p.parseShape(xf)
		return nil
	default:
		p.skipBlock()
		return nil
	}
}

func identXform(p vec3) vec3 { return p }

func (p *wrlParser) parseTransform(parent xform) error {
	end := p.blockEnd(p.i)
	tr := vec3{}
	center := vec3{}
	scale := vec3{1, 1, 1}
	axis := vec3{0, 0, 1}
	var ang float64
	soAxis := vec3{0, 0, 1}
	var soAng float64
	var children [][2]int
	i := p.i
	for i < end {
		if p.toks[i].k != tIdent {
			i++
			continue
		}
		name := p.toks[i].s
		i++
		switch name {
		case "translation":
			tr, i = p.read3(i, end)
		case "center":
			center, i = p.read3(i, end)
		case "scale":
			scale, i = p.read3(i, end)
			if scale.x == 0 && scale.y == 0 && scale.z == 0 {
				scale = vec3{1, 1, 1}
			}
		case "rotation":
			axis, ang, i = p.readRot(i, end)
		case "scaleOrientation":
			soAxis, soAng, i = p.readRot(i, end)
		case "children":
			var spans [][2]int
			spans, i = p.childSpans(i, end)
			children = append(children, spans...)
		default:
			i = p.skipVal(i, end)
		}
	}
	xf := composeTransform(parent, tr, center, axis, ang, scale, soAxis, soAng)
	for _, sp := range children {
		sub := &wrlParser{toks: p.toks[sp[0]:sp[1]], mats: p.mats, mesh: p.mesh}
		for sub.i < len(sub.toks) {
			if err := sub.parseNode(xf); err != nil {
				return err
			}
		}
	}
	p.i = end + 1
	return nil
}

func composeTransform(parent xform, tr, center, axis vec3, ang float64, scale, soAxis vec3, soAng float64) xform {
	return func(v vec3) vec3 {
		v = v.sub(center)
		if soAng != 0 {
			v = rotAxis(v, soAxis, -soAng)
		}
		v = vec3{v.x * scale.x, v.y * scale.y, v.z * scale.z}
		if soAng != 0 {
			v = rotAxis(v, soAxis, soAng)
		}
		if ang != 0 {
			v = rotAxis(v, axis, ang)
		}
		v = v.add(center).add(tr)
		if parent != nil {
			v = parent(v)
		}
		return v
	}
}

func rotAxis(v, axis vec3, ang float64) vec3 {
	a := axis.norm()
	if a.len2() < 1e-18 {
		return v
	}
	c, s := math.Cos(ang), math.Sin(ang)
	return v.scale(c).add(a.cross(v).scale(s)).add(a.scale(a.dot(v) * (1 - c)))
}

// parseChildren reads a node body whose only interesting field is children.
// The opening brace is already consumed.
func (p *wrlParser) parseChildren(parent xform, extra xform) error {
	end := p.blockEnd(p.i)
	xf := parent
	if extra != nil {
		xf = func(v vec3) vec3 {
			v = extra(v)
			if parent != nil {
				return parent(v)
			}
			return v
		}
	}
	i := p.i
	for i < end {
		if p.toks[i].k != tIdent {
			i++
			continue
		}
		name := p.toks[i].s
		i++
		if name == "children" {
			spans, ni := p.childSpans(i, end)
			for _, sp := range spans {
				sub := &wrlParser{toks: p.toks[sp[0]:sp[1]], mats: p.mats, mesh: p.mesh}
				for sub.i < len(sub.toks) {
					if err := sub.parseNode(xf); err != nil {
						return err
					}
				}
			}
			i = ni
			continue
		}
		i = p.skipVal(i, end)
	}
	p.i = end + 1
	return nil
}

func (p *wrlParser) childSpans(i, end int) ([][2]int, int) {
	if i >= end {
		return nil, i
	}
	var out [][2]int
	if p.toks[i].k == tLBrack {
		i++
		for i < end && p.toks[i].k != tRBrack {
			a, b := p.nodeSpan(i, end)
			if b <= a {
				break
			}
			out = append(out, [2]int{a, b})
			i = b
		}
		if i < end && p.toks[i].k == tRBrack {
			i++
		}
		return out, i
	}
	a, b := p.nodeSpan(i, end)
	if b > a {
		out = append(out, [2]int{a, b})
	}
	return out, b
}

func (p *wrlParser) nodeSpan(i, end int) (int, int) {
	start := i
	if i < end && p.toks[i].k == tIdent && p.toks[i].s == "DEF" {
		i += 2
	}
	if i < end && p.toks[i].k == tIdent && p.toks[i].s == "USE" {
		if i+2 <= end {
			return start, i + 2
		}
		return start, end
	}
	if i < end && p.toks[i].k == tIdent {
		i++
	}
	if i < end && p.toks[i].k == tLBrace {
		closeAt := p.matchBrace(i + 1)
		if closeAt < len(p.toks) {
			return start, closeAt + 1
		}
		return start, len(p.toks)
	}
	return start, i
}

func (p *wrlParser) parseShape(xf xform) {
	var idx []int
	var pts []vec3
	mat := wrlMat{col: srgb8(150, 150, 150), metal: 0.15}
	has := false
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrace {
		name, ok := p.takeIdent()
		if !ok {
			p.i++
			continue
		}
		switch name {
		case "geometry":
			idx, pts, has = p.parseGeometry()
		case "appearance":
			if m, ok := p.parseAppearance(); ok {
				mat = m
			}
		default:
			p.skipValue()
		}
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrace {
		p.i++
	}
	if has {
		p.emit(idx, pts, mat, xf)
	}
}

func (p *wrlParser) parseGeometry() ([]int, []vec3, bool) {
	p.skipDEF()
	typ, ok := p.takeIdent()
	if !ok || p.i >= len(p.toks) || p.toks[p.i].k != tLBrace {
		p.skipValue()
		return nil, nil, false
	}
	p.i++
	if typ != "IndexedFaceSet" {
		p.skipBlock()
		return nil, nil, false
	}
	var idx []int
	var pts []vec3
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrace {
		name, ok := p.takeIdent()
		if !ok {
			p.i++
			continue
		}
		switch name {
		case "coordIndex":
			idx = p.readNumList()
		case "coord":
			pts = p.parseCoord()
		default:
			p.skipValue()
		}
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrace {
		p.i++
	}
	return idx, pts, len(pts) > 0 && len(idx) > 0
}

func (p *wrlParser) parseCoord() []vec3 {
	p.skipDEF()
	typ, ok := p.takeIdent()
	if !ok || p.i >= len(p.toks) || p.toks[p.i].k != tLBrace {
		p.skipValue()
		return nil
	}
	p.i++
	if typ != "Coordinate" {
		p.skipBlock()
		return nil
	}
	var pts []vec3
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrace {
		name, ok := p.takeIdent()
		if !ok {
			p.i++
			continue
		}
		if name == "point" {
			nums := p.readFloats()
			for i := 0; i+2 < len(nums); i += 3 {
				pts = append(pts, vec3{nums[i], nums[i+1], nums[i+2]})
			}
			continue
		}
		p.skipValue()
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrace {
		p.i++
	}
	return pts
}

func (p *wrlParser) parseAppearance() (wrlMat, bool) {
	p.skipDEF()
	typ, ok := p.takeIdent()
	if !ok || p.i >= len(p.toks) || p.toks[p.i].k != tLBrace {
		p.skipValue()
		return wrlMat{}, false
	}
	p.i++
	if typ != "Appearance" {
		p.skipBlock()
		return wrlMat{}, false
	}
	var mat wrlMat
	found := false
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrace {
		name, ok := p.takeIdent()
		if !ok {
			p.i++
			continue
		}
		if name == "material" {
			if m, ok := p.parseMaterial(); ok {
				mat = m
				found = true
			}
			continue
		}
		p.skipValue()
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrace {
		p.i++
	}
	return mat, found
}

func (p *wrlParser) parseMaterial() (wrlMat, bool) {
	def := ""
	if p.peekIdent("DEF") {
		p.i++
		def, _ = p.takeIdent()
	}
	if p.peekIdent("USE") {
		p.i++
		name, _ := p.takeIdent()
		if m, ok := p.mats[name]; ok {
			return m, true
		}
		return wrlMat{col: srgb8(150, 150, 150), metal: 0.15}, true
	}
	typ, ok := p.takeIdent()
	if !ok || p.i >= len(p.toks) || p.toks[p.i].k != tLBrace {
		p.skipValue()
		return wrlMat{}, false
	}
	p.i++
	if typ != "Material" {
		p.skipBlock()
		return wrlMat{}, false
	}
	diff := vec3{0.6, 0.6, 0.6}
	var shiny, trans float64
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrace {
		name, ok := p.takeIdent()
		if !ok {
			p.i++
			continue
		}
		switch name {
		case "diffuseColor":
			if v, ni, ok := p.take3(p.i); ok {
				diff = v
				p.i = ni
			}
		case "shininess":
			if n, ok := p.takeNum(); ok {
				shiny = n
			}
		case "transparency":
			if n, ok := p.takeNum(); ok {
				trans = n
			}
		default:
			p.skipValue()
		}
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrace {
		p.i++
	}
	m := wrlMat{
		col:   fcol{lin(clamp01(diff.x)), lin(clamp01(diff.y)), lin(clamp01(diff.z))},
		metal: clamp01(shiny),
		trans: trans,
	}
	if def != "" {
		p.mats[def] = m
	}
	return m, true
}

func (p *wrlParser) emit(idx []int, pts []vec3, mat wrlMat, xf xform) {
	if mat.trans > 0.98 {
		return
	}
	vert := func(id int) (vec3, bool) {
		if id < 0 || id >= len(pts) {
			return vec3{}, false
		}
		v := pts[id]
		if xf != nil {
			v = xf(v)
		}
		return v.scale(vrmlToMM), true
	}
	var poly []int
	flush := func() {
		if len(poly) >= 3 {
			p0, ok0 := vert(poly[0])
			if ok0 {
				for k := 1; k+1 < len(poly); k++ {
					a, oka := vert(poly[k])
					b, okb := vert(poly[k+1])
					if oka && okb {
						p.mesh.tris = append(p.mesh.tris, cadTri{a: p0, b: a, c: b, col: mat.col, metal: mat.metal})
					}
				}
			}
		}
		poly = poly[:0]
	}
	for _, id := range idx {
		if id < 0 {
			flush()
			continue
		}
		poly = append(poly, id)
	}
	flush()
}

func (p *wrlParser) skipDEF() {
	if p.peekIdent("DEF") {
		p.i += 2
	}
}

func (p *wrlParser) peekIdent(s string) bool {
	return p.i < len(p.toks) && p.toks[p.i].k == tIdent && p.toks[p.i].s == s
}

func (p *wrlParser) takeIdent() (string, bool) {
	if p.i >= len(p.toks) || p.toks[p.i].k != tIdent {
		return "", false
	}
	s := p.toks[p.i].s
	p.i++
	return s, true
}

func (p *wrlParser) takeNum() (float64, bool) {
	if p.i >= len(p.toks) || p.toks[p.i].k != tNum {
		return 0, false
	}
	n := p.toks[p.i].n
	p.i++
	return n, true
}

func (p *wrlParser) take3(i int) (vec3, int, bool) {
	if i+2 >= len(p.toks) {
		return vec3{}, i, false
	}
	if p.toks[i].k != tNum || p.toks[i+1].k != tNum || p.toks[i+2].k != tNum {
		return vec3{}, i, false
	}
	return vec3{p.toks[i].n, p.toks[i+1].n, p.toks[i+2].n}, i + 3, true
}

func (p *wrlParser) read3(i, end int) (vec3, int) {
	v, ni, ok := p.take3(i)
	if !ok {
		return vec3{}, p.skipVal(i, end)
	}
	return v, ni
}

func (p *wrlParser) readRot(i, end int) (vec3, float64, int) {
	if i+3 >= end || i+3 >= len(p.toks) {
		return vec3{0, 0, 1}, 0, p.skipVal(i, end)
	}
	for k := 0; k < 4; k++ {
		if p.toks[i+k].k != tNum {
			return vec3{0, 0, 1}, 0, p.skipVal(i, end)
		}
	}
	return vec3{p.toks[i].n, p.toks[i+1].n, p.toks[i+2].n}, p.toks[i+3].n, i + 4
}

func (p *wrlParser) readFloats() []float64 {
	if p.i >= len(p.toks) || p.toks[p.i].k != tLBrack {
		p.skipValue()
		return nil
	}
	p.i++
	var out []float64
	for p.i < len(p.toks) && p.toks[p.i].k != tRBrack {
		if p.toks[p.i].k == tNum {
			out = append(out, p.toks[p.i].n)
		}
		p.i++
	}
	if p.i < len(p.toks) && p.toks[p.i].k == tRBrack {
		p.i++
	}
	return out
}

func (p *wrlParser) readNumList() []int {
	fs := p.readFloats()
	out := make([]int, len(fs))
	for i, n := range fs {
		out[i] = int(math.Round(n))
	}
	return out
}

func (p *wrlParser) skipValue() {
	p.i = p.skipVal(p.i, len(p.toks))
}

func (p *wrlParser) skipVal(i, end int) int {
	if i >= end || i >= len(p.toks) {
		return i
	}
	t := p.toks[i]
	switch t.k {
	case tLBrack:
		return p.matchBrack(i+1) + 1
	case tLBrace:
		return p.matchBrace(i+1) + 1
	case tIdent:
		if t.s == "DEF" && i+2 < len(p.toks) {
			return p.skipVal(i+2, end)
		}
		if t.s == "USE" {
			if i+2 <= len(p.toks) {
				return i + 2
			}
			return len(p.toks)
		}
		if i+1 < len(p.toks) && p.toks[i+1].k == tLBrace {
			return p.matchBrace(i+2) + 1
		}
		return i + 1
	case tNum, tStr:
		j := i
		for j < end && j < len(p.toks) && (p.toks[j].k == tNum || p.toks[j].k == tStr) {
			j++
		}
		return j
	default:
		return i + 1
	}
}

func (p *wrlParser) skipBlock() {
	p.i = p.matchBrace(p.i) + 1
}

// blockEnd is the index of the closing brace for a body whose opening brace
// was already consumed. i is the first token inside the body.
func (p *wrlParser) blockEnd(i int) int {
	return p.matchBrace(i)
}

func (p *wrlParser) matchBrace(i int) int {
	depth := 1
	for i < len(p.toks) && depth > 0 {
		switch p.toks[i].k {
		case tLBrace:
			depth++
		case tRBrace:
			depth--
			if depth == 0 {
				return i
			}
		}
		i++
	}
	return len(p.toks)
}

func (p *wrlParser) matchBrack(i int) int {
	depth := 1
	for i < len(p.toks) && depth > 0 {
		switch p.toks[i].k {
		case tLBrack:
			depth++
		case tRBrack:
			depth--
			if depth == 0 {
				return i
			}
		}
		i++
	}
	return len(p.toks)
}
