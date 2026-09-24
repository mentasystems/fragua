package core

import (
	"encoding/json"
	"math"
	"strings"
)

// Minimum hand-solder gap between footprint pad AABBs (mm).
const MinFootprintGapMM = 2.0

// MountHoleClearanceMM is keepout around NPTH mount holes.
const MountHoleClearanceMM = 0.5

// CastellatedPadInsetMM: pads may sit this far outside the outline when castellated.
const CastellatedPadInsetMM = 0.15

// EdgeSide indicates which board/footprint edge an edge-mounted part faces.
// JSON values match Rust: top/right/bottom/left (lowercase).
type EdgeSide string

const (
	EdgeTop    EdgeSide = "top"
	EdgeRight  EdgeSide = "right"
	EdgeBottom EdgeSide = "bottom"
	EdgeLeft   EdgeSide = "left"
)

// Pad is a copper pad on a footprint.
type Pad struct {
	Number string    `json:"number"`
	Name   string    `json:"name"`
	Offset Point     `json:"offset"`
	Size   [2]Length `json:"size"`
	Layer  Layer     `json:"layer"`
	Net    *string   `json:"net"`
	Drill  *Length   `json:"drill,omitempty"`
}

// FootprintSilkItem is one library-local silk primitive (Rust tagged enum).
type FootprintSilkItem struct {
	Kind     string     `json:"kind"` // "line" | "text"
	Layer    SilkLayer  `json:"layer"`
	Start    Point      `json:"start,omitempty"`
	End      Point      `json:"end,omitempty"`
	Width    Length     `json:"width,omitempty"`
	Position Point      `json:"position,omitempty"`
	Text     string     `json:"text,omitempty"`
	Size     Length     `json:"size,omitempty"`
	Rotation float64    `json:"rotation,omitempty"`
	Anchor   SilkAnchor `json:"anchor,omitempty"`
}

// FootprintSilk is kept as an alias for older call sites (slice of items).
type FootprintSilk = FootprintSilkItem

// Footprint is a placed (or palette) component instance.
type Footprint struct {
	ID          ID        `json:"id"`
	Reference   string    `json:"reference"`
	Value       string    `json:"value"`
	Library     string    `json:"library"`
	Position    Point     `json:"position"`
	Rotation    float64   `json:"rotation"`
	Layer       Layer     `json:"layer"`
	Pads        []Pad     `json:"pads"`
	Key         string    `json:"key"`
	Description string    `json:"description"`
	EdgeMounted bool      `json:"edge_mounted"`
	EdgeSide    *EdgeSide `json:"edge_side"`
	// Pinned marks a footprint the agent positioned itself with `place`.
	// A bare `auto-place` leaves it where it is (same as EdgeMounted);
	// naming it explicitly — `auto-place U2` — still moves it.
	Pinned bool                `json:"pinned,omitempty"`
	Silk   []FootprintSilkItem `json:"silk"`
	// BOM / assembly fields. Copied from the library entry or `sym`/`lib`
	// script tokens. Never invented — empty means the part has no number.
	LcscID       string `json:"lcsc_id,omitempty"`
	MPN          string `json:"mpn,omitempty"`
	Manufacturer string `json:"manufacturer,omitempty"`
	// Model overrides the 3D body used by `fragua render`. A KiCad
	// library-relative path (Resistor_SMD.3dshapes/R_0603_1608Metric.wrl)
	// or a local .wrl/.obj file. Empty means the footprint key is mapped.
	Model string `json:"model,omitempty"`
	// Fiducial is a board optical mark: in CPL, omitted from BOM.
	Fiducial bool `json:"fiducial,omitempty"`
	// Courtyard / body: used by DRC. BodyRect is footprint-local mm (Y-up).
	BodyRect        *BodyRect       `json:"body_rect,omitempty"`
	PlacementMargin PlacementMargin `json:"placement_margin,omitempty"`
	Elevated        bool            `json:"elevated,omitempty"`
}

// Trace is a copper segment.
type Trace struct {
	ID    ID     `json:"id"`
	Layer Layer  `json:"layer"`
	Start Point  `json:"start"`
	End   Point  `json:"end"`
	Width Length `json:"width"`
	Net   string `json:"net"`
}

// Via is a plated through-hole connecting layers.
type Via struct {
	ID       ID     `json:"id"`
	Position Point  `json:"position"`
	Drill    Length `json:"drill"`
	Diameter Length `json:"diameter"`
	Net      string `json:"net"`
	// FromLayer/ToLayer optional for multi-layer; omitted means through-all.
	FromLayer *Layer `json:"from_layer,omitempty"`
	ToLayer   *Layer `json:"to_layer,omitempty"`
}

