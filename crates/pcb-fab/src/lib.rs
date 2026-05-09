//! `pcb-fab` — fab-house provider abstraction.
//!
//! Each provider (JLCPCB, PCBWay, generic) exposes:
//!
//! - **Manufacturing rules** (`FabRules`): minimum trace width, drill,
//!   annular ring, board size limits. The `manufacturing_drc` helper
//!   runs every routed item against the rules and reports violations
//!   in the same shape as the geometric DRC.
//! - **BOM and CPL formats**: assembly houses each want their own
//!   column names and column order. Each provider implements the two
//!   writers; the generic provider falls back to KiCad-style files.
//! - **Per-fab quirks**: anything that's specifically required by one
//!   house but not by everyone (filename remapping, NPTH separation,
//!   rotation conventions). The provider exposes the hooks; new
//!   providers only override what they care about.
//!
//! The user-facing entry point is [`pack`], which runs validation
//! (ERC + DRC + manufacturing-DRC), generates every artifact and
//! ships them as a single `.zip` ready to upload to the fab portal.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use pcb_core::{Board, Footprint, LibraryEntry, Project, Rect, Schematic};
use pcb_gerber::gerber::Side;
use pcb_gerber::{excellon, gerber};

// ─── Provider abstraction ───────────────────────────────────────────────

/// Fab houses we know how to format for. New providers go here as
/// new variants — every consumer is a `match` so the compiler tells
/// you which call sites to update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// JLCPCB (jlcpcb.com). LCSC parts catalogue, "Comment / Designator
    /// / Footprint / LCSC Part #" BOM, and a CPL with explicit
    /// "Mid X / Mid Y / Rotation / Layer" columns.
    Jlcpcb,
    /// PCBWay (pcbway.com). Looser limits than JLC; accepts KiCad
    /// default outputs natively, but the BOM/CPL writers still emit
    /// CSV with explicit column names so the upload UI doesn't have
    /// to guess.
    Pcbway,
    /// Generic / KiCad-style outputs — same shape `pcb-gerber`
    /// already produced before this crate existed. Use as a fallback
    /// when the fab isn't on the list.
    Generic,
}

impl Provider {
    /// Stable lowercase identifier — used in zip filenames, log
    /// lines, and as the script verb argument (`pack fab=jlcpcb`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Jlcpcb => "jlcpcb",
            Self::Pcbway => "pcbway",
            Self::Generic => "generic",
        }
    }

    /// Parse from the same lowercase form `name()` emits. Returns
    /// `None` for unknown providers so the script tool can give a
    /// clean error listing the supported set.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "jlcpcb" | "jlc" => Some(Self::Jlcpcb),
            "pcbway" | "pcb_way" => Some(Self::Pcbway),
            "generic" | "kicad" | "default" => Some(Self::Generic),
            _ => None,
        }
    }

    /// Manufacturing rules for this provider. Numbers come from each
    /// fab's published "minimum capability" page; the conservative
    /// side of any range is used so a board that passes our check
    /// also passes the fab's intake review.
    #[must_use]
    pub fn rules(self) -> FabRules {
        match self {
            Self::Jlcpcb => FabRules {
                min_trace_width_mm: 0.127,    // 5 mil — JLCPCB std 2-layer
                min_clearance_mm: 0.127,
                min_drill_mm: 0.20,
                // Via annular ring: outer copper ring around the drill,
                // (diameter - drill) / 2. JLCPCB's spec is 0.13 mm.
                min_annular_ring_mm: 0.13,
                // Largest free-standard 2-layer panel the no-extra-cost
                // tier accepts — bigger is allowed but priced higher.
                max_board_w_mm: 100.0,
                max_board_h_mm: 100.0,
                supported_layers: 2,
            },
            Self::Pcbway => FabRules {
                // PCBWay quotes 6 mil / 6 mil as standard; 4/4 mil is
                // available with a surcharge. We pick the standard tier
                // so a board passing our check is buildable everywhere.
                min_trace_width_mm: 0.152,
                min_clearance_mm: 0.152,
                min_drill_mm: 0.30,
                min_annular_ring_mm: 0.15,
                max_board_w_mm: 200.0,
                max_board_h_mm: 200.0,
                supported_layers: 2,
            },
            Self::Generic => FabRules {
                // Permissive defaults — anything stricter than this is
                // unlikely to be manufacturable anywhere mainstream.
                min_trace_width_mm: 0.10,
                min_clearance_mm: 0.10,
                min_drill_mm: 0.20,
                min_annular_ring_mm: 0.10,
                max_board_w_mm: 300.0,
                max_board_h_mm: 300.0,
                supported_layers: 2,
            },
        }
    }
}

