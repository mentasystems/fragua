// Package render produces board/schematic SVG matching Rust pcb-render.
package render

import (
	"fmt"
	"math"
	"sort"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
)

var padPalette = []string{
	"#c97a2b", "#2b6cc9", "#3aa66c", "#a63a8c",
	"#d6b500", "#b0303a", "#3aa6a6", "#9c6b3a",
}
var tracePalette = []string{
	"#ffd166", "#4ec9ff", "#84e8b3", "#e495d2",
	"#ffe89a", "#ff95a0", "#9ce5e5", "#deb887",
}

// Marker is one check finding placed on the board, drawn into the `drc`
// layer group so the UI can point at it.
type Marker struct {
	ID       string
	Severity string // error | warning
	Kind     string
	Message  string
	Net      string
	XMM      float64
	YMM      float64
}

// Options tunes a board render. The zero value is what BoardSVG uses.
type Options struct {
	// Markers are DRC/ERC findings drawn into the `drc` group.
	Markers []Marker
}

// BoardSVG renders a top-view SVG in the same visual language as
// crates/pcb-render (dark canvas, mm grid, orange/cyan copper, pad
// names, drills, body outlines, dimension labels).
func BoardSVG(board *core.Board) string {
	return BoardSVGWith(board, Options{})
}

// BoardSVGWith renders the board with options. The document is structured for
// the observer UI: every drawable sits in a `data-layer` group, footprints
// carry `data-ref`, pads `data-pad`, and copper `data-net` / `data-id`.
// ARCHITECTURE.md ("SVG data-attribute contract") is the spec.
func BoardSVGWith(board *core.Board, opts Options) string {
	if board == nil {
		return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 300" width="400" height="300"><rect width="400" height="300" fill="#0e1116"/></svg>`
	}
	vx, vy, vw, vh := viewMM(board)
	pw, ph := svgPixelSize(vw, vh)
	stack := board.StackupOrDefault()
	n := stack.CopperCount()
	var b strings.Builder
	// Pixel width/height (not 100%) so <img> / <object> get an intrinsic
	// size. Percentage-only SVGs collapse to a blank replaced element in
	// several browsers — the observer UI looked dead even when this
	// payload was valid.
	fmt.Fprintf(&b, `<svg xmlns="http://www.w3.org/2000/svg" viewBox="%.3f %.3f %.3f %.3f" width="%d" height="%d" preserveAspectRatio="xMidYMid meet" data-view-mm="%.3f %.3f %.3f %.3f" data-copper-layers="%d">`,
		vx, -(vy + vh), vw, vh, pw, ph, vx, vy, vw, vh, n)
	b.WriteString(`<g data-root="1" transform="scale(1,-1)">`)

	b.WriteString(`<g data-layer="background" pointer-events="none">`)
	fmt.Fprintf(&b, `<rect x="%.3f" y="%.3f" width="%.3f" height="%.3f" fill="#0e1116"/>`, vx, vy, vw, vh)
	writeGrid(&b, vx, vy, vw, vh)
	b.WriteString(`<g stroke="#d6905b" stroke-width="0.08" opacity="0.6"><line x1="-1.5" y1="0" x2="1.5" y2="0"/><line x1="0" y1="-1.5" x2="0" y2="1.5"/></g>`)
	b.WriteString(`<g transform="translate(0.4,0.4) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#d6905b" opacity="0.7">0,0</text></g>`)
	if board.Outline == nil {
		fmt.Fprintf(&b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="2.4" fill="#9aa3b2">empty board</text></g>`,
			vx+vw/2, vy+vh/2)
	}
	b.WriteString(`</g>`)

	writeSubstrate(&b, board)
	writePours(&b, board, stack)

	// Bottom copper first, then footprints (pads), then top copper, then vias:
	// the top layer has to read over the parts it lands on.
	for i := n - 1; i >= 1; i-- {
		writeCopperLayer(&b, board, stack, i)
	}
	writeFootprints(&b, board, stack)
	writeCopperLayer(&b, board, stack, 0)
	writeVias(&b, board)
	writeMask(&b, board, stack)
	writeDrills(&b, board)
	writePadNames(&b, board)
	writeSilk(&b, board)
	writeEdge(&b, board)
	writeRatsnest(&b, board)
	writeMarkers(&b, opts.Markers)

	b.WriteString(`</g></svg>`)
	return b.String()
}

func viewMM(board *core.Board) (x, y, w, h float64) {
	if o := board.Outline; o != nil {
		ow, oh := o.Width().ToMM(), o.Height().ToMM()
		if ow > 0 && oh > 0 {
			px := ow / 10
			py := oh / 10
			return o.Min.X.ToMM() - px, o.Min.Y.ToMM() - py, ow + 2*px, oh + 2*py
		}
	}
	return -5, -5, 60, 60
}

func svgPixelSize(vw, vh float64) (w, h int) {
	const pxPerMM = 16.0
	const maxPx = 1600.0
	if vw < 1 {
		vw = 1
	}
	if vh < 1 {
		vh = 1
	}
	scale := pxPerMM
	if vw*scale > maxPx {
		scale = maxPx / vw
	}
	if vh*scale > maxPx {
		scale = maxPx / vh
	}
	w = int(math.Round(vw * scale))
	h = int(math.Round(vh * scale))
	if w < 1 {
		w = 1
	}
	if h < 1 {
		h = 1
	}
	return w, h
}

func writeGrid(b *strings.Builder, vx, vy, vw, vh float64) {
	if vw > 400 || vh > 400 {
		return
	}
	b.WriteString(`<g stroke-width="0.03" fill="none">`)
	for x := math.Floor(vx); x <= vx+vw+1e-9; x++ {
		stroke := "#161b22"
		if int(math.Round(x))%5 == 0 {
			stroke = "#222a35"
		}
		fmt.Fprintf(b, `<line x1="%.3f" y1="%.3f" x2="%.3f" y2="%.3f" stroke="%s"/>`, x, vy, x, vy+vh, stroke)
	}
	for y := math.Floor(vy); y <= vy+vh+1e-9; y++ {
		stroke := "#161b22"
		if int(math.Round(y))%5 == 0 {
			stroke = "#222a35"
		}
		fmt.Fprintf(b, `<line x1="%.3f" y1="%.3f" x2="%.3f" y2="%.3f" stroke="%s"/>`, vx, y, vx+vw, y, stroke)
	}
	b.WriteString(`</g>`)
	for x := math.Ceil(vx/5) * 5; x <= vx+vw+1e-9; x += 5 {
		fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#3a4452">%d</text></g>`, x, vy+0.4, int(x))
	}
	for y := math.Ceil(vy/5) * 5; y <= vy+vh+1e-9; y += 5 {
		fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" font-family="ui-monospace, monospace" font-size="0.9" fill="#3a4452">%d</text></g>`, vx+0.3, y, int(y))
	}
}

