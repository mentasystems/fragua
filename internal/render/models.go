package render

import (
	"fmt"
	"math"
	"os"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
)

// cadMesh is a triangulated component body in millimetres, Z up.
type cadMesh struct {
	tris []cadTri
}

type cadTri struct {
	a, b, c vec3
	col     fcol
	metal   float64
}

// ModelUse records where one footprint's body came from.
type ModelUse struct {
	Ref    string
	Key    string
	Source string // kicad, easyeda, file, box
	Path   string
	Reason string
}

// SummarizeModels is the one-line count plus a line for each box fallback.
func SummarizeModels(rep []ModelUse) string {
	var k, e, f, b int
	var boxes []string
	for _, r := range rep {
		switch r.Source {
		case "kicad":
			k++
		case "easyeda":
			e++
		case "file":
			f++
		default:
			b++
			if r.Reason == "models disabled" {
				continue
			}
			why := r.Reason
			if why == "" {
				why = "no model"
			}
			boxes = append(boxes, fmt.Sprintf("  %s %s: box (%s)", r.Ref, r.Key, why))
		}
	}
	line := fmt.Sprintf("models: %d kicad, %d easyeda, %d file, %d box", k, e, f, b)
	if len(boxes) == 0 {
		return line
	}
	return line + "\n" + strings.Join(boxes, "\n")
}

type loader struct {
	opt    Shot3D
	kicad  bool
	easy   bool
	track  bool
	parsed map[string]*cadMesh
	failed map[string]string
	report []ModelUse
	get    fetchFunc
}

func newLoader(opt Shot3D) *loader {
	k, e := modelModes(opt.Models)
	return &loader{
		opt:    opt,
		kicad:  k,
		easy:   e,
		track:  opt.Report != nil || opt.Models != "",
		parsed: map[string]*cadMesh{},
		failed: map[string]string{},
		get:    opt.fetch,
	}
}

func modelModes(s string) (kicad, easy bool) {
	s = strings.TrimSpace(strings.ToLower(s))
	if s == "" || s == "none" || s == "box" || s == "boxes" {
		return false, false
	}
	for _, p := range strings.Split(s, ",") {
		switch strings.TrimSpace(p) {
		case "kicad":
			kicad = true
		case "easyeda", "lcsc":
			easy = true
		}
	}
	return kicad, easy
}

type resolved struct {
	mesh                   *cadMesh
	anchorX, anchorY, rotZ float64
	mirrorY                bool
	use                    ModelUse
}

func (l *loader) resolve(fp *core.Footprint) (resolved, bool) {
	use := ModelUse{Ref: fp.Reference, Key: fp.Key, Source: "box"}
	if !l.kicad && !l.easy {
		use.Reason = "models disabled"
		l.note(use)
		return resolved{}, false
	}
	if m := strings.TrimSpace(fp.Model); isLocalModel(m) {
		mesh, err := l.loadFile(m)
		if err != nil {
			use.Reason = err.Error()
			l.note(use)
			return resolved{}, false
		}
		ax, ay, rz := 0.0, 0.0, 0.0
		switch pathKind(m) {
		case placeHeader:
			ax, ay, rz = headerAnchor(fp)
		case placeChip:
			rz = chipRot(fp)
		}
		use.Source = "file"
		use.Path = m
		l.note(use)
		return resolved{mesh: mesh, anchorX: ax, anchorY: ay, rotZ: rz, use: use}, true
	}
	// A specific LCSC model wins over the generic KiCad package when EasyEDA
	// is enabled. A miss falls through; a missing model never fails the render.
	if l.easy && strings.TrimSpace(fp.LcscID) != "" {
		mesh, where, err := l.readEasyEDA(fp.LcscID)
		if err == nil && mesh != nil && len(mesh.tris) > 0 {
			use.Source = "easyeda"
			use.Path = where
			l.note(use)
			rot := 0.0
			if len(fp.Pads) == 2 {
				rot = chipRot(fp)
			}
			return resolved{mesh: mesh, rotZ: rot, use: use}, true
		}
		use.Reason = "easyeda: " + errString(err)
	}
	if l.kicad {
		pl, ok := kicadPlace(fp)
		if !ok {
			if use.Reason == "" {
				use.Reason = fmt.Sprintf("no KiCad model for %q", fp.Key)
			}
			l.note(use)
			return resolved{}, false
		}
		mesh, where, err := l.loadKiCad(pl.Path)
		if err != nil {
			use.Reason = err.Error()
			l.note(use)
			return resolved{}, false
		}
		use.Source = "kicad"
		use.Path = where
		if use.Path == "" {
			use.Path = pl.Path
		}
		l.note(use)
		return resolved{mesh: mesh, anchorX: pl.AnchorX, anchorY: pl.AnchorY, rotZ: pl.RotZ, mirrorY: pl.MirrorY, use: use}, true
	}
	if use.Reason == "" {
		use.Reason = fmt.Sprintf("no model for %q", fp.Key)
	}
	l.note(use)
	return resolved{}, false
}

