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

type LibraryAttachment = {
  id: string;
  kind: string;
  filename: string;
  mime: string;
  added_at: number;
  view_transform?: ViewTransform;
};

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
    <span class="tab" id="toggle-library" title="show/hide library panel">lib</span>
    <span class="tab" id="toggle-activity" title="show/hide activity log">log</span>
    <span class="label">api</span><span class="value accent" id="proj-api">—</span>
  </div>
  <div class="palette-strip" id="palette-strip"></div>
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
  boardW: document.getElementById("board-w")!,
  boardH: document.getElementById("board-h")!,
  infoModal: document.getElementById("info-modal")!,
  infoCard: document.getElementById("info-modal-card")!,
  confirmModal: document.getElementById("confirm-modal")!,
  confirmCard: document.getElementById("confirm-modal-card")!,
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
      <button class="btn-confirm" data-key="${esc(entry.key)}" ${photo ? "" : "disabled title='attach a photo first'"}>Save to library</button>
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
  els.palette.innerHTML = "";
  if (state.palette.length === 0) {
    els.palette.classList.add("empty");
    els.palette.textContent = "palette empty";
    return;
  }
  els.palette.classList.remove("empty");
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
    els.palette.appendChild(chip);
  }
}

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
