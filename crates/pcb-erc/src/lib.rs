//! `pcb-erc` — electrical rules check.
//!
//! Schematic-side validation: catches netlist bugs *before* they
//! propagate into placement and routing. Sister crate to `pcb-drc`,
//! which validates board geometry. The split is conceptual:
//!
//! - DRC: "is the copper geometry legal?" (clearance, drill, edge…)
//! - ERC: "is the netlist coherent?" (pins wired, nets non-trivial…)
//!
//! ERC runs over the schematic plus the board's pad-net assignments,
//! so it can also flag mismatches between the two sides — e.g. a
//! footprint pad referencing a net the schematic doesn't declare.

use serde::Serialize;

use pcb_core::schematic::{PinRole, Schematic};
use pcb_core::Board;

#[derive(Debug, Clone)]
pub struct ErcOptions {
    /// Run the heuristic "design intent" rules in addition to the
    /// strict netlist checks. These flag conventions (decoupling
    /// caps near power pins, pull-ups on I²C lines) that almost
    /// every working design follows but aren't strictly required —
    /// they're heuristics, not hard rules. Default `true`; turn off
    /// with `erc strict_only=true` when you want only the always-bug
    /// findings.
    pub heuristics: bool,
    /// Maximum body-to-body distance (mm) between a chip's PowerIn
    /// pin and a capacitor on the same net for the cap to count as
    /// a decoupling cap for that pin. Beyond this the heuristic
    /// fires `MissingDecouplingCap`.
    pub decoupling_max_dist_mm: f64,
}