// writeSubstrate is the FR-4 body: it is board, not a togglable layer, so it
// lives outside the edge group the human can hide.
func writeSubstrate(b *strings.Builder, board *core.Board) {
	o := board.Outline
	if o == nil {
		return
	}
	rad := board.OutlineCornerRadius.ToMM()
	b.WriteString(`<g data-layer="substrate" pointer-events="none">`)
	fmt.Fprintf(b, `<rect x="%.3f" y="%.3f" width="%.3f" height="%.3f" rx="%.3f" ry="%.3f" fill="#5a3a1f" fill-opacity="0.72"/>`,
		o.Min.X.ToMM(), o.Min.Y.ToMM(), o.Width().ToMM(), o.Height().ToMM(), rad, rad)
	b.WriteString(`</g>`)
}

func writeEdge(b *strings.Builder, board *core.Board) {
	o := board.Outline
	b.WriteString(`<g data-layer="edge" pointer-events="none">`)
	if o != nil {
		rad := board.OutlineCornerRadius.ToMM()
		fmt.Fprintf(b, `<rect x="%.3f" y="%.3f" width="%.3f" height="%.3f" rx="%.3f" ry="%.3f" fill="none" stroke="#d6905b" stroke-width="0.4"/>`,
			o.Min.X.ToMM(), o.Min.Y.ToMM(), o.Width().ToMM(), o.Height().ToMM(), rad, rad)
		fmt.Fprintf(b, `<rect x="%.3f" y="%.3f" width="%.3f" height="%.3f" fill="none" stroke="#e8a86a" stroke-width="0.35"/>`,
			o.Min.X.ToMM(), o.Min.Y.ToMM(), o.Width().ToMM(), o.Height().ToMM())
		cx := (o.Min.X.ToMM() + o.Max.X.ToMM()) / 2
		cy := (o.Min.Y.ToMM() + o.Max.Y.ToMM()) / 2
		fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.4" fill="#d6905b">%.1f mm</text></g>`,
			cx, o.Max.Y.ToMM()+1.8, o.Width().ToMM())
		fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1) rotate(-90)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="1.4" fill="#d6905b">%.1f mm</text></g>`,
			o.Min.X.ToMM()-1.8, cy, o.Height().ToMM())
	}
	for _, c := range board.Cutouts {
		if len(c.Polygon) < 3 {
			continue
		}
		b.WriteString(`<path fill="#0e1116" stroke="#e8a86a" stroke-width="0.2" d="`)
		for i, p := range c.Polygon {
			cmd := "L"
			if i == 0 {
				cmd = "M"
			}
			fmt.Fprintf(b, `%s%.3f,%.3f `, cmd, p.X.ToMM(), p.Y.ToMM())
		}
		b.WriteString(`Z"/>`)
	}
	for _, h := range boardHoles(board) {
		fmt.Fprintf(b, `<circle cx="%.3f" cy="%.3f" r="%.3f" fill="#0e1116" stroke="#e8a86a" stroke-width="0.15"/>`,
			h.Center.X.ToMM(), h.Center.Y.ToMM(), h.Diameter.ToMM()/2)
	}
	b.WriteString(`</g>`)
}

