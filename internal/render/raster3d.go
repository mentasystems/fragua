package render

import (
	"bytes"
	"image"
	"image/png"
	"math"
)

// Software z-buffer for the static product shot. Flat-shaded triangles, no
// GPU and no cgo: the same static binary that writes Gerbers can write a PNG.

type vec2 struct{ x, y float64 }
type vec3 struct{ x, y, z float64 }

func (a vec3) add(b vec3) vec3 { return vec3{a.x + b.x, a.y + b.y, a.z + b.z} }
func (a vec3) sub(b vec3) vec3 { return vec3{a.x - b.x, a.y - b.y, a.z - b.z} }
func (a vec3) scale(s float64) vec3 {
	return vec3{a.x * s, a.y * s, a.z * s}
}
func (a vec3) dot(b vec3) float64 { return a.x*b.x + a.y*b.y + a.z*b.z }
func (a vec3) cross(b vec3) vec3 {
	return vec3{a.y*b.z - a.z*b.y, a.z*b.x - a.x*b.z, a.x*b.y - a.y*b.x}
}
func (a vec3) len() float64  { return math.Sqrt(a.dot(a)) }
func (a vec3) len2() float64 { return a.dot(a) }
func (a vec3) norm() vec3 {
	l := a.len()
	if l < 1e-12 {
		return vec3{}
	}
	return a.scale(1 / l)
}

// fcol is linear-light RGB in 0..1.
type fcol struct{ r, g, b float64 }

func srgb8(r, g, b uint8) fcol {
	return fcol{lin(float64(r) / 255), lin(float64(g) / 255), lin(float64(b) / 255)}
}

func lin(c float64) float64 {
	if c <= 0.04045 {
		return c / 12.92
	}
	return math.Pow((c+0.055)/1.055, 2.4)
}

func enc(c float64) uint8 {
	if c <= 0 {
		return 0
	}
	if c >= 1 {
		c = 1
	}
	var e float64
	if c <= 0.0031308 {
		e = 12.92 * c
	} else {
		e = 1.055*math.Pow(c, 1/2.4) - 0.055
	}
	return uint8(math.Round(e * 255))
}

type face struct {
	a, b, c vec3
	albedo  fcol
	metal   float64
	bias    float32 // mm subtracted from view depth so coplanar layers stack stably
}

type mesh struct {
	faces []face
}

func (m *mesh) tri(a, b, c vec3, albedo fcol, metal, bias float64) {
	n := b.sub(a).cross(c.sub(a))
	if n.len2() < 1e-14 {
		return
	}
	m.faces = append(m.faces, face{a: a, b: b, c: c, albedo: albedo, metal: metal, bias: float32(bias)})
}

func (m *mesh) quad(a, b, c, d vec3, albedo fcol, metal, bias float64) {
	m.tri(a, b, c, albedo, metal, bias)
	m.tri(a, c, d, albedo, metal, bias)
}

type camera struct {
	eye, forward, right, up vec3
	fovY                    float64
	w, h                    int
}

func (c camera) project(p vec3) (sx, sy, depth float64, ok bool) {
	rel := p.sub(c.eye)
	depth = rel.dot(c.forward)
	if depth < 0.4 {
		return 0, 0, depth, false
	}
	tanY := math.Tan(c.fovY / 2)
	aspect := float64(c.w) / float64(c.h)
	ndcX := rel.dot(c.right) / (depth * tanY * aspect)
	ndcY := rel.dot(c.up) / (depth * tanY)
	sx = (ndcX*0.5 + 0.5) * float64(c.w)
	sy = (0.5 - ndcY*0.5) * float64(c.h)
	return sx, sy, depth, true
}