// ThermalRelief matches Rust pcb_core::ThermalRelief ({"kind":"spokes4"| "solid"}).
type ThermalRelief struct {
	Kind         string  `json:"kind"`
	SpokeWidthMM float64 `json:"spoke_width_mm,omitempty"`
	GapMM        float64 `json:"gap_mm,omitempty"`
}

// IsSpokes4 reports the KiCad-default four-spoke relief.
func (t ThermalRelief) IsSpokes4() bool {
	k := t.Kind
	return k == "spokes4" || k == "Spokes4" || k == ""
}

// StitchPolicy for pour via stitching.
// Presence on a pour (including empty `stitching: {}`) means stitching
// was requested — DRC must not silently pass an unstitched plane.
type StitchPolicy struct {
	Enabled  bool    `json:"enabled,omitempty"`
	PitchMM  float64 `json:"pitch_mm,omitempty"`
	DrillMM  float64 `json:"drill_mm,omitempty"`
	Diameter float64 `json:"diameter_mm,omitempty"`
}

// StitchRequested reports that the pour asked for stitching vias.
// A non-nil policy counts, including the empty object `{}`.
func (p *Pour) StitchRequested() bool {
	return p != nil && p.Stitching != nil
}

// Pour is a copper pour region (simplified: full-board or rect).
type Pour struct {
	ID            ID             `json:"id,omitempty"`
	Net           string         `json:"net"`
	Layer         Layer          `json:"layer"`
	ThermalRelief *ThermalRelief `json:"thermal_relief,omitempty"`
	Stitching     *StitchPolicy  `json:"stitching,omitempty"`
	// Optional polygon; empty means board outline.
	Polygon []Point `json:"polygon,omitempty"`
}

// Keepout forbids copper/placement.
type Keepout struct {
	ID       ID      `json:"id"`
	Rect     *Rect   `json:"rect,omitempty"`
	Polygon  []Point `json:"polygon,omitempty"`
	NoCopper bool    `json:"no_copper,omitempty"`
	NoPlace  bool    `json:"no_place,omitempty"`
}

// SilkLine is a board-level silkscreen segment.
type SilkLine struct {
	Layer SilkLayer `json:"layer"`
	Start Point     `json:"start"`
	End   Point     `json:"end"`
	Width Length    `json:"width"`
}

// SilkText is a board-level silkscreen text run.
type SilkText struct {
	Layer    SilkLayer  `json:"layer"`
	Position Point      `json:"position"`
	Text     string     `json:"text"`
	Size     Length     `json:"size"`
	Rotation float64    `json:"rotation"`
	Anchor   SilkAnchor `json:"anchor"`
	Width    Length     `json:"width"`
}

// Cutout is a milled board cutout (polygon).
type Cutout struct {
	ID      ID      `json:"id"`
	Polygon []Point `json:"polygon"`
}

// MountHole is an NPTH hole (JSON matches Rust pcb_core::MountHole).
type MountHole struct {
	ID              ID      `json:"id"`
	Center          Point   `json:"center"`
	Diameter        Length  `json:"diameter"`
	Label           string  `json:"label,omitempty"`
	KeepoutDiameter *Length `json:"keepout_diameter,omitempty"`
}

// Board is the physical layout model.
type Board struct {
	Outline             *Rect                 `json:"outline"`
	OutlinePoly         []Point               `json:"outline_poly,omitempty"`
	OutlineCornerRadius Length                `json:"outline_corner_radius,omitempty"`
	Footprints          map[string]*Footprint `json:"footprints"`
	FootprintOrder      []string              `json:"footprint_order"`
	Traces              []Trace               `json:"traces"`
	Vias                []Via                 `json:"vias"`
	Pours               []Pour                `json:"pours"`
	Keepouts            []Keepout             `json:"keepouts"`
	SilkLines           []SilkLine            `json:"silk_lines"`
	SilkTexts           []SilkText            `json:"silk_texts"`
	RuleAreas           []RuleArea            `json:"rule_areas"`
	FabRules            *FabRules             `json:"fab_rules,omitempty"`
	// EscapeExceptions are per-pad process exceptions (via-in-pad) for
	// pins that cannot leave the package under the fab ceiling.
	EscapeExceptions []EscapeException `json:"escape_exceptions,omitempty"`
	// AutoViaInPadStranded vias any fine-pitch pad still without a dogbone.
	AutoViaInPadStranded bool     `json:"auto_via_in_pad_stranded,omitempty"`
	Cutouts              []Cutout `json:"cutouts,omitempty"`
	// MountHoles is the Rust field name; also accept legacy "holes".
	MountHoles []MountHole   `json:"mount_holes,omitempty"`
	Holes      []MountHole   `json:"holes,omitempty"` // legacy alias
	Stackup    *LayerStackup `json:"stackup,omitempty"`
	// Teardrops, when true, adds tapered copper at trace↔pad and
	// trace↔via junctions (gerber G36 regions). Off by default.
	Teardrops bool `json:"teardrops,omitempty"`
}

