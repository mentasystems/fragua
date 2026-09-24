package core

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"sync"
	"time"
)

// DefaultLibraryDir is the on-disk library root under the user home
// (Rust: ~/.pcb-library/). Index lives at index.json; binary attachments
// under attachments/.
const DefaultLibraryDir = ".pcb-library"

// LibraryPad is a pad authored in a library entry (mm, footprint-local Y-up).
type LibraryPad struct {
	Number  string   `json:"number"`
	Name    string   `json:"name"`
	XMM     float64  `json:"x_mm"`
	YMM     float64  `json:"y_mm"`
	WMM     float64  `json:"w_mm"`
	HMM     float64  `json:"h_mm"`
	DrillMM *float64 `json:"drill_mm,omitempty"`
}

// ViewTransform is an orientation tweak (review UI / spawn-time pad remap).
type ViewTransform struct {
	RotationDeg uint16 `json:"rotation_deg"`
	FlipH       bool   `json:"flip_h"`
	FlipV       bool   `json:"flip_v"`
}

// IsIdentity reports no flip and rotation multiple of 360.
func (v ViewTransform) IsIdentity() bool {
	return !v.FlipH && !v.FlipV && v.RotationDeg%360 == 0
}

// ApplyPointMM applies flip-then-rotate to a footprint-local mm point.
func (v ViewTransform) ApplyPointMM(x, y float64) (float64, float64) {
	if v.FlipH {
		x = -x
	}
	if v.FlipV {
		y = -y
	}
	switch v.RotationDeg % 360 {
	case 0:
		return x, y
	case 90:
		return -y, x
	case 180:
		return -x, -y
	case 270:
		return y, -x
	default:
		theta := float64(v.RotationDeg%360) * math.Pi / 180
		sin, cos := math.Sincos(theta)
		return x*cos - y*sin, x*sin + y*cos
	}
}

// ApplySizeMM swaps width/height for 90°/270° quadrants.
func (v ViewTransform) ApplySizeMM(w, h float64) (float64, float64) {
	r := v.RotationDeg % 360
	if (r >= 45 && r < 135) || (r >= 225 && r < 315) {
		return h, w
	}
	return w, h
}

// PlacementMargin is per-side keep-out around a footprint (mm).
// Elevated is resolution-time only: folded in by BodyKeepout, not written
// into the placement_margin object in index.json.
type PlacementMargin struct {
	TopMM    float64 `json:"top_mm"`
	RightMM  float64 `json:"right_mm"`
	BottomMM float64 `json:"bottom_mm"`
	LeftMM   float64 `json:"left_mm"`
	Elevated bool    `json:"-"`
}

// ClearsOver reports whether two bodies at different heights may overlap
// in plan view (exactly one elevated).
func (m PlacementMargin) ClearsOver(other PlacementMargin) bool {
	return m.Elevated != other.Elevated
}

// MirroredForBottom swaps left/right for bottom-side mounts.
func (m PlacementMargin) MirroredForBottom() PlacementMargin {
	m.LeftMM, m.RightMM = m.RightMM, m.LeftMM
	return m
}

// ForMountSide mirrors when not top-side copper.
func (m PlacementMargin) ForMountSide(topSide bool) PlacementMargin {
	if topSide {
		return m
	}
	return m.MirroredForBottom()
}

// IsZero reports every side ≤ 0.
func (m PlacementMargin) IsZero() bool {
	return m.TopMM <= 0 && m.RightMM <= 0 && m.BottomMM <= 0 && m.LeftMM <= 0
}

// AsTRBLMM packs [top, right, bottom, left].
func (m PlacementMargin) AsTRBLMM() [4]float64 {
	return [4]float64{m.TopMM, m.RightMM, m.BottomMM, m.LeftMM}
}

// BodyRect is the physical body extent in footprint-local mm (Y-up).
type BodyRect struct {
	MinXMM float64 `json:"min_x_mm"`
	MinYMM float64 `json:"min_y_mm"`
	MaxXMM float64 `json:"max_x_mm"`
	MaxYMM float64 `json:"max_y_mm"`
}