// shade is a two-light Lambert plus a tight metal specular. Normals flip
// toward the camera so thin ribbons (silk, traces) light from either side.
func shade(a, b, c, eye vec3, albedo fcol, metal float64) (uint8, uint8, uint8) {
	n := b.sub(a).cross(c.sub(a)).norm()
	ctr := a.add(b).add(c).scale(1.0 / 3)
	view := eye.sub(ctr).norm()
	if n.dot(view) < 0 {
		n = n.scale(-1)
	}
	key := vec3{0.32, -0.48, 0.82}.norm()
	fill := vec3{-0.55, 0.15, 0.45}.norm()
	lambert := 0.40 + 0.62*clamp01(n.dot(key)) + 0.16*clamp01(n.dot(fill))
	half := key.add(view).norm()
	spec := math.Pow(clamp01(n.dot(half)), 48) * metal
	return enc(albedo.r*lambert + spec*0.9),
		enc(albedo.g*lambert + spec*0.85),
		enc(albedo.b*lambert + spec*0.7)
}

func clamp01(v float64) float64 {
	if v < 0 {
		return 0
	}
	if v > 1 {
		return 1
	}
	return v
}

type shot struct {
	cam    camera
	shadow shadow
}

// shadow is a contact ellipse on the table, in board millimetres.
// ax and ay are world-space vectors from center out to the ellipse edge.
type shadow struct {
	center vec3
	ax, ay vec3
	ok     bool
}

func renderMesh(m mesh, s shot, samples int) *image.RGBA {
	if samples < 1 {
		samples = 1
	}
	if samples > 3 {
		samples = 3
	}
	W := s.cam.w * samples
	H := s.cam.h * samples
	hiCam := s.cam
	hiCam.w, hiCam.h = W, H
	img := image.NewRGBA(image.Rect(0, 0, W, H))
	fillStudio(img)
	paintShadow(img, hiCam, s.shadow)
	zbuf := make([]float32, W*H)
	for i := range zbuf {
		zbuf[i] = 1e30
	}
	for i := range m.faces {
		drawFace(img, zbuf, hiCam, &m.faces[i])
	}
	if samples == 1 {
		return img
	}
	return downsample(img, s.cam.w, s.cam.h, samples)
}

func fillStudio(img *image.RGBA) {
	w, h := img.Bounds().Dx(), img.Bounds().Dy()
	// Light cyclorama, deliberately not the dark 2D canvas, so a product
	// shot reads as a different picture even at thumbnail size.
	top := [3]float64{236, 238, 242}
	bot := [3]float64{196, 201, 210}
	for y := 0; y < h; y++ {
		t := 0.0
		if h > 1 {
			t = float64(y) / float64(h-1)
		}
		r := uint8(top[0] + (bot[0]-top[0])*t)
		g := uint8(top[1] + (bot[1]-top[1])*t)
		b := uint8(top[2] + (bot[2]-top[2])*t)
		for x := 0; x < w; x++ {
			i := img.PixOffset(x, y)
			img.Pix[i] = r
			img.Pix[i+1] = g
			img.Pix[i+2] = b
			img.Pix[i+3] = 255
		}
	}
}

func paintShadow(img *image.RGBA, cam camera, sh shadow) {
	if !sh.ok {
		return
	}
	cx, cy, _, okC := cam.project(sh.center)
	axx, axy, _, okA := cam.project(sh.center.add(sh.ax))
	ayx, ayy, _, okB := cam.project(sh.center.add(sh.ay))
	if !okC || !okA || !okB {
		return
	}
	ax := vec2{axx - cx, axy - cy}
	ay := vec2{ayx - cx, ayy - cy}
	w, h := img.Bounds().Dx(), img.Bounds().Dy()
	ext := math.Abs(ax.x) + math.Abs(ay.x)
	eyt := math.Abs(ax.y) + math.Abs(ay.y)
	minX := int(math.Floor(cx - ext))
	maxX := int(math.Ceil(cx + ext))
	minY := int(math.Floor(cy - eyt))
	maxY := int(math.Ceil(cy + eyt))
	if minX < 0 {
		minX = 0
	}
	if minY < 0 {
		minY = 0
	}
	if maxX >= w {
		maxX = w - 1
	}
	if maxY >= h {
		maxY = h - 1
	}
	den := ax.x*ay.y - ax.y*ay.x
	if math.Abs(den) < 1e-6 {
		return
	}
	for y := minY; y <= maxY; y++ {
		for x := minX; x <= maxX; x++ {
			px := float64(x) + 0.5 - cx
			py := float64(y) + 0.5 - cy
			u := (px*ay.y - py*ax.y) / den
			v := (ax.x*py - ay.x*px) / den
			r2 := u*u + v*v
			if r2 >= 1 {
				continue
			}
			k := (1 - r2)
			k = k * k * 0.45
			i := img.PixOffset(x, y)
			img.Pix[i] = uint8(float64(img.Pix[i]) * (1 - k))
			img.Pix[i+1] = uint8(float64(img.Pix[i+1]) * (1 - k))
			img.Pix[i+2] = uint8(float64(img.Pix[i+2]) * (1 - k))
		}
	}
}

