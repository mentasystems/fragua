package render

import (
	"bytes"
	"image"
	"image/png"
	"os"
	"path/filepath"
	"testing"

	"github.com/mentasystems/fragua/internal/core"
)

func TestBoardPNG3DRequiresOutline(t *testing.T) {
	if _, err := BoardPNG3D(nil, Shot3D{Width: 64, Height: 64, Samples: 1}); err == nil {
		t.Fatal("nil board should fail")
	}
	if _, err := BoardPNG3D(core.NewBoard(), Shot3D{Width: 64, Samples: 1}); err == nil {
		t.Fatal("board without an outline should fail")
	}
}

func TestBoardPNG3DProductShot(t *testing.T) {
	pngBytes := mustRender(t, fixtureBoard(), Shot3D{Width: 520, Samples: 1})
	img := decodePNG(t, pngBytes)
	if img.Bounds().Dx() < 400 || img.Bounds().Dy() < 200 {
		t.Fatalf("unexpected size %v", img.Bounds())
	}
	// Corners are the studio backdrop, not the dark 2D canvas.
	cr, cg, cb, _ := img.At(2, 2).RGBA()
	if cr>>8 < 170 || cg>>8 < 170 || cb>>8 < 170 {
		t.Fatalf("corner should be the light studio background, got %d,%d,%d", cr>>8, cg>>8, cb>>8)
	}
	var green, dark, gold int
	b := img.Bounds()
	for y := b.Min.Y; y < b.Max.Y; y += 2 {
		for x := b.Min.X; x < b.Max.X; x += 2 {
			r, g, bl, _ := img.At(x, y).RGBA()
			r8, g8, b8 := int(r>>8), int(g>>8), int(bl>>8)
			if g8 > r8+18 && g8 > b8+10 && g8 > 40 && g8 < 210 {
				green++
			}
			if r8 < 50 && g8 < 50 && b8 < 55 {
				dark++
			}
			if r8 > 140 && g8 > 90 && g8 < 210 && b8 < 140 && r8 > b8+30 {
				gold++
			}
		}
	}
	if green < 40 {
		t.Fatalf("soldermask green is missing (count %d)", green)
	}
	if dark < 8 {
		t.Fatalf("expected dark component bodies or drills, count %d", dark)
	}
	if gold < 4 {
		t.Fatalf("expected ENIG/copper pixels, count %d", gold)
	}
	// Same board, same pixels: agents can hash a render.
	again := mustRender(t, fixtureBoard(), Shot3D{Width: 520, Samples: 1})
	if !bytes.Equal(pngBytes, again) {
		t.Fatal("3D render is not deterministic")
	}
}

func TestBoardPNG3DLoadsRoutedBoard(t *testing.T) {
	path := filepath.Join("..", "..", "stress", "rp2040-minimal.fragua")
	if _, err := os.Stat(path); err != nil {
		t.Skip(path)
	}
	p, err := core.LoadFromPath(path)
	if err != nil {
		t.Fatal(err)
	}
	pngBytes := mustRender(t, p.Board(), Shot3D{Width: 240, Samples: 1})
	img := decodePNG(t, pngBytes)
	if img.Bounds().Dx() != 240 {
		t.Fatalf("width %d", img.Bounds().Dx())
	}
}

