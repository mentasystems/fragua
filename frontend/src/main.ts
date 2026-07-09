import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type PaletteItem = {
  reference: string;
  value: string;
  library: string;
  pad_count: number;
};

type Outline = { x_mm: number; y_mm: number; w_mm: number; h_mm: number };

type PhotoOverlay = {
  reference: string;
  key: string;
  attachment_id: string;
  x_mm: number;
  y_mm: number;
  rotation_deg: number;
  side: string;
  image_w_px: number;
  image_h_px: number;
  transform: { scale_mm_per_px: number; rotation_deg: number; tx_mm: number; ty_mm: number };
  matrix: [number, number, number, number, number, number];
};

type ProjectState = {
  name: string;
  footprint_count: number;
  symbol_count: number;
  net_count: number;
  palette_count: number;
  palette: PaletteItem[];
  api_addr: string;
  board_svg: string;
  schematic_svg: string;
  outline: Outline | null;
  photo_overlays: PhotoOverlay[];
};

type ActivityEvent = {
  kind: "Activity";
  level: "info" | "warn" | "error";
  message: string;
};

type AnyEvent =
  | ActivityEvent
  | { kind: "ProjectChanged" }
  | { kind: "FootprintAdded"; reference: string }
  | { kind: "FootprintMoved" }
  | { kind: "FootprintRemoved" }
  | { kind: "OutlineChanged" }
  | { kind: "SymbolAdded"; reference: string }
  | { kind: "NetChanged" }
  | { kind: "RoutingChanged" }
  | { kind: "PlacementProgress" }
  | { kind: "PaletteChanged" }
  | { kind: "LibraryChanged"; count: number }
  | { kind: "PendingLibraryChanged"; count: number };

type ViewTransform = {
  rotation_deg: number;
  flip_h: boolean;
  flip_v: boolean;
};

type PlacementMargin = {
  top_mm: number;
  right_mm: number;
  bottom_mm: number;
  left_mm: number;
};

type PhotoCalibration = {
  a_px: [number, number];
  b_px: [number, number];
  a_pad: string;
  b_pad: string;
};

type LibraryAttachment = {
  id: string;
  kind: string;
  filename: string;
  mime: string;
  added_at: number;
  view_transform?: ViewTransform;
  calibration?: PhotoCalibration | null;
};

type BodyRect = {
  min_x_mm: number;
  min_y_mm: number;
  max_x_mm: number;
  max_y_mm: number;
};

type ReviewPad = { number: string; x_mm: number; y_mm: number };

type LibraryEntry = {
  key: string;
  description: string;
  default_value: string;
  default_rotation_deg: number;
  edge_mounted: boolean;
  pad_count: number;
  attachments: LibraryAttachment[];
  created_at: number;
};

type View = "board" | "schematic" | "review";

type ReviewEntry = {
  key: string;
  description: string;
  default_value: string;
  edge_mounted: boolean;
  pad_count: number;
  ground_pad_count: number;
  lcsc_id: string | null;
  mpn: string | null;
  attachments: LibraryAttachment[];
  created_at: number;
  review_svg: string;
  footprint_view_transform: ViewTransform;
  placement_margin: PlacementMargin;
  body_rect: BodyRect | null;
  pad_bbox: BodyRect | null;
  pads: ReviewPad[];
};

type PendingPad = {
  number: string;
  name: string;
  x_mm: number;
  y_mm: number;
  w_mm: number;
  h_mm: number;
  drill_mm: number | null;
  is_ground: boolean;
};

type PendingAttachment = {
  kind: string;
  filename: string;
  mime: string;
  data_uri: string;
  bytes: number;
};

type PendingEntry = {
  key: string;
  description: string;
  default_value: string;
  default_rotation_deg: number;
  edge_mounted: boolean;
  pad_count: number;
  ground_pad_count: number;
  lcsc_id: string | null;
  mpn: string | null;
  attachments: PendingAttachment[];
  review_svg: string;
  pads: PendingPad[];
};

const root = document.getElementById("app");
if (!root) throw new Error("missing #app root");

root.innerHTML = `
  <div class="topbar">
    <span class="label">project</span><span class="value" id="proj-name">—</span>
    <span class="tabs">
      <span class="tab" data-view="schematic" id="tab-sch">schematic <span id="proj-symbols">0</span>/<span id="proj-nets">0</span></span>
      <span class="tab" data-view="board" id="tab-board">board <span id="proj-footprints">0</span></span>
      <span class="tab" data-view="review" id="tab-review" title="review every library component side-by-side with its photo">review <span id="proj-review-count">0</span></span>
    </span>
    <span class="board-size">
      <span class="label">size</span>
      <span class="value" id="board-w">—</span>
      <span class="label">×</span>
      <span class="value" id="board-h">—</span>
      <span class="label">mm</span>
    </span>
    <span class="spacer"></span>
    <span class="tab" id="toggle-photos" title="show/hide real-scale component photos on the board" hidden>photos</span>
    <span class="tab" id="toggle-library" title="show/hide library panel">lib</span>
    <span class="tab" id="toggle-activity" title="show/hide activity log">log</span>
    <span class="label">api</span><span class="value accent" id="proj-api">—</span>
  </div>
  <div class="palette-strip" id="palette-strip">
    <button id="autoroute-btn" class="autoroute-btn">Auto Routing</button>
    <button id="stitch-btn" class="autoroute-btn" title="tie floating plane pads to their pour with vias">Stitch GND</button>
    <button id="jlcpcb-btn" class="jlcpcb-btn">JLCPCB</button>
    <button id="odb-btn" class="odb-btn">ODB++</button>
    <span id="autoroute-status" class="autoroute-status"></span>
    <div class="palette-chips" id="palette-chips"></div>
  </div>
  <div class="canvas-pane" id="canvas-pane"></div>
  <div class="activity-pane">
    <h2>activity</h2>
    <div class="activity-log" id="activity-log"></div>
  </div>
  <div class="library-pane" id="library-pane">
    <h2>library <span id="library-count" class="value">0</span></h2>
    <div class="library-list" id="library-list"></div>
  </div>
  <div class="info-modal" id="info-modal" hidden>
    <div class="info-modal-card" id="info-modal-card"></div>
  </div>
  <div class="confirm-modal" id="confirm-modal" hidden>
    <div class="confirm-modal-card" id="confirm-modal-card"></div>
  </div>
  <div class="calib-modal" id="calib-modal" hidden>
    <div class="calib-modal-card" id="calib-modal-card"></div>
  </div>
`;

const els = {
  name: document.getElementById("proj-name")!,
  symbols: document.getElementById("proj-symbols")!,
  nets: document.getElementById("proj-nets")!,
  footprints: document.getElementById("proj-footprints")!,
  api: document.getElementById("proj-api")!,
  canvas: document.getElementById("canvas-pane")!,
  log: document.getElementById("activity-log")!,
  library: document.getElementById("library-list")!,
  libraryCount: document.getElementById("library-count")!,
  tabBoard: document.getElementById("tab-board")!,
  tabSch: document.getElementById("tab-sch")!,
  tabReview: document.getElementById("tab-review")!,
  reviewCount: document.getElementById("proj-review-count")!,
  toggleLibrary: document.getElementById("toggle-library")!,
  toggleActivity: document.getElementById("toggle-activity")!,
  palette: document.getElementById("palette-strip")!,
  paletteChips: document.getElementById("palette-chips")!,
  autorouteBtn: document.getElementById("autoroute-btn") as HTMLButtonElement,
  stitchBtn: document.getElementById("stitch-btn") as HTMLButtonElement,
  autorouteStatus: document.getElementById("autoroute-status")!,
  jlcpcbBtn: document.getElementById("jlcpcb-btn") as HTMLButtonElement,
  odbBtn: document.getElementById("odb-btn") as HTMLButtonElement,
  boardW: document.getElementById("board-w")!,
  boardH: document.getElementById("board-h")!,
  infoModal: document.getElementById("info-modal")!,
  infoCard: document.getElementById("info-modal-card")!,
  confirmModal: document.getElementById("confirm-modal")!,
  confirmCard: document.getElementById("confirm-modal-card")!,
  togglePhotos: document.getElementById("toggle-photos")!,
  calibModal: document.getElementById("calib-modal")!,
  calibCard: document.getElementById("calib-modal-card")!,
};

type DrcViolation = {
  kind: string;
  severity: string;
  message: string;
  x_mm: number;
  y_mm: number;
  involved: string[];
};

let view: View = "board";
let lastState: ProjectState | null = null;
let drcViolations: DrcViolation[] = [];

function setView(v: View) {
  view = v;
  els.tabBoard.classList.toggle("active", v === "board");
  els.tabSch.classList.toggle("active", v === "schematic");
  els.tabReview.classList.toggle("active", v === "review");
  if (v === "review") {
    void paintReview();
  } else if (lastState) {
    paintCanvas(lastState);
  }
}

// All control surface lives behind the agent now. Tabs stay clickable
// so the human can flip between the board and the schematic to watch,
// but every action (place, move, route, DRC, export, reset) goes
// through the local HTTP script API — no UI buttons.
els.tabBoard.addEventListener("click", () => setView("board"));
els.tabSch.addEventListener("click", () => setView("schematic"));
els.tabReview.addEventListener("click", () => setView("review"));

// Side-pane toggles. Default: both panels hidden — the agent log /
// library panes get in the way for most tasks; the human can flip them
// open with these tabs. Choice is remembered in localStorage.
function applyPaneToggle(key: "library" | "activity", visible: boolean) {
  const cls = key === "library" ? "hide-library" : "hide-activity";
  const tab = key === "library" ? els.toggleLibrary : els.toggleActivity;
  root!.classList.toggle(cls, !visible);
  tab.classList.toggle("active", visible);
  localStorage.setItem(`fragua.pane.${key}`, visible ? "1" : "0");
}
function readPanePref(key: "library" | "activity"): boolean {
  const stored = localStorage.getItem(`fragua.pane.${key}`);
  return stored === "1"; // default = hidden
}
applyPaneToggle("library", readPanePref("library"));
applyPaneToggle("activity", readPanePref("activity"));
els.toggleLibrary.addEventListener("click", () =>
  applyPaneToggle("library", root!.classList.contains("hide-library")),
);
els.toggleActivity.addEventListener("click", () =>
  applyPaneToggle("activity", root!.classList.contains("hide-activity")),
);

function appendActivity(level: string, message: string) {
  const entry = document.createElement("div");
  entry.className = `entry ${level}`;
  entry.innerHTML = `<span class="level"></span><span class="msg"></span>`;
  entry.querySelector(".level")!.textContent = level;
  entry.querySelector(".msg")!.textContent = message;
  els.log.appendChild(entry);
  els.log.scrollTop = els.log.scrollHeight;
}

function reportFatal(err: unknown) {
  const msg = err instanceof Error ? `${err.message}\n${err.stack ?? ""}` : String(err);
  appendActivity("error", msg);
  els.canvas.innerHTML = `<pre style="padding:12px;color:#f85149;white-space:pre-wrap;font-size:12px;">${msg}</pre>`;
  console.error(err);
}

/// Per-view viewBox state so a re-render (every project change) does
/// not blow away the user's pan/zoom. Lazily seeded from whatever the
/// renderer emits on first paint of each view.
type ViewBox = { x: number; y: number; w: number; h: number };
const viewBoxState: Record<View, ViewBox | null> = { board: null, schematic: null, review: null };

