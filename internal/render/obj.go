package render

import (
	"bufio"
	"bytes"
	"fmt"
	"math"
	"strconv"
	"strings"
)

type objMat struct {
	col   fcol
	metal float64
	d     float64
	dSet  bool
}

type objFace struct {
	idx []int
	mat string
}

// parseOBJ reads a Wavefront OBJ, including the EasyEDA variant that inlines
// `newmtl` / `Kd` / `d` / `usemtl` in the same file and writes faces as
// `f i// j// k//`. Coordinates are millimetres. A material whose every `d`
// is 0 is treated as opaque: EasyEDA writes `d 0` for solid bodies.
func parseOBJ(data []byte) (*cadMesh, error) {
	mats := map[string]*objMat{}
	curMat := ""
	var verts []vec3
	var faces []objFace
	var cur *objMat

	sc := bufio.NewScanner(bytes.NewReader(data))
	sc.Buffer(make([]byte, 64*1024), 4*1024*1024)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		f := strings.Fields(line)
		if len(f) == 0 {
			continue
		}
		switch f[0] {
		case "v":
			if len(f) < 4 {
				continue
			}
			x, err1 := strconv.ParseFloat(f[1], 64)
			y, err2 := strconv.ParseFloat(f[2], 64)
			z, err3 := strconv.ParseFloat(f[3], 64)
			if err1 != nil || err2 != nil || err3 != nil {
				continue
			}
			verts = append(verts, vec3{x, y, z})
		case "f":
			var idx []int
			for _, tok := range f[1:] {
				n, ok := objIndex(tok, len(verts))
				if !ok {
					idx = nil
					break
				}
				idx = append(idx, n)
			}
			if len(idx) >= 3 {
				faces = append(faces, objFace{idx: idx, mat: curMat})
			}
		case "usemtl":
			if len(f) >= 2 {
				curMat = f[1]
			}
		case "newmtl":
			if len(f) >= 2 {
				cur = &objMat{col: srgb8(160, 160, 160), metal: 0.15, d: 1}
				mats[f[1]] = cur
			}
		case "Kd":
			if cur != nil && len(f) >= 4 {
				r, e1 := strconv.ParseFloat(f[1], 64)
				g, e2 := strconv.ParseFloat(f[2], 64)
				b, e3 := strconv.ParseFloat(f[3], 64)
				if e1 == nil && e2 == nil && e3 == nil {
					cur.col = fcol{lin(clamp01(r)), lin(clamp01(g)), lin(clamp01(b))}
				}
			}
		case "Ks":
			if cur != nil && len(f) >= 4 {
				r, e1 := strconv.ParseFloat(f[1], 64)
				g, e2 := strconv.ParseFloat(f[2], 64)
				b, e3 := strconv.ParseFloat(f[3], 64)
				if e1 == nil && e2 == nil && e3 == nil {
					cur.metal = clamp01((r + g + b) / 3)
				}
			}
		case "d":
			if cur != nil && len(f) >= 2 {
				if d, err := strconv.ParseFloat(f[1], 64); err == nil {
					cur.d = d
					cur.dSet = true
				}
			}
		}
	}
	if err := sc.Err(); err != nil {
		return nil, err
	}
	anySolid := false
	for _, m := range mats {
		if m.dSet && m.d > 0.05 {
			anySolid = true
		}
	}
	mesh := &cadMesh{}
	fallback := objMat{col: srgb8(160, 160, 160), metal: 0.15, d: 1}
	for _, face := range faces {
		m := fallback
		if mm, ok := mats[face.mat]; ok && mm != nil {
			m = *mm
			if anySolid && m.dSet && m.d <= 0.05 {
				continue
			}
		}
		p0 := verts[face.idx[0]]
		for k := 1; k+1 < len(face.idx); k++ {
			mesh.tris = append(mesh.tris, cadTri{
				a: p0, b: verts[face.idx[k]], c: verts[face.idx[k+1]],
				col: m.col, metal: m.metal,
			})
		}
	}
	if len(mesh.tris) == 0 {
		return nil, fmt.Errorf("obj: no faces")
	}
	return mesh, nil
}

func objIndex(tok string, nVert int) (int, bool) {
	tok = strings.Split(tok, "/")[0]
	if tok == "" {
		return 0, false
	}
	n, err := strconv.Atoi(tok)
	if err != nil || n == 0 {
		return 0, false
	}
	if n < 0 {
		n = nVert + n
	} else {
		n = n - 1
	}
	if n < 0 || n >= nVert {
		return 0, false
	}
	return n, true
}

// rotateEuler applies rx, ry, rz degrees in that order (extrinsic X then Y then Z).
func (m *cadMesh) rotateEuler(rx, ry, rz float64) {
	if m == nil || (rx == 0 && ry == 0 && rz == 0) {
		return
	}
	ax, ay, az := rx*math.Pi/180, ry*math.Pi/180, rz*math.Pi/180
	for i := range m.tris {
		m.tris[i].a = eulerDeg(m.tris[i].a, ax, ay, az)
		m.tris[i].b = eulerDeg(m.tris[i].b, ax, ay, az)
		m.tris[i].c = eulerDeg(m.tris[i].c, ax, ay, az)
	}
}

func eulerDeg(p vec3, ax, ay, az float64) vec3 {
	if ax != 0 {
		c, s := math.Cos(ax), math.Sin(ax)
		p.y, p.z = p.y*c-p.z*s, p.y*s+p.z*c
	}
	if ay != 0 {
		c, s := math.Cos(ay), math.Sin(ay)
		p.x, p.z = p.x*c+p.z*s, -p.x*s+p.z*c
	}
	if az != 0 {
		c, s := math.Cos(az), math.Sin(az)
		p.x, p.y = p.x*c-p.y*s, p.x*s+p.y*c
	}
	return p
}