// PhotoCalibration is a two-point photo→board pin correspondence.
type PhotoCalibration struct {
	APx  [2]float64 `json:"a_px"`
	BPx  [2]float64 `json:"b_px"`
	APad string     `json:"a_pad"`
	BPad string     `json:"b_pad"`
}

// Attachment is a binary file linked to a library entry.
type Attachment struct {
	ID            string            `json:"id"`
	Kind          string            `json:"kind"`
	Filename      string            `json:"filename"`
	Mime          string            `json:"mime"`
	AddedAt       uint64            `json:"added_at"`
	ViewTransform ViewTransform     `json:"view_transform"`
	Calibration   *PhotoCalibration `json:"calibration,omitempty"`
}

// LibrarySilk is a silk primitive in footprint-local mm.
// JSON uses tag "kind" with snake_case values "line" / "text" (Rust enum).
type LibrarySilk struct {
	Kind  string    `json:"kind"`
	Layer SilkLayer `json:"layer"`
	// Line fields (present when kind=line)
	X1MM    float64 `json:"x1_mm,omitempty"`
	Y1MM    float64 `json:"y1_mm,omitempty"`
	X2MM    float64 `json:"x2_mm,omitempty"`
	Y2MM    float64 `json:"y2_mm,omitempty"`
	WidthMM float64 `json:"width_mm,omitempty"`
	// Text fields (present when kind=text)
	XMM         float64    `json:"x_mm,omitempty"`
	YMM         float64    `json:"y_mm,omitempty"`
	Text        string     `json:"text,omitempty"`
	SizeMM      float64    `json:"size_mm,omitempty"`
	RotationDeg float32    `json:"rotation_deg,omitempty"`
	Anchor      SilkAnchor `json:"anchor,omitempty"`
}

// LibraryEntry is one component footprint in the user library.
// Source/Datasheet/JLCClass/Pins/SymbolKindName are optional: entries written
// before they existed still load, and hand-authored `lib` entries omit them.
type LibraryEntry struct {
	Key                    string          `json:"key"`
	Description            string          `json:"description"`
	DefaultValue           string          `json:"default_value"`
	DefaultRotationDeg     float32         `json:"default_rotation_deg"`
	EdgeMounted            bool            `json:"edge_mounted"`
	EdgeSide               *EdgeSide       `json:"edge_side"`
	Elevated               bool            `json:"elevated,omitempty"`
	Pads                   []LibraryPad    `json:"pads"`
	Silk                   []LibrarySilk   `json:"silk"`
	LcscID                 *string         `json:"lcsc_id,omitempty"`
	MPN                    *string         `json:"mpn,omitempty"`
	Manufacturer           *string         `json:"manufacturer,omitempty"`
	Attachments            []Attachment    `json:"attachments"`
	CreatedAt              uint64          `json:"created_at"`
	FootprintViewTransform ViewTransform   `json:"footprint_view_transform"`
	PlacementMargin        PlacementMargin `json:"placement_margin"`
	BodyRect               *BodyRect       `json:"body_rect,omitempty"`
	// Source is where the entry came from: "lcsc", "kicad", "ipc" (empty = hand-authored).
	Source string `json:"source,omitempty"`
	// Datasheet is the vendor PDF/product URL, when the source knew one.
	Datasheet *string `json:"datasheet,omitempty"`
	// JLCClass is the JLCPCB part class ("Basic Part" / "Extended Part").
	JLCClass *string `json:"jlc_class,omitempty"`
	// Pins is the schematic pin list captured alongside the footprint, so a
	// second spawn needs no refetch. Numbers match Pads.Number.
	Pins []SchPin `json:"pins,omitempty"`
	// SymbolKindName is the SymbolKind.Kind to spawn ("generic_ic", "resistor", …).
	SymbolKindName string `json:"symbol_kind,omitempty"`
	// Model is an optional 3D override copied onto footprints spawned from
	// this entry. KiCad library-relative .wrl path, or a local .wrl/.obj.
	Model string `json:"model,omitempty"`
}