function paintCanvas(state: ProjectState) {
  els.canvas.innerHTML = view === "schematic" ? state.schematic_svg : state.board_svg;
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (svg) {
    // Capture or restore the per-view viewBox so the user's pan/zoom
    // survives the paint, even when the SVG itself got rebuilt server-side.
    const fresh = parseViewBox(svg.getAttribute("viewBox"));
    if (viewBoxState[view] && fresh) {
      applyViewBox(svg, viewBoxState[view]!);
    } else if (fresh) {
      viewBoxState[view] = fresh;
    }
    attachPanZoom(svg);
  }
  if (view === "board") {
    paintPhotoOverlays(state);
    paintDrcMarkers();
  }
}

function parseViewBox(s: string | null): ViewBox | null {
  if (!s) return null;
  const parts = s.split(/\s+/).map(parseFloat);
  if (parts.length !== 4 || parts.some(Number.isNaN)) return null;
  return { x: parts[0], y: parts[1], w: parts[2], h: parts[3] };
}

function applyViewBox(svg: SVGSVGElement, vb: ViewBox) {
  svg.setAttribute("viewBox", `${vb.x} ${vb.y} ${vb.w} ${vb.h}`);
}

/// Click-drag = pan, wheel = zoom around cursor, plain click on a
/// component → info modal. Operates directly on the SVG `viewBox` so
/// the pan/zoom is purely cosmetic — no reflow, no server roundtrip.
function attachPanZoom(svg: SVGSVGElement) {
  svg.style.cursor = "grab";

  svg.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    svg.setPointerCapture(ev.pointerId);
    svg.style.cursor = "grabbing";
    const start = viewBoxState[view] ?? parseViewBox(svg.getAttribute("viewBox"));
    if (!start) return;
    const px0 = ev.clientX;
    const py0 = ev.clientY;
    const rect = svg.getBoundingClientRect();
    const sx = start.w / rect.width;
    const sy = start.h / rect.height;
    // Walk up to the nearest <g data-board-ref> so a slow click on a
    // pad still resolves the parent component.
    const downTarget = ev.target as Element;
    const refOnDown = downTarget.closest?.("[data-board-ref]") as Element | null;
    let panned = false;
    const onMove = (e: PointerEvent) => {
      const dx = (e.clientX - px0) * sx;
      const dy = (e.clientY - py0) * sy;
      // 4 px threshold — small wobble during a click should not pan.
      if (!panned && Math.hypot(e.clientX - px0, e.clientY - py0) < 4) return;
      panned = true;
      const next: ViewBox = { x: start.x - dx, y: start.y - dy, w: start.w, h: start.h };
      viewBoxState[view] = next;
      applyViewBox(svg, next);
    };
    const onUp = () => {
      svg.removeEventListener("pointermove", onMove);
      svg.removeEventListener("pointerup", onUp);
      svg.removeEventListener("pointercancel", onUp);
      svg.style.cursor = "grab";
      if (!panned && refOnDown) {
        const ref = refOnDown.getAttribute("data-board-ref") ?? "";
        const key = refOnDown.getAttribute("data-library-key") ?? "";
        if (ref) void openComponentModal(ref, key);
      }
    };
    svg.addEventListener("pointermove", onMove);
    svg.addEventListener("pointerup", onUp);
    svg.addEventListener("pointercancel", onUp);
  });

  svg.addEventListener(
    "wheel",
    (ev) => {
      ev.preventDefault();
      const current = viewBoxState[view] ?? parseViewBox(svg.getAttribute("viewBox"));
      if (!current) return;
      const rect = svg.getBoundingClientRect();
      // Cursor in SVG units (anchor of the zoom).
      const fx = current.x + ((ev.clientX - rect.left) / rect.width) * current.w;
      const fy = current.y + ((ev.clientY - rect.top) / rect.height) * current.h;
      // Wheel up → zoom IN (smaller viewBox); wheel down → zoom OUT.
      const k = Math.exp(ev.deltaY * 0.0015);
      const minSpan = 1; // mm — don't zoom past ~1 mm per pane.
      const maxSpan = 5000;
      const newW = clamp(current.w * k, minSpan, maxSpan);
      const newH = clamp(current.h * k, minSpan, maxSpan);
      const next: ViewBox = {
        x: fx - ((fx - current.x) * newW) / current.w,
        y: fy - ((fy - current.y) * newH) / current.h,
        w: newW,
        h: newH,
      };
      viewBoxState[view] = next;
      applyViewBox(svg, next);
    },
    { passive: false },
  );
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

// --- Photo calibration geometry (mirrors pcb-core::library) -----------
// An SVG-style affine [a,b,c,d,e,f] maps (x,y) → (a·x+c·y+e, b·x+d·y+f).
type Affine = [number, number, number, number, number, number];

function applyAffine(m: Affine, x: number, y: number): [number, number] {
  return [m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]];
}

function invertAffine(m: Affine): Affine | null {
  const det = m[0] * m[3] - m[1] * m[2];
  if (Math.abs(det) < 1e-12) return null;
  const a = m[3] / det;
  const b = -m[1] / det;
  const c = -m[2] / det;
  const d = m[0] / det;
  return [a, b, c, d, -(a * m[4] + c * m[5]), -(b * m[4] + d * m[5])];
}

type Similarity = { scale: number; rot: number; tx: number; ty: number };

/// Derive the photo→board similarity (photo px, y-down → footprint-local
/// mm, y-up) from two pin correspondences. Mirrors
/// `pcb_core::derive_photo_transform`; returns null on degenerate input.
function derivePhotoTransform(
  aMm: [number, number],
  bMm: [number, number],
  aPx: [number, number],
  bPx: [number, number],
): Similarity | null {
  const dmx = bMm[0] - aMm[0];
  const dmy = bMm[1] - aMm[1];
  const dpx = bPx[0] - aPx[0];
  const dpy = -(bPx[1] - aPx[1]);
  const den = dpx * dpx + dpy * dpy;
  if (den < 1e-9) return null;
  if (dmx * dmx + dmy * dmy < 1e-12) return null;
  const cx = (dmx * dpx + dmy * dpy) / den;
  const cy = (dmy * dpx - dmx * dpy) / den;
  const scale = Math.hypot(cx, cy);
  const rot = (Math.atan2(cy, cx) * 180) / Math.PI;
  const ax = aPx[0];
  const ay = -aPx[1];
  return { scale, rot, tx: aMm[0] - (cx * ax - cy * ay), ty: aMm[1] - (cy * ax + cx * ay) };
}

/// Flatten a similarity to the px→mm affine (matches `to_affine`).
function simToAffine(s: Similarity): Affine {
  const r = (s.rot * Math.PI) / 180;
  const cos = Math.cos(r);
  const sin = Math.sin(r);
  return [s.scale * cos, s.scale * sin, s.scale * sin, -s.scale * cos, s.tx, s.ty];
}

// --- Board real-scale photo overlay -----------------------------------
const NS_SVG = "http://www.w3.org/2000/svg";
const NS_XLINK = "http://www.w3.org/1999/xlink";
// key/attachment → data URI (photos are big; fetch once, reuse across repaints).
const photoOverlayUriCache = new Map<string, string>();

function readPhotosPref(): boolean {
  const s = localStorage.getItem("fragua.photos");
  return s === null ? true : s === "1"; // default ON
}
let photosEnabled = readPhotosPref();

function applyPhotosToggle(enabled: boolean) {
  photosEnabled = enabled;
  els.togglePhotos.classList.toggle("active", enabled);
  localStorage.setItem("fragua.photos", enabled ? "1" : "0");
  if (view === "board" && lastState) paintPhotoOverlays(lastState);
}
els.togglePhotos.addEventListener("click", () => applyPhotosToggle(!photosEnabled));

/// Fill the board SVG's `#photo-underlay` group with one <image> per
/// calibrated footprint, at real physical scale, UNDER the copper. The
/// image is placed by nesting the calibration matrix (raw px → placed
/// local mm) inside the footprint's own `translate(x,y) rotate(rot)` so
/// it lands exactly on the pads. Data URIs are fetched lazily + cached.
function paintPhotoOverlays(state: ProjectState) {
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (!svg) return;
  const group = svg.querySelector("#photo-underlay") as SVGGElement | null;
  if (!group) return;
  group.innerHTML = "";
  if (!photosEnabled) return;
  // v1: top-side footprints only. TODO: bottom-side needs a mirrored
  // placement; this board is single-side, so skip them without crashing.
  for (const ov of state.photo_overlays.filter((o) => o.side === "top")) {
    const g = document.createElementNS(NS_SVG, "g");
    const [a, b, c, d, e, f] = ov.matrix;
    g.setAttribute(
      "transform",
      `translate(${ov.x_mm},${ov.y_mm}) rotate(${ov.rotation_deg}) matrix(${a} ${b} ${c} ${d} ${e} ${f})`,
    );
    const img = document.createElementNS(NS_SVG, "image");
    img.setAttribute("width", String(ov.image_w_px));
    img.setAttribute("height", String(ov.image_h_px));
    img.setAttribute("preserveAspectRatio", "none");
    img.setAttribute("opacity", "0.9");
    g.appendChild(img);
    group.appendChild(g);
    const cacheKey = `${ov.key}/${ov.attachment_id}`;
    const cached = photoOverlayUriCache.get(cacheKey);
    const setHref = (uri: string) => {
      img.setAttribute("href", uri);
      img.setAttributeNS(NS_XLINK, "href", uri);
    };
    if (cached) {
      setHref(cached);
    } else {
      invoke<string>("library_attachment_data_uri", {
        key: ov.key,
        attachmentId: ov.attachment_id,
      })
        .then((uri) => {
          photoOverlayUriCache.set(cacheKey, uri);
          setHref(uri);
        })
        .catch((err) => appendActivity("error", `photo overlay ${ov.key}: ${err}`));
    }
  }
}

type ComponentInfo = {
  reference: string;
  key: string;
  value: string;
  description: string;
  rotation_deg: number;
  edge_mounted: boolean;
  x_mm: number;
  y_mm: number;
  pads: { number: string; name: string; net: string | null; layer: string }[];
  library: {
    key: string;
    description: string;
    default_value: string;
    edge_mounted: boolean;
    pad_count: number;
    attachments: { id: string; kind: string; filename: string; mime: string }[];
  } | null;
};

