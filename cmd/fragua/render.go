package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mentasystems/fragua/internal/core"
	"github.com/mentasystems/fragua/internal/render"
)

// renderValueFlags are flags whose value may be a separate argument.
var renderValueFlags = map[string]bool{
	"o": true, "out": true, "width": true, "height": true,
	"models": true, "model-cache": true,
}

// runRender is `fragua render [--3d] FILE [-o out.png] [--width PX]`.
// --3d is the only mode; the flag is accepted so the product-shot invocation
// reads the way agents expect.
func runRender(args []string) error {
	file, flags := "", []string(nil)
	found, wantValue := false, false
	for _, a := range args {
		switch {
		case wantValue:
			wantValue = false
		case strings.HasPrefix(a, "-"):
			name := strings.TrimLeft(strings.SplitN(a, "=", 2)[0], "-")
			wantValue = !strings.Contains(a, "=") && renderValueFlags[name]
		case !found:
			file, found = a, true
			continue
		default:
			return fmt.Errorf("usage: fragua render [--3d] FILE [-o out.png] [--width PX] [--models kicad|easyeda|none]")
		}
		flags = append(flags, a)
	}
	fs := flag.NewFlagSet("render", flag.ContinueOnError)
	fs.SetOutput(os.Stderr)
	_ = fs.Bool("3d", true, "angled product-shot render (the default)")
	outShort := fs.String("o", "", "output PNG path")
	outLong := fs.String("out", "", "output PNG path")
	width := fs.Int("width", 1600, "image width in pixels")
	height := fs.Int("height", 0, "image height in pixels (0: match the board)")
	models := fs.String("models", "kicad", "model source: kicad, easyeda, kicad,easyeda, or none")
	cache := fs.String("model-cache", "", "directory for downloaded 3D models")
	offline := fs.Bool("offline", false, "do not download models; use the cache and a local KiCad install")
	if err := fs.Parse(flags); err != nil {
		return err
	}
	if file == "" {
		return fmt.Errorf("usage: fragua render [--3d] FILE [-o out.png] [--width PX] [--models kicad|easyeda|none]")
	}
	p, err := core.LoadFromPath(file)
	if err != nil {
		return err
	}
	var report []render.ModelUse
	pngBytes, err := render.BoardPNG3D(p.Board(), render.Shot3D{
		Width: *width, Height: *height, Samples: 2,
		Models: *models, CacheDir: *cache, Offline: *offline,
		Report: &report,
	})
	if err != nil {
		return err
	}
	out := *outShort
	if out == "" {
		out = *outLong
	}
	if out == "" {
		base := strings.TrimSuffix(filepath.Base(file), filepath.Ext(file))
		out = base + "-3d.png"
	}
	if dir := filepath.Dir(out); dir != "" && dir != "." {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	if err := os.WriteFile(out, pngBytes, 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d bytes)\n", out, len(pngBytes))
	if *models != "" {
		fmt.Println(render.SummarizeModels(report))
	}
	return nil
}