// SymbolKindFor returns the symbol kind this entry should spawn, defaulting to
// generic_ic when pins are known and to freeform (empty) when they are not.
func (e *LibraryEntry) SymbolKindFor() (SymbolKind, bool) {
	kind := e.SymbolKindName
	if kind == "" {
		if len(e.Pins) == 0 {
			return SymbolKind{}, false
		}
		kind = "generic_ic"
	}
	sk := SymbolKind{Kind: kind}
	if kind == "generic_ic" {
		sk.ICPins = append([]SchPin(nil), e.Pins...)
	}
	return sk, true
}

// PadsBBoxMM returns the axis-aligned pad bounding box, or false if no pads.
func (e *LibraryEntry) PadsBBoxMM() (minX, minY, maxX, maxY float64, ok bool) {
	if len(e.Pads) == 0 {
		return 0, 0, 0, 0, false
	}
	p0 := e.Pads[0]
	minX = p0.XMM - p0.WMM/2
	minY = p0.YMM - p0.HMM/2
	maxX = p0.XMM + p0.WMM/2
	maxY = p0.YMM + p0.HMM/2
	for _, p := range e.Pads[1:] {
		minX = math.Min(minX, p.XMM-p.WMM/2)
		minY = math.Min(minY, p.YMM-p.HMM/2)
		maxX = math.Max(maxX, p.XMM+p.WMM/2)
		maxY = math.Max(maxY, p.YMM+p.HMM/2)
	}
	return minX, minY, maxX, maxY, true
}

// GeometryHullMM covers pads + silk + body_rect (what placement/DRC must clear).
func (e *LibraryEntry) GeometryHullMM() (minX, minY, maxX, maxY float64, ok bool) {
	minX, minY = math.Inf(1), math.Inf(1)
	maxX, maxY = math.Inf(-1), math.Inf(-1)
	any := false
	expand := func(x, y float64) {
		any = true
		minX = math.Min(minX, x)
		minY = math.Min(minY, y)
		maxX = math.Max(maxX, x)
		maxY = math.Max(maxY, y)
	}
	if a, b, c, d, has := e.PadsBBoxMM(); has {
		any = true
		minX, minY, maxX, maxY = a, b, c, d
	}
	if e.BodyRect != nil {
		expand(e.BodyRect.MinXMM, e.BodyRect.MinYMM)
		expand(e.BodyRect.MaxXMM, e.BodyRect.MaxYMM)
	}
	for _, s := range e.Silk {
		switch s.Kind {
		case "line":
			hw := math.Max(s.WidthMM, 0) / 2
			expand(s.X1MM-hw, s.Y1MM-hw)
			expand(s.X1MM+hw, s.Y1MM+hw)
			expand(s.X2MM-hw, s.Y2MM-hw)
			expand(s.X2MM+hw, s.Y2MM+hw)
		case "text":
			w := math.Max(s.SizeMM, 0) * 0.6 * float64(max(1, len([]rune(s.Text))))
			h := math.Max(s.SizeMM, 0)
			var x0, x1 float64
			switch s.Anchor {
			case SilkAnchorStart:
				x0, x1 = s.XMM, s.XMM+w
			case SilkAnchorEnd:
				x0, x1 = s.XMM-w, s.XMM
			default: // Middle / empty
				x0, x1 = s.XMM-w/2, s.XMM+w/2
			}
			expand(x0, s.YMM-h/2)
			expand(x1, s.YMM+h/2)
		}
	}
	if !any {
		return 0, 0, 0, 0, false
	}
	return minX, minY, maxX, maxY, true
}

// MarginFromHull inflates the pad AABB out to the given hull.
func (e *LibraryEntry) MarginFromHull(hullMinX, hullMinY, hullMaxX, hullMaxY float64) PlacementMargin {
	minX, minY, maxX, maxY, ok := e.PadsBBoxMM()
	if !ok {
		return PlacementMargin{Elevated: e.Elevated}
	}
	return PlacementMargin{
		TopMM:    math.Max(0, hullMaxY-maxY),
		RightMM:  math.Max(0, hullMaxX-maxX),
		BottomMM: math.Max(0, minY-hullMinY),
		LeftMM:   math.Max(0, minX-hullMinX),
		Elevated: e.Elevated,
	}
}