func errString(err error) string {
	if err == nil {
		return "unavailable"
	}
	return err.Error()
}

func (l *loader) note(u ModelUse) {
	if l.track {
		l.report = append(l.report, u)
	}
}

func (l *loader) loadFile(path string) (*cadMesh, error) {
	if m, ok := l.parsed[path]; ok {
		return m, nil
	}
	if why, ok := l.failed[path]; ok {
		return nil, fmt.Errorf("%s", why)
	}
	b, err := os.ReadFile(path)
	if err != nil {
		l.failed[path] = err.Error()
		return nil, err
	}
	m, err := parseModel(path, b)
	if err != nil {
		l.failed[path] = err.Error()
		return nil, err
	}
	l.parsed[path] = m
	return m, nil
}

func (l *loader) loadKiCad(rel string) (*cadMesh, string, error) {
	if m, ok := l.parsed[rel]; ok {
		return m, rel, nil
	}
	if why, ok := l.failed[rel]; ok {
		return nil, "", fmt.Errorf("%s", why)
	}
	b, where, err := l.readKiCad(rel)
	if err != nil {
		l.failed[rel] = err.Error()
		return nil, "", err
	}
	m, err := parseModel(rel, b)
	if err != nil {
		l.failed[rel] = err.Error()
		return nil, "", fmt.Errorf("%s: %w", rel, err)
	}
	l.parsed[rel] = m
	return m, where, nil
}

func parseModel(name string, data []byte) (*cadMesh, error) {
	ext := strings.ToLower(extOf(name))
	head := data
	if len(head) > 256 {
		head = head[:256]
	}
	h := strings.ToLower(string(head))
	if ext == ".obj" || strings.Contains(h, "newmtl") || strings.HasPrefix(strings.TrimSpace(h), "v ") {
		return parseOBJ(data)
	}
	return parseWRL(data)
}

func extOf(name string) string {
	i := strings.LastIndex(name, ".")
	if i < 0 {
		return ""
	}
	return name[i:]
}

func (s *scene) placeCad(fp *core.Footprint, mesh *cadMesh, anchorX, anchorY, rotZ float64, mirrorY bool, zBase float64) bodyRec {
	if fp.Elevated {
		zBase += 8
	}
	bottom := fp.Layer.Index != 0
	mrot := rotZ * math.Pi / 180
	mc, ms := math.Cos(mrot), math.Sin(mrot)
	frot := fp.Rotation * math.Pi / 180
	fc, fs := math.Cos(frot), math.Sin(frot)
	ox, oy := fp.Position.X.ToMM(), fp.Position.Y.ToMM()
	minX, minY := math.Inf(1), math.Inf(1)
	maxX, maxY := math.Inf(-1), math.Inf(-1)
	maxZ := zBase
	xform := func(p vec3) vec3 {
		px, py := p.x, p.y
		if mirrorY {
			py = -py
		}
		x := px*mc - py*ms + anchorX
		y := px*ms + py*mc + anchorY
		z := p.z
		if bottom {
			x = -x
			z = -z
		}
		wx := ox + x*fc - y*fs
		wy := oy + x*fs + y*fc
		wz := zBase + z
		if bottom {
			wz = z
		}
		return vec3{wx, wy, wz}
	}
	for i := range mesh.tris {
		t := &mesh.tris[i]
		a, b, c := xform(t.a), xform(t.b), xform(t.c)
		s.mesh.tri(a, b, c, t.col, t.metal, 0.50)
		for _, p := range [...]vec3{a, b, c} {
			if p.x < minX {
				minX = p.x
			}
			if p.y < minY {
				minY = p.y
			}
			if p.x > maxX {
				maxX = p.x
			}
			if p.y > maxY {
				maxY = p.y
			}
			if p.z > maxZ {
				maxZ = p.z
			}
		}
	}
	return bodyRec{
		corners: [4]vec2{{minX, minY}, {maxX, minY}, {maxX, maxY}, {minX, maxY}},
		zTop:    maxZ,
	}
}