async function openComponentModal(reference: string, key: string) {
  els.infoCard.innerHTML = `<div class="info-loading">loading ${reference}…</div>`;
  els.infoModal.removeAttribute("hidden");
  let info: ComponentInfo;
  try {
    info = await invoke<ComponentInfo>("component_info", { reference });
  } catch (err) {
    els.infoCard.innerHTML = `<div class="info-error">component_info(${reference}): ${err}</div>`;
    return;
  }
  void key;

  const lib = info.library;
  const photos = lib?.attachments?.filter((a) => /^image\//.test(a.mime)) ?? [];
  const padsByLayer = info.pads.reduce<Record<string, number>>((m, p) => {
    m[p.layer] = (m[p.layer] ?? 0) + 1;
    return m;
  }, {});

  const head = `
    <header>
      <div>
        <div class="info-key">${esc(lib?.key || info.key || info.reference)}</div>
        <div class="info-ref">${esc(info.reference)}${info.value ? ` · ${esc(info.value)}` : ""}</div>
      </div>
      <button class="info-close" aria-label="close">×</button>
    </header>
  `;

  const dataCol = `
    <div class="info-data">
      <section class="info-meta">
        <div><span class="lbl">position</span><span class="val">${info.x_mm.toFixed(2)}, ${info.y_mm.toFixed(2)} mm</span></div>
        <div><span class="lbl">rotation</span><span class="val">${info.rotation_deg.toFixed(0)}°</span></div>
        <div><span class="lbl">edge-mount</span><span class="val">${info.edge_mounted ? "yes" : "no"}</span></div>
        <div><span class="lbl">pads</span><span class="val">${info.pads.length}${
          Object.keys(padsByLayer).length > 1
            ? ` (${Object.entries(padsByLayer).map(([l, n]) => `${n} ${l}`).join(", ")})`
            : ""
        }</span></div>
      </section>
      ${
        info.description || lib?.description
          ? `<section class="info-desc">${esc(info.description || lib?.description || "")}</section>`
          : ""
      }
      <section class="info-pads">
        <h3>pads</h3>
        <table>
          <thead><tr><th>#</th><th>name</th><th>net</th><th>layer</th></tr></thead>
          <tbody>${info.pads
            .map(
              (p) => `<tr>
              <td>${esc(p.number)}</td>
              <td>${esc(p.name || "—")}</td>
              <td>${esc(p.net || "—")}</td>
              <td>${esc(p.layer)}</td>
            </tr>`,
            )
            .join("")}</tbody>
        </table>
      </section>
    </div>
  `;

  const photoCol = photos.length > 0
    ? `<div class="info-photos" id="info-photos"></div>`
    : `<div class="info-photos empty">no photos</div>`;

  els.infoCard.innerHTML = head + `<div class="info-body">${dataCol}${photoCol}</div>`;
  els.infoCard.querySelector(".info-close")?.addEventListener("click", closeComponentModal);

  // Lazily fetch the photo attachments — these can be a few hundred
  // KB each so we kick them off after first paint.
  if (photos.length > 0 && lib) {
    const host = els.infoCard.querySelector("#info-photos") as HTMLElement | null;
    if (host) {
      for (const a of photos) {
        try {
          const uri = await invoke<string>("library_attachment_data_uri", {
            key: lib.key,
            attachmentId: a.id,
          });
          const img = document.createElement("img");
          img.src = uri;
          img.alt = a.filename;
          img.title = a.filename;
          host.appendChild(img);
        } catch (err) {
          appendActivity("error", `attachment ${a.filename}: ${err}`);
        }
      }
    }
  }
}

function closeComponentModal() {
  els.infoModal.setAttribute("hidden", "");
  els.infoCard.innerHTML = "";
}

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && !els.infoModal.hasAttribute("hidden")) {
    closeComponentModal();
  }
});

els.infoModal.addEventListener("click", (ev) => {
  if (ev.target === els.infoModal) closeComponentModal();
});

/// Compose a CSS transform string for a `ViewTransform`. Identity
/// when no rotation / flip is set so the browser can elide it.
function viewTransformCss(t: ViewTransform | undefined): string {
  if (!t) return "";
  const r = ((t.rotation_deg % 360) + 360) % 360;
  const sx = t.flip_h ? -1 : 1;
  const sy = t.flip_v ? -1 : 1;
  if (r === 0 && sx === 1 && sy === 1) return "";
  return `rotate(${r}deg) scaleX(${sx}) scaleY(${sy})`;
}

// --- Photo calibration modal ------------------------------------------
// Two steps over one calibrated photo of the module:
//   1. scale — mark two pin centres, say which pads they are; Fragua
//      derives the photo→footprint-mm transform (no typed measurements).
//   2. body — fit a rectangle to the physical body; Fragua stores it in
//      footprint-local mm and derives the placement margin from it.
// The photo is shown RAW (unrotated — the attachment view_transform is
// deliberately ignored here) so the captured pixel coords are in the
// image's own frame, matching how the backend derives the transform.
type CalibStep = "calibrate" | "body";
let calibKeydown: ((e: KeyboardEvent) => void) | null = null;

function closeCalibModal() {
  els.calibModal.setAttribute("hidden", "");
  els.calibCard.innerHTML = "";
  if (calibKeydown) {
    document.removeEventListener("keydown", calibKeydown);
    calibKeydown = null;
  }
}