// RefreshPlacementMargin recomputes placement_margin from live geometry.
func (e *LibraryEntry) RefreshPlacementMargin() {
	if minX, minY, maxX, maxY, ok := e.GeometryHullMM(); ok {
		e.PlacementMargin = e.MarginFromHull(minX, minY, maxX, maxY)
		return
	}
	e.PlacementMargin = PlacementMargin{Elevated: e.Elevated}
}

// BodyKeepout is the keep-out for placement/DRC: the wider of the live
// geometry hull and the margin stored on the entry, side by side, plus the
// elevated flag. Re-deriving from pads/silk/body_rect stops a stale disk
// margin under-reporting; keeping the stored one stops the derivation
// under-reporting the parts of a body nothing draws. A KF301 screw terminal
// is the case that matters: its 4 mm wire mouth appears in no pad, no silk
// line and no body_rect, only in the authored margin, and dropping it let
// edge-place hang the block off the board edge.
func (e *LibraryEntry) BodyKeepout() PlacementMargin {
	m := e.PlacementMargin
	m.Elevated = e.Elevated
	if minX, minY, maxX, maxY, ok := e.GeometryHullMM(); ok {
		d := e.MarginFromHull(minX, minY, maxX, maxY)
		m.TopMM = math.Max(m.TopMM, d.TopMM)
		m.RightMM = math.Max(m.RightMM, d.RightMM)
		m.BottomMM = math.Max(m.BottomMM, d.BottomMM)
		m.LeftMM = math.Max(m.LeftMM, d.LeftMM)
	}
	return m
}

// ToFootprint spawns a board footprint from this library entry.
// Applies FootprintViewTransform to pad offsets/sizes.
func (e *LibraryEntry) ToFootprint(reference, value string, layer Layer, rotation float64) *Footprint {
	if value == "" {
		value = e.DefaultValue
	}
	if rotation == 0 && e.DefaultRotationDeg != 0 {
		rotation = float64(e.DefaultRotationDeg)
	}
	vt := e.FootprintViewTransform
	bottom := !layer.IsTop()
	pads := make([]Pad, 0, len(e.Pads))
	for _, lp := range e.Pads {
		x, y := vt.ApplyPointMM(lp.XMM, lp.YMM)
		if bottom {
			x = -x
		}
		w, h := vt.ApplySizeMM(lp.WMM, lp.HMM)
		var drill *Length
		if lp.DrillMM != nil {
			d := FromMM(*lp.DrillMM)
			drill = &d
		}
		pads = append(pads, Pad{
			Number: lp.Number,
			Name:   lp.Name,
			Offset: NewPoint(FromMM(x), FromMM(y)),
			Size:   [2]Length{FromMM(w), FromMM(h)},
			Layer:  layer,
			Drill:  drill,
		})
	}
	fp := &Footprint{
		ID:              NewID(),
		Reference:       reference,
		Value:           value,
		Library:         "library:" + e.Key,
		Position:        NewPoint(FromMM(-100), FromMM(-100)),
		Rotation:        rotation,
		Layer:           layer,
		Pads:            pads,
		Key:             e.Key,
		Description:     e.Description,
		EdgeMounted:     e.EdgeMounted,
		EdgeSide:        e.EdgeSide,
		PlacementMargin: e.BodyKeepout(),
		Elevated:        e.Elevated,
		Model:           e.Model,
	}
	if e.BodyRect != nil {
		br := *e.BodyRect
		fp.BodyRect = &br
	}
	if e.LcscID != nil {
		fp.LcscID = *e.LcscID
	}
	if e.MPN != nil {
		fp.MPN = *e.MPN
	}
	if e.Manufacturer != nil {
		fp.Manufacturer = *e.Manufacturer
	}
	return fp
}

// FreeformPads builds a simple linear pad row for N pins.
func FreeformPads(n int) []LibraryPad {
	if n < 1 {
		n = 2
	}
	pads := make([]LibraryPad, n)
	pitch := 2.54
	start := -0.5 * pitch * float64(n-1)
	for i := 0; i < n; i++ {
		pads[i] = LibraryPad{
			Number: strconv.Itoa(i + 1),
			XMM:    start + float64(i)*pitch,
			WMM:    1.5,
			HMM:    1.0,
		}
	}
	return pads
}