// NewBoard returns an empty 2-layer board with a persisted default stackup.
func NewBoard() *Board {
	s := Default2Layer()
	return &Board{
		Footprints:     make(map[string]*Footprint),
		FootprintOrder: nil,
		Traces:         nil,
		Vias:           nil,
		Stackup:        &s,
	}
}

// StackupOrDefault returns stackup or Default2Layer.
func (b *Board) StackupOrDefault() LayerStackup {
	if b.Stackup != nil {
		return *b.Stackup
	}
	return Default2Layer()
}

// Apply4Layer promotes a 2-layer board to the default 4-layer FR-4 stack
// (F.Cu / In1.Cu / In2.Cu / B.Cu). Existing "Bottom" copper (index 1 on 2L)
// is remapped to the new physical bottom. Plane *assigned nets* (GND / a
// detected power rail) are recorded on the stackup; pours are added only
// for nets that already exist on the board — never a hardcoded +3V3/+1V1
// stack pretending to be the only 4-layer design.
func (b *Board) Apply4Layer() {
	if b.Stackup != nil && b.Stackup.CopperCount() >= 4 {
		return
	}
	oldBottom := uint8(1)
	if b.Stackup != nil {
		oldBottom = b.Stackup.BottomLayer().Index
	}
	s := Default4Layer()
	newBottom := s.BottomLayer().Index
	remap := func(l *Layer) {
		if l != nil && l.Index == oldBottom {
			l.Index = newBottom
		}
	}
	for _, fp := range b.Footprints {
		if fp == nil {
			continue
		}
		remap(&fp.Layer)
		for i := range fp.Pads {
			if fp.Pads[i].Drill != nil && *fp.Pads[i].Drill > 0 {
				continue // PTH occupies every layer
			}
			remap(&fp.Pads[i].Layer)
		}
	}
	for i := range b.Traces {
		remap(&b.Traces[i].Layer)
	}
	for i := range b.Pours {
		remap(&b.Pours[i].Layer)
	}
	gnd := firstExistingNet(b, "GND", "Gnd", "gnd")
	pwr := firstExistingNet(b, "+3V3", "3V3", "+5V", "5V", "VCC", "VDD", "+1V8", "1V8", "+1V1", "1V1")
	if gnd != "" && len(s.Layers) > 1 {
		s.Layers[1].AssignedNet = gnd
	}
	if pwr != "" && len(s.Layers) > 2 {
		s.Layers[2].AssignedNet = pwr
	}
	b.Stackup = &s
	if b.FabRules == nil || b.FabRules.Preset == "" || b.FabRules.Preset == "jlcpcb-2l" || b.FabRules.Preset == "jlcpcb-2l-via02" {
		b.FabRules = FabRulesPreset("jlcpcb-4l")
	}
	relief := ThermalRelief{Kind: "spokes4", SpokeWidthMM: 0.4, GapMM: 0.4}
	hasPlane := func(net string, layer uint8) bool {
		for _, p := range b.Pours {
			if p.Net == net && p.Layer.Index == layer {
				return true
			}
		}
		return false
	}
	if gnd != "" && !hasPlane(gnd, 1) {
		b.Pours = append(b.Pours, Pour{Net: gnd, Layer: Layer{Index: 1}, ThermalRelief: &relief})
	}
	if pwr != "" && !hasPlane(pwr, 2) {
		b.Pours = append(b.Pours, Pour{Net: pwr, Layer: Layer{Index: 2}, ThermalRelief: &relief})
	}
}

func boardHasNet(b *Board, names ...string) bool {
	return firstExistingNet(b, names...) != ""
}