func fixtureBoard() *core.Board {
	b := core.NewBoard()
	r := core.RectFromCorners(core.Origin, core.NewPoint(core.FromMM(36), core.FromMM(18)))
	b.Outline = &r
	b.OutlineCornerRadius = core.FromMM(2)
	gnd := "GND"
	sig := "SIG"
	b.AddFootprint(&core.Footprint{
		Reference: "R1", Value: "10k", Key: "r_0603",
		Position: core.NewPoint(core.FromMM(8), core.FromMM(8)),
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(core.FromMM(-0.8), 0), Size: [2]core.Length{core.FromMM(0.8), core.FromMM(0.9)}, Net: &sig},
			{Number: "2", Offset: core.NewPoint(core.FromMM(0.8), 0), Size: [2]core.Length{core.FromMM(0.8), core.FromMM(0.9)}, Net: &gnd},
		},
		Silk: []core.FootprintSilkItem{{
			Kind: "text", Layer: core.SilkTop, Text: "{REF}",
			Position: core.NewPoint(0, core.FromMM(1.3)),
			Size:     core.FromMM(0.6), Anchor: core.SilkAnchorMiddle,
		}},
	})
	drill := core.FromMM(0.9)
	b.AddFootprint(&core.Footprint{
		Reference: "U1", Value: "soic", Key: "w25q16_soic8",
		Position: core.NewPoint(core.FromMM(20), core.FromMM(9)),
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(core.FromMM(-2.7), core.FromMM(1.9)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}, Net: &sig},
			{Number: "2", Offset: core.NewPoint(core.FromMM(-2.7), core.FromMM(0.6)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}},
			{Number: "3", Offset: core.NewPoint(core.FromMM(-2.7), core.FromMM(-0.6)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}},
			{Number: "4", Offset: core.NewPoint(core.FromMM(-2.7), core.FromMM(-1.9)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}, Net: &gnd},
			{Number: "5", Offset: core.NewPoint(core.FromMM(2.7), core.FromMM(-1.9)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}, Net: &gnd},
			{Number: "6", Offset: core.NewPoint(core.FromMM(2.7), core.FromMM(-0.6)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}},
			{Number: "7", Offset: core.NewPoint(core.FromMM(2.7), core.FromMM(0.6)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}},
			{Number: "8", Offset: core.NewPoint(core.FromMM(2.7), core.FromMM(1.9)), Size: [2]core.Length{core.FromMM(1.5), core.FromMM(0.6)}, Net: &sig},
		},
		Silk: []core.FootprintSilkItem{{
			Kind: "text", Layer: core.SilkTop, Text: "{REF}",
			Position: core.Origin, Size: core.FromMM(0.8), Anchor: core.SilkAnchorMiddle,
		}},
	})
	b.AddFootprint(&core.Footprint{
		Reference: "J1", Key: "header_1x03_2.54mm",
		Position: core.NewPoint(core.FromMM(31), core.FromMM(9)),
		Pads: []core.Pad{
			{Number: "1", Offset: core.NewPoint(0, core.FromMM(2.54)), Size: [2]core.Length{core.FromMM(1.6), core.FromMM(1.6)}, Drill: &drill, Net: &sig},
			{Number: "2", Offset: core.Origin, Size: [2]core.Length{core.FromMM(1.6), core.FromMM(1.6)}, Drill: &drill, Net: &gnd},
			{Number: "3", Offset: core.NewPoint(0, core.FromMM(-2.54)), Size: [2]core.Length{core.FromMM(1.6), core.FromMM(1.6)}, Drill: &drill},
		},
	})
	b.Traces = []core.Trace{{
		Layer: core.LayerTop, Net: sig,
		Start: core.NewPoint(core.FromMM(9), core.FromMM(8)),
		End:   core.NewPoint(core.FromMM(17), core.FromMM(11)),
		Width: core.FromMM(0.25),
	}}
	b.Vias = []core.Via{{
		Position: core.NewPoint(core.FromMM(14), core.FromMM(6)),
		Drill:    core.FromMM(0.3), Diameter: core.FromMM(0.6), Net: sig,
	}}
	b.MountHoles = []core.MountHole{{
		Center: core.NewPoint(core.FromMM(3), core.FromMM(3)), Diameter: core.FromMM(2.2),
	}}
	b.SilkTexts = []core.SilkText{{
		Layer: core.SilkTop, Text: "FRAGUA",
		Position: core.NewPoint(core.FromMM(18), core.FromMM(15.5)),
		Size:     core.FromMM(1.1), Anchor: core.SilkAnchorMiddle,
	}}
	return b
}

func mustRender(t *testing.T, b *core.Board, opt Shot3D) []byte {
	t.Helper()
	pngBytes, err := BoardPNG3D(b, opt)
	if err != nil {
		t.Fatal(err)
	}
	if len(pngBytes) < 8 || string(pngBytes[:8]) != "\x89PNG\r\n\x1a\n" {
		t.Fatal("not a PNG")
	}
	return pngBytes
}

func decodePNG(t *testing.T, raw []byte) image.Image {
	t.Helper()
	img, err := png.Decode(bytes.NewReader(raw))
	if err != nil {
		t.Fatal(err)
	}
	return img
}