async function openCalibrateModal(entry: ReviewEntry, startStep: CalibStep) {
  const photo = entry.attachments.find((a) => a.mime.startsWith("image/"));
  if (!photo) return;
  els.calibCard.innerHTML = `<div class="calib-loading">loading photo…</div>`;
  els.calibModal.removeAttribute("hidden");
  calibKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") closeCalibModal();
  };
  document.addEventListener("keydown", calibKeydown);

  let uri: string;
  try {
    uri = await invoke<string>("library_attachment_data_uri", {
      key: entry.key,
      attachmentId: photo.id,
    });
  } catch (err) {
    els.calibCard.innerHTML = `<div class="calib-error">photo: ${esc(String(err))}</div>`;
    return;
  }
  // Natural pixel dimensions — the SVG viewBox and all captured coords
  // live in this raw image-pixel space.
  const dims = await new Promise<{ w: number; h: number }>((resolve, reject) => {
    const im = new Image();
    im.onload = () => resolve({ w: im.naturalWidth, h: im.naturalHeight });
    im.onerror = () => reject(new Error("decode failed"));
    im.src = uri;
  }).catch(() => null);
  if (!dims || dims.w <= 0 || dims.h <= 0) {
    els.calibCard.innerHTML = `<div class="calib-error">could not read image size</div>`;
    return;
  }
  const imgW = dims.w;
  const imgH = dims.h;
  const pads = entry.pads;

  // --- Modal state ----------------------------------------------------
  const existing = photo.calibration ?? null;
  let markerA: [number, number] = existing ? existing.a_px : [imgW / 3, imgH / 2];
  let markerB: [number, number] = existing ? existing.b_px : [(2 * imgW) / 3, imgH / 2];
  const defaultA = pads.find((p) => p.number === "1")?.number ?? pads[0]?.number ?? "";
  let aPad = existing?.a_pad ?? defaultA;
  let bPad = existing?.b_pad ?? farthestPad(pads, aPad);
  let activeMarker: "A" | "B" = "A";
  let step: CalibStep = startStep;
  // Body rect (footprint-local mm). Seed from stored body, else pad bbox.
  const seed = entry.body_rect ?? entry.pad_bbox;
  const body: BodyRect = seed
    ? { ...seed }
    : { min_x_mm: -1, min_y_mm: -1, max_x_mm: 1, max_y_mm: 1 };

  els.calibCard.innerHTML = `
    <header class="calib-head">
      <div class="calib-title">calibrate · ${esc(entry.key)}</div>
      <div class="calib-steps">
        <button type="button" class="calib-step-btn" data-step="calibrate">1 · scale</button>
        <button type="button" class="calib-step-btn" data-step="body">2 · body</button>
      </div>
      <button type="button" class="calib-close" aria-label="close">×</button>
    </header>
    <div class="calib-body">
      <div class="calib-stage">
        <svg class="calib-svg" viewBox="0 0 ${imgW} ${imgH}" preserveAspectRatio="xMidYMid meet">
          <image class="calib-photo" href="${uri}" x="0" y="0" width="${imgW}" height="${imgH}" preserveAspectRatio="none"></image>
          <g class="calib-layer-markers"></g>
          <g class="calib-layer-body"></g>
        </svg>
      </div>
      <div class="calib-side"></div>
    </div>
  `;
  const svg = els.calibCard.querySelector(".calib-svg") as SVGSVGElement;
  const markerLayer = els.calibCard.querySelector(".calib-layer-markers") as SVGGElement;
  const bodyLayer = els.calibCard.querySelector(".calib-layer-body") as SVGGElement;
  const side = els.calibCard.querySelector(".calib-side") as HTMLElement;
  els.calibCard.querySelector(".calib-close")?.addEventListener("click", closeCalibModal);

  // Convert a client point to SVG (raw image-pixel) coords.
  function toSvg(clientX: number, clientY: number): [number, number] {
    const pt = svg.createSVGPoint();
    pt.x = clientX;
    pt.y = clientY;
    const ctm = svg.getScreenCTM();
    if (!ctm) return [0, 0];
    const p = pt.matrixTransform(ctm.inverse());
    return [p.x, p.y];
  }

  // Live similarity transform from the current marks + pads (native mm).
  function currentSim(): Similarity | null {
    const a = pads.find((p) => p.number === aPad);
    const b = pads.find((p) => p.number === bPad);
    if (!a || !b) return null;
    return derivePhotoTransform([a.x_mm, a.y_mm], [b.x_mm, b.y_mm], markerA, markerB);
  }

  const markerR = Math.max(imgW, imgH) * 0.012;

  function renderMarkers() {
    markerLayer.innerHTML = "";
    if (step !== "calibrate") return;
    const mk = (label: string, p: [number, number]) => {
      const g = document.createElementNS(NS_SVG, "g");
      g.setAttribute("class", `calib-marker marker-${label.toLowerCase()}`);
      g.dataset.marker = label;
      const c = document.createElementNS(NS_SVG, "circle");
      c.setAttribute("cx", String(p[0]));
      c.setAttribute("cy", String(p[1]));
      c.setAttribute("r", String(markerR));
      const t = document.createElementNS(NS_SVG, "text");
      t.setAttribute("x", String(p[0]));
      t.setAttribute("y", String(p[1]));
      t.setAttribute("text-anchor", "middle");
      t.setAttribute("dominant-baseline", "central");
      t.setAttribute("font-size", String(markerR * 1.2));
      t.textContent = label;
      g.appendChild(c);
      g.appendChild(t);
      markerLayer.appendChild(g);
      wireMarkerDrag(g, label as "A" | "B");
    };
    mk("A", markerA);
    mk("B", markerB);
  }

  function wireMarkerDrag(g: SVGGElement, which: "A" | "B") {
    g.addEventListener("pointerdown", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      g.setPointerCapture(ev.pointerId);
      const onMove = (mv: PointerEvent) => {
        const p = toSvg(mv.clientX, mv.clientY);
        if (which === "A") markerA = p;
        else markerB = p;
        renderMarkers();
        renderSide();
      };
      const onUp = () => {
        g.removeEventListener("pointermove", onMove);
        g.removeEventListener("pointerup", onUp);
        g.removeEventListener("pointercancel", onUp);
      };
      g.addEventListener("pointermove", onMove);
      g.addEventListener("pointerup", onUp);
      g.addEventListener("pointercancel", onUp);
    });
  }

  function renderBody() {
    bodyLayer.innerHTML = "";
    if (step !== "body") return;
    const sim = currentSim();
    if (!sim) return;
    const pxToMm = simToAffine(sim);
    const mmToPx = invertAffine(pxToMm);
    if (!mmToPx) return;
    // Pad dots (native mm → px) — instant calibration sanity check.
    for (const p of pads) {
      const [x, y] = applyAffine(mmToPx, p.x_mm, p.y_mm);
      const dot = document.createElementNS(NS_SVG, "circle");
      dot.setAttribute("class", "calib-paddot");
      dot.setAttribute("cx", String(x));
      dot.setAttribute("cy", String(y));
      dot.setAttribute("r", String(markerR * 0.5));
      bodyLayer.appendChild(dot);
    }
    // Body rectangle as a quad (corners mm → px; the photo may be
    // rotated so this reads as a tilted polygon).
    const corners: [number, number][] = [
      [body.min_x_mm, body.min_y_mm],
      [body.max_x_mm, body.min_y_mm],
      [body.max_x_mm, body.max_y_mm],
      [body.min_x_mm, body.max_y_mm],
    ];
    const poly = document.createElementNS(NS_SVG, "polygon");
    poly.setAttribute("class", "calib-bodyrect");
    poly.setAttribute(
      "points",
      corners.map(([mx, my]) => applyAffine(mmToPx, mx, my).join(",")).join(" "),
    );
    bodyLayer.appendChild(poly);
    // Edge handles at edge midpoints (in mm), dragged to move that edge.
    const midY = (body.min_y_mm + body.max_y_mm) / 2;
    const midX = (body.min_x_mm + body.max_x_mm) / 2;
    const edges: { side: "top" | "right" | "bottom" | "left"; mm: [number, number] }[] = [
      { side: "top", mm: [midX, body.max_y_mm] },
      { side: "bottom", mm: [midX, body.min_y_mm] },
      { side: "right", mm: [body.max_x_mm, midY] },
      { side: "left", mm: [body.min_x_mm, midY] },
    ];
    for (const e of edges) {
      const [hx, hy] = applyAffine(mmToPx, e.mm[0], e.mm[1]);
      const h = document.createElementNS(NS_SVG, "circle");
      h.setAttribute("class", `calib-bodyhandle handle-${e.side}`);
      h.setAttribute("cx", String(hx));
      h.setAttribute("cy", String(hy));
      h.setAttribute("r", String(markerR * 0.8));
      bodyLayer.appendChild(h);
      wireBodyHandleDrag(h, e.side, pxToMm);
    }
  }

  function wireBodyHandleDrag(
    h: SVGCircleElement,
    edge: "top" | "right" | "bottom" | "left",
    pxToMm: Affine,
  ) {
    h.addEventListener("pointerdown", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      h.setPointerCapture(ev.pointerId);
      const snap = (v: number) => Math.round(v / 0.1) * 0.1;
      const onMove = (mv: PointerEvent) => {
        const [px, py] = toSvg(mv.clientX, mv.clientY);
        const [mmx, mmy] = applyAffine(pxToMm, px, py);
        if (edge === "right") body.max_x_mm = Math.max(snap(mmx), body.min_x_mm + 0.1);
        else if (edge === "left") body.min_x_mm = Math.min(snap(mmx), body.max_x_mm - 0.1);
        else if (edge === "top") body.max_y_mm = Math.max(snap(mmy), body.min_y_mm + 0.1);
        else body.min_y_mm = Math.min(snap(mmy), body.max_y_mm - 0.1);
        renderBody();
        renderSide();
      };
      const onUp = () => {
        h.removeEventListener("pointermove", onMove);
        h.removeEventListener("pointerup", onUp);
        h.removeEventListener("pointercancel", onUp);
      };
      h.addEventListener("pointermove", onMove);
      h.addEventListener("pointerup", onUp);
      h.addEventListener("pointercancel", onUp);
    });
  }

  function padOptions(selected: string): string {
    return pads
      .map(
        (p) =>
          `<option value="${esc(p.number)}" ${p.number === selected ? "selected" : ""}>${esc(p.number)}</option>`,
      )
      .join("");
  }

  function renderSide() {
    if (step === "calibrate") {
      const sim = currentSim();
      const readout = sim
        ? `<div class="calib-readout">
             <div><span>scale</span><b>${(sim.scale * 1000).toFixed(3)} µm/px</b></div>
             <div><span>photo width</span><b>${(imgW * sim.scale).toFixed(2)} mm</b></div>
             <div><span>rotation</span><b>${sim.rot.toFixed(1)}°</b></div>
           </div>`
        : `<div class="calib-readout invalid">pick two different pads and place both marks</div>`;
      side.innerHTML = `
        <p class="calib-hint">Click the two pin centres on the photo (or drag the A / B marks), then say which pad each one is. Fragua works out the scale from the known distance between those pads.</p>
        <div class="calib-active">
          <span>placing:</span>
          <button type="button" class="calib-ab ${activeMarker === "A" ? "active" : ""}" data-ab="A">A</button>
          <button type="button" class="calib-ab ${activeMarker === "B" ? "active" : ""}" data-ab="B">B</button>
        </div>
        <label class="calib-field"><span>mark A = pad</span><select class="calib-pad-a">${padOptions(aPad)}</select></label>
        <label class="calib-field"><span>mark B = pad</span><select class="calib-pad-b">${padOptions(bPad)}</select></label>
        ${readout}
        <div class="calib-actions">
          <button type="button" class="calib-save" ${sim ? "" : "disabled"}>save calibration</button>
          ${existing ? `<button type="button" class="calib-clear">clear</button>` : ""}
        </div>
      `;
      side.querySelector(".calib-pad-a")?.addEventListener("change", (e) => {
        aPad = (e.target as HTMLSelectElement).value;
        renderSide();
      });
      side.querySelector(".calib-pad-b")?.addEventListener("change", (e) => {
        bPad = (e.target as HTMLSelectElement).value;
        renderSide();
      });
      for (const btn of Array.from(side.querySelectorAll(".calib-ab")) as HTMLButtonElement[]) {
        btn.addEventListener("click", () => {
          activeMarker = btn.dataset.ab as "A" | "B";
          renderSide();
        });
      }
      side.querySelector(".calib-save")?.addEventListener("click", saveCalibration);
      side.querySelector(".calib-clear")?.addEventListener("click", clearCalibration);
    } else {
      const sim = currentSim();
      const w = (body.max_x_mm - body.min_x_mm).toFixed(2);
      const h = (body.max_y_mm - body.min_y_mm).toFixed(2);
      side.innerHTML = sim
        ? `<p class="calib-hint">Drag the four edge handles so the rectangle covers the physical body of the module. The white dots are where calibration says the pads are — they should sit on the real pads.</p>
           <div class="calib-readout">
             <div><span>body</span><b>${w} × ${h} mm</b></div>
           </div>
           <div class="calib-actions">
             <button type="button" class="calib-save-body">save body + margin</button>
             ${entry.body_rect ? `<button type="button" class="calib-clear-body">clear</button>` : ""}
           </div>`
        : `<p class="calib-hint invalid">This photo is not calibrated yet. Do step 1 first.</p>`;
      side.querySelector(".calib-save-body")?.addEventListener("click", saveBody);
      side.querySelector(".calib-clear-body")?.addEventListener("click", clearBody);
    }
  }

  async function saveCalibration() {
    const sim = currentSim();
    if (!sim) return;
    try {
      await invoke("library_set_photo_calibration", {
        key: entry.key,
        attachmentId: photo!.id,
        aPxX: markerA[0],
        aPxY: markerA[1],
        bPxX: markerB[0],
        bPxY: markerB[1],
        aPad,
        bPad,
      });
      appendActivity("info", `calibrated photo for ${entry.key}`);
      setStep("body");
    } catch (err) {
      appendActivity("error", `calibration save ${entry.key}: ${err}`);
    }
  }

  async function clearCalibration() {
    try {
      await invoke("library_clear_photo_calibration", {
        key: entry.key,
        attachmentId: photo!.id,
      });
      appendActivity("info", `cleared calibration for ${entry.key}`);
      closeCalibModal();
    } catch (err) {
      appendActivity("error", `calibration clear ${entry.key}: ${err}`);
    }
  }

  async function saveBody() {
    try {
      await invoke("library_set_body_rect", {
        key: entry.key,
        minXMm: body.min_x_mm,
        minYMm: body.min_y_mm,
        maxXMm: body.max_x_mm,
        maxYMm: body.max_y_mm,
      });
      appendActivity("info", `saved body + margin for ${entry.key}`);
      closeCalibModal();
    } catch (err) {
      appendActivity("error", `body save ${entry.key}: ${err}`);
    }
  }

  async function clearBody() {
    try {
      await invoke("library_clear_body_rect", { key: entry.key });
      appendActivity("info", `cleared body for ${entry.key}`);
      closeCalibModal();
    } catch (err) {
      appendActivity("error", `body clear ${entry.key}: ${err}`);
    }
  }

  function setStep(s: CalibStep) {
    step = s;
    for (const b of Array.from(els.calibCard.querySelectorAll(".calib-step-btn")) as HTMLButtonElement[]) {
      b.classList.toggle("active", b.dataset.step === s);
    }
    renderMarkers();
    renderBody();
    renderSide();
  }
  for (const b of Array.from(els.calibCard.querySelectorAll(".calib-step-btn")) as HTMLButtonElement[]) {
    b.addEventListener("click", () => setStep(b.dataset.step as CalibStep));
  }

  // Background click (no drag) places the active marker; drag pans the
  // photo; wheel zooms around the cursor. Marks / handles capture their
  // own pointers first, so this only fires on the bare photo.
  attachCalibPanZoom(svg, imgW, imgH, (p) => {
    if (step !== "calibrate") return;
    if (activeMarker === "A") markerA = p;
    else markerB = p;
    activeMarker = activeMarker === "A" ? "B" : "A";
    renderMarkers();
    renderSide();
  });

  setStep(step);
}

/// Nearest-neighbour default: the pad geometrically farthest from
/// `fromNumber`, so the two default calibration pads are well separated
/// (a long baseline gives a more accurate scale).
function farthestPad(pads: ReviewPad[], fromNumber: string): string {
  if (pads.length === 0) return "";
  const from = pads.find((p) => p.number === fromNumber) ?? pads[0];
  let best = pads[0];
  let bestD = -1;
  for (const p of pads) {
    const dd = (p.x_mm - from.x_mm) ** 2 + (p.y_mm - from.y_mm) ** 2;
    if (dd > bestD) {
      bestD = dd;
      best = p;
    }
  }
  return best.number;
}