func firstExistingNet(b *Board, names ...string) string {
	if b == nil {
		return ""
	}
	want := map[string]bool{}
	for _, n := range names {
		want[n] = true
	}
	seen := func(n string) string {
		if want[n] {
			return n
		}
		return ""
	}
	for _, fp := range b.Footprints {
		if fp == nil {
			continue
		}
		for i := range fp.Pads {
			if fp.Pads[i].Net != nil {
				if hit := seen(*fp.Pads[i].Net); hit != "" {
					return hit
				}
			}
		}
	}
	for _, p := range b.Pours {
		if hit := seen(p.Net); hit != "" {
			return hit
		}
	}
	for _, t := range b.Traces {
		if hit := seen(t.Net); hit != "" {
			return hit
		}
	}
	for _, v := range b.Vias {
		if hit := seen(v.Net); hit != "" {
			return hit
		}
	}
	return ""
}

// AddFootprint inserts a footprint and appends to order.
func (b *Board) AddFootprint(fp *Footprint) {
	if b.Footprints == nil {
		b.Footprints = make(map[string]*Footprint)
	}
	if fp.ID.IsZero() {
		fp.ID = NewID()
	}
	key := fp.ID.String()
	b.Footprints[key] = fp
	b.FootprintOrder = append(b.FootprintOrder, key)
}

// FootprintByRef returns the footprint with the given reference designator.
func (b *Board) FootprintByRef(ref string) *Footprint {
	for _, id := range b.FootprintOrder {
		if fp := b.Footprints[id]; fp != nil && fp.Reference == ref {
			return fp
		}
	}
	for _, fp := range b.Footprints {
		if fp != nil && fp.Reference == ref {
			return fp
		}
	}
	return nil
}

// Clone deep-copies the board via JSON (feasibility search, compact).
func (b *Board) Clone() *Board {
	if b == nil {
		return NewBoard()
	}
	raw, err := json.Marshal(b)
	if err != nil {
		return NewBoard()
	}
	var out Board
	if err := json.Unmarshal(raw, &out); err != nil {
		return NewBoard()
	}
	if out.Footprints == nil {
		out.Footprints = make(map[string]*Footprint)
	}
	return &out
}

// RemoveFootprintByRef deletes a footprint and returns it.
func (b *Board) RemoveFootprintByRef(ref string) *Footprint {
	for i, id := range b.FootprintOrder {
		fp := b.Footprints[id]
		if fp == nil || fp.Reference != ref {
			continue
		}
		delete(b.Footprints, id)
		b.FootprintOrder = append(b.FootprintOrder[:i], b.FootprintOrder[i+1:]...)
		return fp
	}
	for id, fp := range b.Footprints {
		if fp != nil && fp.Reference == ref {
			delete(b.Footprints, id)
			return fp
		}
	}
	return nil
}

// ClearRoute removes all traces and vias.
func (b *Board) ClearRoute() {
	b.Traces = nil
	b.Vias = nil
}

// PadWorldCenter returns the world-space center of a pad on a footprint.
func PadWorldCenter(fp *Footprint, pad *Pad) Point {
	ox, oy := float64(pad.Offset.X), float64(pad.Offset.Y)
	rad := fp.Rotation * math.Pi / 180
	c, s := math.Cos(rad), math.Sin(rad)
	rx := ox*c - oy*s
	ry := ox*s + oy*c
	return Point{
		X: fp.Position.X + Length(math.Round(rx)),
		Y: fp.Position.Y + Length(math.Round(ry)),
	}
}

// PadWorldSize is the pad width/height after 90° rotation (Rust pad_world_size).
func PadWorldSize(fp *Footprint, pad *Pad) (w, h Length) {
	r := math.Mod(fp.Rotation, 360)
	if r < 0 {
		r += 360
	}
	if (r >= 45 && r < 135) || (r >= 225 && r < 315) {
		return pad.Size[1], pad.Size[0]
	}
	return pad.Size[0], pad.Size[1]
}

// PadOccupiesLayer: SMD on its copper layer; PTH (drill set) on every layer.
func PadOccupiesLayer(pad *Pad, target Layer) bool {
	if pad.Drill != nil {
		return true
	}
	return pad.Layer.Index == target.Index
}

// LocalToWorld transforms a footprint-local point into board coords.
func LocalToWorld(fp *Footprint, p Point) Point {
	theta := fp.Rotation * math.Pi / 180
	s, c := math.Sin(theta), math.Cos(theta)
	lx, ly := p.X.ToMM(), p.Y.ToMM()
	rx := lx*c - ly*s
	ry := lx*s + ly*c
	return Point{X: FromMM(fp.Position.X.ToMM() + rx), Y: FromMM(fp.Position.Y.ToMM() + ry)}
}