func boardHoles(board *core.Board) []core.MountHole {
	if len(board.MountHoles) > 0 {
		return board.MountHoles
	}
	return board.Holes
}

func writePours(b *strings.Builder, board *core.Board, stack core.LayerStackup) {
	b.WriteString(`<g data-layer="pours" pointer-events="none">`)
	if o := board.Outline; o != nil {
		for _, pour := range board.Pours {
			writePour(b, board, pour, *o, stack)
		}
	}
	b.WriteString(`</g>`)
}

func writeCopperLayer(b *strings.Builder, board *core.Board, stack core.LayerStackup, idx int) {
	name := stack.LayerName(idx)
	fmt.Fprintf(b, `<g data-layer="%s" data-kind="copper" data-index="%d">`, escape(name), idx)
	for _, tr := range board.Traces {
		if int(tr.Layer.Index) != idx {
			continue
		}
		writeTrace(b, tr)
	}
	b.WriteString(`</g>`)
}

func writeVias(b *strings.Builder, board *core.Board) {
	b.WriteString(`<g data-layer="vias">`)
	for _, v := range board.Vias {
		writeVia(b, v)
	}
	b.WriteString(`</g>`)
}

// writeDrills is every hole on the board — pad barrels and via barrels — so
// the human can read the drill map without the copper on top of it.
func writeDrills(b *strings.Builder, board *core.Board) {
	b.WriteString(`<g data-layer="drills" pointer-events="none" fill="#0e1116">`)
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if pad.Drill == nil || *pad.Drill <= 0 {
				continue
			}
			c := core.PadWorldCenter(fp, pad)
			fmt.Fprintf(b, `<circle cx="%.3f" cy="%.3f" r="%.3f"/>`, c.X.ToMM(), c.Y.ToMM(), pad.Drill.ToMM()/2)
		}
	}
	for _, v := range board.Vias {
		r := v.Drill.ToMM() / 2
		if r < 0.05 {
			r = v.Diameter.ToMM() * 0.2
		}
		fmt.Fprintf(b, `<circle cx="%.3f" cy="%.3f" r="%.3f"/>`, v.Position.X.ToMM(), v.Position.Y.ToMM(), r)
	}
	b.WriteString(`</g>`)
}