/// viewBox pan/zoom for the calibration photo SVG. A click without drag
/// invokes `onClick` with the SVG-space (raw image px) point.
function attachCalibPanZoom(
  svg: SVGSVGElement,
  imgW: number,
  imgH: number,
  onClick: (p: [number, number]) => void,
) {
  let vb = { x: 0, y: 0, w: imgW, h: imgH };
  const apply = () => svg.setAttribute("viewBox", `${vb.x} ${vb.y} ${vb.w} ${vb.h}`);
  svg.style.cursor = "grab";
  svg.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    // Ignore presses that land on a mark / handle (they stopPropagation,
    // but guard anyway).
    if ((ev.target as Element).closest(".calib-marker, .calib-bodyhandle")) return;
    ev.preventDefault();
    svg.setPointerCapture(ev.pointerId);
    svg.style.cursor = "grabbing";
    const rect = svg.getBoundingClientRect();
    const sx = vb.w / rect.width;
    const sy = vb.h / rect.height;
    const px0 = ev.clientX;
    const py0 = ev.clientY;
    const start = { ...vb };
    let panned = false;
    const onMove = (e: PointerEvent) => {
      if (!panned && Math.hypot(e.clientX - px0, e.clientY - py0) < 4) return;
      panned = true;
      vb = { x: start.x - (e.clientX - px0) * sx, y: start.y - (e.clientY - py0) * sy, w: vb.w, h: vb.h };
      apply();
    };
    const onUp = (e: PointerEvent) => {
      svg.removeEventListener("pointermove", onMove);
      svg.removeEventListener("pointerup", onUp);
      svg.removeEventListener("pointercancel", onUp);
      svg.style.cursor = "grab";
      if (!panned) {
        const pt = svg.createSVGPoint();
        pt.x = e.clientX;
        pt.y = e.clientY;
        const ctm = svg.getScreenCTM();
        if (ctm) {
          const p = pt.matrixTransform(ctm.inverse());
          onClick([p.x, p.y]);
        }
      }
    };
    svg.addEventListener("pointermove", onMove);
    svg.addEventListener("pointerup", onUp);
    svg.addEventListener("pointercancel", onUp);
  });
  svg.addEventListener(
    "wheel",
    (ev) => {
      ev.preventDefault();
      const rect = svg.getBoundingClientRect();
      const fx = vb.x + ((ev.clientX - rect.left) / rect.width) * vb.w;
      const fy = vb.y + ((ev.clientY - rect.top) / rect.height) * vb.h;
      const k = Math.exp(ev.deltaY * 0.0015);
      const newW = clamp(vb.w * k, imgW * 0.05, imgW * 4);
      const newH = clamp(vb.h * k, imgH * 0.05, imgH * 4);
      vb = { x: fx - ((fx - vb.x) * newW) / vb.w, y: fy - ((fy - vb.y) * newH) / vb.h, w: newW, h: newH };
      apply();
    },
    { passive: false },
  );
}

els.calibModal.addEventListener("click", (ev) => {
  if (ev.target === els.calibModal) closeCalibModal();
});

/// Two-step inline delete-entry confirmation. The first click flips
/// the trash button into a "✓ confirm" state for `windowMs`; a second
/// click within the window fires `onConfirm`. Used instead of the
/// native `confirm()` dialog so we never block the webview.
function armTwoStepConfirm(
  btn: HTMLButtonElement,
  windowMs: number,
  onConfirm: () => void,
) {
  let armed = false;
  let resetHandle: number | undefined;
  const original = btn.innerHTML;
  const reset = () => {
    armed = false;
    btn.innerHTML = original;
    btn.classList.remove("armed");
    if (resetHandle !== undefined) {
      window.clearTimeout(resetHandle);
      resetHandle = undefined;
    }
  };
  btn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    if (armed) {
      reset();
      onConfirm();
      return;
    }
    armed = true;
    btn.innerHTML = "confirm?";
    btn.classList.add("armed");
    resetHandle = window.setTimeout(reset, windowMs);
  });
}

/// Increments while an optimistic review-pane mutation is in flight.
/// Backend mutations publish a `LibraryChanged` event that would
/// otherwise trigger a full `paintReview()` and lose scroll position.
let reviewMutationInFlight = 0;

type ReviewLocalState = {
  photoT: ViewTransform;
  fpT: ViewTransform;
  margin: PlacementMargin;
};

/// Parse the embedded review SVG's viewBox to derive the body's mm
/// dimensions, then auto-rescale a composition of [footprint body] +
/// [keep-out outline + four edge handles] to fit the cell. The
/// drawing rescales live during a drag so handles never escape the
/// cell. The body+keep-out outlines remain axis-aligned regardless
/// of the SVG's rotate/flip visual transform.
function wireMarginHandles(
  card: HTMLElement,
  key: string,
  localState: Map<string, ReviewLocalState>,
) {
  const frameMaybe = card.querySelector(".footprint-frame") as HTMLElement | null;
  if (!frameMaybe) return;
  const frame: HTMLElement = frameMaybe;
  const svgEl = frame.querySelector("svg") as SVGSVGElement | null;
  if (!svgEl) return;
  const svg: SVGSVGElement = svgEl;
  const hostMaybe = frame.querySelector(".footprint-svg-host") as HTMLElement | null;
  if (!hostMaybe) return;
  const host: HTMLElement = hostMaybe;
  const bodyOutline = frame.querySelector(".body-outline") as HTMLElement | null;
  const keepoutOutline = frame.querySelector(".keepout-outline") as HTMLElement | null;

  // Parse the footprint's viewBox once (the body mm extents are fixed).
  const vbAttr = svg.getAttribute("viewBox") ?? "";
  const vbParts = vbAttr.trim().split(/\s+/).map(Number);
  const vbW = vbParts.length === 4 && Number.isFinite(vbParts[2]) ? vbParts[2] : 0;
  const vbH = vbParts.length === 4 && Number.isFinite(vbParts[3]) ? vbParts[3] : 0;
  // Override the renderer's `width="100%" height="100%"` so the SVG
  // honors the host's pixel size we set below. preserveAspectRatio
  // defaults to xMidYMid meet — fine because host's aspect matches
  // the viewBox exactly, so there is no letterboxing.
  svg.removeAttribute("width");
  svg.removeAttribute("height");

  type Side = "top" | "right" | "bottom" | "left";
  const handles: Record<Side, HTMLElement | null> = {
    top: frame.querySelector(".margin-handle.top"),
    right: frame.querySelector(".margin-handle.right"),
    bottom: frame.querySelector(".margin-handle.bottom"),
    left: frame.querySelector(".margin-handle.left"),
  };

  // Snap step matches the previous number-input's `step="0.5"`.
  const SNAP_MM = 0.5;
  const snap = (mm: number) => Math.round(mm / SNAP_MM) * SNAP_MM;
  const sideKey = (s: Side): keyof PlacementMargin =>
    s === "top" ? "top_mm" : s === "right" ? "right_mm" : s === "bottom" ? "bottom_mm" : "left_mm";
  const fmt = (mm: number) => (Math.abs(mm) < 0.0005 ? "" : `${mm.toFixed(mm % 1 === 0 ? 0 : 1)} mm`);

  // Edge padding (px) reserved for the handle hit-area + labels so
  // handles are never flush with the cell edge.
  const EDGE_PAD_PX = 24;
  // Clamp: at most 50 mm or 5x the body's longest mm dimension,
  // whichever is larger. Keeps the zoom-out bounded for sane parts.
  const longest = Math.max(vbW, vbH, 0.001);
  const MAX_MARGIN_MM = Math.max(50, 5 * longest);

  /// Recompute the pxPerMm that fits [body + current margins] into
  /// the cell (minus EDGE_PAD_PX on each side), then position every
  /// element in pixel space relative to the frame.
  function layout() {
    const s = localState.get(key);
    if (!s) return;
    const cellW = frame.clientWidth;
    const cellH = frame.clientHeight;
    if (cellW <= 0 || cellH <= 0 || vbW <= 0 || vbH <= 0) return;

    const totalMmW = vbW + s.margin.left_mm + s.margin.right_mm;
    const totalMmH = vbH + s.margin.top_mm + s.margin.bottom_mm;
    const availPxW = Math.max(1, cellW - 2 * EDGE_PAD_PX);
    const availPxH = Math.max(1, cellH - 2 * EDGE_PAD_PX);
    const pxPerMm = Math.min(availPxW / totalMmW, availPxH / totalMmH);
    if (!Number.isFinite(pxPerMm) || pxPerMm <= 0) return;

    const bodyPxW = vbW * pxPerMm;
    const bodyPxH = vbH * pxPerMm;
    const leftPx = s.margin.left_mm * pxPerMm;
    const rightPx = s.margin.right_mm * pxPerMm;
    const topPx = s.margin.top_mm * pxPerMm;
    const bottomPx = s.margin.bottom_mm * pxPerMm;
    const keepoutPxW = bodyPxW + leftPx + rightPx;
    const keepoutPxH = bodyPxH + topPx + bottomPx;

    // Center the keep-out composition in the cell.
    const keepoutLeft = (cellW - keepoutPxW) / 2;
    const keepoutTop = (cellH - keepoutPxH) / 2;
    const bodyLeft = keepoutLeft + leftPx;
    const bodyTop = keepoutTop + topPx;

    // SVG host = body bounds in px.
    host.style.left = `${bodyLeft}px`;
    host.style.top = `${bodyTop}px`;
    host.style.width = `${bodyPxW}px`;
    host.style.height = `${bodyPxH}px`;

    if (bodyOutline) {
      bodyOutline.style.left = `${bodyLeft}px`;
      bodyOutline.style.top = `${bodyTop}px`;
      bodyOutline.style.width = `${bodyPxW}px`;
      bodyOutline.style.height = `${bodyPxH}px`;
    }
    if (keepoutOutline) {
      keepoutOutline.style.left = `${keepoutLeft}px`;
      keepoutOutline.style.top = `${keepoutTop}px`;
      keepoutOutline.style.width = `${keepoutPxW}px`;
      keepoutOutline.style.height = `${keepoutPxH}px`;
    }

    // Handles sit on the keep-out edges, spanning the keep-out
    // span on their axis so the dashed line reads as a full edge.
    const place = (s2: Side, mm: number) => {
      const h = handles[s2];
      if (!h) return;
      if (s2 === "top") {
        h.style.top = `${keepoutTop}px`;
        h.style.left = `${keepoutLeft}px`;
        h.style.width = `${keepoutPxW}px`;
        h.style.height = `12px`;
      } else if (s2 === "bottom") {
        h.style.top = `${keepoutTop + keepoutPxH}px`;
        h.style.left = `${keepoutLeft}px`;
        h.style.width = `${keepoutPxW}px`;
        h.style.height = `12px`;
      } else if (s2 === "left") {
        h.style.left = `${keepoutLeft}px`;
        h.style.top = `${keepoutTop}px`;
        h.style.height = `${keepoutPxH}px`;
        h.style.width = `12px`;
      } else if (s2 === "right") {
        h.style.left = `${keepoutLeft + keepoutPxW}px`;
        h.style.top = `${keepoutTop}px`;
        h.style.height = `${keepoutPxH}px`;
        h.style.width = `12px`;
      }
      const label = h.querySelector(".margin-label") as HTMLElement | null;
      if (label) label.textContent = fmt(mm);
    };
    place("top", s.margin.top_mm);
    place("right", s.margin.right_mm);
    place("bottom", s.margin.bottom_mm);
    place("left", s.margin.left_mm);

    // Stash the current pxPerMm so the drag handler can convert
    // pointer delta -> mm using the same scale (re-read after each
    // move because the layout zooms while dragging).
    (frame as unknown as { __pxPerMm?: number }).__pxPerMm = pxPerMm;
  }

  function currentPxPerMm(): number {
    return (frame as unknown as { __pxPerMm?: number }).__pxPerMm ?? 0;
  }

  // Initial layout: now, after a frame (so the cell has real
  // dimensions), and on every resize.
  layout();
  window.requestAnimationFrame(layout);
  const ro = new ResizeObserver(() => layout());
  ro.observe(frame);

  for (const sideStr of ["top", "right", "bottom", "left"] as const) {
    const handle = handles[sideStr];
    if (!handle) continue;
    const side: Side = sideStr;
    handle.addEventListener("pointerdown", (ev) => {
      ev.preventDefault();
      const s = localState.get(key);
      if (!s) return;
      const startPxPerMm = currentPxPerMm();
      if (startPxPerMm <= 0) return;
      const startMm = s.margin[sideKey(side)];
      const startCoord = side === "top" || side === "bottom" ? ev.clientY : ev.clientX;
      // Dragging outward (away from the body) grows the margin.
      const sign =
        side === "top" ? -1 : side === "bottom" ? 1 : side === "left" ? -1 : 1;
      handle.setPointerCapture(ev.pointerId);
      handle.classList.add("dragging");
      let currentMm = startMm;
      const onMove = (mv: PointerEvent) => {
        // Use the *current* pxPerMm — the layout rescales as the
        // user drags, so the pointer-to-mm ratio changes too.
        const pxPerMm = currentPxPerMm() || startPxPerMm;
        const coord = side === "top" || side === "bottom" ? mv.clientY : mv.clientX;
        const deltaPx = (coord - startCoord) * sign;
        let mm = startMm + deltaPx / pxPerMm;
        mm = Math.max(0, Math.min(MAX_MARGIN_MM, mm));
        const snapped = snap(mm);
        currentMm = snapped;
        s.margin[sideKey(side)] = snapped;
        // Re-layout the whole composition so the zoom-out is live.
        layout();
      };
      const onUp = async () => {
        handle.removeEventListener("pointermove", onMove);
        handle.removeEventListener("pointerup", onUp);
        handle.removeEventListener("pointercancel", onUp);
        handle.classList.remove("dragging");
        if (handle.hasPointerCapture(ev.pointerId)) {
          handle.releasePointerCapture(ev.pointerId);
        }
        if (currentMm === startMm) return;
        s.margin[sideKey(side)] = currentMm;
        reviewMutationInFlight++;
        try {
          await invoke("library_set_placement_margin", {
            key,
            topMm: s.margin.top_mm,
            rightMm: s.margin.right_mm,
            bottomMm: s.margin.bottom_mm,
            leftMm: s.margin.left_mm,
          });
        } catch (err) {
          appendActivity("error", `margin save ${key}: ${err}`);
        } finally {
          window.setTimeout(() => { reviewMutationInFlight--; }, 0);
        }
      };
      handle.addEventListener("pointermove", onMove);
      handle.addEventListener("pointerup", onUp);
      handle.addEventListener("pointercancel", onUp);
    });
  }
}

