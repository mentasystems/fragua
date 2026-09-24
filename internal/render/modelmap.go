package render

import (
	"math"
	"regexp"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
)

// modelPlace is where a KiCad model sits in footprint-local millimetres.
// RotZ is applied first (model +Z axis), then Anchor is added. Chip models
// are centered and their +X terminal axis follows pad 2 − pad 1. Vertical
// pin-headers put their origin on pin 1 and run the row along -Y, so the
// anchor is pad 1 and RotZ turns that axis onto pad 2 − pad 1.
type modelPlace struct {
	Path             string
	AnchorX, AnchorY float64
	RotZ             float64
	// MirrorY flips the model through the footprint X axis before RotZ.
	// QFN/DFN StepUp models put the pin-1 chamfer on +Y while the KiCad
	// footprint (and Fragua's QFN pads) put pin 1 on -Y.
	MirrorY bool
}

// kicadPlace maps a footprint to a kicad-packages3D relative path.
// fp.Model, when set, replaces the path but keeps the anchor rules for the
// package kind the path names.
func kicadPlace(fp *core.Footprint) (modelPlace, bool) {
	if fp == nil {
		return modelPlace{}, false
	}
	var pl modelPlace
	if rel, ok := explicitModel(fp.Key); ok {
		pl.Path = rel
	} else if rel, ok := headerPath(fp.Key); ok {
		pl.Path = rel
	} else if rel, ok := packagePath(core.PackageNameFromLibrary(fp.Key)); ok {
		pl.Path = rel
	} else if rel, ok := packagePath(core.PackageNameFromLibrary(fp.Library)); ok {
		pl.Path = rel
	}
	if m := strings.TrimSpace(fp.Model); m != "" && !isLocalModel(m) {
		pl.Path = strings.ReplaceAll(m, "\\", "/")
	}
	if pl.Path == "" {
		return modelPlace{}, false
	}
	kind := pathKind(pl.Path)
	switch kind {
	case placeHeader:
		pl.AnchorX, pl.AnchorY, pl.RotZ = headerAnchor(fp)
	case placeChip:
		pl.RotZ = chipRot(fp)
	}
	low := strings.ToLower(pl.Path)
	if strings.Contains(low, "qfn") || strings.Contains(low, "dfn") {
		pl.MirrorY = true
	}
	return pl, true
}

type placeKind int

const (
	placeBody placeKind = iota
	placeChip
	placeHeader
)

func pathKind(p string) placeKind {
	l := strings.ToLower(p)
	switch {
	case strings.Contains(l, "pinheader"):
		return placeHeader
	case strings.Contains(l, "resistor_smd"), strings.Contains(l, "capacitor_smd"),
		strings.Contains(l, "inductor_smd"), strings.Contains(l, "led_smd"),
		strings.Contains(l, "ferrite"):
		return placeChip
	default:
		return placeBody
	}
}

func explicitModel(key string) (string, bool) {
	k := normKey(key)
	if p, ok := explicitModels[k]; ok {
		return p, true
	}
	return "", false
}

func normKey(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	s = strings.TrimPrefix(s, "library:")
	if i := strings.LastIndex(s, ":"); i >= 0 {
		s = s[i+1:]
	}
	return strings.ReplaceAll(s, "-", "_")
}

// explicitModels are footprint keys whose KiCad filename is not the package
// string PackageNameFromLibrary returns. USB-C receptacles in current
// kicad-packages3D are STEP-only, so usbc_16p is intentionally absent and
// falls back to a box.
var explicitModels = map[string]string{
	"w25q16_soic8":   "Package_SO.3dshapes/SOIC-8_3.9x4.9mm_P1.27mm.wrl",
	"rp2040_qfn56":   "Package_DFN_QFN.3dshapes/QFN-56-1EP_7x7mm_P0.4mm_EP5.6x5.6mm.wrl",
	"esp32_s3_qfn56": "Package_DFN_QFN.3dshapes/QFN-56-1EP_7x7mm_P0.4mm_EP5.6x5.6mm.wrl",
	"ldo_sot23_5":    "Package_TO_SOT_SMD.3dshapes/SOT-23-5.wrl",
	"sot23_5":        "Package_TO_SOT_SMD.3dshapes/SOT-23-5.wrl",
	"xtal_3225":      "Crystal.3dshapes/Crystal_SMD_3225-4Pin_3.2x2.5mm.wrl",
	"crystal_3225":   "Crystal.3dshapes/Crystal_SMD_3225-4Pin_3.2x2.5mm.wrl",
	// The published WRL set has no 4×4 tactile switch. B3U-1000P is a smaller
	// SPST that still reads as a button; the footprint pads stay visible.
	"sw_smd_4x4": "Button_Switch_SMD.3dshapes/SW_SPST_B3U-1000P.wrl",
}

