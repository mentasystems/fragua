package render

import (
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/mentasystems/fragua/internal/core"
)

func TestParseWRLScaleAndMaterial(t *testing.T) {
	// Authored fixture, not KiCad library data. Coordinates are in KiCad's
	// 0.1-inch VRML unit: 0.5/2.54 → 0.5 mm after the loader's ×2.54.
	const half = 0.5 / vrmlToMM
	src := `#VRML V2.0 utf8
# fragua test fixture — not redistributed KiCad library data
Shape {
  appearance Appearance { material DEF RED Material { diffuseColor 1 0 0 shininess 0.4 } }
}
Shape {
  geometry IndexedFaceSet {
    coordIndex [0, 1, 2, 3, -1]
    coord Coordinate { point [
      ` + f(half) + ` ` + f(-half) + ` 0,
      ` + f(half) + ` ` + f(half) + ` 0,
      ` + f(-half) + ` ` + f(half) + ` 0,
      ` + f(-half) + ` ` + f(-half) + ` ` + f(1/vrmlToMM) + `
    ] }
  }
  appearance Appearance { material USE RED }
}
`
	mesh, err := parseWRL([]byte(src))
	if err != nil {
		t.Fatal(err)
	}
	if len(mesh.tris) != 2 {
		t.Fatalf("quad should fan to 2 triangles, got %d", len(mesh.tris))
	}
	if mesh.tris[0].col.r < 0.99 || mesh.tris[0].col.g > 0.01 || mesh.tris[0].col.b > 0.01 {
		t.Fatalf("DEF/USE diffuseColor not applied: %+v", mesh.tris[0].col)
	}
	min, max := mesh.bounds()
	near(t, "x extent", max.x-min.x, 1, 1e-6)
	near(t, "y extent", max.y-min.y, 1, 1e-6)
	near(t, "z extent", max.z-min.z, 1, 1e-6)
}

func TestParseWRLTransformScaleIsMillimetres(t *testing.T) {
	// Coordinates are already millimetres, wrapped in the 1/2.54 scale KiCad
	// applies before the historical ×2.54. Net size stays 1 mm.
	const sc = 1 / vrmlToMM
	src := `#VRML V2.0 utf8
Transform {
  scale ` + f(sc) + ` ` + f(sc) + ` ` + f(sc) + `
  children [
    Shape {
      geometry IndexedFaceSet {
        coordIndex [0, 1, 2, -1]
        coord Coordinate { point [0 0 0, 1 0 0, 0 1 0] }
      }
      appearance Appearance { material Material { diffuseColor 0 1 0 } }
    }
  ]
}
`
	mesh, err := parseWRL([]byte(src))
	if err != nil {
		t.Fatal(err)
	}
	if len(mesh.tris) != 1 {
		t.Fatalf("tris %d", len(mesh.tris))
	}
	min, max := mesh.bounds()
	near(t, "x", max.x-min.x, 1, 1e-6)
	near(t, "y", max.y-min.y, 1, 1e-6)
}

func TestParseOBJEasyEDADissolveZeroIsOpaque(t *testing.T) {
	src := `
v 0 0 0
v 1 0 0
v 0 2 0
newmtl body
Kd 0 0 1
d 0.0
endmtl
usemtl body
f 1// 2// 3//
`
	mesh, err := parseOBJ([]byte(src))
	if err != nil {
		t.Fatal(err)
	}
	if len(mesh.tris) != 1 {
		t.Fatalf("tris %d", len(mesh.tris))
	}
	if mesh.tris[0].col.b < 0.99 {
		t.Fatalf("Kd blue not applied: %+v", mesh.tris[0].col)
	}
	min, max := mesh.bounds()
	near(t, "length", max.y-min.y, 2, 1e-9)
}

