# 3D product shot

`fragua render` writes a static PNG of the board in the current project file.
It is the picture you put in a blog post or a handoff: an angled view with
board thickness, soldermask, copper, silkscreen, holes, and a body on each
part. The agent loop still watches the flat SVG canvas at `GET /screenshot`.

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
| `--models` | `kicad` (default), `easyeda`, `kicad,easyeda`, or `none`. |
| `--model-cache` | Directory for downloaded models. Default: `$XDG_CACHE_HOME/fragua/3d` or `~/.cache/fragua/3d`. |
| `--offline` | Do not download. Use the cache and a local KiCad 3D library only. A miss becomes a box. |

What the picture contains:

- The outline, including rounded corners, extruded to the stackup thickness (about 1.6 mm on a 2-layer board) with a soldermask chamfer and an FR-4 edge.
- Top-layer traces, pads, and vias. Pad drills and mounting holes read as dark openings.
- Silkscreen lines and Hershey text, including reference designators. Text that lands on a body is drawn on the package.
- A 3D body for each part. Top side is drawn; a bottom-side part is drawn hanging below the board when a model loads.

## Models

The default source is the KiCad 3D library
[kicad-packages3D](https://gitlab.com/kicad/libraries/kicad-packages3D).
Fragua loads the `.wrl` files (they carry materials). STEP is not tessellated.
GitLab's current `master` publishes STEP only; the WRL files are still served
by the GitHub mirror of the same library
(`https://raw.githubusercontent.com/KiCad/kicad-packages3D/master/`).
A file that contains `Transform { scale 0.3937… }` is stored in millimetres;
a StepUp export without that node is stored in KiCad's historical 0.1-inch
VRML unit. Both are scaled to millimetres. Vertical pin-headers are anchored
on pad 1, because that is where the KiCad model puts its origin.

Nothing from that library is vendored here. The first render downloads the
models a board actually uses into the cache. A KiCad install is used first
when `KICAD9_3DMODEL_DIR`, `KICAD8_3DMODEL_DIR`, `KICAD7_3DMODEL_DIR`,
`KISYS3DMOD`, or `/usr/share/kicad/3dmodels` contains the file.

Footprint keys that already follow KiCad names map directly
(`r_0603` → `Resistor_SMD.3dshapes/R_0603_1608Metric.wrl`,
`header_1x10_2.54mm` → `PinHeader_1x10_P2.54mm_Vertical.wrl`).
Keys that do not (`rp2040_qfn56`, `w25q16_soic8`, `xtal_3225`, …) are listed
in an explicit table. `model=` on `palette` or `lib`, or the footprint JSON
field `model`, overrides the path with a KiCad library-relative `.wrl` or a
local `.wrl`/`.obj`.

USB-C receptacles in the published library are STEP-only, so `usbc_16p` stays
a box. `sw_smd_4x4` uses `SW_SPST_B3U-1000P.wrl`, a smaller tactile switch
that is actually present as WRL; the 4×4 outline is still the footprint pads.

### License

KiCad 3D models are [CC-BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/)
with the [KiCad Libraries Exception](https://github.com/KiCad/kicad-packages3D/blob/master/LICENSE.md).
Using them in a design, and the PNG this command writes, does not oblige you
to share the design. Redistributing the model collection itself stays
CC-BY-SA and needs attribution. Each cached `.wrl` repeats the license header.
Do not commit the cache into a project repo.

### EasyEDA (opt-in)

`--models=easyeda` (or `kicad,easyeda`) fetches an OBJ from EasyEDA when the
footprint has an LCSC id. The component JSON and `https://modules.easyeda.com/3dmodel/<uuid>`
endpoints are unofficial and the terms are unclear, so this is off unless you
ask for it, it is cached, and a failure falls through to KiCad or a box.
Those models are not redistributed with Fragua.

### Fallback

A missing path, a failed download, or `--offline` with an empty cache draws
the procedural box (chip between its terminals, gull-wing body, header pins).
The PNG is still written. The command prints a count:

```text
models: 35 kicad, 0 easyeda, 0 file, 1 box
  J1 usbc_16p: box (no KiCad model for "usbc_16p")
```

`--models=none` is boxes only, and what CI uses. Tests never download.

The renderer is a software z-buffer in `internal/render`. It does not shell
out, embed a browser, or depend on a GPU. The camera is a fixed three-quarter
product angle.