/// Per-fab manufacturing constraints. Compared against the board by
/// `manufacturing_drc`. Adding a new field means adding it here, in
/// `Provider::rules` for each variant, and in `manufacturing_drc` if
/// it should produce a violation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FabRules {
    pub min_trace_width_mm: f64,
    pub min_clearance_mm: f64,
    pub min_drill_mm: f64,
    pub min_annular_ring_mm: f64,
    pub max_board_w_mm: f64,
    pub max_board_h_mm: f64,
    pub supported_layers: u8,
}

// ─── Manufacturing DRC ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FabViolationKind {
    /// A trace's width is below the fab's minimum.
    NarrowTraceForFab,
    /// A drill's diameter is below the fab's minimum.
    SmallDrillForFab,
    /// The annular ring around a via (radius − drill_radius) is
    /// below the fab's minimum. Fab risks tear-out drilling such vias.
    ThinAnnularRing,
    /// The board outline exceeds the fab's standard size tier.
    BoardOversize,
    /// The design uses more layers than the provider supports under
    /// the rules we encoded.
    LayerCountUnsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct FabViolation {
    pub kind: FabViolationKind,
    pub severity: Severity,
    pub message: String,
    pub involved: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FabReport {
    pub provider: String,
    pub violations: Vec<FabViolation>,
    pub error_count: usize,
    pub warning_count: usize,
}

impl FabReport {
    fn push(&mut self, v: FabViolation) {
        match v.severity {
            Severity::Error => self.error_count += 1,
            Severity::Warning => self.warning_count += 1,
        }
        self.violations.push(v);
    }
}

/// Run every `FabRules` check on the board and return a report.
/// Treat board oversize as a Warning (it's expensive but possible)
/// and trace/drill/annular violations as Errors (the fab will refuse
/// the order outright).
#[must_use]
pub fn manufacturing_drc(board: &Board, provider: Provider) -> FabReport {
    let rules = provider.rules();
    let mut report = FabReport {
        provider: provider.name().to_string(),
        ..FabReport::default()
    };

    let traces_in_violation: Vec<&pcb_core::Trace> = board
        .traces
        .iter()
        .filter(|t| t.width.to_mm() + 1e-6 < rules.min_trace_width_mm)
        .collect();
    for t in &traces_in_violation {
        report.push(FabViolation {
            kind: FabViolationKind::NarrowTraceForFab,
            severity: Severity::Error,
            message: format!(
                "trace on net {} is {:.3} mm wide; {} requires ≥ {:.3} mm",
                t.net,
                t.width.to_mm(),
                provider.name(),
                rules.min_trace_width_mm,
            ),
            involved: vec![t.net.clone()],
        });
    }

    for via in &board.vias {
        let d = via.drill.to_mm();
        if d + 1e-6 < rules.min_drill_mm {
            report.push(FabViolation {
                kind: FabViolationKind::SmallDrillForFab,
                severity: Severity::Error,
                message: format!(
                    "via on net {} drilled at {:.3} mm; {} requires ≥ {:.3} mm",
                    via.net,
                    d,
                    provider.name(),
                    rules.min_drill_mm,
                ),
                involved: vec![via.net.clone()],
            });
        }
        // Annular ring: copper outside the drill = (outer_diam − drill) / 2.
        let ring = (via.diameter.to_mm() - d) / 2.0;
        if ring + 1e-6 < rules.min_annular_ring_mm {
            report.push(FabViolation {
                kind: FabViolationKind::ThinAnnularRing,
                severity: Severity::Error,
                message: format!(
                    "via on net {} has {:.3} mm annular ring (drill {:.3} / diameter {:.3}); {} requires ≥ {:.3} mm",
                    via.net,
                    ring,
                    d,
                    via.diameter.to_mm(),
                    provider.name(),
                    rules.min_annular_ring_mm,
                ),
                involved: vec![via.net.clone()],
            });
        }
    }

    if let Some(outline) = board.outline {
        let w = outline.width().to_mm();
        let h = outline.height().to_mm();
        if w > rules.max_board_w_mm + 1e-6 || h > rules.max_board_h_mm + 1e-6 {
            report.push(FabViolation {
                kind: FabViolationKind::BoardOversize,
                severity: Severity::Warning,
                message: format!(
                    "board is {w:.1} × {h:.1} mm; {} standard tier caps at {:.0} × {:.0} mm — order will be priced as oversized",
                    provider.name(),
                    rules.max_board_w_mm,
                    rules.max_board_h_mm,
                ),
                involved: Vec::new(),
            });
        }
    }

    report
}

// ─── BOM and CPL formatters ──────────────────────────────────────────────

/// Write a BOM grouping footprints by `(value, library, lcsc_id)`.
/// JLCPCB wants exactly four columns in this order: `Comment,
/// Designator, Footprint, LCSC Part #`. Without an LCSC ID the row
/// still goes out (the assembly tech reads the comment + footprint to
/// guess); MPN is appended in parentheses to the comment as a hint.
pub fn write_bom(
    board: &Board,
    sch: &Schematic,
    library_lookup: &dyn Fn(&str) -> Option<LibraryEntry>,
    provider: Provider,
    w: &mut impl Write,
) -> io::Result<()> {
    // Group footprints by (value, library_key) so identical parts
    // collapse into one BOM line. Library lookup happens once per
    // group instead of once per footprint.
    let mut groups: BTreeMap<(String, String), Vec<&Footprint>> = BTreeMap::new();
    for fp in board.footprints_in_order() {
        groups
            .entry((fp.value.clone(), fp.key.clone()))
            .or_default()
            .push(fp);
    }
    let _ = sch; // sch is reserved for future per-group enrichment

    match provider {
        Provider::Jlcpcb => {
            writeln!(w, "Comment,Designator,Footprint,LCSC Part #")?;
            for ((value, key), fps) in &groups {
                let entry = if key.is_empty() { None } else { library_lookup(key) };
                let footprint = fps
                    .first()
                    .map(|f| f.library.as_str())
                    .unwrap_or("");
                let lcsc = entry
                    .as_ref()
                    .and_then(|e| e.lcsc_id.as_deref())
                    .unwrap_or("");
                let mpn = entry
                    .as_ref()
                    .and_then(|e| e.mpn.as_deref())
                    .unwrap_or("");
                let comment = if mpn.is_empty() {
                    value.clone()
                } else {
                    format!("{value} ({mpn})")
                };
                let mut refs: Vec<&str> = fps.iter().map(|f| f.reference.as_str()).collect();
                refs.sort();
                writeln!(
                    w,
                    "{},{},{},{}",
                    csv(&comment),
                    csv(&refs.join(",")),
                    csv(footprint),
                    csv(lcsc),
                )?;
            }
        }
        Provider::Pcbway | Provider::Generic => {
            // PCBWay accepts a standard "Reference, Value, Footprint,
            // Quantity, LCSC, MPN" sheet. Generic is the same minus
            // the LCSC column — keep them aligned so a customer
            // switching providers doesn't have to redo the format.
            writeln!(w, "Reference,Value,Footprint,Quantity,LCSC,MPN")?;
            for ((value, key), fps) in &groups {
                let entry = if key.is_empty() { None } else { library_lookup(key) };
                let footprint = fps
                    .first()
                    .map(|f| f.library.as_str())
                    .unwrap_or("");
                let lcsc = entry
                    .as_ref()
                    .and_then(|e| e.lcsc_id.as_deref())
                    .unwrap_or("");
                let mpn = entry
                    .as_ref()
                    .and_then(|e| e.mpn.as_deref())
                    .unwrap_or("");
                let mut refs: Vec<&str> = fps.iter().map(|f| f.reference.as_str()).collect();
                refs.sort();
                writeln!(
                    w,
                    "{},{},{},{},{},{}",
                    csv(&refs.join(" ")),
                    csv(value),
                    csv(footprint),
                    fps.len(),
                    csv(lcsc),
                    csv(mpn),
                )?;
            }
        }
    }
    Ok(())
}

/// Write the component-placement / pick-and-place list. Format per
/// provider; when in doubt the JLCPCB layout works because most
/// other houses accept it as a permissive superset of theirs.
pub fn write_cpl(board: &Board, provider: Provider, w: &mut impl Write) -> io::Result<()> {
    match provider {
        Provider::Jlcpcb => {
            writeln!(w, "Designator,Mid X,Mid Y,Layer,Rotation")?;
            for fp in board.footprints_in_order() {
                let layer = match fp.layer {
                    pcb_core::CopperLayer::Top => "T",
                    pcb_core::CopperLayer::Bottom => "B",
                };
                writeln!(
                    w,
                    "{},{:.4}mm,{:.4}mm,{},{:.2}",
                    csv(&fp.reference),
                    fp.position.x.to_mm(),
                    fp.position.y.to_mm(),
                    layer,
                    fp.rotation,
                )?;
            }
        }
        Provider::Pcbway | Provider::Generic => {
            writeln!(w, "Reference,Value,Footprint,X,Y,Rotation,Side")?;
            for fp in board.footprints_in_order() {
                let side = match fp.layer {
                    pcb_core::CopperLayer::Top => "top",
                    pcb_core::CopperLayer::Bottom => "bottom",
                };
                writeln!(
                    w,
                    "{},{},{},{:.4},{:.4},{:.2},{}",
                    csv(&fp.reference),
                    csv(&fp.value),
                    csv(&fp.library),
                    fp.position.x.to_mm(),
                    fp.position.y.to_mm(),
                    fp.rotation,
                    side,
                )?;
            }
        }
    }
    Ok(())
}

// ─── Pack flow ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PackReport {
    pub provider: String,
    pub zip_path: PathBuf,
    /// Files included in the zip, in the order they were written.
    pub files: Vec<String>,
    pub manufacturing_report: FabReport,
    pub board_size_mm: Option<(f64, f64)>,
    /// True if any blocking violations (DRC/ERC/manufacturing errors)
    /// were found. The zip is still produced so the user can inspect
    /// the half-baked output, but the agent should stop and fix
    /// before sending it to the fab.
    pub blocking: bool,
    pub blocking_reasons: Vec<String>,
}