/// `paintReview()` wrapper that captures and restores the scroll
/// position of the inner `.review-list` so external events (e.g. a
/// new entry confirmed via the popup) don't yank the user back to the
/// top of the pane.
async function paintReviewPreserveScroll() {
  const prevList = els.canvas.querySelector(".review-list") as HTMLElement | null;
  const prevTop = prevList?.scrollTop ?? 0;
  const prevLeft = prevList?.scrollLeft ?? 0;
  await paintReview();
  const nextList = els.canvas.querySelector(".review-list") as HTMLElement | null;
  if (nextList) {
    nextList.scrollTop = prevTop;
    nextList.scrollLeft = prevLeft;
  }
}

/// Render the library review pane: one card per stored library
/// entry, with the photo and the rendered footprint side by side.
/// GND pads on the footprint are highlighted in magenta by the
/// renderer so a mirrored / mis-numbered pinout is obvious next to
/// the real component photo. The per-cell transform buttons and the
/// margin inputs auto-save to the global library on every change.
async function paintReview() {
  els.canvas.innerHTML = `<div class="review-loading">loading library…</div>`;
  let data: { entries: ReviewEntry[] };
  try {
    data = await invoke<{ entries: ReviewEntry[] }>("library_review_state");
  } catch (err) {
    els.canvas.innerHTML = `<div class="review-error">library_review_state: ${esc(String(err))}</div>`;
    return;
  }
  els.reviewCount.textContent = String(data.entries.length);
  if (data.entries.length === 0) {
    els.canvas.innerHTML = `<div class="review-empty">
      <h2>no library entries yet</h2>
      <p>your agent will save parts here as you design.<br>
      every entry created via the script API queues for human review first —
      a confirmation popup will appear automatically.</p>
    </div>`;
    return;
  }
  const photoUriCache = new Map<string, Promise<string>>();
  async function photoUri(key: string, attId: string): Promise<string> {
    const cacheKey = `${key}/${attId}`;
    let p = photoUriCache.get(cacheKey);
    if (!p) {
      p = invoke<string>("library_attachment_data_uri", {
        key,
        attachmentId: attId,
      });
      photoUriCache.set(cacheKey, p);
    }
    return p;
  }
  // Local mutable copy of each entry's transforms / margin so the DOM
  // can update optimistically without re-fetching the whole pane.
  const localState = new Map<string, ReviewLocalState>();
  for (const e of data.entries) {
    const photo = e.attachments.find((a) => a.mime.startsWith("image/"));
    localState.set(e.key, {
      photoT: photo?.view_transform ?? { rotation_deg: 0, flip_h: false, flip_v: false },
      fpT: e.footprint_view_transform,
      margin: e.placement_margin,
    });
  }
  const list = document.createElement("div");
  list.className = "review-list";
  for (const entry of data.entries) {
    const card = document.createElement("article");
    card.className = "review-card";
    const photo = entry.attachments.find((a) => a.mime.startsWith("image/"));
    const gndBadge = entry.ground_pad_count > 0
      ? `<span class="gnd-badge">${entry.ground_pad_count} GND</span>`
      : `<span class="gnd-badge none">no GND</span>`;
    card.dataset.key = entry.key;
    card.innerHTML = `
      <header class="review-head">
        <h3 class="review-key">${esc(entry.key)}</h3>
        <div class="review-meta">
          <span>${entry.pad_count} pads</span>
          ${gndBadge}
          ${entry.edge_mounted ? `<span class="edge-badge">edge</span>` : ""}
          ${entry.lcsc_id ? `<span class="lcsc-badge">${esc(entry.lcsc_id)}</span>` : ""}
          ${entry.mpn ? `<span class="mpn-badge">${esc(entry.mpn)}</span>` : ""}
          <button type="button" class="btn-delete-entry" data-key="${esc(entry.key)}" title="delete this library entry">trash</button>
        </div>
      </header>
      <div class="review-body">
        <div class="review-cell">
          <div class="review-photo" data-key="${esc(entry.key)}" data-att="${esc(photo?.id ?? "")}">
            ${photo ? `<div class="photo-loading">loading photo…</div>` : `<div class="photo-empty">no photo attached</div>`}
          </div>
          <div class="cell-controls" data-target="photo" ${photo ? "" : "hidden"}>
            <button type="button" data-act="rotate" title="rotate 90° clockwise">rot 90</button>
            <button type="button" data-act="flip-h" title="flip horizontally">flip H</button>
            <button type="button" data-act="flip-v" title="flip vertically">flip V</button>
          </div>
          ${
            photo
              ? `<div class="calib-controls">
                   <span class="calib-badge ${photo.calibration ? "on" : "off"}">${photo.calibration ? "calibrated" : "not calibrated"}</span>
                   <button type="button" class="btn-calibrate" title="mark two pins on the photo to set real scale">${photo.calibration ? "recalibrate" : "calibrate"}</button>
                   <button type="button" class="btn-body" title="draw the physical body on the calibrated photo → auto placement margin" ${photo.calibration ? "" : "disabled"}>body${entry.body_rect ? " ✓" : ""}</button>
                 </div>`
              : ""
          }
        </div>
        <div class="review-cell">
          <div class="review-footprint" title="drag the edge handles to set per-side placement margin (mm)">
            <div class="footprint-frame">
              <div class="footprint-svg-host">${entry.review_svg}</div>
              <div class="body-outline"></div>
              <div class="keepout-outline"></div>
              <div class="margin-handle top" data-side="top"><span class="margin-label"></span></div>
              <div class="margin-handle right" data-side="right"><span class="margin-label"></span></div>
              <div class="margin-handle bottom" data-side="bottom"><span class="margin-label"></span></div>
              <div class="margin-handle left" data-side="left"><span class="margin-label"></span></div>
            </div>
          </div>
          <div class="cell-controls" data-target="footprint">
            <button type="button" data-act="rotate" title="rotate 90° clockwise">rot 90</button>
            <button type="button" data-act="flip-h" title="flip horizontally">flip H</button>
            <button type="button" data-act="flip-v" title="flip vertically">flip V</button>
          </div>
        </div>
      </div>
      ${entry.description ? `<p class="review-desc">${esc(entry.description)}</p>` : ""}
      <footer class="review-foot">
        <span class="review-hint">compare pad positions, pin-1 marker (yellow dot) and GND pads (magenta border) against the photo. Reorder the photo orientation in your head so you are looking at the TOP of the part — same as the footprint render.</span>
      </footer>
    `;
    list.appendChild(card);
  }
  els.canvas.innerHTML = "";
  els.canvas.appendChild(list);

  function applyTransformsForKey(key: string) {
    const s = localState.get(key);
    if (!s) return;
    const card = els.canvas.querySelector(
      `.review-card[data-key="${CSS.escape(key)}"]`,
    ) as HTMLElement | null;
    if (!card) return;
    const photoImg = card.querySelector(".review-photo img") as HTMLElement | null;
    if (photoImg) {
      const css = viewTransformCss(s.photoT);
      photoImg.style.transform = css;
      photoImg.style.transformOrigin = "center";
    }
    const fpSvg = card.querySelector(".review-footprint svg") as HTMLElement | null;
    if (fpSvg) {
      const css = viewTransformCss(s.fpT);
      fpSvg.style.transform = css;
      fpSvg.style.transformOrigin = "center";
    }
    // Reflect the current transform state on the control buttons so the
    // user can tell at a glance which knobs are on. rot button shows the
    // current angle, flip-h / flip-v light up when active.
    for (const ctrl of Array.from(
      card.querySelectorAll(".cell-controls"),
    ) as HTMLElement[]) {
      const target = ctrl.dataset.target as "photo" | "footprint";
      const t = target === "photo" ? s.photoT : s.fpT;
      for (const btn of Array.from(ctrl.querySelectorAll("button")) as HTMLButtonElement[]) {
        const act = btn.dataset.act;
        if (act === "rotate") {
          const r = ((t.rotation_deg % 360) + 360) % 360;
          btn.textContent = r === 0 ? "rot 0" : `rot ${r}`;
          btn.classList.toggle("active", r !== 0);
        } else if (act === "flip-h") {
          btn.classList.toggle("active", t.flip_h);
        } else if (act === "flip-v") {
          btn.classList.toggle("active", t.flip_v);
        }
      }
    }
  }

  // Wire up control bars (photo + footprint).
  for (const ctrl of Array.from(
    els.canvas.querySelectorAll(".cell-controls"),
  ) as HTMLElement[]) {
    const card = ctrl.closest(".review-card") as HTMLElement | null;
    if (!card) continue;
    const key = card.dataset.key ?? "";
    if (!key) continue;
    const target = ctrl.dataset.target as "photo" | "footprint";
    const photoAtt = (card.querySelector(".review-photo") as HTMLElement | null)
      ?.dataset.att ?? "";
    for (const btn of Array.from(ctrl.querySelectorAll("button")) as HTMLButtonElement[]) {
      btn.addEventListener("click", async () => {
        const s = localState.get(key);
        if (!s) return;
        const t = target === "photo" ? s.photoT : s.fpT;
        const act = btn.dataset.act;
        if (act === "rotate") t.rotation_deg = (t.rotation_deg + 90) % 360;
        else if (act === "flip-h") t.flip_h = !t.flip_h;
        else if (act === "flip-v") t.flip_v = !t.flip_v;
        applyTransformsForKey(key);
        reviewMutationInFlight++;
        try {
          if (target === "photo") {
            await invoke("library_set_attachment_view_transform", {
              key,
              attachmentId: photoAtt,
              rotationDeg: t.rotation_deg,
              flipH: t.flip_h,
              flipV: t.flip_v,
            });
          } else {
            await invoke("library_set_footprint_view_transform", {
              key,
              rotationDeg: t.rotation_deg,
              flipH: t.flip_h,
              flipV: t.flip_v,
            });
          }
        } catch (err) {
          appendActivity("error", `view-transform save ${key}: ${err}`);
        } finally {
          window.setTimeout(() => { reviewMutationInFlight--; }, 0);
        }
      });
    }
  }

  // Wire up margin drag handles (one per side, overlaid on the
  // footprint preview). The handle's offset from its edge encodes the
  // margin in mm; the user drags inward to shrink, outward to grow.
  for (const card of Array.from(
    els.canvas.querySelectorAll(".review-card"),
  ) as HTMLElement[]) {
    const key = card.dataset.key ?? "";
    if (!key) continue;
    wireMarginHandles(card, key, localState);
  }

  // Wire up the per-card calibrate / body buttons → the photo
  // calibration modal. Look the entry up by key so the modal gets the
  // pads + pad bbox + current calibration/body it needs.
  const entriesByKey = new Map(data.entries.map((e) => [e.key, e]));
  for (const card of Array.from(els.canvas.querySelectorAll(".review-card")) as HTMLElement[]) {
    const key = card.dataset.key ?? "";
    const entry = entriesByKey.get(key);
    if (!entry) continue;
    card.querySelector(".btn-calibrate")?.addEventListener("click", () => {
      void openCalibrateModal(entry, "calibrate");
    });
    const bodyBtn = card.querySelector(".btn-body") as HTMLButtonElement | null;
    if (bodyBtn && !bodyBtn.disabled) {
      bodyBtn.addEventListener("click", () => {
        void openCalibrateModal(entry, "body");
      });
    }
  }

  // Wire up the trash buttons (two-step inline confirm). Remove the
  // card node directly on success — no full pane repaint.
  for (const btn of Array.from(
    els.canvas.querySelectorAll(".btn-delete-entry"),
  ) as HTMLButtonElement[]) {
    const key = btn.dataset.key ?? "";
    if (!key) continue;
    armTwoStepConfirm(btn, 3000, async () => {
      reviewMutationInFlight++;
      try {
        await invoke<boolean>("library_delete_entry", { key });
        const card = els.canvas.querySelector(
          `.review-card[data-key="${CSS.escape(key)}"]`,
        );
        card?.remove();
        const remaining = els.canvas.querySelectorAll(".review-card").length;
        els.reviewCount.textContent = String(remaining);
        if (remaining === 0) {
          els.canvas.innerHTML = `<div class="review-empty">
            <h2>no library entries yet</h2>
            <p>your agent will save parts here as you design.<br>
            every entry created via the script API queues for human review first —
            a confirmation popup will appear automatically.</p>
          </div>`;
        }
      } catch (err) {
        appendActivity("error", `delete ${key}: ${err}`);
      } finally {
        // Release suppression on the next tick so the in-flight event
        // (already queued) is still ignored.
        window.setTimeout(() => { reviewMutationInFlight--; }, 0);
      }
    });
  }

  // Apply the initial transforms on the footprint SVGs (already in DOM).
  for (const key of localState.keys()) applyTransformsForKey(key);

  // Lazy-load photos.
  for (const slot of Array.from(els.canvas.querySelectorAll(".review-photo")) as HTMLElement[]) {
    const key = slot.dataset.key ?? "";
    const att = slot.dataset.att ?? "";
    if (!key || !att) continue;
    photoUri(key, att)
      .then((uri) => {
        slot.innerHTML = `<img src="${uri}" alt="${esc(key)} photo" />`;
        applyTransformsForKey(key);
      })
      .catch((err) => {
        slot.innerHTML = `<div class="photo-error">${esc(String(err))}</div>`;
      });
  }
}