func TestKiCadModelMapping(t *testing.T) {
	r := &core.Footprint{
		Key: "r_0603",
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(core.FromMM(-0.8), 0)},
			{Number: "2", Offset: core.NewPoint(core.FromMM(0.8), 0)},
		},
	}
	pl, ok := kicadPlace(r)
	if !ok {
		t.Fatal("r_0603 should map")
	}
	if pl.Path != "Resistor_SMD.3dshapes/R_0603_1608Metric.wrl" {
		t.Fatalf("path %s", pl.Path)
	}
	if math.Abs(pl.RotZ) > 1e-6 || pl.AnchorX != 0 || pl.AnchorY != 0 {
		t.Fatalf("chip place %+v", pl)
	}

	h := &core.Footprint{
		Key: "header_1x03_2.54mm",
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(core.FromMM(-2.54), 0)},
			{Number: "2", Offset: core.NewPoint(0, 0)},
			{Number: "3", Offset: core.NewPoint(core.FromMM(2.54), 0)},
		},
	}
	pl, ok = kicadPlace(h)
	if !ok {
		t.Fatal("header should map")
	}
	if pl.Path != "Connector_PinHeader_2.54mm.3dshapes/PinHeader_1x03_P2.54mm_Vertical.wrl" {
		t.Fatalf("header path %s", pl.Path)
	}
	near(t, "header anchor x", pl.AnchorX, -2.54, 1e-9)
	near(t, "header rot", pl.RotZ, 90, 1e-6)
	qfn := &core.Footprint{Key: "rp2040_qfn56"}
	pl, ok = kicadPlace(qfn)
	if !ok || !pl.MirrorY {
		t.Fatalf("qfn place %+v ok=%v", pl, ok)
	}

	if _, ok := kicadPlace(&core.Footprint{Key: "usbc_16p"}); ok {
		t.Fatal("usbc_16p has no WRL in kicad-packages3D; it must fall back")
	}

	over := *r
	over.Model = "Package_SO.3dshapes/SOIC-8_3.9x4.9mm_P1.27mm.wrl"
	pl, ok = kicadPlace(&over)
	if !ok || pl.Path != over.Model {
		t.Fatalf("override %+v ok=%v", pl, ok)
	}
}

func TestModelsOfflineFallsBackToBox(t *testing.T) {
	board := fixtureBoard()
	var rep []ModelUse
	opt := Shot3D{
		Width: 80, Height: 48, Samples: 1,
		Models: "kicad", Offline: true, CacheDir: t.TempDir(),
		SearchDirs: []string{}, Report: &rep,
	}
	pngBytes := mustRender(t, board, opt)
	boxes := mustRender(t, board, Shot3D{Width: 80, Height: 48, Samples: 1})
	if string(pngBytes) != string(boxes) {
		t.Fatal("offline miss should render the same boxes as models disabled")
	}
	if len(rep) == 0 {
		t.Fatal("expected a per-footprint report")
	}
	for _, u := range rep {
		if u.Source != "box" {
			t.Fatalf("%s source %s, want box (%s)", u.Ref, u.Source, u.Reason)
		}
		if !strings.Contains(u.Reason, "offline") && !strings.Contains(u.Reason, "no KiCad") {
			t.Fatalf("%s reason %q", u.Ref, u.Reason)
		}
	}
}