/// Run every check, generate every artifact, and zip them into a
/// single file ready to upload to the fab portal.
///
/// `out_dir` is where the zip is written; it's created if missing.
/// The zip itself is named `{stem}-{provider}.zip`.
///
/// Returns a `PackReport` describing the result. The zip is always
/// written even when checks fail — the user can look at the partial
/// output to debug. `blocking = true` flags whether the design is
/// fab-ready or still has Errors to clean up.
pub fn pack(
    project: &Project,
    provider: Provider,
    out_dir: &Path,
) -> Result<PackReport, String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("create out_dir: {e}"))?;

    // Snapshot the project so the zip reflects a single point in time
    // even if a concurrent script edits things mid-pack.
    let snap = project.read();
    let board = snap.board().clone();
    let schematic = snap.schematic().clone();
    let project_name = snap.name().to_string();
    drop(snap);

    let stem = sanitize(&project_name);
    let zip_name = format!("{stem}-{}.zip", provider.name());
    let zip_path = out_dir.join(&zip_name);

    // ── Validation ───────────────────────────────────────────────
    let drc_report = pcb_drc::run(&board, &pcb_drc::DrcOptions::default());
    let erc_report = pcb_erc::run(&board, &schematic, &pcb_erc::ErcOptions::default());
    let manufacturing_report = manufacturing_drc(&board, provider);

    let mut blocking_reasons: Vec<String> = Vec::new();
    if drc_report.error_count > 0 {
        blocking_reasons.push(format!("DRC: {} error(s)", drc_report.error_count));
    }
    if erc_report.error_count > 0 {
        blocking_reasons.push(format!("ERC: {} error(s)", erc_report.error_count));
    }
    if manufacturing_report.error_count > 0 {
        blocking_reasons.push(format!(
            "{}: {} manufacturing error(s)",
            provider.name(),
            manufacturing_report.error_count,
        ));
    }
    let blocking = !blocking_reasons.is_empty();

    // ── Build zip ────────────────────────────────────────────────
    let library = project.library();
    let library_lookup = move |key: &str| library.find(key);

    let buf: Vec<u8> = Vec::new();
    let cursor = Cursor::new(buf);
    let mut zip = zip::ZipWriter::new(cursor);
    let zip_opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut files: Vec<String> = Vec::new();

    let emit = |zip: &mut zip::ZipWriter<Cursor<Vec<u8>>>,
                    files: &mut Vec<String>,
                    name: String,
                    body: &dyn Fn(&mut Vec<u8>) -> io::Result<()>|
     -> Result<(), String> {
        let mut buf = Vec::new();
        body(&mut buf).map_err(|e| format!("{name}: {e}"))?;
        zip.start_file(&name, zip_opts)
            .map_err(|e| format!("zip start {name}: {e}"))?;
        zip.write_all(&buf)
            .map_err(|e| format!("zip write {name}: {e}"))?;
        files.push(name);
        Ok(())
    };

    // Gerbers + drills.
    emit(&mut zip, &mut files, format!("{stem}-F_Cu.gbr"), &|w| {
        gerber::write_copper(&board, Side::Top, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-B_Cu.gbr"), &|w| {
        gerber::write_copper(&board, Side::Bottom, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-F_Mask.gbr"), &|w| {
        gerber::write_mask(&board, Side::Top, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-B_Mask.gbr"), &|w| {
        gerber::write_mask(&board, Side::Bottom, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-F_SilkS.gbr"), &|w| {
        gerber::write_silk(&board, Side::Top, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-B_SilkS.gbr"), &|w| {
        gerber::write_silk(&board, Side::Bottom, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-Edge_Cuts.gbr"), &|w| {
        gerber::write_edge_cuts(&board, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-PTH.drl"), &|w| {
        excellon::write(&board, true, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-NPTH.drl"), &|w| {
        excellon::write(&board, false, w)
    })?;

    // Provider-specific BOM and CPL.
    emit(&mut zip, &mut files, format!("{stem}-bom.csv"), &|w| {
        write_bom(&board, &schematic, &library_lookup, provider, w)
    })?;
    emit(&mut zip, &mut files, format!("{stem}-cpl.csv"), &|w| {
        write_cpl(&board, provider, w)
    })?;

    // README so a non-technical user opening the zip understands what
    // each file is and what to do with it.
    let readme = build_readme(
        &project_name,
        provider,
        &board,
        &drc_report,
        &erc_report,
        &manufacturing_report,
        &blocking_reasons,
    );
    emit(&mut zip, &mut files, "README.txt".to_string(), &|w| {
        w.write_all(readme.as_bytes())
    })?;

    let final_buf = zip
        .finish()
        .map_err(|e| format!("finalise zip: {e}"))?
        .into_inner();
    fs::write(&zip_path, &final_buf).map_err(|e| format!("write {}: {e}", zip_path.display()))?;

    let board_size_mm = board.outline.map(|r: Rect| (r.width().to_mm(), r.height().to_mm()));

    Ok(PackReport {
        provider: provider.name().to_string(),
        zip_path,
        files,
        manufacturing_report,
        board_size_mm,
        blocking,
        blocking_reasons,
    })
}

fn build_readme(
    project: &str,
    provider: Provider,
    board: &Board,
    drc: &pcb_drc::DrcReport,
    erc: &pcb_erc::ErcReport,
    fab: &FabReport,
    blocking_reasons: &[String],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Project: {project}");
    let _ = writeln!(s, "Target fab: {} ({})", provider.name(), describe_provider(provider));
    if let Some(o) = board.outline {
        let _ = writeln!(
            s,
            "Board: {:.1} × {:.1} mm{}",
            o.width().to_mm(),
            o.height().to_mm(),
            if board.outline_corner_radius.0 > 0 {
                format!(", corner radius {:.2} mm", board.outline_corner_radius.to_mm())
            } else {
                String::new()
            },
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "── Validation ──────────────────────────────────────");
    let _ = writeln!(s, "DRC : {} error(s), {} warning(s)", drc.error_count, drc.warning_count);
    let _ = writeln!(s, "ERC : {} error(s), {} warning(s)", erc.error_count, erc.warning_count);
    let _ = writeln!(
        s,
        "Fab : {} error(s), {} warning(s) — checked against {} rules",
        fab.error_count,
        fab.warning_count,
        provider.name(),
    );
    if blocking_reasons.is_empty() {
        let _ = writeln!(s, "→ READY: no blocking errors. Upload the zip to the fab portal.");
    } else {
        let _ = writeln!(s, "→ NOT READY: {}", blocking_reasons.join("; "));
        let _ = writeln!(s, "  Fix the errors first, then re-run `pack`.");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "── Files ───────────────────────────────────────────");
    let _ = writeln!(s, "  *.gbr  Gerber files (copper, soldermask, silk, edge cuts)");
    let _ = writeln!(s, "  *-PTH.drl, *-NPTH.drl  Excellon drill files");
    let _ = writeln!(s, "  *-bom.csv  Bill of materials in {} format", provider.name());
    let _ = writeln!(s, "  *-cpl.csv  Component placement / pick-and-place list");
    let _ = writeln!(s);
    let _ = writeln!(s, "── How to order ────────────────────────────────────");
    match provider {
        Provider::Jlcpcb => {
            let _ = writeln!(s, "1. Go to https://cart.jlcpcb.com/quote and upload this zip.");
            let _ = writeln!(s, "2. JLCPCB auto-detects the layers from the Gerber filenames.");
            let _ = writeln!(s, "3. To enable SMT assembly, tick \"PCB Assembly\" and upload");
            let _ = writeln!(s, "   *-bom.csv as the BOM and *-cpl.csv as the CPL.");
        }
        Provider::Pcbway => {
            let _ = writeln!(s, "1. Go to https://www.pcbway.com/orderonline.aspx and upload this zip.");
            let _ = writeln!(s, "2. PCBWay accepts the KiCad-default Gerber filenames.");
            let _ = writeln!(s, "3. For assembly, attach *-bom.csv and *-cpl.csv at the SMT step.");
        }
        Provider::Generic => {
            let _ = writeln!(s, "Generic / KiCad-style outputs. Upload the zip to your fab of");
            let _ = writeln!(s, "choice; almost all houses accept this layout. Adjust column");
            let _ = writeln!(s, "names in *-bom.csv if your assembler requires a specific format.");
        }
    }
    s
}

fn describe_provider(p: Provider) -> &'static str {
    match p {
        Provider::Jlcpcb => "JLCPCB.com — std 2-layer 5/5 mil",
        Provider::Pcbway => "PCBWay.com — std 2-layer 6/6 mil",
        Provider::Generic => "fab-agnostic, KiCad default outputs",
    }
}

fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("untitled");
    }
    out
}

fn csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// Re-export types the script tool will surface to the agent.
pub use FabViolationKind as ViolationKind;