/// Open the confirmation modal for every currently-pending library
/// entry. The agent must NOT be able to dismiss this — the modal
/// stays open until the human clicks Save or Discard on every entry.
/// Called when the backend signals `PendingLibraryChanged`.
let confirmModalOpen = false;
async function openConfirmModal() {
  let data: { entries: PendingEntry[] };
  try {
    data = await invoke<{ entries: PendingEntry[] }>("pending_library_entries");
  } catch (err) {
    appendActivity("error", `pending_library_entries: ${err}`);
    return;
  }
  if (data.entries.length === 0) {
    els.confirmModal.setAttribute("hidden", "");
    confirmModalOpen = false;
    return;
  }
  confirmModalOpen = true;
  // Show the first pending entry; once confirmed/discarded the
  // PendingLibraryChanged event will re-trigger this and either
  // surface the next entry or close the modal.
  const entry = data.entries[0];
  const photo = entry.attachments.find((a) => a.mime.startsWith("image/"));
  const remaining = data.entries.length;
  const gndBadge = entry.ground_pad_count > 0
    ? `<span class="gnd-badge">${entry.ground_pad_count} GND pad${entry.ground_pad_count === 1 ? "" : "s"}</span>`
    : `<span class="gnd-badge none">no GND pads detected</span>`;
  els.confirmCard.innerHTML = `
    <header class="confirm-head">
      <div>
        <h2>confirm new library entry</h2>
        <div class="confirm-key">${esc(entry.key)}</div>
        <div class="confirm-sub">${remaining > 1 ? `${remaining} pending — review one at a time` : "1 pending"}</div>
      </div>
      <div class="confirm-warning">
        <strong>verify the pinout vs. the photo.</strong>
        a mirrored footprint = unsolderable PCB.
      </div>
    </header>
    <div class="confirm-meta">
      <span>${entry.pad_count} pads</span>
      ${gndBadge}
      ${entry.edge_mounted ? `<span class="edge-badge">edge-mounted</span>` : ""}
      ${entry.lcsc_id ? `<span class="lcsc-badge">${esc(entry.lcsc_id)}</span>` : ""}
      ${entry.mpn ? `<span class="mpn-badge">${esc(entry.mpn)}</span>` : ""}
    </div>
    ${entry.description ? `<p class="confirm-desc">${esc(entry.description)}</p>` : ""}
    <div class="confirm-body">
      <div class="confirm-photo">
        ${photo ? `<img src="${photo.data_uri}" alt="component photo" />` : `<div class="photo-empty">no photo attached — ask the agent to <code>library.attach</code> a photo before confirming</div>`}
        <div class="confirm-caption">photo</div>
      </div>
      <div class="confirm-footprint">
        ${entry.review_svg}
        <div class="confirm-caption">footprint (TOP view, GND in magenta)</div>
      </div>
    </div>
    <footer class="confirm-foot">
      <button class="btn-discard" data-key="${esc(entry.key)}">Discard</button>
      ${photo
        ? `<button class="btn-confirm" data-key="${esc(entry.key)}">Save to library</button>`
        : `<span class="btn-confirm-wrap" title="attach a photo with library.attach before saving"><button class="btn-confirm" data-key="${esc(entry.key)}" disabled aria-disabled="true">Save to library</button><span class="btn-confirm-hint">attach a photo first</span></span>`}
    </footer>
  `;
  els.confirmModal.removeAttribute("hidden");
  els.confirmCard.querySelector(".btn-confirm")?.addEventListener("click", async (ev) => {
    const btn = ev.currentTarget as HTMLButtonElement;
    const key = btn.dataset.key ?? "";
    btn.disabled = true;
    try {
      await invoke<boolean>("confirm_pending_library_entry", { key });
    } catch (err) {
      appendActivity("error", `confirm ${key}: ${err}`);
      btn.disabled = false;
    }
  });
  els.confirmCard.querySelector(".btn-discard")?.addEventListener("click", async (ev) => {
    const btn = ev.currentTarget as HTMLButtonElement;
    const key = btn.dataset.key ?? "";
    if (!confirm(`Discard pending entry "${key}"? The agent will have to recreate it.`)) return;
    btn.disabled = true;
    try {
      await invoke<boolean>("discard_pending_library_entry", { key });
    } catch (err) {
      appendActivity("error", `discard ${key}: ${err}`);
      btn.disabled = false;
    }
  });
}

function paintDrcMarkers() {
  if (drcViolations.length === 0) return;
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (!svg) return;
  const inner = svg.querySelector("g[transform='scale(1,-1)']") as SVGGElement | null;
  const host = inner ?? svg;
  // SVG namespace required for createElementNS.
  const NS = "http://www.w3.org/2000/svg";
  const layer = document.createElementNS(NS, "g");
  layer.setAttribute("class", "drc-markers");
  for (const v of drcViolations) {
    const color = v.severity === "error" ? "#f85149" : "#d29922";
    const r = 1.2;
    const circle = document.createElementNS(NS, "circle");
    circle.setAttribute("cx", String(v.x_mm));
    circle.setAttribute("cy", String(v.y_mm));
    circle.setAttribute("r", String(r));
    circle.setAttribute("fill", "none");
    circle.setAttribute("stroke", color);
    circle.setAttribute("stroke-width", "0.25");
    const title = document.createElementNS(NS, "title");
    title.textContent = v.message;
    circle.appendChild(title);
    layer.appendChild(circle);
    // X mark inside.
    const len = r * 0.6;
    for (const [x1, y1, x2, y2] of [
      [v.x_mm - len, v.y_mm - len, v.x_mm + len, v.y_mm + len],
      [v.x_mm - len, v.y_mm + len, v.x_mm + len, v.y_mm - len],
    ] as const) {
      const line = document.createElementNS(NS, "line");
      line.setAttribute("x1", String(x1));
      line.setAttribute("y1", String(y1));
      line.setAttribute("x2", String(x2));
      line.setAttribute("y2", String(y2));
      line.setAttribute("stroke", color);
      line.setAttribute("stroke-width", "0.18");
      layer.appendChild(line);
    }
  }
  host.appendChild(layer);
}

function paintPalette(state: ProjectState) {
  // Only clear the chip container — the Auto Routing button and status
  // span are permanent strip residents and must survive every refresh.
  els.paletteChips.innerHTML = "";
  els.palette.classList.toggle("empty", state.palette.length === 0);
  for (const item of state.palette) {
    const chip = document.createElement("div");
    chip.className = "palette-chip";
    chip.dataset.reference = item.reference;
    chip.innerHTML = `
      <span class="chip-ref"></span>
      <span class="chip-val"></span>
      <span class="chip-meta">${item.pad_count}p</span>
    `;
    chip.querySelector(".chip-ref")!.textContent = item.reference;
    chip.querySelector(".chip-val")!.textContent = item.value || item.library;
    // Read-only render: the palette is informational. Placement is
    // agent-driven via the script API (`place REF X Y` in the script).
    els.paletteChips.appendChild(chip);
  }
}