// writeMask is the solder-mask opening around every pad (pad + expansion).
// Hidden by default in the UI; it is what the stencil sees, not copper.
func writeMask(b *strings.Builder, board *core.Board, stack core.LayerStackup) {
	const expandMM = 0.05
	b.WriteString(`<g data-layer="mask" pointer-events="none" fill="none" stroke="#5cd6a0" stroke-width="0.04" opacity="0.75">`)
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			aa := core.PadWorldAABB(fp, pad)
			fmt.Fprintf(b, `<rect data-net="%s" data-layer="%s" x="%.3f" y="%.3f" width="%.3f" height="%.3f"/>`,
				escape(padNet(pad)), escape(stack.LayerName(int(pad.Layer.Index))),
				aa.Min.X.ToMM()-expandMM, aa.Min.Y.ToMM()-expandMM,
				aa.Width().ToMM()+2*expandMM, aa.Height().ToMM()+2*expandMM)
		}
	}
	b.WriteString(`</g>`)
}

func writePour(b *strings.Builder, board *core.Board, pour core.Pour, outline core.Rect, stack core.LayerStackup) {
	inset := 0.3
	x := outline.Min.X.ToMM() + inset
	y := outline.Min.Y.ToMM() + inset
	w := outline.Width().ToMM() - 2*inset
	h := outline.Height().ToMM() - 2*inset
	fill := padFill(pour.Layer)
	// evenodd: board rect minus clearance holes around foreign pads
	fmt.Fprintf(b, `<path data-net="%s" data-layer="%s" fill="%s" fill-opacity="0.45" fill-rule="evenodd" d="M%.3f,%.3f h%.3f v%.3f h%.3f z`,
		escape(pour.Net), escape(stack.LayerName(int(pour.Layer.Index))), fill, x, y, w, h, -w)
	halo := 0.55
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			pad := &fp.Pads[i]
			if !core.PadOccupiesLayer(pad, pour.Layer) {
				continue
			}
			if pad.Net != nil && *pad.Net == pour.Net {
				continue
			}
			aa := core.PadWorldAABB(fp, pad)
			fmt.Fprintf(b, ` M%.3f,%.3f h%.3f v%.3f h%.3f z`,
				aa.Min.X.ToMM()-halo, aa.Min.Y.ToMM()-halo,
				aa.Width().ToMM()+2*halo, aa.Height().ToMM()+2*halo,
				-(aa.Width().ToMM() + 2*halo))
		}
	}
	b.WriteString(`"/>`)
}

func writeTrace(b *strings.Builder, tr core.Trace) {
	fmt.Fprintf(b, `<line data-id="%s" data-net="%s" x1="%.3f" y1="%.3f" x2="%.3f" y2="%.3f" stroke="%s" stroke-width="%.3f" stroke-linecap="round"/>`,
		escape(tr.ID.String()), escape(tr.Net),
		tr.Start.X.ToMM(), tr.Start.Y.ToMM(), tr.End.X.ToMM(), tr.End.Y.ToMM(),
		traceStroke(tr.Layer), tr.Width.ToMM())
}

func writeVia(b *strings.Builder, v core.Via) {
	cx, cy := v.Position.X.ToMM(), v.Position.Y.ToMM()
	outer := v.Diameter.ToMM() / 2
	fmt.Fprintf(b, `<circle data-id="%s" data-net="%s" cx="%.3f" cy="%.3f" r="%.3f" fill="#7d8590"/>`,
		escape(v.ID.String()), escape(v.Net), cx, cy, outer)
}

// footprintsStable iterates footprints in FootprintOrder, then any the order
// map missed, so the SVG is byte-stable across runs.
func footprintsStable(board *core.Board) []*core.Footprint {
	out := make([]*core.Footprint, 0, len(board.Footprints))
	seen := map[string]bool{}
	for _, id := range board.FootprintOrder {
		if fp := board.Footprints[id]; fp != nil {
			out = append(out, fp)
			seen[id] = true
		}
	}
	rest := make([]string, 0, len(board.Footprints))
	for id, fp := range board.Footprints {
		if fp != nil && !seen[id] {
			rest = append(rest, id)
		}
	}
	sort.Strings(rest)
	for _, id := range rest {
		out = append(out, board.Footprints[id])
	}
	return out
}

func writeFootprints(b *strings.Builder, board *core.Board, stack core.LayerStackup) {
	b.WriteString(`<g data-layer="footprints">`)
	for _, fp := range footprintsStable(board) {
		writeFootprint(b, fp, stack)
	}
	b.WriteString(`</g>`)
}