func drawFace(img *image.RGBA, zbuf []float32, cam camera, f *face) {
	x0, y0, z0, ok0 := cam.project(f.a)
	x1, y1, z1, ok1 := cam.project(f.b)
	x2, y2, z2, ok2 := cam.project(f.c)
	if !ok0 || !ok1 || !ok2 {
		return
	}
	minX := int(math.Floor(math.Min(x0, math.Min(x1, x2))))
	maxX := int(math.Ceil(math.Max(x0, math.Max(x1, x2))))
	minY := int(math.Floor(math.Min(y0, math.Min(y1, y2))))
	maxY := int(math.Ceil(math.Max(y0, math.Max(y1, y2))))
	w, h := img.Bounds().Dx(), img.Bounds().Dy()
	if maxX < 0 || maxY < 0 || minX >= w || minY >= h {
		return
	}
	if minX < 0 {
		minX = 0
	}
	if minY < 0 {
		minY = 0
	}
	if maxX >= w {
		maxX = w - 1
	}
	if maxY >= h {
		maxY = h - 1
	}
	den := edge(x1, y1, x2, y2, x0, y0)
	if math.Abs(den) < 1e-8 {
		return
	}
	invDen := 1 / den
	cr, cg, cb := shade(f.a, f.b, f.c, cam.eye, f.albedo, f.metal)
	iz0, iz1, iz2 := 1/z0, 1/z1, 1/z2
	// Incremental edge functions. E01(p) = (px-x0)*(y1-y0) - (py-y0)*(x1-x0).
	// Barycentric w2 = E01(p)/E01(v2), etc. Stepping x adds (y1-y0).
	for y := minY; y <= maxY; y++ {
		py := float64(y) + 0.5
		row := y * w
		for x := minX; x <= maxX; x++ {
			px := float64(x) + 0.5
			w0 := edge(x1, y1, x2, y2, px, py) * invDen
			w1 := edge(x2, y2, x0, y0, px, py) * invDen
			w2 := 1 - w0 - w1
			// Small overlap so shared edges do not leave cracks.
			if w0 < -1e-3 || w1 < -1e-3 || w2 < -1e-3 {
				continue
			}
			iz := w0*iz0 + w1*iz1 + w2*iz2
			if iz <= 1e-8 {
				continue
			}
			z := float32(1/iz) - f.bias
			i := row + x
			if z >= zbuf[i] {
				continue
			}
			zbuf[i] = z
			p := img.PixOffset(x, y)
			img.Pix[p] = cr
			img.Pix[p+1] = cg
			img.Pix[p+2] = cb
			img.Pix[p+3] = 255
		}
	}
}

func edge(ax, ay, bx, by, px, py float64) float64 {
	return (px-ax)*(by-ay) - (py-ay)*(bx-ax)
}

func downsample(src *image.RGBA, w, h, s int) *image.RGBA {
	dst := image.NewRGBA(image.Rect(0, 0, w, h))
	n := s * s
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			var r, g, b int
			for dy := 0; dy < s; dy++ {
				for dx := 0; dx < s; dx++ {
					i := src.PixOffset(x*s+dx, y*s+dy)
					r += int(src.Pix[i])
					g += int(src.Pix[i+1])
					b += int(src.Pix[i+2])
				}
			}
			j := dst.PixOffset(x, y)
			dst.Pix[j] = uint8(r / n)
			dst.Pix[j+1] = uint8(g / n)
			dst.Pix[j+2] = uint8(b / n)
			dst.Pix[j+3] = 255
		}
	}
	return dst
}

func encodePNG(img *image.RGBA) ([]byte, error) {
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}