// packageFiles fills in KiCad filenames that carry an exposed-pad suffix the
// short package alias does not.
var packageFiles = map[string]string{
	"QFN-56-1EP_7x7mm_P0.4mm":  "Package_DFN_QFN.3dshapes/QFN-56-1EP_7x7mm_P0.4mm_EP5.6x5.6mm.wrl",
	"SOT-23":                   "Package_TO_SOT_SMD.3dshapes/SOT-23.wrl",
	"SOT-23-5":                 "Package_TO_SOT_SMD.3dshapes/SOT-23-5.wrl",
	"SOT-23-6":                 "Package_TO_SOT_SMD.3dshapes/SOT-23-6.wrl",
	"SOIC-8_3.9x4.9mm_P1.27mm": "Package_SO.3dshapes/SOIC-8_3.9x4.9mm_P1.27mm.wrl",
	"TSSOP-8_3x3mm_P0.65mm":    "Package_SO.3dshapes/TSSOP-8_3x3mm_P0.65mm.wrl",
}

func packagePath(name string) (string, bool) {
	if name == "" {
		return "", false
	}
	if p, ok := packageFiles[name]; ok {
		return p, true
	}
	switch {
	case strings.HasPrefix(name, "C_") && strings.Contains(name, "Metric"):
		return "Capacitor_SMD.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "R_") && strings.Contains(name, "Metric"):
		return "Resistor_SMD.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "L_") && strings.Contains(name, "Metric"):
		return "Inductor_SMD.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "LED_") && strings.Contains(name, "Metric"):
		return "LED_SMD.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "SOIC-"), strings.HasPrefix(name, "TSSOP-"),
		strings.HasPrefix(name, "SSOP-"), strings.HasPrefix(name, "MSOP-"):
		return "Package_SO.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "SOT-"):
		return "Package_TO_SOT_SMD.3dshapes/" + name + ".wrl", true
	case strings.HasPrefix(name, "QFN-"), strings.HasPrefix(name, "DFN-"):
		return "Package_DFN_QFN.3dshapes/" + name + ".wrl", true
	}
	return "", false
}

var headerRE = regexp.MustCompile(`(?i)(?:header|pinheader)_(\d+)x(\d+).*2\.54`)

func headerPath(key string) (string, bool) {
	m := headerRE.FindStringSubmatch(normKey(key))
	if m == nil {
		// normKey turns 2.54 into 2.54 still, but '-' in 2.54 is not a hyphen
		// between words. The regex runs on the original-ish key. normKey
		// lowercases and strips library prefixes; '.' survives.
		return "", false
	}
	return "Connector_PinHeader_2.54mm.3dshapes/PinHeader_" + m[1] + "x" + pad2(m[2]) + "_P2.54mm_Vertical.wrl", true
}

func pad2(s string) string {
	if len(s) >= 2 {
		return s
	}
	return "0" + s
}

func chipRot(fp *core.Footprint) float64 {
	a := padNum(fp, "1")
	b := padNum(fp, "2")
	if a == nil || b == nil {
		if len(fp.Pads) < 2 {
			return 0
		}
		a, b = &fp.Pads[0], &fp.Pads[1]
	}
	dx := b.Offset.X.ToMM() - a.Offset.X.ToMM()
	dy := b.Offset.Y.ToMM() - a.Offset.Y.ToMM()
	if dx*dx+dy*dy < 1e-8 {
		return 0
	}
	return math.Atan2(dy, dx) * 180 / math.Pi
}

func headerAnchor(fp *core.Footprint) (x, y, rot float64) {
	a := padNum(fp, "1")
	b := padNum(fp, "2")
	if a == nil {
		if len(fp.Pads) == 0 {
			return 0, 0, 0
		}
		a = &fp.Pads[0]
	}
	x, y = a.Offset.X.ToMM(), a.Offset.Y.ToMM()
	if b == nil {
		return x, y, 0
	}
	dx := b.Offset.X.ToMM() - a.Offset.X.ToMM()
	dy := b.Offset.Y.ToMM() - a.Offset.Y.ToMM()
	if dx*dx+dy*dy < 1e-8 {
		return x, y, 0
	}
	// Pin 1 is the model origin. The row runs along -Y (measured on
	// PinHeader_*_Vertical.wrl: a 1x03 spans y -6.35..1.27 mm). θ turns
	// that axis onto pad2−pad1.
	rot = math.Atan2(dx, -dy) * 180 / math.Pi
	return x, y, rot
}

func padNum(fp *core.Footprint, n string) *core.Pad {
	for i := range fp.Pads {
		if fp.Pads[i].Number == n {
			return &fp.Pads[i]
		}
	}
	return nil
}