impl Default for ErcOptions {
    fn default() -> Self {
        Self {
            heuristics: true,
            // 5 mm is generous — best practice is < 2 mm, but smaller
            // boards / hand-routed designs sometimes can't get there
            // and we don't want false positives.
            decoupling_max_dist_mm: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The schematic is provably broken (e.g. same pin in two nets) —
    /// the agent should fix this before doing anything else.
    Error,
    /// Probably a bug, but the design might still build (e.g. a pin
    /// the agent intentionally left floating). Worth surfacing so the
    /// agent can decide.
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErcKind {
    /// A pin on a placed symbol is not assigned to any net.
    FloatingPin,
    /// A net has fewer than two pin connections — it cannot conduct
    /// (or has no electrical purpose).
    FloatingNet,
    /// A `(symbol, pin)` pair appears in 2+ nets — model corruption,
    /// the router will see ambiguous connectivity.
    DuplicatePin,
    /// A net is declared in the schematic but has no connections at
    /// all. Either the agent forgot to wire it or it's stale.
    EmptyNet,
    /// A footprint pad references a net the schematic doesn't declare.
    /// Indicates the agent edited footprint pad nets without keeping
    /// the schematic in sync.
    PhantomNet,
    /// A symbol exists on the schematic but none of its pins are in
    /// any net — the part is electrically disconnected. Often the
    /// agent forgot to wire the whole component.
    OrphanSymbol,
    /// Two or more `Output` pins drive the same net — an electrical
    /// short. `Bidir` pins are tolerated (they negotiate at the
    /// protocol level on a shared bus).
    MultipleDrivers,
    /// A net has at least one `PowerIn` pin but no `PowerOut` source —
    /// the classic "forgot to wire the regulator" bug.
    UnpoweredPowerNet,
    /// An `Input` pin sits on a net with no driver (no `Output`,
    /// `Bidir`, or `PowerOut`). Either the input is meant to float
    /// (rare; signal it explicitly) or the agent forgot a driver.
    UnconnectedInput,
    /// Heuristic: a chip's `PowerIn` pin has no capacitor on the same
    /// net within `decoupling_max_dist_mm`. Decoupling caps belong
    /// physically close to the chip pin; absence usually means the
    /// agent forgot to add one or placed it across the board.
    MissingDecouplingCap,
    /// Heuristic: a net whose name looks like an I²C line (SDA/SCL)
    /// is `Bidir` on every endpoint but no resistor sits on it. I²C
    /// requires pull-ups to VCC; a missing one means the bus won't
    /// communicate. Triggers only on the obvious naming convention
    /// to keep false positives down.
    MissingPullup,
}

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub kind: ErcKind,
    pub severity: Severity,
    pub message: String,
    /// Schematic-side identifier(s) involved: pin labels like
    /// `"R1.1"`, net names, or symbol references.
    pub involved: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ErcReport {
    pub violations: Vec<Violation>,
    pub error_count: usize,
    pub warning_count: usize,
}

impl ErcReport {
    fn push(&mut self, v: Violation) {
        match v.severity {
            Severity::Error => self.error_count += 1,
            Severity::Warning => self.warning_count += 1,
        }
        self.violations.push(v);
    }
}

#[must_use]
pub fn run(board: &Board, sch: &Schematic, opts: &ErcOptions) -> ErcReport {
    let mut report = ErcReport::default();
    check_duplicate_pins(sch, &mut report);
    check_floating_pins(sch, &mut report);
    check_floating_and_empty_nets(sch, &mut report);
    check_orphan_symbols(sch, &mut report);
    check_phantom_nets(board, sch, &mut report);
    check_role_based_rules(board, sch, &mut report);
    if opts.heuristics {
        check_decoupling(board, sch, opts.decoupling_max_dist_mm, &mut report);
        check_i2c_pullups(sch, &mut report);
    }
    report
}

/// Same `(symbol_id, pin_number)` declared in 2+ nets. The router
/// would see contradictory connectivity; flag as Error so the agent
/// fixes the schematic before doing anything else.
fn check_duplicate_pins(sch: &Schematic, report: &mut ErcReport) {
    use std::collections::HashMap;
    // (symbol_id, pin_number) -> set of net names referencing it.
    let mut homes: HashMap<(pcb_core::Id, String), Vec<String>> = HashMap::new();
    for (net_name, net) in &sch.nets {
        for c in &net.connections {
            homes
                .entry((c.symbol_id, c.pin_number.clone()))
                .or_default()
                .push(net_name.clone());
        }
    }
    for ((sym_id, pin), mut nets) in homes {
        if nets.len() < 2 {
            continue;
        }
        nets.sort();
        nets.dedup();
        if nets.len() < 2 {
            // Same net listed twice on the same pin — also wrong but
            // less alarming; keep this as the duplicate-pin error path
            // so it still surfaces.
        }
        let label = pin_label(sch, sym_id, &pin);
        report.push(Violation {
            kind: ErcKind::DuplicatePin,
            severity: Severity::Error,
            message: format!(
                "{label} appears in {n} nets ({}); the router will see ambiguous connectivity",
                nets.join(", "),
                n = nets.len(),
            ),
            involved: std::iter::once(label).chain(nets).collect(),
        });
    }
}

/// Symbol pins not in any net. Common cause: the agent declared the
/// symbol but forgot to wire one of its pins. Fully-orphan symbols
/// (no pin on any net) get a single `OrphanSymbol` violation instead
/// — emitting `FloatingPin` per pin too would be redundant noise.
fn check_floating_pins(sch: &Schematic, report: &mut ErcReport) {
    use std::collections::HashSet;
    let mut wired: HashSet<(pcb_core::Id, String)> = HashSet::new();
    let mut covered_symbols: HashSet<pcb_core::Id> = HashSet::new();
    for net in sch.nets.values() {
        for c in &net.connections {
            wired.insert((c.symbol_id, c.pin_number.clone()));
            covered_symbols.insert(c.symbol_id);
        }
    }
    for sym in sch.symbols_in_order() {
        if !covered_symbols.contains(&sym.id) {
            // Whole symbol is orphan; OrphanSymbol fires instead.
            continue;
        }
        for pin in sym.kind.pins() {
            if wired.contains(&(sym.id, pin.number.clone())) {
                continue;
            }
            let label = format!("{}.{}", sym.reference, pin.number);
            report.push(Violation {
                kind: ErcKind::FloatingPin,
                severity: Severity::Warning,
                message: format!("{label} is not connected to any net"),
                involved: vec![label],
            });
        }
    }
}

/// Nets with 0 or 1 connections. 0 = empty (declared but never wired);
/// 1 = floating (only one endpoint, can't form a circuit).
fn check_floating_and_empty_nets(sch: &Schematic, report: &mut ErcReport) {
    for net in sch.nets.values() {
        // Dedup connections so a (mistakenly) twice-listed pin doesn't
        // mask a real "only one pin" net.
        // Dedup via a set since `Id` doesn't impl `Ord`.
        let endpoints: std::collections::HashSet<(pcb_core::Id, String)> = net
            .connections
            .iter()
            .map(|c| (c.symbol_id, c.pin_number.clone()))
            .collect();
        let endpoints: Vec<_> = endpoints.into_iter().collect();
        match endpoints.len() {
            0 => report.push(Violation {
                kind: ErcKind::EmptyNet,
                severity: Severity::Warning,
                message: format!("net {} has no connections", net.name),
                involved: vec![net.name.clone()],
            }),
            1 => {
                let (sym_id, pin) = &endpoints[0];
                let label = pin_label(sch, *sym_id, pin);
                report.push(Violation {
                    kind: ErcKind::FloatingNet,
                    severity: Severity::Warning,
                    message: format!(
                        "net {} only touches {label} — needs at least 2 endpoints to conduct",
                        net.name,
                    ),
                    involved: vec![net.name.clone(), label],
                });
            }
            _ => {}
        }
    }
}

/// Symbols whose pins are entirely absent from every net. The whole
/// component is dangling — usually a schematic mistake (forgotten
/// wiring) rather than a deliberate "no-connect" symbol.
fn check_orphan_symbols(sch: &Schematic, report: &mut ErcReport) {
    use std::collections::HashSet;
    let mut covered: HashSet<pcb_core::Id> = HashSet::new();
    for net in sch.nets.values() {
        for c in &net.connections {
            covered.insert(c.symbol_id);
        }
    }
    for sym in sch.symbols_in_order() {
        if covered.contains(&sym.id) {
            continue;
        }
        // Skip symbols with zero pins (degenerate placeholders).
        if sym.kind.pins().is_empty() {
            continue;
        }
        report.push(Violation {
            kind: ErcKind::OrphanSymbol,
            severity: Severity::Warning,
            message: format!(
                "symbol {} ({}) has no pins on any net",
                sym.reference,
                sym.kind.label(),
            ),
            involved: vec![sym.reference.clone()],
        });
    }
}

/// Footprint pads referencing nets the schematic doesn't declare.
/// Catches the case where the agent assigned `pad.net` directly
/// (bypassing the schematic) or renamed a net on one side only.
fn check_phantom_nets(board: &Board, sch: &Schematic, report: &mut ErcReport) {
    use std::collections::HashSet;
    let known_nets: HashSet<&str> = sch.nets.keys().map(String::as_str).collect();
    // Also count pours as a legitimate source of the net name (a
    // ground pour on its own implicitly defines GND even before any
    // schematic-side connection).
    let pour_nets: HashSet<&str> = board.pours.iter().map(|p| p.net.as_str()).collect();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for fp in board.footprints_in_order() {
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            if known_nets.contains(net) || pour_nets.contains(net) {
                continue;
            }
            let label = format!("{}.{}", fp.reference, pad.number);
            // Dedup by (net, pad ref) so we only emit one violation
            // per offending pad even if the same phantom net appears
            // on many pads of one footprint.
            if !seen.insert((net.to_string(), label.clone())) {
                continue;
            }
            report.push(Violation {
                kind: ErcKind::PhantomNet,
                severity: Severity::Error,
                message: format!(
                    "footprint pad {label} is on net `{net}` but the schematic doesn't declare that net",
                ),
                involved: vec![label, net.to_string()],
            });
        }
    }
}

/// Per-net checks that depend on `PinRole`:
///
/// - **MultipleDrivers**: 2+ `Output` pins on the same net.
/// - **UnpoweredPowerNet**: at least one `PowerIn` and no `PowerOut`
///   source. (A net with only `PowerIn` pins is sinking energy from
///   nowhere — the agent forgot to wire a regulator or supply.)
/// - **UnconnectedInput**: an `Input` pin on a net that has no
///   driver (no Output, Bidir, or PowerOut). The input is left
///   electrically floating; if intentional the agent should re-wire
///   it to GND or VCC explicitly.
///
/// Pours of a power net (e.g. a bottom-layer GND pour) count as a
/// PowerOut source: the pour itself is the supply geometry, even if
/// no schematic pin declares Power explicitly. Avoids spurious
/// "unpowered GND" warnings on every project that uses a ground pour.
fn check_role_based_rules(board: &Board, sch: &Schematic, report: &mut ErcReport) {
    use std::collections::{HashMap, HashSet};
    /// All pins on a given net, with their resolved roles.
    type NetPinRoles = HashMap<String, Vec<(String, PinRole)>>;

    // Nets with a pour are implicitly "powered": the pour itself is
    // the supply geometry. Without this, every project with a GND
    // pour would fire an UnpoweredPowerNet warning on GND.
    let poured_nets: HashSet<&str> = board.pours.iter().map(|p| p.net.as_str()).collect();

    let mut roles: NetPinRoles = HashMap::new();
    for (net_name, net) in &sch.nets {
        let entries = roles.entry(net_name.clone()).or_default();
        for c in &net.connections {
            let Some(sym) = sch.symbols.get(&c.symbol_id) else {
                continue;
            };
            let role = sym
                .kind
                .pins()
                .into_iter()
                .find(|p| p.number == c.pin_number)
                .map(|p| p.role)
                .unwrap_or_default();
            entries.push((format!("{}.{}", sym.reference, c.pin_number), role));
        }
    }

    for (net_name, pins) in &roles {
        let outputs: Vec<&str> = pins
            .iter()
            .filter(|(_, r)| *r == PinRole::Output)
            .map(|(label, _)| label.as_str())
            .collect();
        let has_power_out = pins.iter().any(|(_, r)| *r == PinRole::PowerOut)
            || poured_nets.contains(net_name.as_str());
        let has_power_in = pins.iter().any(|(_, r)| *r == PinRole::PowerIn);
        let has_driver = pins
            .iter()
            .any(|(_, r)| matches!(r, PinRole::Output | PinRole::Bidir | PinRole::PowerOut))
            || poured_nets.contains(net_name.as_str());

        if outputs.len() >= 2 {
            report.push(Violation {
                kind: ErcKind::MultipleDrivers,
                severity: Severity::Error,
                message: format!(
                    "net {net_name} has {n} Output drivers ({}); only one Output pin may drive a net",
                    outputs.join(", "),
                    n = outputs.len(),
                ),
                involved: std::iter::once(net_name.clone())
                    .chain(outputs.iter().map(|s| s.to_string()))
                    .collect(),
            });
        }

        if has_power_in && !has_power_out {
            report.push(Violation {
                kind: ErcKind::UnpoweredPowerNet,
                severity: Severity::Warning,
                message: format!(
                    "net {net_name} has PowerIn pin(s) but no PowerOut source — did you forget to wire the supply?",
                ),
                involved: vec![net_name.clone()],
            });
        }

        for (label, role) in pins {
            if *role == PinRole::Input && !has_driver {
                report.push(Violation {
                    kind: ErcKind::UnconnectedInput,
                    severity: Severity::Warning,
                    message: format!(
                        "input pin {label} on net {net_name} has no driver (no Output, Bidir, or PowerOut)",
                    ),
                    involved: vec![net_name.clone(), label.clone()],
                });
            }
        }
    }
}

/// Heuristic: every IC's `PowerIn` pin should have a capacitor (any
/// kind, any value) on the same net within `max_dist_mm` of the
/// chip's body. The capacitor decouples high-frequency noise from
/// the rail; missing it is the most common "blue smoke" mistake.
///
/// We measure footprint-to-footprint center distance (not pad-to-pad)
/// — close enough for the heuristic, and avoids surprising the agent
/// with errors when a cap is far from the chip on a pad-by-pad metric
/// but visually right next to it.
fn check_decoupling(board: &Board, sch: &Schematic, max_dist_mm: f64, report: &mut ErcReport) {
    use std::collections::HashMap;
    // Map symbol_id -> footprint position (if placed on the board).
    // Symbols whose footprints aren't placed yet are skipped — the
    // heuristic only makes sense once positions exist.
    let mut sym_pos: HashMap<pcb_core::Id, (f64, f64)> = HashMap::new();
    for sym in sch.symbols_in_order() {
        if let Some(fp) = board
            .footprints
            .values()
            .find(|f| f.reference == sym.reference)
        {
            sym_pos.insert(sym.id, (fp.position.x.to_mm(), fp.position.y.to_mm()));
        }
    }

    // Group capacitor footprints by net.
    let mut caps_by_net: HashMap<String, Vec<(String, f64, f64)>> = HashMap::new();
    for sym in sch.symbols_in_order() {
        let is_cap = matches!(sym.kind, pcb_core::SymbolKind::Capacitor);
        if !is_cap {
            continue;
        }
        let Some(&(x, y)) = sym_pos.get(&sym.id) else {
            continue;
        };
        // Find every net this capacitor is on.
        for net in sch.nets.values() {
            if net.connections.iter().any(|c| c.symbol_id == sym.id) {
                caps_by_net.entry(net.name.clone()).or_default().push((
                    sym.reference.clone(),
                    x,
                    y,
                ));
            }
        }
    }

    for sym in sch.symbols_in_order() {
        // Only ICs care about decoupling caps. Discretes are caps
        // themselves or one-pin parts.
        if !matches!(sym.kind, pcb_core::SymbolKind::GenericIc { .. }) {
            continue;
        }
        let Some(&(sx, sy)) = sym_pos.get(&sym.id) else {
            continue;
        };
        for pin in sym.kind.pins() {
            if pin.role != PinRole::PowerIn {
                continue;
            }
            // What net is this pin on?
            let net_name = sch.net_for_pin(sym.id, &pin.number);
            let Some(net_name) = net_name else { continue };
            // Pours are decoupling-equivalent for power planes.
            if board.pours.iter().any(|p| p.net == net_name) {
                continue;
            }
            // Cap on this net within max_dist_mm of this chip?
            let close_cap = caps_by_net
                .get(net_name)
                .map(|caps| {
                    caps.iter().any(|(_, cx, cy)| {
                        let dx = cx - sx;
                        let dy = cy - sy;
                        (dx * dx + dy * dy).sqrt() <= max_dist_mm
                    })
                })
                .unwrap_or(false);
            if !close_cap {
                let label = format!("{}.{}", sym.reference, pin.number);
                report.push(Violation {
                    kind: ErcKind::MissingDecouplingCap,
                    severity: Severity::Warning,
                    message: format!(
                        "no decoupling cap within {max_dist_mm:.1} mm of {label} (net {net_name}); add e.g. 100 nF on the same net close to the pin",
                    ),
                    involved: vec![label, net_name.to_string()],
                });
            }
        }
    }
}

/// Heuristic: I²C nets named `SDA` / `SCL` (and common variants)
/// must have a pull-up resistor; the bus uses open-drain drivers and
/// the line floats high through the resistor. We only check the
/// well-known names — false positives on a custom-named bus are
/// worse than false negatives.
fn check_i2c_pullups(sch: &Schematic, report: &mut ErcReport) {
    use std::collections::HashSet;
    fn is_i2c(name: &str) -> bool {
        let n = name.to_ascii_uppercase();
        let n = n.trim_start_matches('+').trim_start_matches('-');
        let n = n.trim_start_matches("I2C_").trim_start_matches("I2C");
        matches!(n, "SDA" | "SCL")
            || n.ends_with("_SDA")
            || n.ends_with("_SCL")
            || n.starts_with("SDA")
            || n.starts_with("SCL")
    }

    // Resistor symbol_ids for quick membership check.
    let resistor_ids: HashSet<pcb_core::Id> = sch
        .symbols_in_order()
        .filter(|s| matches!(s.kind, pcb_core::SymbolKind::Resistor))
        .map(|s| s.id)
        .collect();

    for net in sch.nets.values() {
        if !is_i2c(&net.name) {
            continue;
        }
        let has_resistor = net
            .connections
            .iter()
            .any(|c| resistor_ids.contains(&c.symbol_id));
        if !has_resistor {
            report.push(Violation {
                kind: ErcKind::MissingPullup,
                severity: Severity::Warning,
                message: format!(
                    "I²C net {} has no pull-up resistor; add one to VCC (typical 4.7 kΩ)",
                    net.name,
                ),
                involved: vec![net.name.clone()],
            });
        }
    }
}

fn pin_label(sch: &Schematic, sym_id: pcb_core::Id, pin: &str) -> String {
    sch.symbols
        .get(&sym_id)
        .map(|s| format!("{}.{}", s.reference, pin))
        .unwrap_or_else(|| format!("?.{pin}"))
}