func writeFootprint(b *strings.Builder, fp *core.Footprint, stack core.LayerStackup) {
	fmt.Fprintf(b, `<g data-board-ref="%s" data-ref="%s" data-value="%s" data-key="%s" data-side="%s" transform="translate(%.3f,%.3f) rotate(%.2f)">`,
		escape(fp.Reference), escape(fp.Reference), escape(fp.Value), escape(fp.Key),
		escape(stack.LayerName(int(fp.Layer.Index))),
		fp.Position.X.ToMM(), fp.Position.Y.ToMM(), fp.Rotation)
	// local pad bbox + 0.4 mm body
	if len(fp.Pads) > 0 {
		minX, minY := 1e9, 1e9
		maxX, maxY := -1e9, -1e9
		for i := range fp.Pads {
			p := &fp.Pads[i]
			ox, oy := p.Offset.X.ToMM(), p.Offset.Y.ToMM()
			hw, hh := p.Size[0].ToMM()/2, p.Size[1].ToMM()/2
			if ox-hw < minX {
				minX = ox - hw
			}
			if oy-hh < minY {
				minY = oy - hh
			}
			if ox+hw > maxX {
				maxX = ox + hw
			}
			if oy+hh > maxY {
				maxY = oy + hh
			}
		}
		minX -= 0.4
		minY -= 0.4
		maxX += 0.4
		maxY += 0.4
		fmt.Fprintf(b, `<rect class="fp-body" x="%.3f" y="%.3f" width="%.3f" height="%.3f" fill="rgba(255,255,255,0.01)" stroke="#8b949e" stroke-width="0.1"/>`,
			minX, minY, maxX-minX, maxY-minY)
	}
	for i := range fp.Pads {
		writePad(b, &fp.Pads[i], stack)
	}
	b.WriteString(`</g>`)
}

func padNet(pad *core.Pad) string {
	if pad.Net == nil {
		return ""
	}
	return *pad.Net
}

func writePad(b *strings.Builder, pad *core.Pad, stack core.LayerStackup) {
	cx, cy := pad.Offset.X.ToMM(), pad.Offset.Y.ToMM()
	w, h := pad.Size[0].ToMM(), pad.Size[1].ToMM()
	through := ""
	if pad.Drill != nil && *pad.Drill > 0 {
		through = ` data-through="1"`
	}
	name := pad.Name
	if name == "" {
		name = pad.Number
	}
	fmt.Fprintf(b, `<rect data-pad="%s" data-pad-name="%s" data-net="%s" data-layer="%s"%s x="%.3f" y="%.3f" width="%.3f" height="%.3f" fill="%s"/>`,
		escape(pad.Number), escape(name), escape(padNet(pad)),
		escape(stack.LayerName(int(pad.Layer.Index))), through,
		cx-w/2, cy-h/2, w, h, padFill(pad.Layer))
	if pad.Net != nil && isGND(*pad.Net) {
		fmt.Fprintf(b, `<rect pointer-events="none" x="%.3f" y="%.3f" width="%.3f" height="%.3f" fill="none" stroke="#ff2bd6" stroke-width="0.15"/>`,
			cx-w/2, cy-h/2, w, h)
	}
}

