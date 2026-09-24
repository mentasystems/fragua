package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestRunRenderWritesPNG(t *testing.T) {
	out := filepath.Join(t.TempDir(), "board-3d.png")
	err := runRender([]string{
		"--3d",
		filepath.Join("..", "..", "stress", "rp2040-minimal.fragua"),
		"-o", out,
		"--width", "280",
		"--models", "none",
	})
	if err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	if len(raw) < 8 || string(raw[:8]) != "\x89PNG\r\n\x1a\n" {
		t.Fatal("render did not write a PNG")
	}
}