// ResolveSilkText replaces {REF}/{VAL} placeholders.
func ResolveSilkText(fp *Footprint, raw string) string {
	s := strings.ReplaceAll(raw, "{REF}", fp.Reference)
	return strings.ReplaceAll(s, "{VAL}", fp.Value)
}

// CourtyardMarginMM is applied around the pad-union AABB when a footprint
// has no library body_rect and no placement_margin. Documented default
// for DRC courtyard overlap (not a fab rule).
const CourtyardMarginMM = 0.25

// CourtyardWorld returns the world-space courtyard AABB.
// Preference: library body_rect (rotated), else pad-union expanded by
// placement_margin, else pad-union + CourtyardMarginMM.
func CourtyardWorld(fp *Footprint) (Rect, bool) {
	if fp == nil {
		return Rect{}, false
	}
	if fp.BodyRect != nil {
		corners := []Point{
			{X: FromMM(fp.BodyRect.MinXMM), Y: FromMM(fp.BodyRect.MinYMM)},
			{X: FromMM(fp.BodyRect.MaxXMM), Y: FromMM(fp.BodyRect.MinYMM)},
			{X: FromMM(fp.BodyRect.MaxXMM), Y: FromMM(fp.BodyRect.MaxYMM)},
			{X: FromMM(fp.BodyRect.MinXMM), Y: FromMM(fp.BodyRect.MaxYMM)},
		}
		w0 := LocalToWorld(fp, corners[0])
		out := Rect{Min: w0, Max: w0}
		for _, c := range corners[1:] {
			p := LocalToWorld(fp, c)
			if p.X < out.Min.X {
				out.Min.X = p.X
			}
			if p.Y < out.Min.Y {
				out.Min.Y = p.Y
			}
			if p.X > out.Max.X {
				out.Max.X = p.X
			}
			if p.Y > out.Max.Y {
				out.Max.Y = p.Y
			}
		}
		return out, true
	}
	if len(fp.Pads) == 0 {
		return Rect{}, false
	}
	body := PadWorldAABB(fp, &fp.Pads[0])
	for i := 1; i < len(fp.Pads); i++ {
		body = body.Union(PadWorldAABB(fp, &fp.Pads[i]))
	}
	m := fp.PlacementMargin
	if m.IsZero() {
		return body.Expand(FromMM(CourtyardMarginMM)), true
	}
	// Placement margin is footprint-local (top/right/bottom/left). After
	// 90° snaps, expand the world AABB by the matching sides.
	rot := math.Mod(fp.Rotation, 360)
	if rot < 0 {
		rot += 360
	}
	top, right, bottom, left := m.TopMM, m.RightMM, m.BottomMM, m.LeftMM
	switch {
	case rot > 45 && rot <= 135:
		top, right, bottom, left = left, top, right, bottom
	case rot > 135 && rot <= 225:
		top, right, bottom, left = bottom, left, top, right
	case rot > 225 && rot <= 315:
		top, right, bottom, left = right, bottom, left, top
	}
	return Rect{
		Min: Point{X: body.Min.X - FromMM(left), Y: body.Min.Y - FromMM(bottom)},
		Max: Point{X: body.Max.X + FromMM(right), Y: body.Max.Y + FromMM(top)},
	}, true
}

// PadWorldAABB returns the axis-aligned bounding box of a pad (90° rotations).
func PadWorldAABB(fp *Footprint, pad *Pad) Rect {
	// For non-orthogonal rotations use center + max extent; for 0/90/180/270
	// swap width/height when needed.
	rot := math.Mod(fp.Rotation, 360)
	if rot < 0 {
		rot += 360
	}
	w, h := pad.Size[0], pad.Size[1]
	// near 90 or 270 → swap
	if (rot > 45 && rot < 135) || (rot > 225 && rot < 315) {
		w, h = h, w
	}
	c := PadWorldCenter(fp, pad)
	return RectFromCenter(c, w, h)
}

// size is stored as [w,h] Length array in JSON — custom unmarshal if needed.
// encoding/json handles [2]Length when JSON is [n,n].

// Ensure JSON maps for footprints deserialize even when null.
func (b *Board) UnmarshalJSON(data []byte) error {
	type alias Board
	aux := &struct {
		*alias
	}{alias: (*alias)(b)}
	if err := json.Unmarshal(data, aux); err != nil {
		return err
	}
	if b.Footprints == nil {
		b.Footprints = make(map[string]*Footprint)
	}
	return nil
}