func TestCachedWRLReplacesBox(t *testing.T) {
	// Bright magenta cube, authored here. Large enough to show up at 160px.
	const half = 3.0 / vrmlToMM
	wrl := `#VRML V2.0 utf8
# fragua test fixture — not redistributed KiCad library data
Shape {
  geometry IndexedFaceSet {
    coordIndex [0,1,2,-1, 0,2,3,-1, 4,5,6,-1, 4,6,7,-1]
    coord Coordinate { point [
      ` + f(-half) + ` ` + f(-half) + ` 0,
      ` + f(half) + ` ` + f(-half) + ` 0,
      ` + f(half) + ` ` + f(half) + ` 0,
      ` + f(-half) + ` ` + f(half) + ` 0,
      ` + f(-half) + ` ` + f(-half) + ` ` + f(half) + `,
      ` + f(half) + ` ` + f(-half) + ` ` + f(half) + `,
      ` + f(half) + ` ` + f(half) + ` ` + f(half) + `,
      ` + f(-half) + ` ` + f(half) + ` ` + f(half) + `
    ] }
  }
  appearance Appearance { material Material { diffuseColor 1 0 1 shininess 0.1 } }
}
`
	dir := t.TempDir()
	rel := "Resistor_SMD.3dshapes/R_0603_1608Metric.wrl"
	dest := filepath.Join(dir, "kicad-packages3d", filepath.FromSlash(rel))
	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(dest, []byte(wrl), 0o644); err != nil {
		t.Fatal(err)
	}
	board := core.NewBoard()
	r := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(20), core.FromMM(12)))
	board.Outline = &r
	board.AddFootprint(&core.Footprint{
		Reference: "R1", Key: "r_0603",
		Position: core.NewPoint(core.FromMM(10), core.FromMM(6)),
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(core.FromMM(-0.8), 0), Size: [2]core.Length{core.FromMM(0.8), core.FromMM(0.9)}},
			{Number: "2", Offset: core.NewPoint(core.FromMM(0.8), 0), Size: [2]core.Length{core.FromMM(0.8), core.FromMM(0.9)}},
		},
	})
	var rep []ModelUse
	pngBytes := mustRender(t, board, Shot3D{
		Width: 160, Height: 100, Samples: 1,
		Models: "kicad", Offline: true, CacheDir: dir,
		SearchDirs: []string{}, Report: &rep,
	})
	if len(rep) != 1 || rep[0].Source != "kicad" {
		t.Fatalf("report %+v", rep)
	}
	img := decodePNG(t, pngBytes)
	mag := 0
	b := img.Bounds()
	for y := b.Min.Y; y < b.Max.Y; y++ {
		for x := b.Min.X; x < b.Max.X; x++ {
			r8, g8, b8, _ := img.At(x, y).RGBA()
			rr, gg, bb := int(r8>>8), int(g8>>8), int(b8>>8)
			if rr > 140 && bb > 140 && gg < 80 {
				mag++
			}
		}
	}
	if mag < 20 {
		t.Fatalf("cached model did not draw (magenta pixels %d)", mag)
	}
	plain := mustRender(t, board, Shot3D{Width: 160, Height: 100, Samples: 1})
	if string(plain) == string(pngBytes) {
		t.Fatal("model render matches the box render")
	}
}

func TestFindEasyEDA3D(t *testing.T) {
	raw := []byte(`{"result":{"packageDetail":{"dataStr":{"head":{"uuid_3d":"should-not-win"},"shape":["{\"attrs\":{\"uuid\":\"abc123\",\"c_rotation\":\"0,0,90\",\"c_etype\":\"outline3D\"}}"]}}}}`)
	ref, err := findEasyEDA3D(raw)
	if err != nil {
		t.Fatal(err)
	}
	if ref.UUID != "abc123" || ref.Rot != "0,0,90" {
		t.Fatalf("%+v", ref)
	}
	rx, ry, rz := parseEasyRot(ref.Rot)
	if rx != 0 || ry != 0 || rz != 90 {
		t.Fatalf("rot %v %v %v", rx, ry, rz)
	}
}

func TestSafeRelRejectsEscape(t *testing.T) {
	if _, err := safeRel("../secret.wrl"); err == nil {
		t.Fatal("expected rejection")
	}
	if _, err := safeRel("/etc/passwd"); err == nil {
		t.Fatal("expected rejection of absolute path")
	}
	got, err := safeRel("${KISYS3DMOD}/Resistor_SMD.3dshapes/R_0603_1608Metric.wrl")
	if err != nil || got != "Resistor_SMD.3dshapes/R_0603_1608Metric.wrl" {
		t.Fatalf("%q %v", got, err)
	}
}

func (m *cadMesh) bounds() (min, max vec3) {
	min = vec3{math.Inf(1), math.Inf(1), math.Inf(1)}
	max = vec3{math.Inf(-1), math.Inf(-1), math.Inf(-1)}
	for _, t := range m.tris {
		for _, p := range []vec3{t.a, t.b, t.c} {
			min.x, min.y, min.z = math.Min(min.x, p.x), math.Min(min.y, p.y), math.Min(min.z, p.z)
			max.x, max.y, max.z = math.Max(max.x, p.x), math.Max(max.y, p.y), math.Max(max.z, p.z)
		}
	}
	return min, max
}

func f(v float64) string {
	return strconv.FormatFloat(v, 'f', 8, 64)
}

func near(t *testing.T, name string, got, want, eps float64) {
	t.Helper()
	if math.Abs(got-want) > eps {
		t.Fatalf("%s = %g, want %g", name, got, want)
	}
}