// ResistorCapPads is a small 0603-style two-pad footprint.
func ResistorCapPads() []LibraryPad {
	return []LibraryPad{
		{Number: "1", XMM: -0.8, WMM: 0.8, HMM: 0.9},
		{Number: "2", XMM: 0.8, WMM: 0.8, HMM: 0.9},
	}
}

// libraryIndex is the on-disk index.json shape.
type libraryIndex struct {
	Entries []LibraryEntry `json:"entries"`
}

// Library is the process-local, disk-backed component store.
// Thread-safe; every mutation rewrites index.json.
type Library struct {
	indexPath      string
	attachmentsDir string
	mu             sync.RWMutex
	index          libraryIndex
}

// DefaultRoot returns ~/.pcb-library (or ./ .pcb-library if HOME is unset).
func DefaultRoot() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".", DefaultLibraryDir)
	}
	return filepath.Join(home, DefaultLibraryDir)
}

// OpenDefault opens (or creates) the library at DefaultRoot().
func OpenDefault() (*Library, error) {
	return OpenAt(DefaultRoot())
}

// OpenAt opens (or creates) a library rooted at root.
func OpenAt(root string) (*Library, error) {
	attachmentsDir := filepath.Join(root, "attachments")
	indexPath := filepath.Join(root, "index.json")
	if err := os.MkdirAll(attachmentsDir, 0o755); err != nil {
		return nil, fmt.Errorf("library: create %s: %w", attachmentsDir, err)
	}
	lib := &Library{
		indexPath:      indexPath,
		attachmentsDir: attachmentsDir,
	}
	data, err := os.ReadFile(indexPath)
	if err != nil {
		if os.IsNotExist(err) {
			return lib, nil
		}
		return nil, fmt.Errorf("library: read %s: %w", indexPath, err)
	}
	if err := json.Unmarshal(data, &lib.index); err != nil {
		return nil, fmt.Errorf("library: parse %s: %w", indexPath, err)
	}
	return lib, nil
}

func (l *Library) saveLocked() error {
	// Omit elevated=false from entries for readable index (match Rust is_false).
	data, err := json.MarshalIndent(&l.index, "", "  ")
	if err != nil {
		return fmt.Errorf("library: serialise: %w", err)
	}
	data = append(data, '\n')
	tmp := l.indexPath + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return fmt.Errorf("library: write %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, l.indexPath); err != nil {
		return fmt.Errorf("library: rename %s: %w", l.indexPath, err)
	}
	return nil
}

// Get returns a copy of the entry with the given key, or false if missing.
func (l *Library) Get(key string) (LibraryEntry, bool) {
	l.mu.RLock()
	defer l.mu.RUnlock()
	for i := range l.index.Entries {
		if l.index.Entries[i].Key == key {
			return cloneEntry(l.index.Entries[i]), true
		}
	}
	return LibraryEntry{}, false
}

// Put inserts or replaces an entry by key. Empty key is rejected.
// Replacing preserves existing attachments when the new list is empty.
// Refreshes placement_margin from geometry before write.
func (l *Library) Put(entry LibraryEntry) (LibraryEntry, error) {
	if entry.Key == "" {
		return LibraryEntry{}, fmt.Errorf("library: key must not be empty")
	}
	if entry.CreatedAt == 0 {
		entry.CreatedAt = uint64(time.Now().Unix())
	}
	if entry.Pads == nil {
		entry.Pads = []LibraryPad{}
	}
	if entry.Silk == nil {
		entry.Silk = []LibrarySilk{}
	}
	if entry.Attachments == nil {
		entry.Attachments = []Attachment{}
	}
	entry.RefreshPlacementMargin()

	l.mu.Lock()
	defer l.mu.Unlock()
	found := -1
	for i := range l.index.Entries {
		if l.index.Entries[i].Key == entry.Key {
			found = i
			break
		}
	}
	if found >= 0 {
		if len(entry.Attachments) == 0 {
			entry.Attachments = append([]Attachment(nil), l.index.Entries[found].Attachments...)
		}
		l.index.Entries[found] = entry
	} else {
		l.index.Entries = append(l.index.Entries, entry)
	}
	if err := l.saveLocked(); err != nil {
		return LibraryEntry{}, err
	}
	return cloneEntry(entry), nil
}