// writePadNames labels pads in world space so the human can hide the labels
// without hiding the copper under them. Prefers the pad's net name (Hand/Astra
// style) so WLP pads show GND/VSTOR rather than pin numbers.
func writePadNames(b *strings.Builder, board *core.Board) {
	b.WriteString(`<g data-layer="pad-names" pointer-events="none">`)
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			p := &fp.Pads[i]
			label := padNet(p)
			if label == "" {
				label = p.Name
			}
			if label == "" {
				label = p.Number
			}
			if label == "" {
				continue
			}
			pw, ph := core.PadWorldSize(fp, p)
			wmm, hmm := pw.ToMM(), ph.ToMM()
			c := core.PadWorldCenter(fp, p)
			cx, cy := c.X.ToMM(), c.Y.ToMM()
			chars := float64(len([]rune(label)))
			if chars < 1 {
				chars = 1
			}
			// Tiny pads (WLP balls ~0.25 mm) cannot hold readable text on-copper;
			// place a light label beside them (outboard of the footprint center).
			if wmm < 0.8 || hmm < 0.8 {
				sz := clampF(math.Min(0.50, math.Max(wmm, hmm)*1.8), 0.45, 0.55)
				fc := fp.Position
				dx := cx - fc.X.ToMM()
				gap := math.Max(wmm, hmm)/2 + 0.50
				if dx < 0 {
					fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="end" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="%.2f" fill="#f0e68c">%s</text></g>`,
						cx-gap, cy, sz, escape(label))
				} else {
					fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="start" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="%.2f" fill="#f0e68c">%s</text></g>`,
						cx+gap, cy, sz, escape(label))
				}
				continue
			}
			cap := math.Min(wmm, hmm) * 0.55
			cap = clampF(cap, 0.30, 1.0)
			byW := clampF(wmm/chars*1.4, 0.30, 1.0)
			sz := math.Min(cap, byW)
			fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="%.2f" fill="#0e1116">%s</text></g>`,
				cx, cy, sz, escape(label))
		}
	}
	b.WriteString(`</g>`)
}

func clampF(v, lo, hi float64) float64 {
	if v < lo {
		return lo
	}
	if v > hi {
		return hi
	}
	return v
}

func writeSilk(b *strings.Builder, board *core.Board) {
	b.WriteString(`<g data-layer="silk" pointer-events="none">`)
	b.WriteString(`<g stroke="#e6edf3" stroke-linecap="round" fill="none">`)
	for _, ln := range board.SilkLines {
		fmt.Fprintf(b, `<line x1="%.3f" y1="%.3f" x2="%.3f" y2="%.3f" stroke-width="%.3f"/>`,
			ln.Start.X.ToMM(), ln.Start.Y.ToMM(), ln.End.X.ToMM(), ln.End.Y.ToMM(), ln.Width.ToMM())
	}
	b.WriteString(`</g>`)
	for _, t := range board.SilkTexts {
		sz := t.Size.ToMM()
		if sz < 0.4 {
			sz = 0.9
		}
		fmt.Fprintf(b, `<g transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" dominant-baseline="middle" font-family="ui-monospace, monospace" font-size="%.2f" fill="#e6edf3">%s</text></g>`,
			t.Position.X.ToMM(), t.Position.Y.ToMM(), sz, escape(t.Text))
	}
	// Default REF if the footprint has no silk text.
	for _, fp := range footprintsStable(board) {
		if fp.Reference == "" {
			continue
		}
		has := false
		for _, s := range fp.Silk {
			if s.Kind == "text" && strings.Contains(s.Text, "{REF}") {
				has = true
				break
			}
		}
		if has {
			// still draw resolved REF from silk items
			for _, s := range fp.Silk {
				if s.Kind != "text" {
					continue
				}
				txt := core.ResolveSilkText(fp, s.Text)
				wx := fp.Position.X.ToMM() + s.Position.X.ToMM()
				wy := fp.Position.Y.ToMM() + s.Position.Y.ToMM()
				sz := s.Size.ToMM()
				if sz < 0.4 {
					sz = 0.9
				}
				fmt.Fprintf(b, `<g data-ref-label="%s" transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="%.2f" fill="#e6edf3">%s</text></g>`,
					escape(fp.Reference), wx, wy, sz, escape(txt))
			}
			continue
		}
		fmt.Fprintf(b, `<g data-ref-label="%s" transform="translate(%.3f,%.3f) scale(1,-1)"><text x="0" y="0" text-anchor="middle" font-family="ui-monospace, monospace" font-size="0.9" fill="#e6edf3">%s</text></g>`,
			escape(fp.Reference), fp.Position.X.ToMM(), fp.Position.Y.ToMM()+2.2, escape(fp.Reference))
	}
	b.WriteString(`</g>`)
}

// RatLine is one unrouted connection between two pad centres, in mm.
type RatLine struct {
	Net string  `json:"net"`
	X1  float64 `json:"x1_mm"`
	Y1  float64 `json:"y1_mm"`
	X2  float64 `json:"x2_mm"`
	Y2  float64 `json:"y2_mm"`
}