// Auto Routing button: kicks off an in-process GA search on the backend
// and streams generation/trial progress into the status span via
// `autoroute:*` events. Toggles between Start/Stop while running.
let autorouteRunning = false;
function setAutorouteIdle(label = "Auto Routing") {
  autorouteRunning = false;
  els.autorouteBtn.textContent = label;
  els.autorouteBtn.classList.remove("running");
  els.autorouteBtn.disabled = false;
}
function setAutorouteRunning() {
  autorouteRunning = true;
  els.autorouteBtn.textContent = "Stop";
  els.autorouteBtn.classList.add("running");
  els.autorouteBtn.disabled = false;
}
els.autorouteBtn.addEventListener("click", async () => {
  if (autorouteRunning) {
    els.autorouteBtn.disabled = true;
    try {
      await invoke("stop_autoroute");
      els.autorouteStatus.textContent = "stopping… (will commit best so far)";
    } catch (e) {
      els.autorouteStatus.textContent = `stop error: ${e}`;
      els.autorouteStatus.classList.add("error");
      els.autorouteBtn.disabled = false;
    }
    return;
  }
  setAutorouteRunning();
  els.autorouteStatus.className = "autoroute-status";
  els.autorouteStatus.textContent = "starting…";
  try {
    await invoke("start_autoroute", { budgetSecs: 600 });
  } catch (e) {
    els.autorouteStatus.textContent = `error: ${e}`;
    els.autorouteStatus.classList.add("error");
    setAutorouteIdle();
  }
});

type AutorouteProgress = {
  generation: number;
  evaluations: number;
  cache_hits: number;
  elapsed_secs: number;
  best_score: number;
  best_drc_errors: number;
  best_failed_nets: number;
  best_length_mm: number;
  best_vias: number;
  best_cell_mm: number;
  best_via_cost: number;
  best_clearance_mm: number;
  improved: boolean;
};

type AutorouteOutcome = {
  generations: number;
  total_evaluations: number;
  cache_hits: number;
  elapsed_secs: number;
  best: AutorouteProgress | null;
};

void listen<AutorouteProgress>("autoroute:progress", (e) => {
  const p = e.payload;
  els.autorouteStatus.textContent =
    `gen ${p.generation} | eval ${p.evaluations} (+${p.cache_hits} cached) | ` +
    `best ${p.best_length_mm.toFixed(1)}mm vias ${p.best_vias} ` +
    `(err ${p.best_drc_errors}) | ${p.elapsed_secs.toFixed(0)}s`;
});
void listen<AutorouteOutcome>("autoroute:done", (e) => {
  const o = e.payload;
  const b = o.best;
  els.autorouteStatus.classList.remove("error");
  els.autorouteStatus.classList.add("done");
  els.autorouteStatus.textContent = b
    ? `done: ${o.total_evaluations} evals (+${o.cache_hits} cached), ${o.generations} gens in ${o.elapsed_secs.toFixed(0)}s. ` +
      `best ${b.best_length_mm.toFixed(1)}mm, ${b.best_vias} vias, ${b.best_drc_errors} DRC err, ` +
      `cell=${b.best_cell_mm}mm via_cost=${b.best_via_cost} clr=${b.best_clearance_mm}mm`
    : `done: no trials completed`;
  setAutorouteIdle();
});
void listen<string>("autoroute:error", (e) => {
  els.autorouteStatus.classList.add("error");
  els.autorouteStatus.textContent = `error: ${e.payload}`;
  setAutorouteIdle();
});

type JlcpcbPackResult = {
  ready: boolean;
  zip_path: string;
  file_count: number;
  blocking_reasons: string[];
};

type OdbPackResult = {
  tgz_path: string;
  file_count: number;
};

els.odbBtn.addEventListener("click", async () => {
  els.odbBtn.disabled = true;
  els.autorouteStatus.className = "autoroute-status";
  els.autorouteStatus.textContent = "exporting ODB++…";
  try {
    const res = await invoke<OdbPackResult>("export_odb_pack");
    els.autorouteStatus.classList.add("done");
    els.autorouteStatus.textContent =
      `ODB++ written: ${res.file_count} files → ${res.tgz_path}`;
  } catch (e) {
    els.autorouteStatus.classList.add("error");
    els.autorouteStatus.textContent = `ODB++ error: ${e}`;
  } finally {
    els.odbBtn.disabled = false;
  }
});

els.stitchBtn.addEventListener("click", async () => {
  els.stitchBtn.disabled = true;
  els.autorouteStatus.className = "autoroute-status";
  els.autorouteStatus.textContent = "stitching isolated GND pads…";
  try {
    const res = await invoke<{ stitched: number; unreachable: string[] }>(
      "stitch_isolated_pads",
    );
    if (res.unreachable.length > 0) {
      els.autorouteStatus.classList.add("error");
      els.autorouteStatus.textContent =
        `stitched ${res.stitched}; ${res.unreachable.length} still need a reroute: ${res.unreachable.join(", ")}`;
    } else {
      els.autorouteStatus.classList.add("done");
      els.autorouteStatus.textContent =
        res.stitched > 0
          ? `stitched ${res.stitched} isolated pad(s)`
          : "no isolated pads";
    }
  } catch (e) {
    els.autorouteStatus.classList.add("error");
    els.autorouteStatus.textContent = `stitch error: ${e}`;
  } finally {
    els.stitchBtn.disabled = false;
  }
});

els.jlcpcbBtn.addEventListener("click", async () => {
  els.jlcpcbBtn.disabled = true;
  els.autorouteStatus.className = "autoroute-status";
  els.autorouteStatus.textContent = "packing for JLCPCB…";
  try {
    const res = await invoke<JlcpcbPackResult>("export_jlcpcb_pack");
    if (res.ready) {
      els.autorouteStatus.classList.add("done");
      els.autorouteStatus.textContent =
        `JLCPCB ready: ${res.file_count} files → ${res.zip_path}`;
    } else {
      els.autorouteStatus.classList.add("error");
      els.autorouteStatus.textContent =
        `JLCPCB NOT READY (${res.blocking_reasons.length}): ${res.blocking_reasons.join("; ")} — zip at ${res.zip_path}`;
    }
  } catch (e) {
    els.autorouteStatus.classList.add("error");
    els.autorouteStatus.textContent = `JLCPCB error: ${e}`;
  } finally {
    els.jlcpcbBtn.disabled = false;
  }
});

async function refresh() {
  const state = await invoke<ProjectState>("project_state");
  lastState = state;
  els.name.textContent = state.name;
  els.symbols.textContent = String(state.symbol_count);
  els.nets.textContent = String(state.net_count);
  els.footprints.textContent = String(state.footprint_count);
  els.api.textContent = state.api_addr;
  if (state.outline) {
    els.boardW.textContent = String(Math.round(state.outline.w_mm));
    els.boardH.textContent = String(Math.round(state.outline.h_mm));
  } else {
    els.boardW.textContent = "—";
    els.boardH.textContent = "—";
  }
  // Only surface the photos toggle when there is something to show.
  const hasOverlays = state.photo_overlays.length > 0;
  els.togglePhotos.toggleAttribute("hidden", !hasOverlays);
  els.togglePhotos.classList.toggle("active", photosEnabled);
  paintPalette(state);
  paintCanvas(state);
}

let libraryThumbCache = new Map<string, string>(); // attachment_id → data URI

async function refreshLibrary() {
  const data = await invoke<{ entries: LibraryEntry[] }>("library_state");
  els.libraryCount.textContent = String(data.entries.length);
  els.library.innerHTML = "";
  if (data.entries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "library-empty";
    empty.textContent = "no components yet — your agent will save parts here as you design";
    els.library.appendChild(empty);
    return;
  }
  for (const entry of data.entries) {
    const card = document.createElement("div");
    card.className = "library-card";
    card.dataset.key = entry.key;

    // Thumbnail = first photo attachment, if any.
    const thumb = document.createElement("div");
    thumb.className = "library-thumb";
    const photo = entry.attachments.find((a) =>
      a.mime.startsWith("image/")
    );
    if (photo) {
      const cached = libraryThumbCache.get(photo.id);
      if (cached) {
        thumb.style.backgroundImage = `url(${cached})`;
      } else {
        invoke<string>("library_attachment_data_uri", {
          key: entry.key,
          attachmentId: photo.id,
        })
          .then((uri) => {
            libraryThumbCache.set(photo.id, uri);
            thumb.style.backgroundImage = `url(${uri})`;
          })
          .catch(() => {});
      }
    } else {
      thumb.classList.add("library-thumb-empty");
      thumb.textContent = entry.key.slice(0, 2).toUpperCase();
    }
    card.appendChild(thumb);

    const body = document.createElement("div");
    body.className = "library-body";
    const title = document.createElement("div");
    title.className = "library-key";
    title.textContent = entry.key;
    body.appendChild(title);
    if (entry.default_value) {
      const val = document.createElement("div");
      val.className = "library-value";
      val.textContent = entry.default_value;
      body.appendChild(val);
    }
    const meta = document.createElement("div");
    meta.className = "library-meta";
    const parts = [`${entry.pad_count} pads`];
    if (entry.edge_mounted) parts.push("edge");
    if (entry.attachments.length > 0)
      parts.push(`${entry.attachments.length} attached`);
    meta.textContent = parts.join(" · ");
    body.appendChild(meta);
    if (entry.description) {
      const desc = document.createElement("div");
      desc.className = "library-desc";
      desc.textContent = entry.description;
      body.appendChild(desc);
    }
    card.appendChild(body);
    els.library.appendChild(card);
  }
}

// Every backend mutation publishes an event immediately and we just
// repaint from `project_state`. No queueing, no animation — what the
// agent did appears the instant it lands.
async function playEvent(data: AnyEvent) {
  if (data.kind === "Activity") {
    appendActivity(data.level, data.message);
    return;
  }
  if (data.kind === "LibraryChanged") {
    await refreshLibrary();
    // Suppress the full review repaint when the change came from one
    // of our own optimistic mutations — the DOM is already up to date
    // and a repaint here would reset the user's scroll position.
    if (view === "review" && reviewMutationInFlight === 0) {
      void paintReviewPreserveScroll();
    }
    return;
  }
  if (data.kind === "PendingLibraryChanged") {
    await openConfirmModal();
    return;
  }
  const isBoardEvent =
    data.kind === "PlacementProgress" ||
    data.kind === "RoutingChanged" ||
    data.kind === "FootprintAdded" ||
    data.kind === "FootprintMoved" ||
    data.kind === "FootprintRemoved" ||
    data.kind === "OutlineChanged";
  const isSchematicEvent =
    data.kind === "SymbolAdded" || data.kind === "NetChanged";
  if (isBoardEvent && view !== "board") setView("board");
  else if (isSchematicEvent && view !== "schematic") setView("schematic");
  await refresh();
}

async function start() {
  setView("board");
  appendActivity("info", "ui boot");
  await refresh();
  await refreshLibrary();
  // Check on boot in case the agent queued entries before the UI
  // attached — the modal then opens immediately.
  await openConfirmModal().catch(() => {});
  appendActivity("info", "ui ready");

  await listen<AnyEvent>("pcb://event", (ev) => {
    playEvent(ev.payload).catch(reportFatal);
  });
}

// Used to silence the "noUnusedLocals" warning when confirmModalOpen
// is touched only inside async paths.
void confirmModalOpen;

start().catch(reportFatal);
