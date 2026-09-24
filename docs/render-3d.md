# 3D product shot

`fragua render` writes a static PNG of the board in the current project file.
It is the picture you put in a blog post or a handoff: an angled view with
board thickness, soldermask, copper, silkscreen, holes, and boxy component
bodies. The agent loop still watches the flat SVG canvas at `GET /screenshot`.

```bash
fragua render --3d path/to/board.fragua -o board.png --width 1600
fragua render3d path/to/board.fragua -o board.png   # same command
```

| Flag | Meaning |
|------|---------|
| `--3d` | Product-shot mode. It is the only mode; the flag is explicit so the invocation reads clearly. |
| `-o`, `--out` | PNG path. Default: `<file>-3d.png` in the current directory. |
| `--width` | Image width in pixels. Default 1600. |
| `--height` | Image height. Default follows the board aspect. |

What the picture contains:

- The outline, including rounded corners, extruded to the stackup thickness (about 1.6 mm on a 2-layer board) with a soldermask chamfer and an FR-4 edge.
- Top-layer traces, pads, and vias. Pad drills and mounting holes read as dark openings.
- Silkscreen lines and Hershey text, including reference designators. Text that lands on a body is drawn on the package.
- A box for each top-side part, sized from the pads: chip passives between their terminals, gull-wing bodies for SOIC/SOT, a low black body for QFN, plastic plus pins for headers. There is no STEP model.

The renderer is a software z-buffer in `internal/render`. It does not shell out, embed a browser, or depend on a GPU. It is not a mechanical CAD view: parts are procedural boxes, and the camera is a fixed three-quarter product angle.