// Ratsnest returns the still-unrouted connections: a per-net MST over the pad
// centres of every multi-pad net that has no copper and no pour yet. Cheap on
// purpose — it is a hint about what is left, not a connectivity solver.
func Ratsnest(board *core.Board) []RatLine {
	if board == nil {
		return nil
	}
	routed := map[string]bool{}
	for _, t := range board.Traces {
		routed[t.Net] = true
	}
	for _, p := range board.Pours {
		routed[p.Net] = true
	}
	type pt struct{ x, y float64 }
	nets := map[string][]pt{}
	for _, fp := range footprintsStable(board) {
		for i := range fp.Pads {
			net := padNet(&fp.Pads[i])
			if net == "" || routed[net] {
				continue
			}
			c := core.PadWorldCenter(fp, &fp.Pads[i])
			nets[net] = append(nets[net], pt{c.X.ToMM(), c.Y.ToMM()})
		}
	}
	names := make([]string, 0, len(nets))
	for n := range nets {
		if len(nets[n]) >= 2 {
			names = append(names, n)
		}
	}
	sort.Strings(names)
	var out []RatLine
	for _, name := range names {
		ps := nets[name]
		// Prim over pad centres: O(n²) on a handful of pads per net.
		inTree := make([]bool, len(ps))
		inTree[0] = true
		for k := 1; k < len(ps); k++ {
			bi, bj, bd := -1, -1, math.Inf(1)
			for i := range ps {
				if !inTree[i] {
					continue
				}
				for j := range ps {
					if inTree[j] {
						continue
					}
					dx, dy := ps[i].x-ps[j].x, ps[i].y-ps[j].y
					if d := dx*dx + dy*dy; d < bd {
						bi, bj, bd = i, j, d
					}
				}
			}
			if bj < 0 {
				break
			}
			inTree[bj] = true
			out = append(out, RatLine{Net: name, X1: ps[bi].x, Y1: ps[bi].y, X2: ps[bj].x, Y2: ps[bj].y})
		}
	}
	return out
}

func writeRatsnest(b *strings.Builder, board *core.Board) {
	b.WriteString(`<g data-layer="ratsnest" pointer-events="none" stroke="#7d8da8" stroke-width="0.07" stroke-dasharray="0.45 0.35" opacity="0.85">`)
	for _, r := range Ratsnest(board) {
		fmt.Fprintf(b, `<line data-net="%s" x1="%.3f" y1="%.3f" x2="%.3f" y2="%.3f"/>`,
			escape(r.Net), r.X1, r.Y1, r.X2, r.Y2)
	}
	b.WriteString(`</g>`)
}

func writeMarkers(b *strings.Builder, ms []Marker) {
	b.WriteString(`<g data-layer="drc" pointer-events="none">`)
	for _, m := range ms {
		if m.XMM == 0 && m.YMM == 0 {
			continue // no location: the panel lists it, the canvas cannot point at it
		}
		stroke := "#ff6b6b"
		if m.Severity == "warning" {
			stroke = "#e3b341"
		}
		fmt.Fprintf(b, `<g data-marker="%s" data-severity="%s" data-kind="%s" data-net="%s" transform="translate(%.3f,%.3f)"><circle r="0.9" fill="none" stroke="%s" stroke-width="0.14"/><line x1="-1.4" y1="0" x2="1.4" y2="0" stroke="%s" stroke-width="0.1"/><line x1="0" y1="-1.4" x2="0" y2="1.4" stroke="%s" stroke-width="0.1"/></g>`,
			escape(m.ID), escape(m.Severity), escape(m.Kind), escape(m.Net), m.XMM, m.YMM, stroke, stroke, stroke)
	}
	b.WriteString(`</g>`)
}

func padFill(l core.Layer) string {
	i := int(l.Index)
	if i >= 0 && i < len(padPalette) {
		return padPalette[i]
	}
	return "#888"
}

func traceStroke(l core.Layer) string {
	i := int(l.Index)
	if i >= 0 && i < len(tracePalette) {
		return tracePalette[i]
	}
	return "#aaa"
}

func isGND(n string) bool {
	u := strings.ToUpper(strings.TrimSpace(n))
	return u == "GND" || u == "GROUND" || u == "VSS" || u == "0V" || u == "AGND" || strings.HasPrefix(u, "GND")
}

func escape(s string) string {
	s = strings.ReplaceAll(s, "&", "&amp;")
	s = strings.ReplaceAll(s, "<", "&lt;")
	s = strings.ReplaceAll(s, ">", "&gt;")
	s = strings.ReplaceAll(s, `"`, "&quot;")
	return s
}