// List returns all entry keys in sorted order.
func (l *Library) List() []string {
	l.mu.RLock()
	defer l.mu.RUnlock()
	keys := make([]string, 0, len(l.index.Entries))
	for _, e := range l.index.Entries {
		keys = append(keys, e.Key)
	}
	slices.Sort(keys)
	return keys
}

// ListEntries returns a copy of every entry (order matches index; not sorted).
func (l *Library) ListEntries() []LibraryEntry {
	l.mu.RLock()
	defer l.mu.RUnlock()
	out := make([]LibraryEntry, len(l.index.Entries))
	for i, e := range l.index.Entries {
		out[i] = cloneEntry(e)
	}
	return out
}

// Delete removes an entry by key. Returns true if something was removed.
func (l *Library) Delete(key string) (bool, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	pos := -1
	for i := range l.index.Entries {
		if l.index.Entries[i].Key == key {
			pos = i
			break
		}
	}
	if pos < 0 {
		return false, nil
	}
	for _, att := range l.index.Entries[pos].Attachments {
		_ = os.Remove(l.attachmentPath(att))
	}
	l.index.Entries = append(l.index.Entries[:pos], l.index.Entries[pos+1:]...)
	if err := l.saveLocked(); err != nil {
		return false, err
	}
	return true, nil
}

// Root returns the library root directory (parent of index.json).
func (l *Library) Root() string {
	return filepath.Dir(l.indexPath)
}

// Count returns the number of entries.
func (l *Library) Count() int {
	l.mu.RLock()
	defer l.mu.RUnlock()
	return len(l.index.Entries)
}

func (l *Library) attachmentPath(att Attachment) string {
	return filepath.Join(l.attachmentsDir, att.ID+"."+extForMime(att.Mime))
}

func extForMime(mime string) string {
	switch mime {
	case "image/jpeg", "image/jpg":
		return "jpg"
	case "image/png":
		return "png"
	case "image/gif":
		return "gif"
	case "image/webp":
		return "webp"
	case "application/pdf":
		return "pdf"
	case "text/plain":
		return "txt"
	case "text/markdown":
		return "md"
	default:
		return "bin"
	}
}

func cloneEntry(e LibraryEntry) LibraryEntry {
	out := e
	if e.Pads != nil {
		out.Pads = append([]LibraryPad(nil), e.Pads...)
	}
	if e.Silk != nil {
		out.Silk = append([]LibrarySilk(nil), e.Silk...)
	}
	if e.Attachments != nil {
		out.Attachments = append([]Attachment(nil), e.Attachments...)
	}
	if e.EdgeSide != nil {
		s := *e.EdgeSide
		out.EdgeSide = &s
	}
	if e.BodyRect != nil {
		b := *e.BodyRect
		out.BodyRect = &b
	}
	if e.LcscID != nil {
		s := *e.LcscID
		out.LcscID = &s
	}
	if e.MPN != nil {
		s := *e.MPN
		out.MPN = &s
	}
	if e.Manufacturer != nil {
		s := *e.Manufacturer
		out.Manufacturer = &s
	}
	if e.Datasheet != nil {
		s := *e.Datasheet
		out.Datasheet = &s
	}
	if e.JLCClass != nil {
		s := *e.JLCClass
		out.JLCClass = &s
	}
	if e.Pins != nil {
		out.Pins = append([]SchPin(nil), e.Pins...)
	}
	return out
}

// openLibraryBestEffort opens the default library, falling back to a
// temp-rooted store so Project construction never fails.
func openLibraryBestEffort() *Library {
	lib, err := OpenDefault()
	if err == nil {
		return lib
	}
	tmp := filepath.Join(os.TempDir(), "pcb-library")
	lib, err = OpenAt(tmp)
	if err != nil {
		// Last resort: empty in-memory root under temp with unique name.
		tmp = filepath.Join(os.TempDir(), fmt.Sprintf("pcb-library-%d", time.Now().UnixNano()))
		lib, err = OpenAt(tmp)
		if err != nil {
			// Should be unreachable on writable temp.
			return &Library{indexPath: filepath.Join(tmp, "index.json"), attachmentsDir: filepath.Join(tmp, "attachments")}
		}
	}
	return lib
}
